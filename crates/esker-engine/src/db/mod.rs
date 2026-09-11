//! The database: one directory, many column families, one write-ahead log.
//!
//! * [`open`] — creating and recovering, including replaying the log
//! * [`mod@write`] — group commit, and where invariant 1 is enforced
//! * [`flush`] — switching memtables, and turning the full ones into L0 files
//! * [`compact`] — running compactions against real files, and the pool that does it
//! * [`checkpoint`] — a consistent copy, made of hard links
//! * [`ingest`] — adopting an SST that was built elsewhere
//! * [`iter`] — many versions in, one entry per user key out
//! * [`level_iter`] — one cursor over a whole level, opening the file it has reached
//! * [`merge`] — several sorted cursors walked as one
//! * [`read`] — point lookups, through memtables and then down the levels
//! * [`table_cache`] — open SSTs, kept open
//! * [`snapshot`] — read positions, and the floor compaction may not collect below
//!
//! # What is shared and what is not
//!
//! Column families share three things and nothing else: the write-ahead log, the sequence
//! number space, and the comparator. That is what makes a `WriteBatch` spanning several of
//! them atomic — it is one log record — while letting each family have its own memtable,
//! levels, block size and filter (`docs/DESIGN.md` §4.8).
//!
//! # Sequence numbers
//!
//! Two counters, and the difference between them is the point. `next_seqno` is what the write
//! path hands out; `visible_seqno` is what readers are allowed to see. A group-commit leader
//! claims a range from the first, does the log write and the memtable inserts, and only then
//! publishes the second. Until it does, the writes are durable but invisible, which is exactly
//! the window in which a half-applied batch would otherwise be readable.

pub mod checkpoint;
pub mod compact;
pub mod flush;
pub mod ingest;
pub mod iter;
pub mod level_iter;
pub mod merge;
pub mod open;
pub mod read;
pub mod snapshot;
pub mod table_cache;
pub mod tier;
pub mod write;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;

pub use snapshot::{Snapshot, SnapshotList};

use crate::batch::WriteBatch;
use crate::dbformat::{InternalKeyComparator, SeqNo};
use crate::error::{Error, Result};
use crate::fs::FileSystem;
use crate::memtable::MemTable;
use crate::options::{CfOptions, Options};
use crate::version::VersionSet;
use crate::wal::LogWriter;

use table_cache::TableCache;

/// A log-structured key-value store in one directory.
///
/// Every method takes `&self` and concurrent readers and writers are the normal case, but the
/// handle itself is not `Clone`: it owns the background flush thread and stops it on drop.
/// Share it with an `Arc`, the way `LevelDB` and `RocksDB` are shared.
#[derive(Debug)]
pub struct Db {
    pub(crate) inner: Arc<DbInner>,
    /// `None` only after `Drop` has taken it to join.
    flusher: Option<JoinHandle<()>>,
    /// The bounded compaction pool (`docs/DESIGN.md` §14: two threads).
    compactors: Vec<JoinHandle<()>>,
    /// The uploader, present only when the filesystem has an object tier.
    uploader: Option<JoinHandle<()>>,
    /// The write-ahead log syncer, present only under [`crate::WalSyncMode::Interval`].
    syncer: Option<JoinHandle<()>>,
}

