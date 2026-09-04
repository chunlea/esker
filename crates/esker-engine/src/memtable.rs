//! The in-memory half of the log-structured design: a sorted map of internal keys.
//!
//! Every write lands here after its bytes are in the log, and stays until the memtable is
//! flushed to an SST. Since [ADR 0041](../../docs/adr/0041-the-in-house-arena-skiplist.md) it is
//! an in-house arena skiplist — `skiplist::SkipList`. It was `crossbeam-skiplist`, the last
//! piece of concurrent code the project bought rather than wrote; that dependency is gone, and
//! with it three crates from the runtime budget.
//!
//! # Nothing is ever removed
//!
//! A delete is an *insert* of a tombstone, and a memtable's entries are never taken out of it.
//! Three things depend on that. Older versions of a key may still exist in lower levels, so only
//! a stored tombstone can hide them; an iterator holds a position in the map, so removing an
//! entry underneath it would break the snapshot it thinks it has; and the arena store frees
//! nothing until the last `Arc<MemTable>` goes, which is what makes its cursor a borrow rather
//! than a copy. Memtables die whole, after a flush, and never piecemeal.
//!
//! # Ordering
//!
//! Internal keys, ordered by the column family's comparator: user key ascending, then the tag
//! descending, so the newest version of a key sorts first and one seek answers a point read.
//! The store holds the comparator once, on the table, rather than once per key.

mod arena;
mod skiplist;
mod store;

#[cfg(test)]
mod differential;

use std::cmp::Ordering;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use store::Store;

use crate::range_del::{RangeTombstone, RangeTombstones};

use crate::dbformat::{
    EntryKind, InternalKeyComparator, SeqNo, extract_user_key, lookup_key, pack_tag,
    split_internal_key,
};

/// Which store the engine uses.
///
/// The one line ADR 0041's decision lives on. It had two candidates while the benchmark was
/// being taken and now has one, and the seam is kept rather than inlined because the number that
/// came out was mixed: `docs/bench/skiplist.md` records an eighteen-fold cheaper scan against an
/// insert between 1.2 and 1.8 times dearer. If someone takes that insert on — `LevelDB`'s layout,
/// with the key bytes inside the node's own allocation, is the obvious next thing to try — this
/// is what keeps the attempt to one file.
type Selected = skiplist::SkipList;

/// Where a cursor is, in whichever store [`Selected`] names.
type Position = <Selected as Store>::Pos;

/// Bytes charged per entry on top of its key and value, to account for the node and its
/// forward pointers. Approximate on purpose: it decides when to flush, and being a little wrong
/// there costs a slightly early or late flush and nothing else.
///
/// Deliberately unchanged across ADR 0041, although the arena node is much smaller than the
/// `crossbeam-skiplist` one plus its two `Vec` headers and its comparator handle. The number
/// decides when every flush in the engine fires, and changing the storage and the flush
/// schedule in one commit would leave no way to tell which of them moved a benchmark.
const ENTRY_OVERHEAD: usize = 64;

/// What a memtable knows about a key.
///
/// The distinction between "no entry here" and "an entry that says it is gone" is the whole
/// point: the first means keep looking in older memtables and in the levels below, the second
/// means stop, because a tombstone hides everything older than it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The newest visible entry is a value.
    Found {
        /// The value.
        value: Vec<u8>,
        /// The sequence number it was written at.
        ///
        /// The caller needs it to ask whether a range tombstone from this source or a newer
        /// one hides it: a tombstone hides an entry only when it is strictly newer
        /// ([ADR 0017](../../docs/adr/0017-range-tombstones.md)).
        seqno: SeqNo,
    },
    /// The newest visible entry is a tombstone. The key does not exist at this snapshot.
    Deleted,
}

