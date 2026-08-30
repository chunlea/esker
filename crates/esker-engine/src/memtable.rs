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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use crossbeam_skiplist::SkipMap;
use crossbeam_skiplist::map::Entry as SkipEntry;

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
    Found(Vec<u8>),
    /// The newest visible entry is a tombstone. The key does not exist at this snapshot.
    Deleted,
}

/// One memtable: a sorted, append-only map of internal keys to values.
#[derive(Debug)]
pub struct MemTable {
    map: SkipMap<MemKey, Vec<u8>>,
    comparator: Arc<InternalKeyComparator>,
    approximate_size: AtomicUsize,
}

impl MemTable {
    /// An empty memtable ordered by `comparator`.
    pub fn new(comparator: Arc<InternalKeyComparator>) -> Self {
        Self {
            map: SkipMap::new(),
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
            Some((_, _, EntryKind::Put)) => Some(Lookup::Found(entry.value().clone())),
            // TODO(phase-5): a range tombstone hides every key in `[begin, end)`, but this
            // lookup only sees the one stored at `begin`, so it answers for that key and no
            // other. Keys strictly inside the range are the documented v1 limitation of
            // `docs/DESIGN.md` §4.7.
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

    /// Whether nothing has been written to this table.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// A cursor over the table, positioned nowhere until it is seeked.
    pub fn iter(&self) -> MemTableIter<'_> {
        MemTableIter {
            table: self,
            entry: None,
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
#[derive(Debug)]
pub struct MemTableIter<'a> {
    table: &'a MemTable,
    entry: Option<SkipEntry<'a, MemKey, Vec<u8>>>,
}

impl MemTableIter<'_> {
    /// Whether the cursor is on an entry.
    pub fn valid(&self) -> bool {
        self.entry.is_some()
    }

    /// The internal key under the cursor.
    ///
    /// # Panics
    ///
    /// If the cursor is not [`valid`](Self::valid). Callers check first; this mirrors the
    /// `LevelDB` iterator contract, where reading an invalid position is a caller bug rather
    /// than a runtime condition.
    pub fn key(&self) -> &[u8] {
        #[allow(clippy::expect_used)] // The invariant is stated on the method and checked here.
        self.entry
            .as_ref()
            .expect("key() on an invalid iterator")
            .key()
            .bytes
            .as_slice()
    }

    /// The user key under the cursor, without its tag.
    pub fn user_key(&self) -> &[u8] {
        extract_user_key(self.key())
    }

    /// The value under the cursor. Empty for a tombstone.
    ///
    /// # Panics
    ///
    /// If the cursor is not [`valid`](Self::valid); see [`key`](Self::key).
    pub fn value(&self) -> &[u8] {
        #[allow(clippy::expect_used)] // As above.
        self.entry
            .as_ref()
            .expect("value() on an invalid iterator")
            .value()
            .as_slice()
    }

    /// Positions the cursor on the first entry at or after `target` (an internal key).
    pub fn seek(&mut self, target: &[u8]) {
        let target = self.table.key(target.to_vec());
        self.entry = self.table.map.lower_bound(Bound::Included(&target));
    }

    /// Positions the cursor on the last entry at or before `target` (an internal key).
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        let target = self.table.key(target.to_vec());
        self.entry = self.table.map.upper_bound(Bound::Included(&target));
    }

    /// Positions the cursor on the first entry.
    pub fn seek_to_first(&mut self) {
        self.entry = self.table.map.front();
    }

    /// Positions the cursor on the last entry.
    pub fn seek_to_last(&mut self) {
        self.entry = self.table.map.back();
    }

    /// Advances forward. Becomes invalid past the end.
    pub fn next(&mut self) {
        self.entry = self.entry.as_ref().and_then(SkipEntry::next);
    }

    /// Steps backward. Becomes invalid before the start.
    pub fn prev(&mut self) {
        self.entry = self.entry.as_ref().and_then(SkipEntry::prev);
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

    fn table() -> MemTable {
        MemTable::new(Arc::new(InternalKeyComparator::new(Arc::new(
            BytewiseComparator,
        ))))
    }

    /// Every (user key, seqno) pair in the table, in iteration order.
    fn walk(table: &MemTable) -> Vec<(Vec<u8>, u64)> {
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

    #[test]
    fn a_put_is_visible_at_and_above_its_sequence_number() {
        let table = table();
        table.add(5, EntryKind::Put, b"key", b"value");
        assert_eq!(table.get(b"key", 5), Some(Lookup::Found(b"value".to_vec())));
        assert_eq!(
            table.get(b"key", 100),
            Some(Lookup::Found(b"value".to_vec()))
        );
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
        assert_eq!(table.get(b"k", 3), Some(Lookup::Found(b"three".to_vec())));
        assert_eq!(table.get(b"k", 2), Some(Lookup::Found(b"two".to_vec())));
        assert_eq!(table.get(b"k", 1), Some(Lookup::Found(b"one".to_vec())));
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
        assert_eq!(table.get(b"k", 1), Some(Lookup::Found(b"value".to_vec())));
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
                    .map(|(_, value)| match value {
                        Some(value) => Lookup::Found(value.clone()),
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
        let table = Arc::new(table());
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
