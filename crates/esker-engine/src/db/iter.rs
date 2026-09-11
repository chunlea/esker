//! Iterating a column family: many versions in, one per key out.
//!
//! Underneath is a [`MergeCursor`] over every memtable and every SST that could hold the
//! range, walking **internal** keys: several versions of a key, tombstones included, in
//! newest-first order. This turns that into the sequence a caller expects — one entry per user
//! key, at the snapshot, with deleted keys absent.
//!
//! # The rule
//!
//! Within a user key, entries are ordered newest first. So going forward, the first entry at
//! or below the snapshot decides the key: a value is yielded, a tombstone hides it, and every
//! older entry for that key is skipped either way. Going backward they arrive oldest first, so
//! the decision is the *last* one seen before the user key changes. That asymmetry is why the
//! two directions are separate code and why `LevelDB` writes them separately too.
//!
//! # Changing direction
//!
//! The underlying cursor sits on the entry that produced the current answer, which is in the
//! middle of that key's versions. Turning around therefore has to step past all of them first,
//! or the same key would be returned twice.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::dbformat::{
    EntryKind, InternalKeyComparator, MAX_SEQNO, SeqNo, extract_user_key, internal_key, lookup_key,
    split_internal_key,
};
use crate::error::{Error, Result};
use crate::iterator::Cursor;
use crate::options::{PrefixExtractor, ReadOptions};
use crate::range_del::RangeTombstones;
use crate::sst::TableIter;
use crate::version::Version;

use super::level_iter::LevelCursor;
use super::merge::MergeCursor;
use super::{ColumnFamily, Db, DbInner, Snapshot, lock, read_lock};

/// Which way the iterator is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
}

/// Adapts the SST lane's table cursor to [`Cursor`].
///
/// A wrapper rather than an implementation on `TableIter` itself, because `src/sst/` belongs
/// to the other lane. `TODO(sibling)`: once `TableIter` implements `Cursor` directly this type
/// can go.
#[derive(Debug)]
struct TableCursor(TableIter);

/// Wraps a table cursor so it can join a merge. Used by iteration and by compaction.
pub(crate) fn table_cursor(iter: TableIter) -> Box<dyn Cursor + Send> {
    Box::new(TableCursor(iter))
}

impl Cursor for TableCursor {
    fn valid(&self) -> bool {
        self.0.valid()
    }
    fn key(&self) -> &[u8] {
        self.0.key()
    }
    fn value(&self) -> &[u8] {
        self.0.value()
    }
    fn seek(&mut self, target: &[u8]) {
        self.0.seek(target);
    }
    fn seek_for_prev(&mut self, target: &[u8]) {
        self.0.seek_for_prev(target);
    }
    fn seek_to_first(&mut self) {
        self.0.seek_to_first();
    }
    fn seek_to_last(&mut self) {
        self.0.seek_to_last();
    }
    fn next(&mut self) {
        self.0.next();
    }
    fn prev(&mut self) {
        self.0.prev();
    }
    fn status(&self) -> Result<()> {
        self.0.status()
    }
}

/// How many superseded entries of one key to step over before seeking past the rest.
///
/// A step is `O(1)` and a seek is `O(log n)` in every source the merger holds, so the threshold is
/// the point where the history is deep enough to pay for one seek. Eight is small enough that a
/// key with a handful of versions never reaches it and large enough that the seek is amortised
/// over at least that many steps when it does.
const SEQUENTIAL_SKIPS_BEFORE_SEEK: usize = 8;

/// The smallest user key strictly greater than `user`.
///
/// Appending a zero byte rather than incrementing: every key greater than `user` is at least
/// `user ++ [0]`, and unlike an increment this needs no case for a key ending in `0xff`.
fn past_user_key(user: &[u8]) -> Vec<u8> {
    let mut past = Vec::with_capacity(user.len() + 1);
    past.extend_from_slice(user);
    past.push(0);
    past
}