impl Drop for Db {
    fn drop(&mut self) {
        // The flag is set while holding the lock the background thread waits on, so it either
        // has not taken the lock yet — and will see the flag — or is already waiting and gets
        // the notification. Setting it outside the lock loses the wake-up in between.
        // Set under both locks the background threads wait on, so no wake-up is lost in the
        // window between a thread's check and its wait.
        if let Ok(_state) = self.inner.flush.lock() {
            self.inner.shutdown.store(true, Ordering::Release);
        }
        if let Ok(_state) = self.inner.compact.lock() {
            self.inner.shutdown.store(true, Ordering::Release);
        }
        if let Ok(_state) = self.inner.tier.lock() {
            self.inner.shutdown.store(true, Ordering::Release);
        }
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.flush_wanted.notify_all();
        self.inner.flush_done.notify_all();
        self.inner.compact_wanted.notify_all();
        self.inner.compaction_done.notify_all();
        self.inner.tier_wanted.notify_all();
        if let Some(handle) = self.uploader.take() {
            let _unused = handle.join();
        }
        for handle in self.compactors.drain(..) {
            let _unused = handle.join();
        }
        if let Some(handle) = self.syncer.take() {
            let _unused = handle.join();
        }
        if let Some(handle) = self.flusher.take() {
            // A background thread that panicked has already reported through `flush.error`;
            // there is nothing useful to do with the join result here.
            let _unused = handle.join();
        }
        // **The close syncs the log, whatever the mode says.** `Never` and `Interval` trade away
        // durability *for a crash*, and an orderly shutdown is not one: without this, closing a
        // database cleanly could lose its most recent writes, which is not a trade either mode
        // offers. After the syncer has been joined, so nothing is writing behind it.
        //
        // Ignored on failure and deliberately: a `Drop` cannot report, the process is going away,
        // and the log's own recovery is what covers a tail that did not reach the device.
        if let Err(error) = self.inner.sync_wal_now() {
            tracing::warn!(%error, "the write-ahead log could not be synced at close");
        }
    }
}

/// The state behind a [`Db`]. Separate so that background work can hold a `Weak` to it and
/// stop when the last handle goes away.
#[derive(Debug)]
pub(crate) struct DbInner {
    pub(crate) fs: Arc<dyn FileSystem>,
    pub(crate) dir: PathBuf,
    /// This process's claim on [`dir`](Self::dir), held for as long as the database is open and
    /// released when it closes — or when the process dies, which is the case that matters.
    ///
    /// `None` for a read-only open, which takes nothing: see [`crate::Options::read_only`].
    #[expect(dead_code, reason = "held for its lifetime, never read")]
    pub(crate) directory: Option<Box<dyn crate::fs::DirectoryLock>>,
    /// Whether this database refuses every write ([`crate::Options::read_only`]).
    pub(crate) read_only: bool,
    pub(crate) options: Options,
    pub(crate) comparator: Arc<InternalKeyComparator>,
    pub(crate) versions: Mutex<VersionSet>,
    pub(crate) cfs: RwLock<BTreeMap<u32, Arc<ColumnFamily>>>,
    pub(crate) wal: Mutex<Wal>,
    pub(crate) writers: Mutex<WriteQueue>,
    pub(crate) write_ready: Condvar,
    /// Behind an `Arc` so a [`level_iter::LevelCursor`] can hold the cache for the life of a
    /// scan without holding the whole database: the cursor opens the file it has reached and
    /// drops it on the way past, which needs the cache and nothing else.
    pub(crate) table_cache: Arc<TableCache>,
    /// Guards the background thread's wake-up flag and its last error.
    pub(crate) flush: Mutex<FlushState>,
    /// Signalled to wake the background thread.
    pub(crate) flush_wanted: Condvar,
    /// Signalled when it has finished a table, to release stalled writers.
    pub(crate) flush_done: Condvar,
    /// Guards the compaction pool's wake-up flag and its last error.
    pub(crate) compact: Mutex<CompactState>,
    /// Signalled to wake a compaction thread.
    pub(crate) compact_wanted: Condvar,
    /// Signalled when one finishes, so a waiter can look again.
    pub(crate) compaction_done: Condvar,
    /// What running compactions have claimed: their input files, and the key range each will
    /// write into one level ([`crate::db::compact::Reservations`]).
    pub(crate) compacting: Mutex<compact::Reservations>,
    /// Files written but not yet named by any version. The obsolete-file sweep skips them.
    pub(crate) pending_outputs: Mutex<BTreeSet<u64>>,
    /// Where the last compaction of `(cf, level)` stopped, so the next starts after it.
    /// In memory only: losing it costs the spreading and nothing else.
    pub(crate) compact_pointers: Mutex<BTreeMap<(u32, usize), Vec<u8>>>,
    pub(crate) compactions: AtomicU64,
    /// Tables a point read did not open because their bloom filter ruled the key out, and
    /// tables it did open. Together they say whether the filter is earning its bits.
    pub(crate) bloom_skips: AtomicU64,
    pub(crate) bloom_probes: AtomicU64,
    /// Stored entries an iterator has examined, across every scan this database has served.
    ///
    /// **The work a scan does, as opposed to the answer it returns.** A scan over MVCC data
    /// returns one row per key and walks one entry per *version*, so a set that dedupes the
    /// answer hides the cost completely: the rows do not grow and the steps to produce them grow
    /// with every commit in the range's history. That is #58, and a count is how it is asserted —
    /// a timer on a shared machine measures the machine.
    pub(crate) entries_stepped: Arc<AtomicU64>,
    pub(crate) shutdown: AtomicBool,
    /// Whether the uploader has work waiting, and the condvar it sleeps on.
    ///
    /// A pair of its own rather than a share of the compaction signal: an upload must not wake
    /// a compactor and a compaction must not wake the uploader, or one starves the other's
    /// wake-ups on a busy database.
    pub(crate) tier: Mutex<TierSignal>,
    /// Notified when [`DbInner::signal_tier`] has set `tier.wanted`.
    pub(crate) tier_wanted: Condvar,
    /// Times a writer was stopped outright, and times it was merely slowed. Both are
    /// properties, because a database that mysteriously goes slow is one nobody can operate.
    pub(crate) stalls: AtomicU64,
    pub(crate) slowdowns: AtomicU64,
    /// The next sequence number to hand out.
    pub(crate) next_seqno: AtomicU64,
    /// The highest sequence number a reader may see.
    pub(crate) visible_seqno: AtomicU64,
    pub(crate) snapshots: Arc<SnapshotList>,
}

