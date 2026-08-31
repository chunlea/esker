//! The in-memory half of the log-structured design: a sorted map of internal keys.
//!
//! Every write lands here after its bytes are in the log, and stays until the memtable is
//! flushed to an SST. It is a `crossbeam-skiplist` map — the one piece of concurrent code the
//! project buys rather than writes (`CLAUDE.md`, "Dependency policy") — because a lock-free
//! ordered map that supports concurrent readers during a write is exactly the hard part, and
//! an in-house arena skiplist is a post-v1 replacement behind this same surface.
//!
//! # Nothing is ever removed
//!
//! A delete is an *insert* of a tombstone, and a memtable's entries are never taken out of it.
//! Two things depend on that. Older versions of a key may still exist in lower levels, so only
//! a stored tombstone can hide them; and an iterator holds a position in the map, so removing
//! an entry underneath it would break the snapshot it thinks it has. Memtables die whole,
//! after a flush, and never piecemeal.
//!
//! # Ordering
//!
//! `crossbeam-skiplist` orders by the key type's [`Ord`], but the order the engine needs is
//! chosen at runtime — a column family carries its own comparator. So each key carries a
//! handle to the comparator and delegates to it. That is one `Arc` clone per
//! insert, which the in-house skiplist will remove by holding the comparator once per table.

use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use crossbeam_skiplist::SkipMap;
use crossbeam_skiplist::map::Entry as SkipEntry;

use crate::range_del::{RangeTombstone, RangeTombstones};

use crate::dbformat::{
    Comparator, EntryKind, InternalKeyComparator, SeqNo, append_internal_key, extract_user_key,
    lookup_key, split_internal_key,
};

/// Bytes charged per entry on top of its key and value, to account for the skiplist node, the
/// two `Vec` headers and the comparator handle. Approximate on purpose: it decides when to
/// flush, and being a little wrong there costs a slightly early or late flush and nothing else.
const ENTRY_OVERHEAD: usize = 64;

/// An internal key ordered by its column family's comparator.
#[derive(Debug, Clone)]
struct MemKey {
    bytes: Vec<u8>,
    order: Arc<InternalKeyComparator>,
}

impl Ord for MemKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.cmp(&self.bytes, &other.bytes)
    }
}

impl PartialOrd for MemKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MemKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MemKey {}

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
    map: SkipMap<MemKey, Vec<u8>>,
    range_tombstones: Mutex<RangeTombstones>,
    comparator: Arc<InternalKeyComparator>,
    approximate_size: AtomicUsize,
}

impl MemTable {
    /// An empty memtable ordered by `comparator`.
    pub fn new(comparator: Arc<InternalKeyComparator>) -> Self {
        Self {
            map: SkipMap::new(),
            range_tombstones: Mutex::new(RangeTombstones::new()),
            comparator,
            approximate_size: AtomicUsize::new(0),
        }
    }

    /// The comparator this table is ordered by. Iterators and merge iterators need it.
    pub fn comparator(&self) -> &Arc<InternalKeyComparator> {
        &self.comparator
    }