/// A cursor over one column family's user keys, at one snapshot.
#[derive(Debug)]
pub struct DbIterator {
    merger: MergeCursor,
    comparator: Arc<InternalKeyComparator>,
    snapshot: SeqNo,
    direction: Direction,
    /// The user key and value the cursor is on.
    current: Option<(Vec<u8>, Vec<u8>)>,
    /// Every range tombstone any source of this scan declares, collected when the iterator was
    /// built. A tombstone hides keys that are nowhere in the merged run, so there is no entry
    /// to meet it at — the set has to be held and asked
    /// ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)).
    tombstones: RangeTombstones,
    /// Set by [`ReadOptions::prefix_same_as_start`]: iteration ends when the prefix changes.
    prefix: Option<Vec<u8>>,
    extractor: Option<Arc<dyn PrefixExtractor>>,
    status: Option<Error>,
    /// Pins every file the merger reads. Dropping it is what lets them be deleted.
    _version: Arc<Version>,
    /// Every stored entry this iterator examines, counted into the database's own total.
    ///
    /// Shared rather than per-iterator because the question it answers is about a *workload* —
    /// "did this scan get dearer" — and a scan opens an iterator, spends it and drops it.
    stepped: Arc<AtomicU64>,
    /// Pins the sequence number, so a compaction cannot collect versions this iterator needs.
    _snapshot: Option<Snapshot>,
}

impl DbIterator {
    /// Whether the cursor is on an entry.
    pub fn valid(&self) -> bool {
        self.current.is_some()
    }

    /// The user key under the cursor.
    pub fn key(&self) -> &[u8] {
        self.current.as_ref().map_or(&[], |(key, _)| key.as_slice())
    }

    /// The value under the cursor.
    pub fn value(&self) -> &[u8] {
        self.current
            .as_ref()
            .map_or(&[], |(_, value)| value.as_slice())
    }

    /// Whether everything read so far was readable.
    ///
    /// Checked after iteration ends: an unreadable block stops the cursor exactly as reaching
    /// the last key does, and treating the first as the second turns a damaged file into an
    /// empty range.
    pub fn status(&self) -> Result<()> {
        if let Some(error) = &self.status {
            return Err(Error::corruption("iterator", error.to_string()));
        }
        self.merger.status()
    }

    /// Positions on the first key.
    pub fn seek_to_first(&mut self) {
        self.prefix = None;
        self.merger.seek_to_first();
        self.direction = Direction::Forward;
        self.find_next(None);
    }

    /// Positions on the last key.
    pub fn seek_to_last(&mut self) {
        self.prefix = None;
        self.merger.seek_to_last();
        self.direction = Direction::Reverse;
        self.find_prev();
    }

    /// Positions on the first key at or after `target`.
    pub fn seek(&mut self, target: &[u8]) {
        self.set_prefix(target);
        // The largest tag for the user key, which sorts before every stored version of it.
        self.merger.seek(&lookup_key(target, MAX_SEQNO));
        self.direction = Direction::Forward;
        self.find_next(None);
    }