/// What the compaction pool is doing, and what went wrong if anything did.
#[derive(Debug, Default)]
pub(crate) struct CompactState {
    /// Set when something has changed that may want compacting.
    pub(crate) wanted: bool,
    /// The first background failure.
    pub(crate) error: Option<String>,
}

/// Whether the uploader has something to do.
///
/// No `error` field, unlike its neighbours: an upload that fails is not an error anybody is
/// waiting on. The file stays local and readable and the tier retries
/// ([ADR 0024](../../../../docs/adr/0024-tiering-failure-semantics.md) decision 2), so there is
/// nothing to report to a foreground caller and nothing to hold onto.
#[derive(Debug, Default)]
pub(crate) struct TierSignal {
    /// Set when an SST has become durable, or when a retry is due.
    pub(crate) wanted: bool,
}

/// What the background flush thread is doing, and what went wrong if anything did.
#[derive(Debug, Default)]
pub(crate) struct FlushState {
    /// Set by a writer that has made a memtable immutable.
    pub(crate) wanted: bool,
    /// The first background failure. Reported to foreground callers rather than kept quiet
    /// while writes pile up behind it.
    pub(crate) error: Option<String>,
}

/// The log segment writes currently go to.
#[derive(Debug)]
pub(crate) struct Wal {
    pub(crate) writer: LogWriter,
    pub(crate) number: u64,
}

/// One column family: a name, its own options, and its own memtables.
#[derive(Debug)]
pub struct ColumnFamily {
    id: u32,
    name: String,
    options: CfOptions,
    pub(crate) mem: RwLock<MemState>,
}

/// A column family's in-memory tables: one taking writes, and those waiting to be flushed.
#[derive(Debug)]
pub(crate) struct MemState {
    pub(crate) active: Arc<MemTable>,
    /// The log segment the active table's writes are going to. A memtable and the segment it
    /// was filled from live and die together: the segment may be deleted only once the table
    /// is on disk.
    pub(crate) active_log: u64,
    /// Oldest first, each with the log segment it was filled from. The newest is at the back,
    /// so a read walks it backwards.
    pub(crate) immutable: Vec<(Arc<MemTable>, u64)>,
    /// **How many flushes of this family have finished, sweep included.**
    ///
    /// `immutable` empties three steps before the obsolete-file sweep runs, so a caller waiting
    /// on `immutable.is_empty()` was told its flush was done while the segment that flush
    /// reclaims was still on disk — the race
    /// `flushed_data_survives_a_reopen_and_the_old_log_is_reclaimed` lost once under load on
    /// 2026-09-10. This is bumped after the sweep and under this same lock, so a waiter that
    /// samples it before signalling has one predicate for "the whole job is over".
    ///
    /// Per family rather than one counter for the database: another family's flush finishing
    /// inside ours would otherwise satisfy the wait.
    pub(crate) swept: u64,
}

