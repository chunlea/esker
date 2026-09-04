//! Ingest: adopting an SST that was built elsewhere.
//!
//! Phase 4 receives a region as a checkpoint's files and has to make them part of the local
//! database; phase 6 bulk-loads a table the same way (`docs/DESIGN.md` §4.1). Neither wants
//! the data read out and written back in, so ingest links the file into place, gives it a
//! number and names it in a manifest edit. The bytes are never touched.
//!
//! # The rule, stated as a rule about keys
//!
//! **An ingest is refused exactly when the file holds a user key the column family already has
//! an entry for** — a value, a point tombstone, or a range tombstone covering it.
//!
//! The reason is sequence numbers, and it is worth being exact about *where* they bite. A file
//! built elsewhere carries the sequence numbers of the database that built it, and this one's
//! numbering is unrelated, so for two entries under the **same** user key there is no answer to
//! "which is newer" that is not a guess. For entries under **different** user keys the question
//! is never asked: a read resolves one user key at a time, finds exactly one version of it, and
//! the sequence number decides nothing. `RocksDB` widens this further by rewriting the file's
//! sequence numbers on the way in; that is still a v2 feature here, and it is what a *key* overlap
//! would need.
//!
//! # Why that is not the same as "the ranges do not overlap", and why it matters here
//!
//! Range disjointness is a cheap sufficient proxy for key disjointness, and it is a bad fit for
//! this system in particular. `esker-txn` encodes the MVCC version **into the key** —
//! `'x' ++ enc(user_key) ++ !ts` (`docs/DESIGN.md` §3) — so two versions of one row are two
//! distinct engine keys, and two files can interleave completely across a range while sharing not
//! one key. A bulk load of a time range for rows that already exist is exactly that shape: it
//! overlaps everything and collides with nothing. Refusing it was refusing arithmetic on the
//! ranges, not a real ambiguity.
//!
//! # The structural constraint is separate, and it is not a reason to refuse
//!
//! Levels 1 and below are sorted runs: their files must be range-disjoint or a seek cannot binary
//! search the level. L0 has no such rule — its files overlap by construction and a read opens each
//! of them. So a file whose *keys* are free but whose *range* is not is not a refusal, it is a
//! placement: it goes to L0, and a later compaction sorts it downward like anything else. The
//! deepest level whose ranges leave room is still preferred, so a genuinely disjoint bulk load
//! still lands deep and does not immediately compact itself back down.
//!
//! # What the check consults, and why it is the read path's own list
//!
//! `DbInner::merge_sources` — the same cursors and the same range-tombstone set a read builds,
//! from the same pinned version. An ingest that consulted a different set than a read would refuse
//! ingests that are safe or, far worse, allow one whose keys a reader can already see. The
//! memtable is flushed first so that what is checked is what is on disk, and the active memtable
//! is checked anyway, because a concurrent writer can have added to the new one.
//!
//! # What ingest does with sequence numbers, and the one thing it does not
//!
//! It raises the database's own above the file's, so that every later write sorts above the
//! ingested data and the ingested data is visible to reads at the current snapshot.
//!
//! It does **not** give the file a sequence number of this database's own, so an ingested entry
//! keeps a number from a numbering this database never issued. Where that shows is a *reader that
//! is older than the ingest*: a snapshot taken before it can see the ingested keys, because their
//! numbers may fall below its own. Recorded as a debt in `docs/plans/debt-c6.md` rather than fixed
//! here — closing it means a per-file global sequence number in the SST footer, which is a format
//! change with a golden test. It is unaffected by the rule above: it is about *when* an ingest
//! becomes visible, not about which of two versions wins.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::dbformat::{MAX_SEQNO, extract_user_key, lookup_key};
use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::iterator::Cursor;
use crate::range_del::RangeTombstones;
use crate::sst::{TableOptions, TableReader};
use crate::version::{FileLocation, FileMeta, VersionEdit};