    /// Positions on the last key at or before `target`.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.set_prefix(target);
        // Tag zero sorts after every stored version of the user key, so this lands on the
        // oldest one; the backward scan then walks up to the newest.
        self.merger
            .seek_for_prev(&internal_key(target, 0, EntryKind::Delete));
        self.direction = Direction::Reverse;
        self.find_prev();
    }

    /// Moves to the next key. A no-op when the cursor is not valid.
    pub fn next(&mut self) {
        let Some((user, _)) = self.current.clone() else {
            return;
        };
        if self.direction == Direction::Reverse {
            self.step_forward_past(&user);
            self.direction = Direction::Forward;
        } else {
            self.merger.next();
        }
        self.find_next(Some(user));
    }

    /// Moves to the previous key. A no-op when the cursor is not valid.
    pub fn prev(&mut self) {
        let Some((user, _)) = self.current.clone() else {
            return;
        };
        if self.direction == Direction::Forward {
            self.step_back_past(&user);
            self.direction = Direction::Reverse;
        }
        self.find_prev();
    }

    /// Scans forward for the next visible value, skipping older versions of `skipping`.
    ///
    /// # Stepping past a decided key, and when to stop stepping
    ///
    /// Once a key is decided — returned, deleted, or hidden by a range tombstone — every older
    /// entry under it is decided too, and they are pure cost. Stepping through them is what made a
    /// `lock` column family scan cost `keys × 2V`: a prewrite puts a lock and a commit deletes it,
    /// so a key committed V times carries 2V superseded entries and the scan walked all of them to
    /// learn the key is currently absent (#58).
    ///
    /// A seek jumps the lot, but a seek is `O(log n)` **per source** where a step is `O(1)`, so
    /// seeking on the first repeat would make shallow data dearer to buy nothing. So: step up to
    /// [`SEQUENTIAL_SKIPS_BEFORE_SEEK`] times, then seek. Shallow keys never reach the threshold
    /// and pay nothing; deep ones pay it once.
    fn find_next(&mut self, mut skipping: Option<Vec<u8>>) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        let mut skipped = 0_usize;
        while self.merger.valid() {
            // Counted here, at the one place every forward step passes through, and counted per
            // *stored entry* rather than per answer: the versions walked past are the whole of
            // what #58 is about and they never reach the caller.
            self.stepped.fetch_add(1, AtomicOrdering::Relaxed);
            let key = self.merger.key().to_vec();
            let Some((user, seqno, kind)) = split_internal_key(&key) else {
                self.fail("an entry whose tag cannot be decoded");
                return;
            };
            if seqno <= self.snapshot {
                // An entry at or below the key we have already decided is an older version of
                // it, whatever it says.
                let decided = skipping
                    .as_ref()
                    .is_some_and(|key| user_order.cmp(user, key) != Ordering::Greater);
                if decided {
                    skipped += 1;
                    if skipped >= SEQUENTIAL_SKIPS_BEFORE_SEEK {
                        let past = past_user_key(user);
                        self.merger.seek(&lookup_key(&past, MAX_SEQNO));
                        skipped = 0;
                        continue;
                    }
                }
                if !decided {
                    if !self.in_prefix(user) {
                        self.current = None;
                        return;
                    }
                    match kind {
                        EntryKind::Put if !self.hidden(user, seqno) => {
                            self.current = Some((user.to_vec(), self.merger.value().to_vec()));
                            return;
                        }
                        // A covered `Put` behaves exactly like a tombstone: it is not a value,
                        // and every older version of the same key is covered too — the
                        // tombstone that hides this one is newer than all of them.
                        EntryKind::Put | EntryKind::Delete | EntryKind::DeleteRange => {
                            skipping = Some(user.to_vec());
                            skipped = 0;
                        }
                    }
                }
            }
            self.merger.next();
        }
        self.current = None;
    }

    /// Scans backward. Versions arrive oldest first, so the answer is the last one seen before
    /// the user key changes.
    fn find_prev(&mut self) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        let mut found: Option<(Vec<u8>, Vec<u8>)> = None;
        while self.merger.valid() {
            // The backward mirror of `find_next`'s count, so a reverse scan is measured by the
            // same number and a fix that only helped one direction would be visible.
            self.stepped.fetch_add(1, AtomicOrdering::Relaxed);
            let key = self.merger.key().to_vec();
            let Some((user, seqno, kind)) = split_internal_key(&key) else {
                self.fail("an entry whose tag cannot be decoded");
                return;
            };
            if seqno <= self.snapshot {
                if let Some((decided, _)) = &found
                    && user_order.cmp(user, decided) == Ordering::Less
                {
                    // Moved off the key we had settled on; that one is the answer.
                    break;
                }
                match kind {
                    EntryKind::Put if !self.hidden(user, seqno) => {
                        found = Some((user.to_vec(), self.merger.value().to_vec()));
                    }
                    // A tombstone cancels the older versions seen so far for this key, and a
                    // `Put` a range tombstone covers cancels them for the same reason.
                    EntryKind::Put | EntryKind::Delete | EntryKind::DeleteRange => found = None,
                }
            }
            self.merger.prev();
        }
        self.current = found.filter(|(user, _)| self.in_prefix(user));
    }

    /// Whether a range tombstone hides this entry from this scan's snapshot.
    ///
    /// The check is skipped entirely when there are no tombstones, which is every ordinary
    /// scan.
    fn hidden(&self, user: &[u8], seqno: SeqNo) -> bool {
        !self.tombstones.is_empty()
            && self.tombstones.hides(
                user,
                seqno,
                self.snapshot,
                self.comparator.user_comparator().as_ref(),
            )
    }

    /// Moves the underlying cursor past every version of `user`, going forward.
    fn step_forward_past(&mut self, user: &[u8]) {
        // Tag zero is smaller than any real tag, so it sorts after every version of the key.
        self.merger.seek(&internal_key(user, 0, EntryKind::Delete));
    }

    /// Moves the underlying cursor before every version of `user`, going backward.
    fn step_back_past(&mut self, user: &[u8]) {
        let user_order = Arc::clone(self.comparator.user_comparator());
        self.merger.seek_for_prev(&lookup_key(user, MAX_SEQNO));
        while self.merger.valid() {
            let key_user = extract_user_key(self.merger.key()).to_vec();
            if user_order.cmp(&key_user, user) == Ordering::Less {
                return;
            }
            self.merger.prev();
        }
    }

    fn set_prefix(&mut self, target: &[u8]) {
        self.prefix = self.extractor.as_ref().map(|extractor| {
            if extractor.in_domain(target) {
                extractor.prefix(target).to_vec()
            } else {
                target.to_vec()
            }
        });
    }

    /// Whether `user` still shares the prefix iteration started at.
    fn in_prefix(&self, user: &[u8]) -> bool {
        let Some(prefix) = &self.prefix else {
            return true;
        };
        let Some(extractor) = &self.extractor else {
            return true;
        };
        extractor.in_domain(user) && extractor.prefix(user) == prefix.as_slice()
    }

    fn fail(&mut self, detail: &str) {
        self.current = None;
        self.status
            .get_or_insert_with(|| Error::corruption("iterator", detail.to_string()));
    }
}

