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
use crate::fs::{DirectoryLock, FileSystem, LocalFileSystem, SstTier};
use crate::options::Options;
use crate::version::{VersionEdit, VersionSet};
use crate::wal::{LogReader, LogWriter, ReadOutcome};

use super::table_cache::TableCache;
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
        let read_only = options.is_read_only();
        let comparator = Arc::new(InternalKeyComparator::new(Arc::clone(&options.comparator)));
        let (mut versions, directory) = open_versions(&fs, &dir, &comparator, &options, cfs)?;

        let families = column_families(&versions, &options, &comparator);

        let replayed = replay_logs(fs.as_ref(), &dir, &versions, &options, &families)?;
        let last_seqno = versions.last_seqno().max(replayed.max_seqno);

        // Writes go to a fresh segment. The log number stays at the oldest segment whose
        // contents are still only in memory, so a crash before the first flush replays them
        // again; the flush that follows moves it forward.
        // A reader takes the number the recovered version already names rather than minting one:
        // minting is harmless in memory, and asking for a file number a reader will never use is
        // the kind of thing that stops being harmless the day somebody persists it.
        let wal_number = if read_only {
            versions.log_number()
        } else {
            versions.new_file_number()
        };
        let log_number = replayed.oldest_segment.unwrap_or(wal_number);
        for cf in families.values() {
            let mut mem = cf.mem.write().map_err(|_| {
                Error::Poisoned("a thread panicked while holding a memtable lock".to_string())
            })?;
            mem.active_log = log_number;
        }
        let mut writer = log_writer(fs.as_ref(), &dir, read_only, wal_number)?;
        writer.set_sync_call(options.sync_call);

        versions.set_last_seqno(last_seqno);
        if !read_only {
            versions.set_log_number(log_number);
            let mut edit = VersionEdit::new();
            edit.log_number = Some(log_number);
            versions.log_and_apply(&mut edit)?;
        }

        let table_cache = Arc::new(TableCache::new(
            Arc::clone(&fs),
            dir.clone(),
            options.max_open_tables,
            options.block_cache.clone(),
        ));
        let inner = Arc::new(DbInner {
            fs,
            dir,
            directory,
            read_only,
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
            compacting: Mutex::new(crate::db::compact::Reservations::default()),
            pending_outputs: Mutex::new(BTreeSet::new()),
            compact_pointers: Mutex::new(BTreeMap::new()),
            compactions: AtomicU64::new(0),
            bloom_skips: AtomicU64::new(0),
            bloom_probes: AtomicU64::new(0),
            entries_stepped: Arc::new(AtomicU64::new(0)),
            shutdown: AtomicBool::new(false),
            tier: Mutex::new(crate::db::TierSignal::default()),
            tier_wanted: Condvar::new(),
            stalls: AtomicU64::new(0),
            slowdowns: AtomicU64::new(0),
            next_seqno: AtomicU64::new(last_seqno + 1),
            visible_seqno: AtomicU64::new(last_seqno),
            snapshots: SnapshotList::new(),
        });

        // **A reader starts nothing and deletes nothing.** Both of those are the writer's, and a
        // reader that swept would delete files out from under the process that owns them.
        if read_only {
            return Ok(Self {
                inner,
                flusher: None,
                compactors: Vec::new(),
                uploader: None,
                syncer: None,
            });
        }

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
        self.inner.writable("sweep its obsolete files")?;
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
/// How long [`claim`] waits for a directory somebody else still holds.
///
/// **A held directory is held for ever, or for a moment.** A live writer keeps its claim for as
/// long as it lives, so waiting cannot let two of them through — no amount of patience turns a
/// running store into a stopped one. What waiting does let through is the *other* case, which is
/// the common one and was not survivable: a caller that closed a database and reopened the same
/// directory, where "closed" is not instantaneous. `Store::stop` aborts its tasks, and `abort` is
/// a request — the runtime drops the future, and everything it holds, when it next gets to it.
///
/// That cost a gate on 2026-09-10: `esker-store::schema_fetch`'s
/// `a_learner_without_the_catalog_fetches_the_schema_and_answers` does `stop(); drop; open` and
/// met `InUse` at 0.026 s, in a run whose only other change was in another crate. **It did not
/// reproduce**: a hundred rounds of open/stop/drop/open under a deliberately starved runtime, in
/// two different store arrangements, stayed green with and without a fix aimed at the tasks. So
/// this is not a fix for a mechanism that was cornered — it is the shape that does not need one
/// cornered, because it cannot admit a second live writer however long it waits.
///
/// Five seconds because it is far longer than any shutdown takes and far shorter than an operator
/// waits before reading the message; the refusal, when it comes, says the same thing it always did.
const CLAIM_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// Who holds `dir`'s claim, from the note the holder left in the lock file.
///
/// Best effort by construction: the file is written after the lock is taken and read without one,
/// so a reader can meet it empty or half-written. Every one of those answers is "not known", and
/// none of them changes what the caller is told to do.
fn holder_of(fs: &dyn FileSystem, dir: &Path) -> String {
    let path = dir.join(crate::fs::LOCK_FILE);
    let Ok(file) = fs.open(&path) else {
        return "no holder record".to_owned();
    };
    let mut bytes = [0_u8; 256];
    let Ok(read) = file.read_at(0, &mut bytes) else {
        return "no holder record".to_owned();
    };
    let line = String::from_utf8_lossy(&bytes[..read]);
    let Some(line) = line.lines().next().filter(|line| !line.is_empty()) else {
        return "no holder record".to_owned();
    };
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(name))
            .map(str::to_owned)
    };
    let (Some(pid), Some(exe)) = (field("pid="), field("exe=")) else {
        // Something is in the file and it is not ours to interpret. Said verbatim, because a
        // reader chasing this would rather see the bytes than a summary of them.
        return format!("the lock file says {line:?}");
    };
    match field("since_unix=").and_then(|since| since.parse::<u64>().ok()) {
        Some(since) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |now| now.as_secs());
            format!(
                "pid {pid}, {exe}, holding it for {}s",
                now.saturating_sub(since)
            )
        }
        None => format!("pid {pid}, {exe}"),
    }
}

