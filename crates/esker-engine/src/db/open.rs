//! Opening a database, and replaying the log into memtables.
//!
//! # The rule recovery turns on
//!
//! A log that stops part-way through a record is normal — it is what a crash looks like — but
//! **only in the last segment**. Anywhere else it means a segment that was supposed to be
//! complete is not, which is a lost write rather than an unfinished one. Getting this backwards
//! is how an engine silently drops data it acknowledged, so the two cases are separate here
//! and the earlier one is an error (`docs/DESIGN.md` §4.3).
//!
//! # Where the sequence number comes from
//!
//! From **both** the manifest and the replayed log, whichever is higher. A flush that raced the
//! crash can leave either ahead: the manifest records a sequence number when a version is
//! installed, and the log holds every write since. Taking only one of them hands out a
//! sequence number that has already been used, which puts two different values under one
//! internal key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;

use crate::batch::WriteBatch;
use crate::dbformat::{InternalKeyComparator, SeqNo};
use crate::error::{Error, IoResultExt, Result};
use crate::filename::{self, FileKind};
use crate::fs::{FileSystem, LocalFileSystem, SstTier};
use crate::options::Options;
use crate::version::{VersionEdit, VersionSet};
use crate::wal::{LogReader, LogWriter, ReadOutcome};

use super::table_cache::TableCache;

/// How many SSTs are kept open at once. Small enough to bound file descriptors, large enough
/// that a hot working set is not reopened on every lookup.
const MAX_OPEN_TABLES: usize = 256;
use super::{ColumnFamily, CompactState, Db, DbInner, FlushState, SnapshotList, Wal, WriteQueue};

/// Column families a new database is created with, unless the caller names others.
pub const DEFAULT_COLUMN_FAMILIES: &[&str] = &[crate::cf::DEFAULT];

impl Db {
    /// Opens the database in `path` on the real filesystem.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        Self::open_with(
            path,
            options,
            Arc::new(LocalFileSystem::new()),
            DEFAULT_COLUMN_FAMILIES,
        )
    }

    /// Opens it with an explicit filesystem and set of column families.
    ///
    /// The filesystem is a parameter so that tests can inject faults and the simulator can
    /// replay a run (`docs/DESIGN.md` §13). `cfs` names the families the caller expects; any
    /// others already in the database are opened as well, because hiding data a database
    /// contains is worse than opening more than was asked for.
    pub fn open_with(
        path: impl AsRef<Path>,
        options: Options,
        fs: Arc<dyn FileSystem>,
        cfs: &[&str],
    ) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        let dir_for_error = dir.clone();
        let comparator = Arc::new(InternalKeyComparator::new(Arc::clone(&options.comparator)));
        let mut versions = open_versions(&fs, &dir, &comparator, &options, cfs)?;

        // Build a column family for every family the manifest knows about, not only the ones
        // the caller named. The log number is filled in below, once replay has said which
        // segment the recovered data came from.
        let mut families: BTreeMap<u32, Arc<ColumnFamily>> = BTreeMap::new();
        for (id, name) in versions.column_families().clone() {
            // A named override, or the defaults. Families are not interchangeable, and the
            // MVCC collector is the setting that must reach exactly one of them.
            let cf_options = options
                .cf_overrides
                .get(&name)
                .cloned()
                .unwrap_or_else(|| options.cf_options.clone());
            families.insert(
                id,
                Arc::new(ColumnFamily::new(
                    id,
                    name,
                    cf_options,
                    &comparator,
                    versions.log_number(),
                )),
            );
        }

        let replayed = replay_logs(fs.as_ref(), &dir, &versions, &options, &families)?;
        let last_seqno = versions.last_seqno().max(replayed.max_seqno);

        // Writes go to a fresh segment. The log number stays at the oldest segment whose
        // contents are still only in memory, so a crash before the first flush replays them
        // again; the flush that follows moves it forward.
        let wal_number = versions.new_file_number();
        let log_number = replayed.oldest_segment.unwrap_or(wal_number);
        for cf in families.values() {
            let mut mem = cf.mem.write().map_err(|_| {
                Error::Poisoned("a thread panicked while holding a memtable lock".to_string())
            })?;
            mem.active_log = log_number;
        }
        let wal_path = filename::wal(&dir, wal_number);
        let mut writer = LogWriter::new(
            fs.create(&wal_path).at(&wal_path)?,
            wal_path.display().to_string(),
        );
        writer.set_sync_call(options.sync_call);

        versions.set_last_seqno(last_seqno);
        versions.set_log_number(log_number);
        let mut edit = VersionEdit::new();
        edit.log_number = Some(log_number);
        versions.log_and_apply(&mut edit)?;

        let table_cache = Arc::new(TableCache::new(
            Arc::clone(&fs),
            dir.clone(),
            MAX_OPEN_TABLES,
            options.block_cache.clone(),
        ));
        let inner = Arc::new(DbInner {
            fs,
            dir,
            options,
            comparator,
            versions: Mutex::new(versions),
            cfs: RwLock::new(families),
            wal: Mutex::new(Wal {
                writer,
                number: wal_number,
            }),
            writers: Mutex::new(WriteQueue::default()),
            write_ready: Condvar::new(),
            table_cache,
            flush: Mutex::new(FlushState::default()),
            flush_wanted: Condvar::new(),
            flush_done: Condvar::new(),
            compact: Mutex::new(CompactState::default()),
            compact_wanted: Condvar::new(),
            compaction_done: Condvar::new(),
            compacting: Mutex::new(BTreeSet::new()),
            pending_outputs: Mutex::new(BTreeSet::new()),
            compact_pointers: Mutex::new(BTreeMap::new()),
            compactions: AtomicU64::new(0),
            bloom_skips: AtomicU64::new(0),
            bloom_probes: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            tier: Mutex::new(crate::db::TierSignal::default()),
            tier_wanted: Condvar::new(),
            stalls: AtomicU64::new(0),
            slowdowns: AtomicU64::new(0),
            next_seqno: AtomicU64::new(last_seqno + 1),
            visible_seqno: AtomicU64::new(last_seqno),
            snapshots: SnapshotList::new(),
        });

        let (flusher, compactors, uploader, syncer) = spawn_background(&inner, &dir_for_error)?;

        let db = Self {
            inner,
            flusher: Some(flusher),
            compactors,
            uploader,
            syncer,
        };
        db.purge_obsolete_files()?;
        Ok(db)
    }

    /// Deletes files no live version needs. Called at open, and after every flush and
    /// compaction from step 6b on.
    pub fn purge_obsolete_files(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut versions = super::lock(&self.inner.versions)?;
        versions.purge_obsolete_files()
    }
}

