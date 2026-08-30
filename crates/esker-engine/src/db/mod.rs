//! The database: one directory, many column families, one write-ahead log.
//!
//! * [`open`] — creating and recovering, including replaying the log
//! * [`mod@write`] — group commit, and where invariant 1 is enforced
//! * [`flush`] — switching memtables, and turning the full ones into L0 files
//! * [`iter`] — many versions in, one entry per user key out
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

pub mod flush;
pub mod iter;
pub mod merge;
pub mod open;
pub mod read;
pub mod snapshot;
pub mod table_cache;
pub mod write;

use std::collections::{BTreeMap, VecDeque};
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
}

impl Drop for Db {
    fn drop(&mut self) {
        // The flag is set while holding the lock the background thread waits on, so it either
        // has not taken the lock yet — and will see the flag — or is already waiting and gets
        // the notification. Setting it outside the lock loses the wake-up in between.
        if let Ok(_state) = self.inner.flush.lock() {
            self.inner.shutdown.store(true, Ordering::Release);
        } else {
            self.inner.shutdown.store(true, Ordering::Release);
        }
        self.inner.flush_wanted.notify_all();
        self.inner.flush_done.notify_all();
        if let Some(handle) = self.flusher.take() {
            // A background thread that panicked has already reported through `flush.error`;
            // there is nothing useful to do with the join result here.
            let _unused = handle.join();
        }
    }
}

/// The state behind a [`Db`]. Separate so that background work can hold a `Weak` to it and
/// stop when the last handle goes away.
#[derive(Debug)]
pub(crate) struct DbInner {
    pub(crate) fs: Arc<dyn FileSystem>,
    pub(crate) dir: PathBuf,
    pub(crate) options: Options,
    pub(crate) comparator: Arc<InternalKeyComparator>,
    pub(crate) versions: Mutex<VersionSet>,
    pub(crate) cfs: RwLock<BTreeMap<u32, Arc<ColumnFamily>>>,
    pub(crate) wal: Mutex<Wal>,
    pub(crate) writers: Mutex<WriteQueue>,
    pub(crate) write_ready: Condvar,
    pub(crate) table_cache: TableCache,
    /// Guards the background thread's wake-up flag and its last error.
    pub(crate) flush: Mutex<FlushState>,
    /// Signalled to wake the background thread.
    pub(crate) flush_wanted: Condvar,
    /// Signalled when it has finished a table, to release stalled writers.
    pub(crate) flush_done: Condvar,
    pub(crate) shutdown: AtomicBool,
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

    /// A named statistic, for `esker-cli` and for tests that need to see a stall rather than
    /// infer one (`docs/DESIGN.md` §4.4, §12).
    ///
    /// Recognised names: `esker.num-column-families`, `esker.snapshots`,
    /// `esker.last-sequence`, `esker.write-stalls`, `esker.write-slowdowns`,
    /// `esker.open-tables`, `esker.mem-table-size.<cf>`, `esker.num-immutable-mem-table.<cf>`,
    /// `esker.oldest-log.<cf>`, `esker.num-files-at-level<n>.<cf>`.
    pub fn property(&self, name: &str) -> Option<String> {
        let inner = &self.inner;
        match name {
            "esker.num-column-families" => Some(inner.cfs.read().ok()?.len().to_string()),
            "esker.snapshots" => Some(inner.snapshots.len().to_string()),
            "esker.last-sequence" => Some(self.last_seqno().to_string()),
            "esker.write-stalls" => Some(inner.stalls.load(Ordering::Relaxed).to_string()),
            "esker.write-slowdowns" => Some(inner.slowdowns.load(Ordering::Relaxed).to_string()),
            "esker.open-tables" => Some(inner.table_cache.len().to_string()),
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