use super::iter::table_cursor;
use super::merge::MergeCursor;
use super::{ColumnFamily, Db, DbInner, lock};

/// Why one user key stops an ingest, carrying the key so the message can name it.
///
/// Two variants and not one, because they are two different things to have got wrong: `Held` is
/// data the caller did not know was there, and `Deleted` is a deletion the caller is about to
/// un-delete or not, depending on numbers nobody can compare.
enum Conflict {
    /// The column family holds an entry for it: a value, or a point tombstone.
    Held(Vec<u8>),
    /// A range tombstone in the column family covers it.
    Deleted(Vec<u8>),
    /// Another file of this same ingest holds it.
    Sibling(Vec<u8>),
}

impl Conflict {
    /// The refusal, in the words the caller can act on.
    fn message(&self, path: &Path) -> String {
        match self {
            Self::Held(key) => format!(
                "{} holds key {:?}, which this column family already has an entry for; \
                 overlapping ingest of the same key is a v2 feature (docs/DESIGN.md §4.1)",
                path.display(),
                bytes::Bytes::copy_from_slice(key)
            ),
            Self::Deleted(key) => format!(
                "{} holds key {:?}, which a range tombstone in this column family covers; \
                 overlapping ingest of the same key is a v2 feature (docs/DESIGN.md §4.1)",
                path.display(),
                bytes::Bytes::copy_from_slice(key)
            ),
            Self::Sibling(key) => format!(
                "{} and an earlier file of this ingest hold the same key {:?}; overlapping \
                 ingest of the same key is a v2 feature (docs/DESIGN.md §4.1)",
                path.display(),
                bytes::Bytes::copy_from_slice(key)
            ),
        }
    }
}

impl Db {
    /// Adds externally built SSTs to `cf`.
    ///
    /// Flushes the column family first, then refuses any file holding a **user key** this column
    /// family already has an entry for — see the module documentation for why that, and not a
    /// range overlap, is the rule. Each accepted file is hard-linked into the database (copied if
    /// the filesystem will not link it) and placed at the deepest level whose ranges leave room,
    /// which is L0 when none does, so a disjoint bulk load still lands deep and an interleaved one
    /// is still adopted.
    ///
    /// Either every file is ingested or none is: the manifest edit is one record, and every
    /// refusal happens before the first file is linked.
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