impl ColumnFamily {
    pub(crate) fn new(
        id: u32,
        name: String,
        options: CfOptions,
        comparator: &Arc<InternalKeyComparator>,
        log_number: u64,
    ) -> Self {
        Self {
            id,
            name,
            options,
            mem: RwLock::new(MemState {
                active: Arc::new(MemTable::new(Arc::clone(comparator))),
                active_log: log_number,
                immutable: Vec::new(),
                swept: 0,
            }),
        }
    }

    /// The id this family is known by in a [`WriteBatch`] and in the manifest.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The name it was created with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its options.
    pub fn options(&self) -> &CfOptions {
        &self.options
    }
}

/// One writer waiting its turn in a group commit.
#[derive(Debug)]
pub(crate) struct Pending {
    pub(crate) ticket: u64,
    pub(crate) batch: WriteBatch,
    pub(crate) sync: bool,
}

/// Writers waiting to be committed, and results for those already committed.
#[derive(Debug, Default)]
pub(crate) struct WriteQueue {
    pub(crate) next_ticket: u64,
    pub(crate) pending: VecDeque<Pending>,
    /// Filled in by the leader; each waiter removes its own entry.
    pub(crate) done: BTreeMap<u64, std::result::Result<SeqNo, String>>,
    /// True while a leader is committing with the queue lock released.
    pub(crate) writing: bool,
}

impl Db {
    /// The directory this database lives in.
    pub fn path(&self) -> &Path {
        &self.inner.dir
    }

    /// The filesystem it reads and writes through. `esker-cli` and the crash tests use it to
    /// look at the same files the engine does, through the same seam.
    pub fn filesystem(&self) -> &Arc<dyn FileSystem> {
        &self.inner.fs
    }

    /// The internal-key comparator every memtable and SST in this database is ordered by.
    pub fn comparator(&self) -> &Arc<InternalKeyComparator> {
        &self.inner.comparator
    }

    /// The column family called `name`.
    pub fn cf(&self, name: &str) -> Result<Arc<ColumnFamily>> {
        self.inner.cf_by_name(name)
    }

    /// The id of the column family called `name`.
    pub fn cf_id(&self, name: &str) -> Option<u32> {
        self.inner.cf_by_name(name).ok().map(|cf| cf.id())
    }

    /// Every open column family, by name.
    pub fn cf_names(&self) -> Vec<String> {
        self.inner.cfs.read().map_or_else(
            |_| Vec::new(),
            |cfs| cfs.values().map(|cf| cf.name().to_string()).collect(),
        )
    }

    /// Creates a column family and returns its id.
    ///
    /// The manifest edit is logged and made durable before the family exists in memory: a
    /// family that took writes and then vanished on reopen would lose them, while one the
    /// manifest knows about and this process does not is fixed by opening the database again.
    pub fn create_cf(&self, name: &str, options: CfOptions) -> Result<u32> {
        self.inner.writable("create a column family")?;
        let id = {
            let mut versions = lock(&self.inner.versions)?;
            // The manifest may roll on this edit, and a rolled manifest is the only remaining
            // record of the sequence number. Stamp it first; see `DbInner::log_and_apply`.
            versions.set_last_seqno(self.inner.visible_seqno.load(Ordering::Acquire));
            versions.create_cf(name)?
        };
        let log_number = lock(&self.inner.wal)?.number;
        let cf = Arc::new(ColumnFamily::new(
            id,
            name.to_string(),
            options,
            &self.inner.comparator,
            log_number,
        ));
        write_lock(&self.inner.cfs)?.insert(id, cf);
        Ok(id)
    }

