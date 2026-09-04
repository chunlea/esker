//! The surface the memtable's storage answers to
//! ([ADR 0041](../../../docs/adr/0041-the-in-house-arena-skiplist.md)).
//!
//! One implementation now — the in-house arena [`skiplist`](super::skiplist) — and two while the
//! benchmark in `docs/bench/skiplist.md` was being taken. This trait is what let the same
//! generated programme run through the arena store and through the `crossbeam-skiplist` one it
//! replaces and require the same answers of both, and what made the A/B a one-line change rather
//! than a revert.
//!
//! It is kept for the same reason, because the number that came out was mixed: a scan eighteen
//! times cheaper against an insert between 1.2 and 1.8 times dearer. A third layout —
//! `LevelDB`'s, with the key bytes inside the node's own allocation — is the obvious next
//! attempt, and this is the seam that would keep it to one file.
//!
//! # Why a cursor is a position and not a struct
//!
//! The two implementations disagreed about what a cursor *is*, and that disagreement is the
//! whole reason ADR 0041 exists. `crossbeam-skiplist` handed out entries that borrow the map, so
//! its cursor could not hold one without being self-referential; it copied the entry instead,
//! and its position was that copy. The arena skiplist's position is a `u32` node offset, and its
//! key and value are borrowed straight out of the arena.
//!
//! So a position is an associated type, and reading through one takes both the store and the
//! position — `fn key<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8]`. That signature is what let
//! the copying implementation borrow from the position and the arena implementation borrow from
//! the store, through one caller.

use std::sync::Arc;

use crate::dbformat::InternalKeyComparator;

/// An append-only ordered store of internal keys to values.
///
/// Every method except [`Store::insert`] may be called from any number of threads at once, and
/// concurrently with an insert. `insert` is what a single writer calls; the implementations
/// differ in whether they enforce that themselves.
pub(super) trait Store: Send + Sync + std::fmt::Debug {
    /// Where a cursor is. [`Default`] is "nowhere", which is where a fresh cursor starts and
    /// where one lands when it walks off either end.
    type Pos: Clone + Default + Send + Sync + std::fmt::Debug;

    /// An empty store ordered by `comparator`. `seed` decides whatever the implementation draws
    /// at random; one that draws nothing ignores it.
    fn new(comparator: Arc<InternalKeyComparator>, seed: u64) -> Self;

    /// The comparator this store is ordered by.
    fn comparator(&self) -> &Arc<InternalKeyComparator>;

    /// Inserts `head ++ tail` mapped to `value`, and returns whether it landed.
    ///
    /// The key arrives in two pieces because the caller's is a user key and an eight-byte tag,
    /// and joining them above here would be an allocation per insert that the arena store does
    /// not otherwise need.
    fn insert(&self, head: &[u8], tail: &[u8], value: &[u8]) -> bool;

    /// How many entries have been published.
    fn len(&self) -> usize;

    /// Whether `pos` is on an entry.
    fn valid(&self, pos: &Self::Pos) -> bool;

    /// The internal key at `pos`, empty when it is on nothing.
    fn key<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8];

    /// The value at `pos`, empty when it is on nothing or on a tombstone.
    fn value<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8];

    /// The first entry at or after `target`.
    fn seek(&self, target: &[u8]) -> Self::Pos;

    /// The last entry at or before `target`.
    fn seek_for_prev(&self, target: &[u8]) -> Self::Pos;

    /// The first entry.
    fn first(&self) -> Self::Pos;

    /// The last entry.
    fn last(&self) -> Self::Pos;

    /// The entry after `pos`. Nowhere past the end, and a no-op from nowhere.
    fn after(&self, pos: &Self::Pos) -> Self::Pos;

    /// The entry before `pos`. Nowhere before the start, and a no-op from nowhere.
    fn before(&self, pos: &Self::Pos) -> Self::Pos;
}
