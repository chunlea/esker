//! A single-writer, multi-reader, append-only skiplist over the arena beside it
//! ([ADR 0041](../../../docs/adr/0041-the-in-house-arena-skiplist.md)).
//!
//! This is `LevelDB`'s structure, which is the right one because the engine's memtable has
//! `LevelDB`'s concurrency and not a general lock-free map's: one thread inserts at a time,
//! nothing is ever removed or changed, readers run unsynchronised throughout, and a reader may
//! outlive the table because it holds an `Arc` to it. That last pair is what makes the hard
//! problem — when may a node be freed while a reader might be inside it — not arise at all.
//!
//! # The publication rule
//!
//! **A node is written in full — its header, its key and value bytes, and all of its own
//! forward pointers — before any existing node's forward pointer is made to reach it, and that
//! last store is a `Release` matched by an `Acquire` in every reader.** A reader therefore sees
//! either no node or a complete one, and never a half-built one.
//!
//! That is the whole safety argument, and it is one sentence so that a reviewer can hold the
//! code against it. Everything else follows: the header words are read `Relaxed` because the
//! `Acquire` that reached the node already ordered them, and a node's own forward pointers are
//! written `Relaxed` because nothing can reach the node while they are being written.
//!
//! # The writer's mutex
//!
//! ADR 0041 argued from `db/write.rs` that writers are serialised, and they are: group commit
//! makes a thread the leader only when `!queue.writing`, and `commit_group` holds the only call
//! to [`MemTable::add`](super::MemTable::add) on the write path. But `add` takes `&self` and is
//! `pub`, and this crate's own `readers_and_writers_run_concurrently` inserts from four threads
//! at once. A structure whose soundness rests on a discipline its signature cannot express is
//! what invariant 8 exists to refuse, so [`SkipList::insert`] takes a mutex.
//!
//! No reader ever touches it. Under group commit it is uncontended, so it costs an atomic
//! swap per insert; in exchange, two threads calling `add` get a correct list instead of a data
//! race, and nothing above `memtable.rs` has to know the rule.
//!
//! # Heights
//!
//! Drawn from a seeded [`Pcg32`] held on the table, so a memtable's shape is a function of its
//! seed and a failing test replays from it. `crossbeam-skiplist` drew from thread-local entropy
//! and could not.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering as Memory};
use std::sync::{Arc, Mutex, PoisonError};

use esker_base::rng::Pcg32;

use super::arena::Chunks;
use crate::dbformat::{Comparator, InternalKeyComparator};

/// The offset that means "there is no node here".
///
/// Zero, and [`SkipList::new`] spends the arena's first word on nothing so that no node can ever
/// sit there. A fresh chunk of words is all zeros, so an unwritten forward pointer already reads
/// `NIL` and the writer never has to store it.
pub(super) const NIL: u32 = 0;

/// The head node, at the first offset after the reserved word. Always this, because it is the
/// second thing [`SkipList::new`] allocates out of an empty arena.
const HEAD: u32 = 1;

/// The tallest a node may be. `BRANCHING.pow(16)` is four billion entries, which is past the
/// point where the arena's four-gigabyte offset space runs out first.
const MAX_HEIGHT: usize = 16;

/// [`MAX_HEIGHT`] as a `u32`, for the arena offsets. The assertion below is what keeps the two
/// from drifting: a head node allocated shorter than a node's height would send a link into the
/// words of whatever was allocated next.
const MAX_HEIGHT_U32: u32 = 16;
const _: () = assert!(MAX_HEIGHT == MAX_HEIGHT_U32 as usize);

/// One node in four is promoted to the next level. `LevelDB`'s number.
const BRANCHING: u32 = 4;

/// Words of node header before the forward pointers: the byte offset of `key ++ value`, the key
/// length, the value length, and the height.
const HEADER: u32 = 4;

/// Chunk zero of the byte arena holds 4 KiB of keys and values.
const BYTES_SHIFT: u32 = 12;

/// Chunk zero of the word arena holds 1024 words, which is also 4 KiB.
const WORDS_SHIFT: u32 = 10;