/// The log this open writes to, or one that refuses every byte.
///
/// **A read-only open creates no segment.** The writer exists because the database has one; every
/// path that would use it is refused before it gets here ([`DbInner::writable`]), and if one ever
/// is not, [`RefusesToWrite`] says so rather than dropping the bytes.
fn log_writer(
    fs: &dyn FileSystem,
    dir: &Path,
    read_only: bool,
    wal_number: u64,
) -> Result<LogWriter> {
    if read_only {
        return Ok(LogWriter::new(
            Box::new(RefusesToWrite) as Box<dyn crate::fs::WritableFile>,
            "a read-only database has no log".to_owned(),
        ));
    }
    let path = filename::wal(dir, wal_number);
    Ok(LogWriter::new(
        fs.create(&path).at(&path)?,
        path.display().to_string(),
    ))
}

/// One [`ColumnFamily`] per family the **manifest** knows about, not per family the caller named.
///
/// Hiding data a database contains is worse than opening more than was asked for.
fn column_families(
    versions: &VersionSet,
    options: &Options,
    comparator: &Arc<InternalKeyComparator>,
) -> BTreeMap<u32, Arc<ColumnFamily>> {
    let mut families = BTreeMap::new();
    for (id, name) in versions.column_families().clone() {
        // A named override, or the defaults. Families are not interchangeable, and the MVCC
        // collector is the setting that must reach exactly one of them.
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
                comparator,
                versions.log_number(),
            )),
        );
    }
    families
}

