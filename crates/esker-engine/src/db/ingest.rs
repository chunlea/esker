//! Ingest: adopting an SST that was built elsewhere.
//!
//! Phase 4 receives a region as a checkpoint's files and has to make them part of the local
//! database; phase 6 bulk-loads a table the same way (`docs/DESIGN.md` §4.1). Neither wants
//! the data read out and written back in, so ingest links the file into place, gives it a
//! number and names it in a manifest edit. The bytes are never touched.
//!
//! # The v1 limitation, stated plainly
//!
//! **An ingested file's key range must not overlap anything already in the column family.**
//!
//! The reason is sequence numbers. A file built elsewhere carries the sequence numbers of the
//! database that built it, and this one's numbering is unrelated — so if the two ranges
//! overlapped there would be no answer to "which version of this key is newer" that was not a
//! guess. `RocksDB` solves it by rewriting the file's sequence numbers on the way in; that is
//! a v2 feature here, and until then an overlap is an error rather than a silent reordering.
//!
//! Both callers this exists for hand over a range nothing else holds: a region being moved,
//! and a table being created. The check is cheap and it makes the semantics unambiguous.
//!
//! What ingest does do about sequence numbers is raise the database's own above the file's, so
//! that every later write sorts above the ingested data and the ingested data is visible to
//! reads at the current snapshot.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::dbformat::extract_user_key;
use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::sst::TableReader;
use crate::version::{FileMeta, VersionEdit};

use super::{ColumnFamily, Db, DbInner, lock, read_lock};

impl Db {
    /// Adds externally built SSTs to `cf`.
    ///
    /// Flushes the column family first, then checks that no level and no memtable holds a key
    /// in any file's range — see the module documentation for why an overlap is refused rather
    /// than merged. Each file is hard-linked into the database (copied if the filesystem will
    /// not link it) and placed at the deepest level where it fits, so a bulk load does not
    /// pile up at L0 and immediately compact itself back down.
    ///
    /// Either every file is ingested or none is: the manifest edit is one record.
    pub fn ingest(&self, cf: &str, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let handle = self.inner.cf_by_name(cf)?;
        // A memtable can hold a key in the range, and it is not a file that can be checked
        // against; flushing turns it into one.
        self.flush(cf)?;

        let table_options = self.inner.table_options(&handle);
        let mut adopted: Vec<(FileMeta, usize)> = Vec::new();
        let mut highest_seqno = 0;

        for path in paths {
            let meta = self.inner.describe(path, &table_options)?;
            highest_seqno = highest_seqno.max(meta.largest_seqno);
            let level = self.inner.place(&handle, &meta, &adopted)?;
            adopted.push((meta, level));
        }

        // Link everything before the edit: a file the manifest names must already exist.
        let mut edit = VersionEdit::new();
        for (index, (meta, level)) in adopted.iter_mut().enumerate() {
            let number = lock(&self.inner.versions)?.new_file_number();
            self.inner.hold_pending(number)?;
            meta.number = number;
            self.inner.adopt_file(&paths[index], number)?;
            edit.add_file(
                handle.id(),
                u32::try_from(*level).unwrap_or(u32::MAX),
                meta.clone(),
            );
        }

        // Later writes must sort above what was just ingested, or a put would be invisible
        // behind a file that carries a higher sequence number than the database has issued.
        self.inner.raise_seqno_above(highest_seqno);
        self.inner.log_and_apply(&mut edit)?;
        for (meta, _) in &adopted {
            self.inner.drop_pending(meta.number)?;
        }
        self.inner.purge_and_evict()?;
        tracing::info!(
            cf = handle.id(),
            files = adopted.len(),
            "ingested external files"
        );
        Ok(())
    }
}