/// One memtable: a sorted, append-only map of internal keys to values, plus the ranges
/// deleted while it was active.
///
/// The tombstones sit *beside* the map rather than in it, because a range delete hides keys
/// that are not in the map and keys that do not exist yet
/// ([ADR 0017](../../docs/adr/0017-range-tombstones.md)). Behind a `Mutex` and not a skiplist
/// because there are very few of them — a range delete is an administrative act, not something
/// a write path emits per key — and because a reader takes a whole snapshot of them at once
/// rather than seeking within them.
#[derive(Debug)]
pub struct MemTable {
    store: Selected,
    range_tombstones: Mutex<RangeTombstones>,
    approximate_size: AtomicUsize,
}

impl MemTable {
    /// An empty memtable ordered by `comparator`.
    ///
    /// Uses the default height seed. Deterministic, which is what a replay needs; a caller that
    /// wants a stream of its own — the simulator, or a test replaying a failure — takes
    /// [`MemTable::with_seed`].
    pub fn new(comparator: Arc<InternalKeyComparator>) -> Self {
        Self::with_seed(comparator, skiplist::DEFAULT_SEED)
    }

    /// An empty memtable whose node heights are drawn from `seed`.
    ///
    /// A memtable's shape is a function of its seed, so a failing test replays from one.
    /// `crossbeam-skiplist` drew from thread-local entropy and could not, which is the
    /// difference ADR 0041 was after; this is the way in for `esker-sim` and for a proptest
    /// that wants to pin a shape.
    pub fn with_seed(comparator: Arc<InternalKeyComparator>, seed: u64) -> Self {
        Self {
            // Through the trait, not the inherent `new`: this file must not compile against
            // anything but [`Store`]. See [`MemTable::store`].
            store: <Selected as Store>::new(comparator, seed),
            range_tombstones: Mutex::new(RangeTombstones::new()),
            approximate_size: AtomicUsize::new(0),
        }
    }

    /// The store, seen only as a [`Store`].
    ///
    /// Opaque on purpose. Reaching `self.store` directly would resolve to whichever inherent
    /// method [`Selected`]'s concrete type happens to have, and this file would quietly depend
    /// on the implementation rather than on the surface — so changing that one line would stop
    /// compiling instead of just switching stores. Through here, only [`Store`] is visible.
    fn store(&self) -> &impl Store<Pos = Position> {
        &self.store
    }

    /// The comparator this table is ordered by. Iterators and merge iterators need it.
    pub fn comparator(&self) -> &Arc<InternalKeyComparator> {
        self.store().comparator()
    }

    /// Inserts one entry. Takes `&self`: readers keep working while this runs.
    ///
    /// The key goes in as a user key and a tag rather than as one joined buffer, which is what
    /// keeps the insert free of an allocation the arena would only copy out of again.
    ///
    /// The store refuses an insert only when its arena's four-gigabyte offset space is
    /// exhausted, which needs a single memtable four gigabytes deep — sixty-four times the
    /// default `write_buffer_size`, and past the point where the batch that carried it would
    /// have exhausted memory first. It is reported rather than swallowed, and the size is still
    /// charged so the flush that would relieve it still fires.
    pub fn add(&self, seqno: SeqNo, kind: EntryKind, key: &[u8], value: &[u8]) {
        let tag = pack_tag(seqno, kind).to_le_bytes();
        let charge = key.len() + tag.len() + value.len() + ENTRY_OVERHEAD;
        if !self.store().insert(key, &tag, value) {
            tracing::error!(
                seqno,
                key_len = key.len(),
                value_len = value.len(),
                "the memtable arena is full and the entry was not stored"
            );
        }
        self.approximate_size
            .fetch_add(charge, AtomicOrdering::Relaxed);
    }

    /// Records that `[begin, end)` was deleted at `seqno`.
    ///
    /// Kept beside the map: putting it at `begin` in the map is the v1 behaviour
    /// `docs/DESIGN.md` §4.7 refuses, which deletes one key while claiming a range.
    ///
    /// A poisoned lock is reported by the *next* read rather than here, because a write that
    /// has already been logged cannot be un-logged: see [`MemTable::range_tombstones`].
    pub fn add_range(&self, seqno: SeqNo, begin: &[u8], end: &[u8]) {
        let charge = begin.len() + end.len() + ENTRY_OVERHEAD;
        if let Ok(mut tombstones) = self.range_tombstones.lock() {
            tombstones.push(
                RangeTombstone::new(begin.to_vec(), end.to_vec(), seqno),
                self.comparator().user_comparator().as_ref(),
            );
        }
        self.approximate_size
            .fetch_add(charge, AtomicOrdering::Relaxed);
    }