/// The default seed for a table's height draws.
///
/// Fixed rather than drawn from the clock, because determinism is the property that matters:
/// two runs of the same test build the same shape, and a failure replays. Tables do not need
/// *independent* heights — a height is drawn per insert and never depends on the key, so the
/// balance argument holds however many tables share a stream.
pub(super) const DEFAULT_SEED: u64 = 0x5EED_5217_C0DE_1EA5;

/// Everything only the writer touches, behind the writer's mutex.
#[derive(Debug)]
struct Writer {
    /// The height source. Seeded, so a shape replays.
    rng: Pcg32,
    /// Scratch for [`SkipList::insert`]: the node at each level whose forward pointer will be
    /// made to reach the new one. Kept here so an insert allocates nothing.
    prev: [u32; MAX_HEIGHT],
}

/// An append-only ordered multimap of byte keys to byte values, backed by an arena.
///
/// Keys are compared with the column family's comparator, held once here rather than once per
/// key — which is the `Arc` clone per insert that ADR 0041 item 2 set out to remove.
#[derive(Debug)]
pub(super) struct SkipList {
    /// Key and value bytes. A node names a contiguous run of `key ++ value` in here.
    bytes: Chunks<u8>,
    /// Node headers and forward pointers.
    words: Chunks<AtomicU32>,
    comparator: Arc<InternalKeyComparator>,
    /// The height of the tallest node, and so the level every search starts at.
    height: AtomicU32,
    /// How many nodes have been published.
    len: AtomicUsize,
    writer: Mutex<Writer>,
    /// The ordering [`SkipList::publish`] uses for the store that makes a node reachable.
    /// `Release` in every build that is not a test; see [`SkipList::with_relaxed_publication`].
    #[cfg(test)]
    publication: Memory,
}

impl SkipList {
    /// A list whose arenas give up after a few dozen bytes, so a test can reach the exhaustion
    /// path without allocating four gigabytes to get there.
    #[cfg(test)]
    pub(super) fn cramped(comparator: Arc<InternalKeyComparator>, seed: u64) -> Self {
        let mut list = Self::new(comparator, seed);
        list.bytes = Chunks::<u8>::cramped(4, 2);
        list
    }

    /// An empty list ordered by `comparator`, drawing heights from `seed`.
    pub(super) fn new(comparator: Arc<InternalKeyComparator>, seed: u64) -> Self {
        let words = Chunks::<AtomicU32>::new(WORDS_SHIFT);
        // Word zero is spent so `NIL == 0` can never name a node; the head is what comes next.
        // Both allocations are tens of words out of a chunk of a thousand, so neither can fail
        // — and if one somehow did, the head's words would resolve to nothing, every search
        // would find `NIL`, and the table would read as permanently empty rather than panic.
        let reserved = words.alloc(1);
        let head = words.alloc(HEADER + MAX_HEIGHT_U32);
        debug_assert_eq!(reserved, Some(NIL));
        debug_assert_eq!(head, Some(HEAD));
        Self {
            bytes: Chunks::<u8>::new(BYTES_SHIFT),
            words,
            comparator,
            height: AtomicU32::new(1),
            len: AtomicUsize::new(0),
            writer: Mutex::new(Writer {
                rng: Pcg32::from_seed(seed),
                prev: [HEAD; MAX_HEIGHT],
            }),
            #[cfg(test)]
            publication: Memory::Release,
        }
    }

    /// A deliberately broken list: the store that makes a node reachable is `Relaxed`, so a
    /// reader that sees the node has no happens-before edge to the bytes it names.
    ///
    /// ADR 0041 item 3, which the ADR calls not negotiable and which this repository has its
    /// own lesson about: a checker that has never been shown red is evidence of nothing. The
    /// checker that can see this is Miri, whose data-race detector implements the C++ model —
    /// on x86 and on ARM the two orderings compile to instructions that will very likely never
    /// diverge in a test run, so a thread test passing against this list says nothing at all.
    ///
    /// See `relaxed_publication_is_a_data_race` in the tests beside this file for how to run it.
    #[cfg(test)]
    pub(super) fn with_relaxed_publication(comparator: Arc<InternalKeyComparator>) -> Self {
        Self {
            publication: Memory::Relaxed,
            ..Self::new(comparator, DEFAULT_SEED)
        }
    }