impl Cursor for DbIterator {
    fn valid(&self) -> bool {
        Self::valid(self)
    }
    fn key(&self) -> &[u8] {
        Self::key(self)
    }
    fn value(&self) -> &[u8] {
        Self::value(self)
    }
    fn seek(&mut self, target: &[u8]) {
        Self::seek(self, target);
    }
    fn seek_for_prev(&mut self, target: &[u8]) {
        Self::seek_for_prev(self, target);
    }
    fn seek_to_first(&mut self) {
        Self::seek_to_first(self);
    }
    fn seek_to_last(&mut self) {
        Self::seek_to_last(self);
    }
    fn next(&mut self) {
        Self::next(self);
    }
    fn prev(&mut self) {
        Self::prev(self);
    }
    fn status(&self) -> Result<()> {
        Self::status(self)
    }
}

/// Everything a merged read of one column family needs: the cursors, the range tombstones that
/// apply to them, and the version that keeps their files on disk.
///
/// The three travel together because they are one snapshot of the column family taken at one
/// instant. Split apart, a caller can hold cursors over files a dropped version has let a
/// compaction delete, or apply a tombstone set to a run it was not collected from.
pub(crate) struct MergeSources {
    /// One cursor per memtable and per L0 file, and one per deeper level.
    pub(crate) children: Vec<Box<dyn Cursor + Send>>,
    /// Every range tombstone in the column family, which lives only in memtables and L0.
    pub(crate) tombstones: RangeTombstones,
    /// Held, not read: dropping it lets a compaction delete a file a cursor is inside.
    pub(crate) version: Arc<Version>,
}