    /// The ranges this table declares deleted.
    ///
    /// Cloned rather than borrowed: a reader holds the set for the whole of a lookup while
    /// writers carry on adding, and a lock held across a multi-level read would make one
    /// `delete_range` block every write behind it.
    ///
    /// A poisoned lock answers *empty*, which is wrong in the safe direction only in the sense
    /// that it is the same answer a table with no tombstones gives — and a poisoned memtable
    /// lock means a writer panicked mid-insert, which the layers above already treat as fatal.
    pub fn range_tombstones(&self) -> RangeTombstones {
        self.range_tombstones
            .lock()
            .map(|tombstones| tombstones.clone())
            .unwrap_or_default()
    }

    /// Whether this table has any range tombstones at all. The common answer is `false`, and
    /// the read path short-circuits on it.
    pub fn has_range_tombstones(&self) -> bool {
        self.range_tombstones
            .lock()
            .is_ok_and(|tombstones| !tombstones.is_empty())
    }

    /// The newest entry for `user_key` visible at `snapshot`, or `None` if this table has
    /// nothing to say about it.
    ///
    /// One seek answers this, because internal keys sort newest-first within a user key: the
    /// first entry at or after `(user_key, snapshot)` is already the answer.
    pub fn get(&self, user_key: &[u8], snapshot: SeqNo) -> Option<Lookup> {
        let position = self.store().seek(&lookup_key(user_key, snapshot));
        if !self.store().valid(&position) {
            return None;
        }
        let internal = self.store().key(&position);
        if self
            .comparator()
            .user_comparator()
            .cmp(extract_user_key(internal), user_key)
            != Ordering::Equal
        {
            return None;
        }
        match split_internal_key(internal) {
            Some((_, seqno, EntryKind::Put)) => Some(Lookup::Found {
                value: self.store().value(&position).to_vec(),
                seqno,
            }),
            // `DeleteRange` never reaches the map — `add_range` puts it in the tombstone list
            // instead ([ADR 0017](../../docs/adr/0017-range-tombstones.md)). A tag saying
            // otherwise is memory corruption; reading it as a point delete is the safe way to
            // be wrong, because it hides a key rather than resurrecting one.
            Some((_, _, EntryKind::Delete | EntryKind::DeleteRange)) => Some(Lookup::Deleted),
            // An unreadable tag cannot come from `add`, so this is memory corruption rather
            // than disk corruption. Report "nothing here" instead of panicking (invariant 9).
            None => None,
        }
    }

    /// Roughly how much memory the entries occupy. The flush trigger reads this.
    pub fn approximate_size(&self) -> usize {
        self.approximate_size.load(AtomicOrdering::Relaxed)
    }

    /// How many entries the table holds, tombstones included.
    pub fn len(&self) -> usize {
        self.store().len()
    }

    /// Whether nothing has been written to this table — **range deletes included**.
    ///
    /// A table holding only `delete_range` entries has an empty map and is not empty: the
    /// flush path uses this to decide whether there is anything to write, and answering "yes,
    /// empty" would drop the deletes on the floor at the next memtable switch
    /// ([ADR 0017](../../docs/adr/0017-range-tombstones.md)).
    pub fn is_empty(&self) -> bool {
        self.store().len() == 0 && !self.has_range_tombstones()
    }

    /// A cursor over the table, positioned nowhere until it is seeked.
    ///
    /// Takes an `Arc` and keeps it: a cursor outlives the memtable switch that retires the
    /// table it is reading, which is what lets a scan carry on across a flush — and, with the
    /// arena store, it is also what keeps the bytes the cursor points at alive.
    pub fn iter(self: &Arc<Self>) -> MemTableIter {
        MemTableIter {
            table: Arc::clone(self),
            position: Position::default(),
        }
    }
}