        for (index, path) in paths.iter().enumerate() {
            let meta = self.inner.describe(path, &table_options)?;
            highest_seqno = highest_seqno.max(meta.largest_seqno);
            // Permission first, then placement. A file that may not be adopted is refused before
            // anything is linked, so a refusal leaves the database exactly as it was.
            if let Some(conflict) =
                self.inner
                    .first_conflicting_key(&handle, path, &paths[..index], &table_options)?
            {
                return Err(Error::Unsupported(conflict.message(path)));
            }
            let level = self.inner.place(&handle, &meta)?;
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
    fn describe(&self, path: &Path, options: &TableOptions) -> Result<FileMeta> {
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
            location: FileLocation::Local,
        })
    }

    /// The first user key this file may not adopt, or `None` when every one of them is free.
    ///
    /// The whole of the rule in the module header, and the only thing that can refuse an ingest.
    /// A key is not free when the column family holds **any** entry for it — a value or a point
    /// tombstone, which the merged cursor finds because it walks internal keys and does not
    /// resolve them — or when a range tombstone covers it, which no cursor would show because a
    /// range delete hides keys the merged run has never seen (ADR 0017).
    ///
    /// `earlier` is the files already accepted by this same ingest, and they are asked **first**,
    /// so the refusal can say which of the two it is. They are checked at all for the same reason
    /// the column family is: two files handed over together need not come from one database, so
    /// their sequence numbers need not be comparable either.
    fn first_conflicting_key(
        &self,
        cf: &Arc<ColumnFamily>,
        candidate: &Path,
        earlier: &[PathBuf],
        table_options: &TableOptions,
    ) -> Result<Option<Conflict>> {
        if !earlier.is_empty() {
            let mut siblings: Vec<Box<dyn Cursor + Send>> = Vec::with_capacity(earlier.len());
            for path in earlier {
                let file = self.fs.open(path).at(path)?;
                let reader = TableReader::open(file, 0, table_options.clone(), None)?;
                siblings.push(table_cursor(reader.iter()));
            }
            if let Some((key, _)) =
                self.first_key_held(candidate, table_options, siblings, &RangeTombstones::new())?
            {
                return Ok(Some(Conflict::Sibling(key)));
            }
        }

        // The read path's own sources, from one pinned version: `MergeSources` keeps the version
        // alive for the walk, because dropping it would let a compaction delete a file a cursor is
        // inside.
        let sources = self.merge_sources(cf)?;
        Ok(self
            .first_key_held(
                candidate,
                table_options,
                sources.children,
                &sources.tombstones,
            )?
            .map(|(key, deleted)| {
                if deleted {
                    Conflict::Deleted(key)
                } else {
                    Conflict::Held(key)
                }
            }))
    }

    /// The first user key of `candidate` that `sources` holds an entry for or `tombstones` covers,
    /// and which of the two it was.
    ///
    /// The walk is over the *candidate's* keys, seeking the merged cursor to each. That costs a
    /// seek per distinct key in the file rather than a scan of the range, which matters because
    /// the range can be the whole column family while the file is small — under MVCC keys that is
    /// the ordinary case, not the corner.
    fn first_key_held(
        &self,
        candidate: &Path,
        table_options: &TableOptions,
        sources: Vec<Box<dyn Cursor + Send>>,
        tombstones: &RangeTombstones,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        let user = Arc::clone(self.comparator.user_comparator());
        let mut held = MergeCursor::new(sources, Arc::clone(&self.comparator));

        let file = self.fs.open(candidate).at(candidate)?;
        let reader = TableReader::open(file, 0, table_options.clone(), None)?;
        let mut wanted = reader.iter();
        wanted.seek_to_first();
        let mut previous: Option<Vec<u8>> = None;
        while wanted.valid() {
            let key = extract_user_key(wanted.key());
            // One file holds every version of a key together, so this skips the repeats without
            // remembering more than the last one.
            if previous.as_deref().is_none_or(|last| last != key) {
                let key = key.to_vec();
                if tombstones
                    .newest_covering(&key, MAX_SEQNO, user.as_ref())
                    .is_some()
                {
                    return Ok(Some((key, true)));
                }
                // `MAX_SEQNO` sorts before every entry for the key, so this lands on the first of
                // them when there is one.
                held.seek(&lookup_key(&key, MAX_SEQNO));
                held.status()?;
                if held.valid()
                    && user.cmp(extract_user_key(held.key()), &key) == std::cmp::Ordering::Equal
                {
                    return Ok(Some((key, false)));
                }
                previous = Some(key);
            }
            wanted.next();
        }
        wanted.status()?;
        Ok(None)
    }

    /// The deepest level `meta` can sink to, which is L0 when its range leaves room nowhere.
    ///
    /// Placement, not permission: [`first_conflicting_key`](Self::first_conflicting_key) has
    /// already decided whether the file may be adopted at all. Levels 1 and below are sorted runs
    /// and their files must be range-disjoint, so a range overlap there is a reason to stop
    /// sinking; L0's files overlap by construction, so it always has room and a file that reaches
    /// it is placed rather than refused.
    fn place(&self, cf: &Arc<ColumnFamily>, meta: &FileMeta) -> Result<usize> {
        let user = self.comparator.user_comparator();
        let user = user.as_ref();
        let begin = extract_user_key(&meta.smallest);
        let end = extract_user_key(&meta.largest);

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
                break;
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
