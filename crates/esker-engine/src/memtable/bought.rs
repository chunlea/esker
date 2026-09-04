//! The memtable Esker bought, kept beside the one it wrote
//! ([ADR 0041](../../../docs/adr/0041-the-in-house-arena-skiplist.md)).
//!
//! A `crossbeam-skiplist` map behind the same [`Store`] surface as
//! [`SkipList`](super::skiplist::SkipList). It was the right call in phase 1 — a lock-free
//! ordered map with concurrent readers during a write is the hard part, and getting it wrong is
//! a data race rather than a failing test — and it stays compiled until a benchmark says which
//! of the two the engine should use.
//!
//! # The two costs this exists to demonstrate
//!
//! **A cursor is a copy.** `crossbeam_skiplist::map::Entry` borrows the map, so a cursor holding
//! both the `Arc<MemTable>` and an entry would be self-referential. The position is therefore
//! an owned copy of the key and the value, and every step re-finds the key it left off at:
//! `O(log n)` and one allocation per entry, where the arena store is a pointer hop and none.
//!
//! **A key carries a comparator.** The map orders by the key type's [`Ord`] and the order the
//! engine needs is chosen per column family at runtime, so every key holds its own
//! `Arc<InternalKeyComparator>` — one clone per insert.

use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::Arc;

use crossbeam_skiplist::SkipMap;
use crossbeam_skiplist::map::Entry as SkipEntry;

use super::store::Store;
use crate::dbformat::{Comparator, InternalKeyComparator};

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

/// The bought store: a `crossbeam-skiplist` map and the comparator its keys carry.
#[derive(Debug)]
pub(super) struct Bought {
    map: SkipMap<MemKey, Vec<u8>>,
    comparator: Arc<InternalKeyComparator>,
}

impl Bought {
    fn key(&self, bytes: Vec<u8>) -> MemKey {
        MemKey {
            bytes,
            order: Arc::clone(&self.comparator),
        }
    }
}

/// Copies an entry out of the map, which is what makes the cursor owned.
fn take(entry: Option<SkipEntry<'_, MemKey, Vec<u8>>>) -> Option<(Vec<u8>, Vec<u8>)> {
    entry.map(|entry| (entry.key().bytes.clone(), entry.value().clone()))
}

impl Store for Bought {
    /// The entry itself, copied. See the module docs for why it cannot be a borrow.
    type Pos = Option<(Vec<u8>, Vec<u8>)>;

    /// The seed is unused: `crossbeam-skiplist` draws its heights from thread-local entropy,
    /// which is the reason a failure inside it cannot be replayed from one.
    fn new(comparator: Arc<InternalKeyComparator>, _seed: u64) -> Self {
        Self {
            map: SkipMap::new(),
            comparator,
        }
    }

    fn comparator(&self) -> &Arc<InternalKeyComparator> {
        &self.comparator
    }

    /// Always lands: the map allocates per node and aborts rather than refusing, so there is no
    /// exhaustion to report. Joining the two halves of the key costs the allocation the arena
    /// store avoids.
    fn insert(&self, head: &[u8], tail: &[u8], value: &[u8]) -> bool {
        let mut bytes = Vec::with_capacity(head.len() + tail.len());
        bytes.extend_from_slice(head);
        bytes.extend_from_slice(tail);
        self.map.insert(self.key(bytes), value.to_vec());
        true
    }

    /// Counts distinct keys: inserting an equal key replaces it, where the append-only store
    /// keeps both. Nothing in the engine can produce that pair.
    fn len(&self) -> usize {
        self.map.len()
    }

    fn valid(&self, pos: &Self::Pos) -> bool {
        pos.is_some()
    }

    fn key<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8] {
        pos.as_ref().map_or(&[], |(key, _)| key.as_slice())
    }

    fn value<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8] {
        pos.as_ref().map_or(&[], |(_, value)| value.as_slice())
    }

    fn seek(&self, target: &[u8]) -> Self::Pos {
        take(
            self.map
                .lower_bound(Bound::Included(&self.key(target.to_vec()))),
        )
    }

    fn seek_for_prev(&self, target: &[u8]) -> Self::Pos {
        take(
            self.map
                .upper_bound(Bound::Included(&self.key(target.to_vec()))),
        )
    }

    fn first(&self) -> Self::Pos {
        take(self.map.front())
    }

    fn last(&self) -> Self::Pos {
        take(self.map.back())
    }

    /// Re-finds the key it left off at. This is the `O(log n)` step ADR 0041 set out to remove.
    fn after(&self, pos: &Self::Pos) -> Self::Pos {
        let (key, _) = pos.as_ref()?;
        take(
            self.map
                .lower_bound(Bound::Excluded(&self.key(key.clone()))),
        )
    }

    fn before(&self, pos: &Self::Pos) -> Self::Pos {
        let (key, _) = pos.as_ref()?;
        take(
            self.map
                .upper_bound(Bound::Excluded(&self.key(key.clone()))),
        )
    }
}