impl DbInner {
    /// Every internal cursor over `cf`, the range tombstones that cover it, and the version they
    /// were taken from.
    ///
    /// The whole of "what is currently in this column family", as sources a [`MergeCursor`] can
    /// merge. It has two callers that must not disagree: a read
    /// ([`Db::iter`](crate::Db::iter)), and the overlap check an ingest runs before it adopts a
    /// file ([`crate::db::ingest`]). An ingest that consulted a *different* set of sources than a
    /// read would refuse ingests that are safe, or worse, allow one whose keys a reader can
    /// already see.
    ///
    /// The version is returned rather than dropped because it pins the files the cursors read:
    /// dropping it lets a compaction delete a file a cursor is still inside.
    ///
    /// # Why the shape is what it is
    ///
    /// **L0 is opened file by file, and has to be.** Its files overlap, so any of them can hold
    /// the next key and all of them are live at once — and it is the only level a range tombstone
    /// can be in (ADR 0017 decision 6: a compaction whose inputs carry one becomes a discharge).
    /// Collecting the tombstone set is therefore an L0 walk rather than a walk of every file in
    /// the database.
    ///
    /// Every deeper level **partitions** the key space, so it is one cursor that opens the file it
    /// has reached ([`LevelCursor`]). A scan of a database with a full L4 used to open every L4
    /// file — four reads each, index and filter resident for the scan's life — before it read a
    /// byte of the range it wanted.
    pub(crate) fn merge_sources(&self, cf: &Arc<ColumnFamily>) -> Result<MergeSources> {
        // The tombstone set for the whole scan, collected once up front rather than per key:
        // a range delete hides keys the merged run has never seen, so there is nothing to
        // consult it *at* ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)).
        let user_order = self.comparator.user_comparator();
        let mut tombstones = RangeTombstones::new();

        let mut children: Vec<Box<dyn Cursor + Send>> = Vec::new();
        {
            let mem = read_lock(&cf.mem)?;
            if mem.active.has_range_tombstones() {
                tombstones.extend(&mem.active.range_tombstones(), user_order.as_ref());
            }
            children.push(Box::new(mem.active.iter()));
            for (table, _) in &mem.immutable {
                if table.has_range_tombstones() {
                    tombstones.extend(&table.range_tombstones(), user_order.as_ref());
                }
                children.push(Box::new(table.iter()));
            }
        }

        let version = lock(&self.versions)?.current();
        let table_options = self.table_options(cf);
        let levels = version
            .cf(cf.id())
            .map_or(0, crate::version::CfVersion::num_levels);

        for file in version.files(cf.id(), 0) {
            let reader = self.table_cache.get(file.number, &table_options)?;
            if !reader.range_tombstones().is_empty() {
                tombstones.extend(reader.range_tombstones(), user_order.as_ref());
            }
            children.push(table_cursor(reader.iter()));
        }

        for level in 1..levels {
            let files = version.files(cf.id(), level);
            if files.is_empty() {
                continue;
            }
            children.push(Box::new(LevelCursor::new(
                files.to_vec(),
                Arc::clone(&self.table_cache),
                table_options.clone(),
                Arc::clone(&self.comparator),
            )));
        }
        Ok(MergeSources {
            children,
            tombstones,
            version,
        })
    }
}

impl Db {
    /// A cursor over `cf`, positioned nowhere until it is seeked.
    ///
    /// The iterator pins a version and a snapshot for its whole life, so every file it can
    /// read stays on disk and every version it can see survives compaction. Holding one for a
    /// long time therefore holds disk space; that is the trade a consistent scan costs.
    pub fn iter(&self, cf: &str, options: &ReadOptions) -> Result<DbIterator> {
        let cf = self.inner.cf_by_name(cf)?;
        let snapshot = self.inner.read_seqno(options)?;

        let sources = self.inner.merge_sources(&cf)?;

        Ok(DbIterator {
            merger: MergeCursor::new(sources.children, Arc::clone(&self.inner.comparator)),
            comparator: Arc::clone(&self.inner.comparator),
            snapshot,
            direction: Direction::Forward,
            current: None,
            tombstones: sources.tombstones,
            prefix: None,
            extractor: options
                .prefix_same_as_start
                .then(|| cf.options().prefix_extractor.clone())
                .flatten(),
            status: None,
            stepped: Arc::clone(&self.inner.entries_stepped),
            _version: sources.version,
            _snapshot: options.snapshot.clone(),
        })
    }
}