/// A two-directional cursor over a memtable, keyed by internal keys.
///
/// The shape every iterator in the engine shares (`docs/DESIGN.md` §4.1), so that the merge
/// iterator can drive memtables and SSTs through the same calls.
///
/// # Why it holds a position and not an entry
///
/// It holds an `Arc<MemTable>` and a position in that table's store, and reads through both.
/// With the arena store that position is a node offset and [`MemTableIter::key`] borrows
/// straight out of the arena, so a step is a pointer hop with no allocation. The arena is owned
/// by the `MemTable` the `Arc` keeps alive, which is the whole lifetime argument: the borrow
/// cannot outlive the bytes because it cannot outlive the table.
///
/// That is what [ADR 0041](../../docs/adr/0041-the-in-house-arena-skiplist.md) was for.
/// `crossbeam-skiplist` handed out entries that borrow the map, so a cursor built from one would
/// have been self-referential; its position was a copy of the entry and every step re-found the
/// key, which is `O(log n)` and an allocation.
#[derive(Debug)]
pub struct MemTableIter {
    table: Arc<MemTable>,
    position: Position,
}

impl MemTableIter {
    /// Whether the cursor is on an entry.
    pub fn valid(&self) -> bool {
        self.table.store().valid(&self.position)
    }

    /// The internal key under the cursor, empty when the cursor is not valid.
    pub fn key(&self) -> &[u8] {
        self.table.store().key(&self.position)
    }

    /// The user key under the cursor, without its tag.
    pub fn user_key(&self) -> &[u8] {
        extract_user_key(self.key())
    }

    /// The value under the cursor, empty for a tombstone or an invalid cursor.
    pub fn value(&self) -> &[u8] {
        self.table.store().value(&self.position)
    }

    /// Positions the cursor on the first entry at or after `target` (an internal key).
    pub fn seek(&mut self, target: &[u8]) {
        self.position = self.table.store().seek(target);
    }

    /// Positions the cursor on the last entry at or before `target` (an internal key).
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.position = self.table.store().seek_for_prev(target);
    }

    /// Positions the cursor on the first entry.
    pub fn seek_to_first(&mut self) {
        self.position = self.table.store().first();
    }

    /// Positions the cursor on the last entry.
    pub fn seek_to_last(&mut self) {
        self.position = self.table.store().last();
    }

    /// Advances forward. Becomes invalid past the end; a no-op when already invalid.
    pub fn next(&mut self) {
        self.position = self.table.store().after(&self.position);
    }

    /// Steps backward. Becomes invalid before the start; a no-op when already invalid.
    pub fn prev(&mut self) {
        self.position = self.table.store().before(&self.position);
    }
}

impl crate::iterator::Cursor for MemTableIter {
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