    /// Drops a column family. Its files and memtables go with it.
    ///
    /// Writes already queued for it are logged and then skipped, exactly as replay skips log
    /// records for a family that no longer exists.
    pub fn drop_cf(&self, name: &str) -> Result<()> {
        self.inner.writable("drop a column family")?;
        let id = self.inner.cf_by_name(name)?.id();
        {
            let mut versions = lock(&self.inner.versions)?;
            versions.set_last_seqno(self.inner.visible_seqno.load(Ordering::Acquire));
            versions.drop_cf(name)?;
        }
        write_lock(&self.inner.cfs)?.remove(&id);
        self.inner.purge_and_evict()?;
        Ok(())
    }

    /// A read position: everything written so far is visible through it, nothing later is.
    pub fn snapshot(&self) -> Snapshot {
        self.inner
            .snapshots
            .acquire(self.inner.visible_seqno.load(Ordering::Acquire))
    }

    /// The highest sequence number a reader can currently see.
    pub fn last_seqno(&self) -> SeqNo {
        self.inner.visible_seqno.load(Ordering::Acquire)
    }

    /// The log segment writes are currently going to.
    pub fn wal_number(&self) -> Result<u64> {
        Ok(lock(&self.inner.wal)?.number)
    }

    /// Roughly how many bytes of `cf` lie in `[begin, end)`, where `None` is unbounded.
    ///
    /// Sums the sizes of the SSTs that overlap the range and adds the memtables' share of it.
    /// **Approximate on purpose**, and in three named directions:
    ///
    /// * a file **entirely inside** the range is counted in full and one that only *straddles* a
    ///   bound is counted as **half**, because interpolating properly means reading the file's
    ///   index block and this is a number a caller acts on in the large. A region is a contiguous
    ///   slice, so at most two files per level straddle its bounds — the error is a file or two,
    ///   not a level;
    /// * the memtables are counted by the fraction of their *entries* that fall in the range, not
    ///   their bytes, because a skip-list holds no per-range byte count;
    /// * overwritten and deleted keys are counted until a compaction drops them.
    ///
    /// All three over-count, which is the safe direction for the caller this exists for: a region
    /// split trigger that fires slightly early costs a split, while one that fires late costs a
    /// region that has outgrown its bounds (`docs/DESIGN.md` §6, `docs/plans/phase-4.md` §12.3).
    ///
    /// # It is bytes on disk, and it drops when a memtable is flushed
    ///
    /// A file's size is what the file *is* — compressed. A memtable's is the entries as they sit
    /// in memory. So the same data reports smaller once it has been flushed, by whatever the
    /// compression ratio is, and a caller watching the number will see it fall without anything
    /// having been deleted.
    ///
    /// That is the honest number rather than a wart to paper over: what a region costs is what it
    /// occupies, and `docs/DESIGN.md` §14's 96 MiB is a size on disk. A caller that needs "how
    /// much data is in here" independent of compression wants an entry count, which is a different
    /// question and not this one.
    pub fn approximate_size(
        &self,
        cf: &str,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<u64> {
        let handle = self.inner.cf_by_name(cf)?;
        let user = self.inner.comparator.user_comparator();
        let mut total: u64 = 0;

        let version = lock(&self.inner.versions)?.current();
        if let Some(cf_version) = version.cf(handle.id()) {
            for level in 0..cf_version.num_levels() {
                for file in cf_version.overlapping(level, begin, end, user.as_ref()) {
                    let smallest = crate::dbformat::extract_user_key(&file.smallest);
                    let largest = crate::dbformat::extract_user_key(&file.largest);
                    let inside = begin
                        .is_none_or(|begin| user.cmp(smallest, begin) != std::cmp::Ordering::Less)
                        && end.is_none_or(|end| user.cmp(largest, end) == std::cmp::Ordering::Less);
                    total += if inside { file.size } else { file.size / 2 };
                }
            }
        }

        // The memtables hold what has not reached a file yet. A skip-list has a byte count for
        // the whole table and no way to bound it by key, so the range's share is taken by
        // counting entries — one walk of the range against one walk of the table.
        let mem = read_lock(&handle.mem)?;
        let tables =
            std::iter::once(&mem.active).chain(mem.immutable.iter().map(|(table, _)| table));
        for table in tables {
            let bytes = table.approximate_size() as u64;
            if bytes == 0 {
                continue;
            }
            let entries = table.len() as u64;
            if entries == 0 {
                continue;
            }
            let mut cursor = table.iter();
            let mut in_range: u64 = 0;
            match begin {
                Some(begin) => cursor.seek(&crate::dbformat::lookup_key(
                    begin,
                    crate::dbformat::MAX_SEQNO,
                )),
                None => cursor.seek_to_first(),
            }
            while cursor.valid() {
                let key = crate::dbformat::extract_user_key(cursor.key());
                if end.is_some_and(|end| user.cmp(key, end) != std::cmp::Ordering::Less) {
                    break;
                }
                in_range += 1;
                cursor.next();
            }
            total += bytes * in_range / entries;
        }
        Ok(total)
    }

    /// A named statistic, for `esker-cli` and for tests that need to see a stall rather than
    /// infer one (`docs/DESIGN.md` §4.4, §12).
    ///
    /// # `esker.compactions-running` counts **files**, not compactions
    ///
    /// It is the size of the reservation set every compaction claims its *inputs* in
    /// (`db/compact.rs`, "two compactions must not touch one file"), so one compaction over
    /// five input files reports `5`. The name is the older of the two and the count is the true
    /// one; it is documented rather than renamed because `esker-cli` and existing tests ask for it
    /// by name. A caller wanting "is anything compacting" wants `> "0"`, and a caller wanting how
    /// many have *finished* wants [`Db::compactions_run`].
    ///
    /// Recognised names: `esker.num-column-families`, `esker.snapshots`,
    /// `esker.compaction-floor`, `esker.instance`,
    /// `esker.last-sequence`, `esker.write-stalls`, `esker.write-slowdowns`,
    /// `esker.open-tables`, `esker.table-cache-hits`, `esker.table-cache-misses`,
    /// `esker.table-cache-evictions`, `esker.compactions`, `esker.compactions-running`,
    /// `esker.bloom-skips`, `esker.bloom-probes`,
    /// `esker.mem-table-size.<cf>`, `esker.num-immutable-mem-table.<cf>`,
    /// `esker.oldest-log.<cf>`, `esker.block-size.<cf>`, `esker.num-files-at-level<n>.<cf>`.
    pub fn property(&self, name: &str) -> Option<String> {
        let inner = &self.inner;
        match name {
            "esker.num-column-families" => Some(inner.cfs.read().ok()?.len().to_string()),
            "esker.snapshots" => Some(inner.snapshots.len().to_string()),
            "esker.compaction-floor" => Some(inner.compaction_floor().to_string()),
            "esker.instance" => Some(inner.snapshots.instance().to_string()),
            "esker.last-sequence" => Some(self.last_seqno().to_string()),
            "esker.write-stalls" => Some(inner.stalls.load(Ordering::Relaxed).to_string()),
            "esker.write-slowdowns" => Some(inner.slowdowns.load(Ordering::Relaxed).to_string()),
            "esker.open-tables" => Some(inner.table_cache.len().to_string()),
            "esker.table-cache-hits" => Some(inner.table_cache.hits().to_string()),
            "esker.table-cache-misses" => Some(inner.table_cache.misses().to_string()),
            "esker.table-cache-evictions" => Some(inner.table_cache.evictions().to_string()),
            "esker.compactions" => Some(inner.compactions.load(Ordering::Relaxed).to_string()),
            "esker.bloom-skips" => Some(inner.bloom_skips.load(Ordering::Relaxed).to_string()),
            "esker.bloom-probes" => Some(inner.bloom_probes.load(Ordering::Relaxed).to_string()),
            "esker.entries-stepped" => {
                Some(inner.entries_stepped.load(Ordering::Relaxed).to_string())
            }
            "esker.compactions-running" => Some(
                inner
                    .compacting
                    .lock()
                    .map_or(0, |busy| busy.files())
                    .to_string(),
            ),
            _ => {
                let (prefix, cf_name) = name.rsplit_once('.')?;
                let cf = inner.cf_by_name(cf_name).ok()?;
                match prefix {
                    "esker.mem-table-size" => {
                        let mem = cf.mem.read().ok()?;
                        let total: usize = mem.active.approximate_size()
                            + mem
                                .immutable
                                .iter()
                                .map(|(table, _)| table.approximate_size())
                                .sum::<usize>();
                        Some(total.to_string())
                    }
                    "esker.num-immutable-mem-table" => {
                        Some(cf.mem.read().ok()?.immutable.len().to_string())
                    }
                    "esker.oldest-log" => {
                        let mem = cf.mem.read().ok()?;
                        Some(
                            mem.immutable
                                .first()
                                .map_or(mem.active_log, |(_, log)| *log)
                                .to_string(),
                        )
                    }
                    "esker.block-size" => {
                        // The *resolved* size, which is the only interesting one: the option can
                        // say `Storage`, and what that means depends on the filesystem.
                        Some(inner.table_options(&cf).block_size.to_string())
                    }
                    other => {
                        let level: usize = other
                            .strip_prefix("esker.num-files-at-level")?
                            .parse()
                            .ok()?;
                        let versions = inner.versions.lock().ok()?;
                        Some(versions.current().files(cf.id(), level).len().to_string())
                    }
                }
            }
        }
    }

    /// Every SST this database currently holds for `cf`, as `(level, file number)`.
    ///
    /// Exists for the invariant that keeps range tombstones sound — *no SST below L0 holds
    /// one* ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md) decision 6) — which cannot be
    /// checked without knowing which level a file is at. `esker-cli sst-dump` reports the
    /// tombstone count for one file; this is how a test sweeps all of them.
    pub fn files_by_level(&self, cf: &str) -> Result<Vec<(usize, u64)>> {
        let handle = self.inner.cf_by_name(cf)?;
        let version = lock(&self.inner.versions)?.current();
        let levels = version
            .cf(handle.id())
            .map_or(0, crate::version::CfVersion::num_levels);
        let mut out = Vec::new();
        for level in 0..levels {
            for file in version.files(handle.id(), level) {
                out.push((level, file.number));
            }
        }
        Ok(out)
    }
}

impl DbInner {
    /// Logs a manifest edit, stamping it with the sequence number reached so far.
    ///
    /// Every edit carries it, and it must: once a flush lets the log segments behind it be
    /// deleted, the manifest is the **only** remaining record of how far the sequence numbers
    /// got. Recovery takes the higher of the manifest's number and the replayed log's, so an
    /// edit that forgot to stamp it would silently restart numbering from an older point and
    /// hide every write that had been flushed.
    ///
    /// Stamping the *visible* number can overshoot when an unsynced write is lost in a power
    /// cut. Overshooting is harmless — a sequence number is never reused — while
    /// undershooting hands out one that is already in use.
    pub(crate) fn log_and_apply(&self, edit: &mut crate::version::VersionEdit) -> Result<()> {
        let mut versions = lock(&self.versions)?;
        versions.set_last_seqno(self.visible_seqno.load(Ordering::Acquire));
        versions.log_and_apply(edit)
    }