    /// The ordering that publishes a node. Always `Release` outside tests.
    #[cfg(not(test))]
    #[allow(
        clippy::unused_self,
        reason = "the test build reads a field here; see with_relaxed_publication"
    )]
    fn publication(&self) -> Memory {
        Memory::Release
    }

    /// The ordering that publishes a node.
    #[cfg(test)]
    fn publication(&self) -> Memory {
        self.publication
    }

    /// The comparator this list is ordered by.
    pub(super) fn comparator(&self) -> &Arc<InternalKeyComparator> {
        &self.comparator
    }

    /// How many entries have been published.
    ///
    /// Counts nodes, so a key inserted twice at the same tag counts twice — an append-only
    /// structure cannot replace one, and nothing in the engine can produce that pair because
    /// sequence numbers come from a single `fetch_add`.
    pub(super) fn len(&self) -> usize {
        self.len.load(Memory::Relaxed)
    }

    // ---- reading a node -------------------------------------------------------------------

    /// All four header words of `node` at once, or an empty slice for a node this list never
    /// allocated.
    ///
    /// One arena resolution rather than one per field, and that is a measured choice rather
    /// than a tidy one. Turning an offset into an address costs a dozen instructions — the
    /// chunk index, the bounds, the directory load — and a seek compares thirty-odd keys, each
    /// of which reads this. The first interleaved A/B (`docs/bench/skiplist.md`) had a scan
    /// eighteen times cheaper than `crossbeam-skiplist`'s and a *seek and an insert
    /// substantially dearer*, which is the shape of paying that cost once per field on the
    /// comparison path.
    ///
    /// `alloc` never straddles a chunk, so four words that start at `node` are four words in
    /// one chunk and one resolution reaches all of them.
    ///
    /// `Relaxed` at the point of use: a header is written before the node is published and
    /// never again, and the reader got here by an `Acquire` load of the pointer that reaches
    /// it, so the header is already ordered before that.
    #[inline]
    fn header(&self, node: u32) -> &[AtomicU32] {
        self.words.get(node, HEADER)
    }

    /// The forward pointer of `node` at `level`, or `NIL` past the end.
    ///
    /// The `Acquire` here is one half of the publication rule; [`SkipList::publish`] is the
    /// other. A level this node does not have reads as zero — `NIL` — which ends a traversal
    /// rather than following a pointer that was never written.
    #[inline]
    fn next(&self, node: u32, level: u32) -> u32 {
        self.words
            .word(node.saturating_add(HEADER).saturating_add(level))
            .map_or(NIL, |word| word.load(Memory::Acquire))
    }

    /// The internal key stored at `node`. Empty for [`HEAD`], which has no key.
    ///
    /// The returned slice borrows the arena through `&self`, so it cannot outlive the table —
    /// which is the lifetime the cursor needed and could not have while entries were owned by
    /// `crossbeam-skiplist`.
    #[inline]
    pub(super) fn key(&self, node: u32) -> &[u8] {
        let Some([offset, key_len, _, _]) = self.header(node).first_chunk::<4>() else {
            return &[];
        };
        self.bytes
            .get(offset.load(Memory::Relaxed), key_len.load(Memory::Relaxed))
    }

    /// The value stored at `node`. Empty for a tombstone and for [`HEAD`].
    #[inline]
    pub(super) fn value(&self, node: u32) -> &[u8] {
        let Some([offset, key_len, value_len, _]) = self.header(node).first_chunk::<4>() else {
            return &[];
        };
        let offset = offset
            .load(Memory::Relaxed)
            .saturating_add(key_len.load(Memory::Relaxed));
        self.bytes.get(offset, value_len.load(Memory::Relaxed))
    }

    /// The height `node` was built at. Read only by the tests that check a shape replays.
    #[cfg(test)]
    fn height_of(&self, node: u32) -> u32 {
        self.header(node)
            .get(3)
            .map_or(0, |word| word.load(Memory::Relaxed))
    }

    // ---- searching ------------------------------------------------------------------------

    /// The level every search starts at. At least one, so the loops below always run.
    fn top(&self) -> u32 {
        self.height.load(Memory::Acquire).max(1) - 1
    }

    /// Whether `node` exists and sorts strictly before `key`.
    #[inline]
    fn is_before(&self, node: u32, key: &[u8]) -> bool {
        node != NIL && self.comparator.cmp(self.key(node), key) == Ordering::Less
    }

    /// The first node at or after `key`, or `NIL` past the end.
    ///
    /// When `prev` is given, it is filled with the last node strictly before `key` at each
    /// level from zero up to the list's current height — which is exactly where a new node has
    /// to be linked in. Filling it here rather than searching again is why an insert is one
    /// descent and not two.
    ///
    /// Among nodes with equal keys this lands on the *first*, and an insert links a new node
    /// ahead of its equals, so the newest of a set of equal keys is the one a lookup finds.
    /// That is what makes [`super::MemTable::get`] agree with the replacing map it replaces.
    fn find_ge(&self, key: &[u8], mut prev: Option<&mut [u32; MAX_HEIGHT]>) -> u32 {
        let mut node = HEAD;
        let mut level: u32 = self.top();
        loop {
            let next = self.next(node, level);
            if self.is_before(next, key) {
                node = next;
            } else {
                if let Some(prev) = prev.as_deref_mut() {
                    prev[level as usize] = node;
                }
                if level == 0 {
                    return next;
                }
                level -= 1;
            }
        }
    }

    /// The last node strictly before `key`, or [`HEAD`] when there is none.
    fn find_lt(&self, key: &[u8]) -> u32 {
        let mut node = HEAD;
        let mut level: u32 = self.top();
        loop {
            let next = self.next(node, level);
            if self.is_before(next, key) {
                node = next;
            } else if level == 0 {
                return node;
            } else {
                level -= 1;
            }
        }
    }

    /// The last node at or before `key`, or [`HEAD`] when there is none.
    fn find_le(&self, key: &[u8]) -> u32 {
        let mut node = HEAD;
        let mut level: u32 = self.top();
        loop {
            let next = self.next(node, level);
            // "Not after `key`" rather than "before `key`": that is the whole difference from
            // `find_lt`, and it is what makes `seek_for_prev` land *on* an exact match.
            if next != NIL && self.comparator.cmp(self.key(next), key) != Ordering::Greater {
                node = next;
            } else if level == 0 {
                return node;
            } else {
                level -= 1;
            }
        }
    }

    /// The first node, or `NIL` when the list is empty.
    pub(super) fn first(&self) -> u32 {
        self.next(HEAD, 0)
    }

    /// The last node, or `NIL` when the list is empty.
    ///
    /// A skiplist has no back pointers, so this walks forward at each level rather than
    /// stepping back from the end.
    pub(super) fn last(&self) -> u32 {
        let mut node = HEAD;
        let mut level: u32 = self.top();
        loop {
            let next = self.next(node, level);
            if next == NIL {
                if level == 0 {
                    return if node == HEAD { NIL } else { node };
                }
                level -= 1;
            } else {
                node = next;
            }
        }
    }

    /// The first node at or after `key`, for a cursor. `NIL` past the end.
    pub(super) fn seek(&self, key: &[u8]) -> u32 {
        self.find_ge(key, None)
    }

    /// The last node at or before `key`, for a cursor. `NIL` before the start.
    pub(super) fn seek_for_prev(&self, key: &[u8]) -> u32 {
        let node = self.find_le(key);
        if node == HEAD { NIL } else { node }
    }

    /// The node after `node`, for a cursor. One pointer hop and no allocation, which is the
    /// whole reason this structure exists.
    pub(super) fn after(&self, node: u32) -> u32 {
        if node == NIL { NIL } else { self.next(node, 0) }
    }

    /// The node before `node`, for a cursor.
    ///
    /// `O(log n)`, because there are no back pointers: it is a search for the largest key
    /// strictly below this one. `LevelDB`'s cursor pays the same, and a backwards scan is rare
    /// enough that giving every node a second pointer array to avoid it is the wrong trade.
    pub(super) fn before(&self, node: u32) -> u32 {
        if node == NIL {
            return NIL;
        }
        let found = self.find_lt(self.key(node));
        // A `find_lt` that answered "the last node at or before" rather than "strictly before"
        // would return `node` itself, and every backwards walk in the engine would loop
        // forever rather than fail. That is worth a line to turn into a test failure: an
        // injected version of exactly this bug hung the suite instead of reddening it.
        debug_assert_ne!(found, node, "a backwards step landed where it started");
        if found == HEAD { NIL } else { found }
    }

    // ---- writing --------------------------------------------------------------------------

    /// Inserts `head ++ tail` mapped to `value`, and returns whether it landed.
    ///
    /// The key arrives in two pieces because the caller's is a user key and an eight-byte tag,
    /// and joining them above here would be an allocation per insert.
    ///
    /// `false` means the arena's four-gigabyte offset space is exhausted and nothing was
    /// published — see [`Chunks::alloc`]. Nothing in the engine can reach it: a memtable is
    /// switched once it passes `write_buffer_size`, so it overshoots by at most one batch.
    pub(super) fn insert(&self, head: &[u8], tail: &[u8], value: &[u8]) -> bool {
        // A poisoned lock means a writer panicked part way through an insert. Recovering it is
        // safe and losing the write is not: the publication rule says nothing is reachable
        // until its pointer is stored, and a node linked at some levels and not others is a
        // correct skiplist that is merely shorter than it meant to be.
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let writer = &mut *writer;

        let Some(bytes) = self.bytes.alloc_bytes(&[head, tail, value]) else {
            return false;
        };
        let (Ok(key_len), Ok(value_len)) = (
            u32::try_from(head.len() + tail.len()),
            u32::try_from(value.len()),
        ) else {
            return false;
        };

        // The key is in the arena and nothing points at it, so the search compares against it
        // where it lies rather than against a copy of it.
        let key = self.bytes.get(bytes, key_len);
        let was = self.height.load(Memory::Relaxed).max(1);
        writer.prev = [HEAD; MAX_HEIGHT];
        self.find_ge(key, Some(&mut writer.prev));

        let height = draw_height(&mut writer.rng);
        let Some(node) = self.words.alloc(HEADER + height) else {
            return false;
        };
        if height > was {
            // Publishing the taller list before linking it in is deliberate. A reader that sees
            // the new height starts at a level where the head still points at `NIL`, and simply
            // descends; a reader that sees the old height searches a shorter list, which is
            // slower and just as correct. Level zero has everything either way.
            self.height.store(height, Memory::Release);
        }
        // One resolution for all four, for the same reason `header` reads them in one.
        if let Some([offset, key, value, tall]) = self.header(node).first_chunk::<4>() {
            offset.store(bytes, Memory::Relaxed);
            key.store(key_len, Memory::Relaxed);
            value.store(value_len, Memory::Relaxed);
            tall.store(height, Memory::Relaxed);
        }
        self.publish(node, height, &writer.prev);
        self.len.fetch_add(1, Memory::Relaxed);
        true
    }

    /// Links `node` into the list at every level below `height`.
    ///
    /// This is the publication rule in code. At each level the new node's own forward pointer
    /// is written first and `Relaxed` — nothing can reach the node, so nothing can read it —
    /// and then the pointer that *does* reach it is stored `Release`. Every reader loads that
    /// pointer with `Acquire` in [`SkipList::next`], so a reader that sees the node sees
    /// everything written before it: header, key, value, and the forward pointers of every
    /// level up to and including this one.
    ///
    /// Levels above the one being linked are still `NIL` in the fresh arena words, and no
    /// reader can reach the node at those levels yet, so they are never observed unwritten.
    fn publish(&self, node: u32, height: u32, prev: &[u32; MAX_HEIGHT]) {
        // The new node's own forward pointers are one contiguous run inside one chunk — its
        // whole allocation is — so they cost one arena resolution rather than one per level.
        let links = self.words.get(node.saturating_add(HEADER), height);
        for level in 0..height {
            let previous = prev[level as usize];
            let after = self.next(previous, level);
            if let Some(word) = links.get(level as usize) {
                word.store(after, Memory::Relaxed);
            }
            if let Some(word) = self.words.word(previous.saturating_add(HEADER) + level) {
                word.store(node, self.publication());
            }
        }
    }
}