/// A writable file that refuses every byte, for a database opened read-only.
///
/// The log writer is a field of the database, so a read-only open has to have one; it must never
/// have anything to write. If a path is ever found that reaches it, this says so loudly rather
/// than silently accepting bytes nobody will ever read back.
#[derive(Debug)]
struct RefusesToWrite;

impl crate::fs::WritableFile for RefusesToWrite {
    fn append(&mut self, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "this database is open read-only and has no log",
        ))
    }

    fn sync_data(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn sync_all(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Claims `dir` for this process, or says who has it.
///
/// Never a permanent block: [`CLAIM_WITHIN`] bounds it, and past that the answer is
/// [`Error::InUse`] — a node that waited on a held directory for ever would be a node an operator
/// reads as hung.
fn claim(fs: &dyn FileSystem, dir: &Path) -> Result<Box<dyn DirectoryLock>> {
    let deadline = std::time::Instant::now() + CLAIM_WITHIN;
    loop {
        match fs.lock_directory(dir) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(Error::InUse {
                        dir: dir.to_path_buf(),
                        holder: holder_of(fs, dir),
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(Error::io(dir, error)),
        }
    }
}

/// The version set, and this process's claim on the directory it came from.
///
/// **The claim is taken before the first byte is read or written**, which is why it is here rather
/// than in the caller: this function is where the directory either exists, comes to exist, or is
/// refused, and the claim has to be on the near side of all three.
///
/// It takes two attempts and not one. A directory that is not there yet cannot be claimed, and
/// creating one in order to claim it would make an open that is about to refuse leave a directory
/// behind to prove it was here — `create_if_missing` is off by default, so that refusal is the
/// common case for a mistyped path. A database that is created instead is claimed the moment it
/// exists, and two processes creating one at the same instant are separated by `CURRENT`, which
/// [`VersionSet::create`] creates exclusively.
fn open_versions(
    fs: &Arc<dyn FileSystem>,
    dir: &Path,
    comparator: &Arc<InternalKeyComparator>,
    options: &Options,
    cfs: &[&str],
) -> Result<(VersionSet, Option<Box<dyn DirectoryLock>>)> {
    // **A reader takes nothing.** See [`OpenMode::ReadOnly`]: a shared lock would be refused by
    // the very writer this open exists to look at, and a reader that writes nothing has nothing
    // to protect from one.
    let claimed = if options.is_read_only() || !fs.exists(dir).at(dir)? {
        None
    } else {
        Some(claim(fs.as_ref(), dir)?)
    };
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

    if options.is_read_only() {
        // No claim and no family: what is there is what is reported.
        return Ok((versions, None));
    }
    // Before `create_cf` below, which writes a manifest edit.
    let directory = match claimed {
        Some(lock) => lock,
        None => claim(fs.as_ref(), dir)?,
    };

    for name in cfs {
        if versions.cf_id(name).is_none() {
            versions.create_cf(name)?;
        }
    }
    Ok((versions, Some(directory)))
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
                    // and nowhere else — **unless this open is a reader**, which cannot tell a
                    // writer that is mid-record from a file that is damaged. A reader that is
                    // not the owner reports what it could read and does not call the database
                    // corrupt on the strength of a race it was never party to.
                    if Some(*number) == last || options.is_read_only() {
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
                // ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)). Replaying it into
                // the map instead would make a recovered database disagree with the one that
                // crashed, which is the one thing recovery may never do.
                if entry.kind == crate::dbformat::EntryKind::DeleteRange {
                    mem.active.add_range(entry.seqno, entry.key, entry.value);
                } else {
                    // **Recovery fails rather than opens short.** An arena that refuses here
                    // means this database cannot hold its own log, and an open that carried on
                    // would present a database missing writes that were acknowledged before the
                    // crash — the same silent loss as on the write path, arrived at from the
                    // other direction (`docs/plans/debt-c6.md` §15).
                    mem.active
                        .add(entry.seqno, entry.kind, entry.key, entry.value)?;
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