    /// Runs the installed pause hook, if a test installed one. Absent from a normal build.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn pause_at(&self, point: crate::testing::PausePoint) {
        if let Some(hook) = &self.options.pause_hook {
            hook.pause(point);
        }
    }

    /// The column family called `name`.
    pub(crate) fn cf_by_name(&self, name: &str) -> Result<Arc<ColumnFamily>> {
        let cfs = read_lock(&self.cfs)?;
        cfs.values()
            .find(|cf| cf.name() == name)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("no column family {name:?}")))
    }
}

/// Locks a mutex, turning a panic elsewhere into an error rather than a second panic.
///
/// A poisoned lock means some other thread died holding it, so the state behind it may be
/// half-updated. That is precisely [`Error::Poisoned`]: reopen, and let recovery re-derive the
/// truth from what reached the disk.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| poisoned())
}

pub(crate) fn read_lock<T>(lock: &RwLock<T>) -> Result<RwLockReadGuard<'_, T>> {
    lock.read().map_err(|_| poisoned())
}

pub(crate) fn write_lock<T>(lock: &RwLock<T>) -> Result<RwLockWriteGuard<'_, T>> {
    lock.write().map_err(|_| poisoned())
}

fn poisoned() -> Error {
    Error::Poisoned("a thread panicked while holding a database lock".to_string())
}
