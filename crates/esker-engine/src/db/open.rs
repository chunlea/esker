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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::batch::WriteBatch;
use crate::dbformat::{InternalKeyComparator, SeqNo};
use crate::error::{Error, IoResultExt, Result};
use crate::filename::{self, FileKind};
use crate::fs::{FileSystem, LocalFileSystem};
use crate::options::Options;
use crate::version::{VersionEdit, VersionSet};
use crate::wal::{LogReader, LogWriter, ReadOutcome};

use super::{ColumnFamily, Db, DbInner, SnapshotList, Wal, WriteQueue};

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
        let comparator = Arc::new(InternalKeyComparator::new(Arc::clone(&options.comparator)));
        let current = filename::current(&dir);
        let exists = fs.exists(&current).at(&current)?;

        if exists && options.error_if_exists {
            return Err(Error::InvalidArgument(format!(
                "{} already contains a database",
                dir.display()
            )));
        }

        let mut versions = if exists {
            VersionSet::recover(
                Arc::clone(&fs),
                &dir,
                Arc::clone(&comparator),
                options.num_levels,
            )?
        } else if options.create_if_missing {
            VersionSet::create(
                Arc::clone(&fs),
                &dir,
                Arc::clone(&comparator),
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

        // Build a column family for every family the manifest knows about, not only the ones
        // the caller named.
        let mut families: BTreeMap<u32, Arc<ColumnFamily>> = BTreeMap::new();
        for (id, name) in versions.column_families().clone() {
            families.insert(
                id,
                Arc::new(ColumnFamily::new(
                    id,
                    name,
                    options.cf_options.clone(),
                    &comparator,
                )),
            );
        }

        let replayed = replay_logs(fs.as_ref(), &dir, &versions, &options, &families)?;
        let last_seqno = versions.last_seqno().max(replayed.max_seqno);

        // Writes go to a fresh segment. The log number stays at the oldest segment whose
        // contents are still only in memory, so a crash before the next flush replays them
        // again.
        // TODO(step-6b): flush the recovered memtables here, as LevelDB does, and advance the
        // log number past them; until then segments accumulate across reopens.
        let wal_number = versions.new_file_number();
        let log_number = replayed.oldest_segment.unwrap_or(wal_number);
        let wal_path = filename::wal(&dir, wal_number);
        let writer = LogWriter::new(
            fs.create(&wal_path).at(&wal_path)?,
            wal_path.display().to_string(),
        );

        versions.set_last_seqno(last_seqno);
        versions.set_log_number(log_number);
        let mut edit = VersionEdit::new();
        edit.log_number = Some(log_number);
        versions.log_and_apply(&mut edit)?;

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
            next_seqno: AtomicU64::new(last_seqno + 1),
            visible_seqno: AtomicU64::new(last_seqno),
            snapshots: SnapshotList::new(),
        });

        let db = Self { inner };
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
                mem.active
                    .add(entry.seqno, entry.kind, entry.key, entry.value);
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