impl super::store::Store for SkipList {
    /// A node offset. [`Default`] is zero, which is [`NIL`] — the arena spends its first word
    /// so that no node can sit there, which is what lets "nowhere" and "the first node" be
    /// told apart without a second field.
    type Pos = u32;

    fn new(comparator: Arc<InternalKeyComparator>, seed: u64) -> Self {
        Self::new(comparator, seed)
    }

    fn comparator(&self) -> &Arc<InternalKeyComparator> {
        Self::comparator(self)
    }

    fn insert(&self, head: &[u8], tail: &[u8], value: &[u8]) -> bool {
        Self::insert(self, head, tail, value)
    }

    fn len(&self) -> usize {
        Self::len(self)
    }

    fn valid(&self, pos: &Self::Pos) -> bool {
        *pos != NIL
    }

    fn key<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8] {
        Self::key(self, *pos)
    }

    fn value<'a>(&'a self, pos: &'a Self::Pos) -> &'a [u8] {
        Self::value(self, *pos)
    }

    fn seek(&self, target: &[u8]) -> Self::Pos {
        Self::seek(self, target)
    }

    fn seek_for_prev(&self, target: &[u8]) -> Self::Pos {
        Self::seek_for_prev(self, target)
    }

    fn first(&self) -> Self::Pos {
        Self::first(self)
    }

    fn last(&self) -> Self::Pos {
        Self::last(self)
    }

    fn after(&self, pos: &Self::Pos) -> Self::Pos {
        Self::after(self, *pos)
    }

    fn before(&self, pos: &Self::Pos) -> Self::Pos {
        Self::before(self, *pos)
    }
}