/// Starts the flush thread and the compaction pool.
///
/// Each holds a `Weak`, so dropping the last handle lets the state go even if a thread is
/// mid-wait. The pool is bounded rather than one thread per compaction: compaction is
/// throughput work, and an unbounded pool starves the foreground of the disk
/// (`docs/DESIGN.md` §4.7).
/// The flusher, the compaction pool, the uploader when there is a tier to upload to, and the
/// write-ahead log syncer when the sync mode names an interval.
type Background = (
    JoinHandle<()>,
    Vec<JoinHandle<()>>,
    Option<JoinHandle<()>>,
    Option<JoinHandle<()>>,
);

fn spawn_background(inner: &Arc<DbInner>, dir: &Path) -> Result<Background> {
    let weak = Arc::downgrade(inner);
    let flusher = std::thread::Builder::new()
        .name("esker-flush".to_string())
        .spawn(move || {
            if let Some(inner) = weak.upgrade() {
                inner.flush_loop();
            }
        })
        .map_err(|err| Error::io(dir, err))?;

    let mut compactors = Vec::new();
    for index in 0..inner.options.compaction_threads.max(1) {
        let weak = Arc::downgrade(inner);
        compactors.push(
            std::thread::Builder::new()
                .name(format!("esker-compact-{index}"))
                .spawn(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.compaction_loop();
                    }
                })
                .map_err(|err| Error::io(dir, err))?,
        );
    }
    // Only when there is something to upload to. A database on a plain filesystem does not
    // carry a thread whose whole job would be to find nothing to do.
    let uploader = if inner
        .fs
        .tier()
        .is_some_and(SstTier::wants_background_thread)
    {
        let weak = Arc::downgrade(inner);
        Some(
            std::thread::Builder::new()
                .name("esker-tier".to_string())
                .spawn(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.tier_loop();
                    }
                })
                .map_err(|err| Error::io(dir, err))?,
        )
    } else {
        None
    };

    // Only when the mode names one. A database that syncs per write, or never, does not carry a
    // thread whose whole job would be to have nothing to do.
    let syncer = match inner.options.wal_sync_mode {
        crate::options::WalSyncMode::Interval(interval) => {
            let weak = Arc::downgrade(inner);
            Some(
                std::thread::Builder::new()
                    .name("esker-wal-sync".to_string())
                    .spawn(move || {
                        if let Some(inner) = weak.upgrade() {
                            inner.wal_sync_loop(interval);
                        }
                    })
                    .map_err(|err| Error::io(dir, err))?,
            )
        }
        _ => None,
    };

    Ok((flusher, compactors, uploader, syncer))
}