impl DbInner {
    /// Reads an external file's footer and properties to learn what it holds.
    fn describe(&self, path: &Path, options: &crate::sst::TableOptions) -> Result<FileMeta> {
        let file = self.fs.open(path).at(path)?;
        let size = file.size().at(path)?;
        // File number zero is a placeholder: it only names cache entries, and the real number
        // is assigned once the file is known to be ingestable.
        let reader = TableReader::open(file, 0, options.clone(), None)?;
        let properties = reader.properties();
        if properties.entry_count == 0 {
            return Err(Error::InvalidArgument(format!(
                "{} has no entries",
                path.display()
            )));
        }
        Ok(FileMeta {
            number: 0,
            size,
            smallest: properties.smallest_key.clone(),
            largest: properties.largest_key.clone(),
            smallest_seqno: properties.smallest_seqno,
            largest_seqno: properties.largest_seqno,
        })
    }

    /// The deepest level `meta` can go to without overlapping anything.
    ///
    /// Fails if it overlaps existing data at any level, or another file in the same ingest.
    fn place(
        &self,
        cf: &Arc<ColumnFamily>,
        meta: &FileMeta,
        already: &[(FileMeta, usize)],
    ) -> Result<usize> {
        let user = self.comparator.user_comparator().as_ref();
        let begin = extract_user_key(&meta.smallest);
        let end = extract_user_key(&meta.largest);

        for (other, _) in already {
            let other_begin = extract_user_key(&other.smallest);
            let other_end = extract_user_key(&other.largest);
            if user.cmp(begin, other_end) != std::cmp::Ordering::Greater
                && user.cmp(other_begin, end) != std::cmp::Ordering::Greater
            {
                return Err(Error::Unsupported(
                    "two files in one ingest cover the same keys; overlapping ingest is a v2 \
                     feature (docs/DESIGN.md §4.1)"
                        .to_string(),
                ));
            }
        }

        // Nothing may be left in the memtable either: `ingest` flushed, but a concurrent
        // writer can have added to the new one.
        let mem = read_lock(&cf.mem)?;
        let mut cursor = mem.active.iter();
        cursor.seek(&crate::dbformat::lookup_key(
            begin,
            crate::dbformat::MAX_SEQNO,
        ));
        if cursor.valid()
            && user.cmp(extract_user_key(cursor.key()), end) != std::cmp::Ordering::Greater
        {
            return Err(Error::Unsupported(
                "a write arrived in the ingested range while ingest was running; overlapping \
                 ingest is a v2 feature (docs/DESIGN.md §4.1)"
                    .to_string(),
            ));
        }
        drop(mem);

        let version = lock(&self.versions)?.current();
        let Some(cf_version) = version.cf(cf.id()) else {
            return Ok(0);
        };
        let mut target = 0;
        for level in 0..cf_version.num_levels() {
            if !cf_version
                .overlapping(level, Some(begin), Some(end), user)
                .is_empty()
            {
                return Err(Error::Unsupported(format!(
                    "the ingested range overlaps existing data at level {level}; overlapping \
                     ingest is a v2 feature (docs/DESIGN.md §4.1)"
                )));
            }
            // No overlap here, so the file can sink at least this far.
            target = level;
        }
        Ok(target)
    }

    /// Links `path` into the database as `number`, copying if it cannot be linked.
    fn adopt_file(&self, path: &Path, number: u64) -> Result<()> {
        let target = filename::sst(&self.dir, number);
        if self.fs.hard_link(path, &target).is_ok() {
            return Ok(());
        }
        let reader = self.fs.open(path).at(path)?;
        let size = reader.size().at(path)?;
        let mut writer = self.fs.create(&target).at(&target)?;
        let mut offset = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        while offset < size {
            let read = reader.read_at(offset, &mut buffer).at(path)?;
            if read == 0 {
                break;
            }
            writer.append(&buffer[..read]).at(&target)?;
            offset += read as u64;
        }
        writer.sync_data().at(&target)
    }

    /// Moves the sequence number above `seqno`, never backwards.
    fn raise_seqno_above(&self, seqno: u64) {
        self.next_seqno.fetch_max(seqno + 1, Ordering::SeqCst);
        self.visible_seqno.fetch_max(seqno, Ordering::Release);
    }
}