    /// A memtable lives in memory: there is nothing that can fail to be read.
    fn status(&self) -> crate::error::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Lookup, MemTable};
    use crate::dbformat::{
        BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, internal_key,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Tests hold the table through an `Arc`, because a cursor keeps one.
    fn table() -> Arc<MemTable> {
        Arc::new(MemTable::new(Arc::new(InternalKeyComparator::new(
            Arc::new(BytewiseComparator),
        ))))
    }

    /// Every (user key, seqno) pair in the table, in iteration order.
    fn walk(table: &Arc<MemTable>) -> Vec<(Vec<u8>, u64)> {
        let mut iter = table.iter();
        iter.seek_to_first();
        let mut out = Vec::new();
        while iter.valid() {
            let (user, seqno, _) = crate::dbformat::split_internal_key(iter.key()).unwrap();
            out.push((user.to_vec(), seqno));
            iter.next();
        }
        out
    }

    /// The value a lookup should answer with, at the sequence number it was written at.
    fn found(value: &[u8], seqno: crate::dbformat::SeqNo) -> Lookup {
        Lookup::Found {
            value: value.to_vec(),
            seqno,
        }
    }

    #[test]
    fn a_put_is_visible_at_and_above_its_sequence_number() {
        let table = table();
        table.add(5, EntryKind::Put, b"key", b"value");
        assert_eq!(table.get(b"key", 5), Some(found(b"value", 5)));
        assert_eq!(table.get(b"key", 100), Some(found(b"value", 5)));
        assert_eq!(
            table.get(b"key", 4),
            None,
            "not yet written at this snapshot"
        );
        assert_eq!(table.get(b"other", 100), None);
    }

    /// The newest version wins, and older ones stay reachable through older snapshots.
    #[test]
    fn snapshots_see_the_version_that_was_current() {
        let table = table();
        table.add(1, EntryKind::Put, b"k", b"one");
        table.add(2, EntryKind::Put, b"k", b"two");
        table.add(3, EntryKind::Put, b"k", b"three");
        assert_eq!(table.get(b"k", 3), Some(found(b"three", 3)));
        assert_eq!(table.get(b"k", 2), Some(found(b"two", 2)));
        assert_eq!(table.get(b"k", 1), Some(found(b"one", 1)));
        assert_eq!(table.len(), 3, "every version is still stored");
    }

    /// "Nothing here" and "an entry saying it is gone" have to be different answers: the first
    /// sends the read to older tables and lower levels, the second stops it.
    #[test]
    fn a_tombstone_is_not_the_same_as_an_absence() {
        let table = table();
        table.add(1, EntryKind::Put, b"k", b"value");
        table.add(2, EntryKind::Delete, b"k", b"");
        assert_eq!(table.get(b"k", 2), Some(Lookup::Deleted));
        assert_eq!(table.get(b"k", 1), Some(found(b"value", 1)));
        assert_eq!(table.get(b"gone", 2), None, "never written is not deleted");
        assert_eq!(table.len(), 2, "the tombstone is stored, not applied");
    }

    #[test]
    fn iteration_is_user_key_ascending_then_newest_first() {
        let table = table();
        table.add(1, EntryKind::Put, b"b", b"");
        table.add(2, EntryKind::Put, b"a", b"");
        table.add(3, EntryKind::Put, b"a", b"");
        table.add(4, EntryKind::Delete, b"c", b"");
        assert_eq!(
            walk(&table),
            vec![
                (b"a".to_vec(), 3),
                (b"a".to_vec(), 2),
                (b"b".to_vec(), 1),
                (b"c".to_vec(), 4),
            ]
        );
    }

    #[test]
    fn seek_and_seek_for_prev_land_on_the_right_side() {
        let table = table();
        for (seqno, key) in [(1u64, "a"), (2, "c"), (3, "e")] {
            table.add(seqno, EntryKind::Put, key.as_bytes(), b"");
        }
        let mut iter = table.iter();

        iter.seek(&internal_key(b"b", 100, EntryKind::Put));
        assert!(iter.valid());
        assert_eq!(iter.user_key(), b"c", "seek goes forward");

        iter.seek_for_prev(&internal_key(b"b", 0, EntryKind::Delete));
        assert!(iter.valid());
        assert_eq!(iter.user_key(), b"a", "seek_for_prev goes backward");

        iter.seek(&internal_key(b"z", 100, EntryKind::Put));
        assert!(!iter.valid(), "past the end");

        iter.seek_for_prev(&internal_key(b"", 0, EntryKind::Delete));
        assert!(!iter.valid(), "before the start");
    }

    #[test]
    fn the_cursor_walks_both_ways() {
        let table = table();
        for key in ["a", "b", "c"] {
            table.add(1, EntryKind::Put, key.as_bytes(), key.as_bytes());
        }
        let mut iter = table.iter();
        iter.seek_to_last();
        assert_eq!(iter.user_key(), b"c");
        iter.prev();
        assert_eq!(iter.user_key(), b"b");
        iter.next();
        assert_eq!(iter.user_key(), b"c");
        iter.next();
        assert!(!iter.valid());

        iter.seek_to_first();
        assert_eq!(iter.user_key(), b"a");
        iter.prev();
        assert!(!iter.valid());
    }

    #[test]
    fn an_empty_table_iterates_to_nothing() {
        let table = table();
        assert!(table.is_empty());
        let mut iter = table.iter();
        iter.seek_to_first();
        assert!(!iter.valid());
        iter.seek_to_last();
        assert!(!iter.valid());
        iter.seek(b"anything");
        assert!(!iter.valid());
    }

    #[test]
    fn approximate_size_grows_with_every_entry() {
        let table = table();
        assert_eq!(table.approximate_size(), 0);
        table.add(1, EntryKind::Put, b"key", b"value");
        let after_one = table.approximate_size();
        assert!(after_one >= 3 + 8 + 5, "key, tag and value are all charged");
        table.add(2, EntryKind::Delete, b"key", b"");
        assert!(
            table.approximate_size() > after_one,
            "a tombstone occupies memory too"
        );
    }

    /// The same reads a `BTreeMap` would give, over a mixed history of puts and deletes at
    /// several snapshots.
    #[test]
    fn reads_match_a_map_model() {
        let table = table();
        let mut model: BTreeMap<(Vec<u8>, u64), Option<Vec<u8>>> = BTreeMap::new();
        let keys = [b"a".as_slice(), b"bb", b"ccc"];
        for seqno in 1u64..40 {
            let key = keys[(seqno % 3) as usize];
            if seqno % 5 == 0 {
                table.add(seqno, EntryKind::Delete, key, b"");
                model.insert((key.to_vec(), seqno), None);
            } else {
                let value = format!("v{seqno}").into_bytes();
                table.add(seqno, EntryKind::Put, key, &value);
                model.insert((key.to_vec(), seqno), Some(value));
            }
        }

        for key in keys {
            for snapshot in 0u64..42 {
                let expected = model
                    .range((key.to_vec(), 0)..=(key.to_vec(), snapshot))
                    .next_back()
                    .map(|((_, seqno), value)| match value {
                        Some(value) => Lookup::Found {
                            value: value.clone(),
                            seqno: *seqno,
                        },
                        None => Lookup::Deleted,
                    });
                assert_eq!(
                    table.get(key, snapshot),
                    expected,
                    "key {key:?} at snapshot {snapshot}"
                );
            }
        }
    }

    /// Readers must keep working while writers insert, which is the reason for a skiplist
    /// rather than a locked map.
    #[test]
    fn readers_and_writers_run_concurrently() {
        // Miri interprets every instruction, and four readers each walking a two-thousand-entry
        // list five hundred times is millions of them. Small is still meaningful here: what Miri
        // is looking for is a missing happens-before edge, and one insert either has it or does
        // not. Without this, `cargo miri test -- memtable` never finishes.
        #[cfg(miri)]
        const ROUNDS: u64 = 20;
        #[cfg(not(miri))]
        const ROUNDS: u64 = 500;

        let table = table();
        let writers: Vec<_> = (0..4u64)
            .map(|worker| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    for i in 0..ROUNDS {
                        let key = format!("key-{:04}", i % 100);
                        table.add(worker * 1000 + i + 1, EntryKind::Put, key.as_bytes(), b"v");
                    }
                })
            })
            .collect();
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    // Note the comparator: internal keys are *not* ordered bytewise. The tag
                    // is little-endian and sorts descending, so `previous < key` on the raw
                    // bytes is exactly the mistake this crate exists to avoid.
                    let order = Arc::clone(table.comparator());
                    for _ in 0..ROUNDS {
                        let mut iter = table.iter();
                        iter.seek_to_first();
                        let mut previous: Option<Vec<u8>> = None;
                        while iter.valid() {
                            let key = iter.key().to_vec();
                            if let Some(previous) = &previous {
                                assert_eq!(
                                    order.cmp(previous, &key),
                                    std::cmp::Ordering::Less,
                                    "iteration went backwards under concurrent writes"
                                );
                            }
                            previous = Some(key);
                            iter.next();
                        }
                    }
                })
            })
            .collect();
        for handle in writers.into_iter().chain(readers) {
            handle.join().unwrap();
        }
        assert_eq!(
            table.len(),
            4 * usize::try_from(ROUNDS).unwrap(),
            "every insert is present"
        );
    }
}