/// Creates or recovers the version set, and makes sure every column family the caller named
/// exists.
fn open_versions(
    fs: &Arc<dyn FileSystem>,
    dir: &Path,
    comparator: &Arc<InternalKeyComparator>,
    options: &Options,
    cfs: &[&str],
) -> Result<VersionSet> {
    let current = filename::current(dir);
    let exists = fs.exists(&current).at(&current)?;
    if exists && options.error_if_exists {
        return Err(Error::InvalidArgument(format!(
            "{} already contains a database",
            dir.display()
        )));
    }

    let mut versions = if exists {
        VersionSet::recover(
            Arc::clone(fs),
            dir,
            Arc::clone(comparator),
            options.num_levels,
        )?
    } else if options.create_if_missing {
        VersionSet::create(
            Arc::clone(fs),
            dir,
            Arc::clone(comparator),
            options.num_levels,
            cfs,
        )?
    } else {
        return Err(Error::NotFound(format!(
            "{} does not contain a database and create_if_missing is off",
            dir.display()
        )));
    };

    for name in cfs {
        if versions.cf_id(name).is_none() {
            versions.create_cf(name)?;
        }
    }
    Ok(versions)
}

/// What replaying the log found.
struct Replayed {
    max_seqno: SeqNo,
    /// The lowest segment number that was replayed, if any.
    oldest_segment: Option<u64>,
}

fn replay_logs(
    fs: &dyn FileSystem,
    dir: &Path,
    versions: &VersionSet,
    options: &Options,
    families: &BTreeMap<u32, Arc<ColumnFamily>>,
) -> Result<Replayed> {
    // Sort by parsed number, not by name: the six-digit padding stops being enough at a
    // million segments, and a listing that silently reorders after that would be a
    // spectacular bug to find later.
    let mut segments: Vec<u64> = fs
        .list(dir)
        .at(dir)?
        .iter()
        .filter_map(|path| match filename::classify_path(path) {
            Some(FileKind::Wal(number)) => Some(number),
            _ => None,
        })
        .filter(|number| *number >= versions.log_number())
        .collect();
    segments.sort_unstable();

    let mut replayed = Replayed {
        max_seqno: 0,
        oldest_segment: segments.first().copied(),
    };
    let last = segments.last().copied();

    for number in &segments {
        let path = filename::wal(dir, *number);
        let file = fs.open(&path).at(&path)?;
        let mut reader = LogReader::new(file, path.display().to_string());
        loop {
            match reader.read_record()? {
                ReadOutcome::Record(bytes) => {
                    let batch = WriteBatch::from_bytes(&bytes)?;
                    apply_to_memtables(&batch, families, &mut replayed.max_seqno)?;
                }
                ReadOutcome::Eof => break,
                ReadOutcome::Torn(why) => {
                    // Legal at the tail of the segment that was open when the process died,
                    // and nowhere else.
                    if Some(*number) == last {
                        tracing::info!(segment = number, reason = %why, "log ends in a torn record");
                        break;
                    }
                    return Err(Error::corruption(
                        path.display().to_string(),
                        format!("a torn record in a segment that is not the last: {why}"),
                    ));
                }
                ReadOutcome::Corrupt(why) => {
                    if options.paranoid_checks {
                        return Err(Error::corruption(path.display().to_string(), why));
                    }
                    tracing::warn!(segment = number, reason = %why, "dropping the rest of a log segment");
                    break;
                }
            }
        }
    }
    Ok(replayed)
}

fn apply_to_memtables(
    batch: &WriteBatch,
    families: &BTreeMap<u32, Arc<ColumnFamily>>,
    max_seqno: &mut SeqNo,
) -> Result<()> {
    for entry in batch {
        let entry = entry?;
        *max_seqno = (*max_seqno).max(entry.seqno);
        match families.get(&entry.cf) {
            Some(cf) => {
                let mem = cf.mem.read().map_err(|_| {
                    Error::Poisoned("a thread panicked while holding a memtable lock".to_string())
                })?;
                // A range delete replays into the tombstone list beside the map, exactly as
                // it was applied when it was written
                // ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)). Replaying it into
                // the map instead would make a recovered database disagree with the one that
                // crashed, which is the one thing recovery may never do.
                if entry.kind == crate::dbformat::EntryKind::DeleteRange {
                    mem.active.add_range(entry.seqno, entry.key, entry.value);
                } else {
                    mem.active
                        .add(entry.seqno, entry.kind, entry.key, entry.value);
                }
            }
            None => {
                // The record's checksum passed, so the column family id is intact: this is a
                // family that was dropped after the record was written, and its data is meant
                // to be gone. Skipping is the correct outcome, not a silent loss.
                tracing::debug!(cf = entry.cf, "log entry for a dropped column family");
            }
        }
    }
    Ok(())
}