/// Draws a height: one, then one more for each consecutive draw that hits one chance in
/// [`BRANCHING`], up to [`MAX_HEIGHT`].
fn draw_height(rng: &mut Pcg32) -> u32 {
    let mut height = 1;
    while height < MAX_HEIGHT_U32 && rng.next_u32() % BRANCHING == 0 {
        height += 1;
    }
    height
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SEED, HEAD, MAX_HEIGHT, NIL, SkipList};
    use crate::dbformat::{
        BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, internal_key,
    };
    use std::collections::BTreeSet;
    use std::sync::Arc;

    /// How many entries the bulk tests build. Miri interprets every instruction, so a run
    /// sized for a native build would take hours there and get skipped instead of run.
    #[cfg(miri)]
    const BULK: u64 = 60;
    #[cfg(not(miri))]
    const BULK: u64 = 3000;

    fn list() -> SkipList {
        SkipList::new(
            Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator))),
            DEFAULT_SEED,
        )
    }

    /// An internal key for user key `key` at `seqno`, as a `Put`.
    fn key(key: &str, seqno: u64) -> Vec<u8> {
        internal_key(key.as_bytes(), seqno, EntryKind::Put)
    }

    /// Every key in the list, in iteration order.
    ///
    /// Bounded by `len`, because a `publish` that linked a tower from the wrong predecessor can
    /// leave a cycle in the level-zero list — and an unbounded walk over one hangs the suite
    /// instead of failing it. An injected version of exactly that bug did.
    fn walk(list: &SkipList) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut node = list.first();
        while node != NIL {
            assert!(out.len() < list.len(), "the level-zero list has a cycle");
            out.push(list.key(node).to_vec());
            node = list.after(node);
        }
        out
    }

    /// What a `BTreeSet` under the same comparator would say the order is. The comparator is
    /// not bytewise — the tag sorts descending — so this cannot be a plain sort.
    fn ordered(keys: &[Vec<u8>], list: &SkipList) -> Vec<Vec<u8>> {
        let mut sorted = keys.to_vec();
        sorted.sort_by(|a, b| list.comparator().cmp(a, b));
        sorted
    }

    #[test]
    fn the_head_sits_where_nil_cannot_reach_it() {
        let list = list();
        assert_eq!(HEAD, 1, "word zero is spent so NIL == 0 names nothing");
        assert_ne!(HEAD, NIL);
        assert_eq!(list.first(), NIL, "an empty list starts nowhere");
        assert_eq!(list.last(), NIL);
        assert_eq!(list.len(), 0);
        assert_eq!(list.seek(&key("a", 1)), NIL);
        assert_eq!(list.seek_for_prev(&key("a", 1)), NIL);
    }

    /// The order is the comparator's, not the bytes'. Internal keys sort by user key ascending
    /// then tag *descending*, so a structure that sorted bytewise would pass a test built on
    /// distinct user keys and fail on this one.
    #[test]
    fn entries_come_back_in_comparator_order() {
        let list = list();
        let mut inserted = Vec::new();
        for (user, seqno) in [
            ("m", 3u64),
            ("a", 1),
            ("m", 9),
            ("z", 2),
            ("a", 7),
            ("m", 1),
            ("b", 4),
        ] {
            let internal = key(user, seqno);
            assert!(list.insert(&internal, b"", user.as_bytes()));
            inserted.push(internal);
        }
        assert_eq!(walk(&list), ordered(&inserted, &list));
        assert_eq!(list.len(), inserted.len());
        let sorted = ordered(&inserted, &list);
        assert_eq!(list.key(list.first()), &sorted[0][..]);
        assert_eq!(list.key(list.last()), &sorted[sorted.len() - 1][..]);
    }

    /// The key may arrive in two pieces, because the caller's is a user key and a tag and
    /// joining them would be an allocation per insert. It has to land as if it were one.
    #[test]
    fn a_key_in_two_pieces_is_one_key() {
        let list = list();
        let whole = key("split", 5);
        let (head, tail) = whole.split_at(3);
        assert!(list.insert(head, tail, b"value"));
        let node = list.seek(&whole);
        assert_ne!(node, NIL);
        assert_eq!(list.key(node), &whole[..]);
        assert_eq!(list.value(node), b"value");
    }

    #[test]
    fn seek_and_seek_for_prev_land_on_the_right_side() {
        let list = list();
        for user in ["a", "c", "e"] {
            assert!(list.insert(&key(user, 1), b"", user.as_bytes()));
        }
        let at = |k: &[u8]| {
            let node = list.seek(k);
            (node != NIL).then(|| list.value(node).to_vec())
        };
        assert_eq!(at(&key("b", 1)), Some(b"c".to_vec()), "seek goes forward");
        assert_eq!(
            at(&key("c", 1)),
            Some(b"c".to_vec()),
            "an exact match stays"
        );
        assert_eq!(at(&key("z", 1)), None, "past the end");

        let before = |k: &[u8]| {
            let node = list.seek_for_prev(k);
            (node != NIL).then(|| list.value(node).to_vec())
        };
        assert_eq!(before(&key("b", 1)), Some(b"a".to_vec()), "goes backward");
        assert_eq!(
            before(&key("c", 1)),
            Some(b"c".to_vec()),
            "and lands on an exact match rather than stepping past it"
        );
        assert_eq!(before(&key("", 1)), None, "before the start");
    }

    #[test]
    fn before_and_after_walk_the_same_list_both_ways() {
        let list = list();
        for user in ["a", "b", "c", "d"] {
            assert!(list.insert(&key(user, 1), b"", user.as_bytes()));
        }
        let forward = walk(&list);
        let mut backward = Vec::new();
        let mut node = list.last();
        while node != NIL {
            backward.push(list.key(node).to_vec());
            node = list.before(node);
        }
        backward.reverse();
        assert_eq!(forward, backward);
        assert_eq!(
            list.before(list.first()),
            NIL,
            "before the first is nowhere"
        );
        assert_eq!(list.after(list.last()), NIL, "after the last is nowhere");
    }

    /// An append-only structure cannot replace an equal key the way a map does, so it puts the
    /// newer node first — which is what makes a lookup ("the first entry at or after this")
    /// give the answer replacement would have given.
    #[test]
    fn the_newest_of_two_equal_keys_is_found_first() {
        let list = list();
        let internal = key("k", 1);
        assert!(list.insert(&internal, b"", b"first"));
        assert!(list.insert(&internal, b"", b"second"));
        assert_eq!(list.value(list.seek(&internal)), b"second");
        assert_eq!(list.len(), 2, "both are stored; nothing was replaced");
        assert_eq!(
            walk(&list),
            vec![internal.clone(), internal],
            "and both are reachable, in that order"
        );
    }

    /// A shape is a function of its seed. `crossbeam-skiplist` drew heights from thread-local
    /// entropy, so a failure it produced could not be replayed; this is what buys that back.
    #[test]
    fn heights_replay_from_the_seed() {
        let shape = |seed: u64| {
            let list = SkipList::new(
                Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator))),
                seed,
            );
            let mut heights = Vec::new();
            for i in 0..BULK.min(200) {
                assert!(list.insert(&key("k", i), b"", b"v"));
            }
            let mut node = list.first();
            while node != NIL {
                heights.push(list.height_of(node));
                node = list.after(node);
            }
            heights
        };
        let once = shape(DEFAULT_SEED);
        assert_eq!(once, shape(DEFAULT_SEED), "the same seed is the same shape");
        assert_ne!(once, shape(1), "and a different seed is a different one");
        assert!(
            once.iter().any(|&h| h > 1),
            "the draw has to actually promote sometimes, or the list is a linked list"
        );
        assert!(once.iter().all(|&h| h as usize <= MAX_HEIGHT));
    }

    /// The trap the arena exists for, at the level that matters: a key read out of the list
    /// stays readable while the writer grows the arena underneath it.
    #[test]
    fn a_key_read_before_the_arena_grew_still_reads_after() {
        let list = list();
        assert!(list.insert(&key("first", 1), b"", b"payload"));
        let node = list.first();
        let borrowed = list.key(node);
        let value = list.value(node);
        for i in 0..BULK {
            assert!(list.insert(&key("filler", i), b"", &[b'x'; 64]));
        }
        assert_eq!(borrowed, &key("first", 1)[..]);
        assert_eq!(value, b"payload");
    }

    /// Thousands of entries across many arena chunks, checked entry for entry against the order
    /// a `BTreeSet` gives under the same comparator.
    #[test]
    fn a_large_list_agrees_with_a_sorted_model() {
        let list = list();
        let mut model = BTreeSet::new();
        let mut seed = 12_345u64;
        for _ in 0..BULK {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let internal = key(&format!("k{:05}", seed % 900), seed % 50);
            if model.insert(internal.clone()) {
                assert!(list.insert(&internal, b"", b"v"));
            }
        }
        let mut expected: Vec<Vec<u8>> = model.into_iter().collect();
        expected.sort_by(|a, b| list.comparator().cmp(a, b));
        assert_eq!(walk(&list), expected);
        for internal in &expected {
            assert_eq!(
                list.key(list.seek(internal)),
                &internal[..],
                "seek finds it"
            );
        }
    }

    /// An insert the arena refuses publishes nothing: the list keeps the entries it had, in
    /// order, and stops counting. Reaching this for real needs four gigabytes in one memtable,
    /// so the arenas here are capped down to a few dozen bytes instead.
    #[test]
    fn a_refused_insert_leaves_the_list_exactly_as_it_was() {
        let list = SkipList::cramped(
            Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator))),
            DEFAULT_SEED,
        );
        let mut accepted = Vec::new();
        let mut refused = 0;
        for i in 0..64u64 {
            let internal = key("k", i);
            if list.insert(&internal, b"", b"v") {
                accepted.push(internal);
            } else {
                refused += 1;
            }
        }
        assert!(
            refused > 0,
            "the cap has to actually bite for this to test anything"
        );
        assert!(
            !accepted.is_empty(),
            "and it must not bite on the first insert"
        );
        assert_eq!(list.len(), accepted.len(), "a refusal counts nothing");
        assert_eq!(walk(&list), ordered(&accepted, &list), "and stores nothing");
    }

    /// The table is shared across threads through an `Arc`, so this has to hold; it is checked
    /// at compile time because a regression would otherwise show up as a borrow error in a
    /// caller rather than here.
    #[test]
    fn the_list_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SkipList>();
    }
}