    /// Inserts one entry. Takes `&self`: writers hold no lock over the map, and readers keep
    /// working while this runs.
    pub fn add(&self, seqno: SeqNo, kind: EntryKind, key: &[u8], value: &[u8]) {
        let mut bytes = Vec::with_capacity(key.len() + 8);
        append_internal_key(key, seqno, kind, &mut bytes);
        let charge = bytes.len() + value.len() + ENTRY_OVERHEAD;
        self.map.insert(
            MemKey {
                bytes,
                order: Arc::clone(&self.comparator),
            },
            value.to_vec(),
        );
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
                self.comparator.user_comparator().as_ref(),
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
        let target = self.key(lookup_key(user_key, snapshot));
        let entry = self.map.lower_bound(Bound::Included(&target))?;
        let internal = entry.key().bytes.as_slice();
        if self
            .comparator
            .user_comparator()
            .cmp(extract_user_key(internal), user_key)
            != Ordering::Equal
        {
            return None;
        }
        match split_internal_key(internal) {
            Some((_, seqno, EntryKind::Put)) => Some(Lookup::Found {
                value: entry.value().clone(),
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
        self.map.len()
    }

    /// Whether nothing has been written to this table — **range deletes included**.
    ///
    /// A table holding only `delete_range` entries has an empty map and is not empty: the
    /// flush path uses this to decide whether there is anything to write, and answering "yes,
    /// empty" would drop the deletes on the floor at the next memtable switch
    /// ([ADR 0017](../../docs/adr/0017-range-tombstones.md)).
    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && !self.has_range_tombstones()
    }

    /// A cursor over the table, positioned nowhere until it is seeked.
    ///
    /// Takes an `Arc` and keeps it: a cursor outlives the memtable switch that retires the
    /// table it is reading, which is what lets a scan carry on across a flush.
    pub fn iter(self: &Arc<Self>) -> MemTableIter {
        MemTableIter {
            table: Arc::clone(self),
            current: None,
        }
    }

    fn key(&self, bytes: Vec<u8>) -> MemKey {
        MemKey {
            bytes,
            order: Arc::clone(&self.comparator),
        }
    }
}

/// A two-directional cursor over a memtable, keyed by internal keys.
///
/// The shape every iterator in the engine shares (`docs/DESIGN.md` §4.1), so that the merge
/// iterator can drive memtables and SSTs through the same calls.
///
/// # Why it navigates by key
///
/// `crossbeam-skiplist` hands out entries that borrow the map, so a cursor built from one
/// would have to hold both the `Arc<MemTable>` and a reference into it — a self-referential
/// struct, which in safe Rust means either a lifetime the caller has to thread through
/// everything above, or a crate we are not allowed to add. Instead the cursor remembers its
/// position as a key and re-finds it, which makes each step `O(log n)` and copies the entry
/// it lands on.
///
/// That is a real cost and it is taken deliberately: `CLAUDE.md` says to prefer safe code and
/// to optimise after a profile. `TODO(post-v1)`: the in-house arena skiplist that replaces
/// this one can hand out an owned cursor, and this goes back to `O(1)`.
#[derive(Debug)]
pub struct MemTableIter {
    table: Arc<MemTable>,
    /// The entry under the cursor: its internal key and its value.
    current: Option<(Vec<u8>, Vec<u8>)>,
}

impl MemTableIter {
    /// Whether the cursor is on an entry.
    pub fn valid(&self) -> bool {
        self.current.is_some()
    }

    /// The internal key under the cursor, empty when the cursor is not valid.
    pub fn key(&self) -> &[u8] {
        self.current.as_ref().map_or(&[], |(key, _)| key.as_slice())
    }

    /// The user key under the cursor, without its tag.
    pub fn user_key(&self) -> &[u8] {
        extract_user_key(self.key())
    }

    /// The value under the cursor, empty for a tombstone or an invalid cursor.
    pub fn value(&self) -> &[u8] {
        self.current
            .as_ref()
            .map_or(&[], |(_, value)| value.as_slice())
    }

    /// Positions the cursor on the first entry at or after `target` (an internal key).
    pub fn seek(&mut self, target: &[u8]) {
        let key = self.table.key(target.to_vec());
        self.current = take(self.table.map.lower_bound(Bound::Included(&key)));
    }

    /// Positions the cursor on the last entry at or before `target` (an internal key).
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        let key = self.table.key(target.to_vec());
        self.current = take(self.table.map.upper_bound(Bound::Included(&key)));
    }

    /// Positions the cursor on the first entry.
    pub fn seek_to_first(&mut self) {
        self.current = take(self.table.map.front());
    }

    /// Positions the cursor on the last entry.
    pub fn seek_to_last(&mut self) {
        self.current = take(self.table.map.back());
    }

    /// Advances forward. Becomes invalid past the end; a no-op when already invalid.
    pub fn next(&mut self) {
        let Some((key, _)) = self.current.take() else {
            return;
        };
        let key = self.table.key(key);
        self.current = take(self.table.map.lower_bound(Bound::Excluded(&key)));
    }

    /// Steps backward. Becomes invalid before the start; a no-op when already invalid.
    pub fn prev(&mut self) {
        let Some((key, _)) = self.current.take() else {
            return;
        };
        let key = self.table.key(key);
        self.current = take(self.table.map.upper_bound(Bound::Excluded(&key)));
    }
}

/// Copies an entry out of the skiplist, which is what makes the cursor owned.
fn take(entry: Option<SkipEntry<'_, MemKey, Vec<u8>>>) -> Option<(Vec<u8>, Vec<u8>)> {
    entry.map(|entry| (entry.key().bytes.clone(), entry.value().clone()))
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
        let table = table();
        let writers: Vec<_> = (0..4u64)
            .map(|worker| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    for i in 0..500u64 {
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
                    for _ in 0..500 {
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
        assert_eq!(table.len(), 2000, "every insert is present");
    }
}
