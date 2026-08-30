//! A sharded least-recently-used cache of uncompressed table blocks.
//!
//! # Shape
//!
//! [`SHARDS`] independent shards, each a `Mutex` over its own map and LRU list. A key goes
//! to the shard picked by [`hash64`] of its 16 identifying bytes, so contention scales down
//! with shard count while each shard stays a plain, obviously-correct data structure.
//! Every shard gets `capacity / SHARDS` bytes; a total capacity below [`SHARDS`] bytes
//! therefore gives every shard a capacity of zero and the cache stores nothing, which is a
//! legitimate way to ask for no caching at all.
//!
//! # Why an index-based list
//!
//! The classic LRU is a hash map beside an intrusive doubly-linked list, which in Rust means
//! either `unsafe` raw pointers or `Rc<RefCell<..>>` in every node. This uses neither: nodes
//! live in one `Vec<Slot>` and the links are `u32` indices into it, with [`NIL`] for "no
//! node". Freed slots are recycled through a free list threaded on the same `next` field, so
//! a steady-state cache never allocates. `CLAUDE.md` invariant 8 asks for safe code until a
//! profile says otherwise, and this costs a bounds check per link hop.
//!
//! Charge is supplied by the caller rather than read off the block, so a caller that knows
//! its per-entry overhead can account for it. The table reader passes the block's length and
//! inserts the *uncompressed* bytes: what the cache charges for has to be what it holds.
//!
//! [`hash64`]: esker_base::hash::hash64

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use esker_base::hash::hash64;

use crate::cache_api::{BlockCache, CacheKey};
use crate::options::defaults;

/// Number of independent shards, fixed by `docs/DESIGN.md` §14. Re-exported from
/// [`defaults::BLOCK_CACHE_SHARDS`] rather than restated, so the two cannot drift.
pub const SHARDS: usize = defaults::BLOCK_CACHE_SHARDS;

/// Mask that turns a hash into a shard index. Correct only while [`SHARDS`] is a power of
/// two, which [`shards_is_a_power_of_two`] asserts.
///
/// [`shards_is_a_power_of_two`]: tests::shards_is_a_power_of_two
const SHARD_MASK: u64 = 7;

/// The null link. A `u32` index can address every slot a shard will ever hold, because a
/// shard refuses to grow to `u32::MAX` slots so that this value never aliases a real one.
const NIL: u32 = u32::MAX;

/// A shard's aggregate counters, for `docs/DESIGN.md` §12 metrics and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups that found a block.
    pub hits: u64,
    /// Lookups that did not.
    pub misses: u64,
    /// Blocks dropped to stay under capacity.
    pub evictions: u64,
    /// Bytes currently charged, summed over shards.
    pub usage: usize,
    /// Blocks currently resident, summed over shards.
    pub entries: usize,
}

/// One cached block plus the accounting that belongs to it.
#[derive(Debug)]
struct Entry {
    /// The owning [`CacheKey`] flattened to its two words, so the shard map needs no trait
    /// bounds on a type another lane owns.
    key: (u64, u64),
    value: Arc<[u8]>,
    /// Bytes this entry contributes to `Shard::usage`, as the caller declared them.
    charge: usize,
}

/// A slot in the shard's arena: either a live entry linked into the LRU list, or a free slot
/// linked into the free list through `next`.
#[derive(Debug)]
struct Slot {
    entry: Option<Entry>,
    prev: u32,
    next: u32,
}

/// One shard: a map from key to slot index, and an LRU list from `head` (most recently used)
/// to `tail` (the next victim).
#[derive(Debug)]
struct Shard {
    slots: Vec<Slot>,
    index: HashMap<(u64, u64), u32>,
    head: u32,
    tail: u32,
    free: u32,
    usage: usize,
    capacity: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl Shard {
    fn new(capacity: usize) -> Self {
        Self {
            slots: Vec::new(),
            index: HashMap::new(),
            head: NIL,
            tail: NIL,
            free: NIL,
            usage: 0,
            capacity,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// Detaches `slot` from the LRU list, repairing whichever ends it occupied.
    ///
    /// Every index passed here comes from this shard's own map or list, so it is in range.
    fn unlink(&mut self, slot: u32) {
        let (prev, next) = {
            let s = &self.slots[slot as usize];
            (s.prev, s.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
    }

    /// Makes `slot` the most recently used entry. The slot must not currently be linked.
    fn push_front(&mut self, slot: u32) {
        let old_head = self.head;
        {
            let s = &mut self.slots[slot as usize];
            s.prev = NIL;
            s.next = old_head;
        }
        if old_head == NIL {
            self.tail = slot;
        } else {
            self.slots[old_head as usize].prev = slot;
        }
        self.head = slot;
    }

    /// Takes a slot off the free list, or grows the arena. `None` only when the arena has
    /// reached the one index that would alias [`NIL`], which needs 4 billion resident blocks.
    fn alloc(&mut self, entry: Entry) -> Option<u32> {
        if self.free != NIL {
            let slot = self.free;
            self.free = self.slots[slot as usize].next;
            let s = &mut self.slots[slot as usize];
            s.entry = Some(entry);
            s.prev = NIL;
            s.next = NIL;
            return Some(slot);
        }
        let slot = u32::try_from(self.slots.len()).ok()?;
        if slot == NIL {
            return None;
        }
        self.slots.push(Slot {
            entry: Some(entry),
            prev: NIL,
            next: NIL,
        });
        Some(slot)
    }

    /// Returns an unlinked, emptied slot to the free list.
    fn release(&mut self, slot: u32) {
        let free = self.free;
        let s = &mut self.slots[slot as usize];
        s.entry = None;
        s.prev = NIL;
        s.next = free;
        self.free = slot;
    }

    /// Drops least-recently-used entries until the shard is within capacity.
    fn evict_to_fit(&mut self) {
        while self.usage > self.capacity && self.tail != NIL {
            let victim = self.tail;
            self.unlink(victim);
            if let Some(entry) = self.slots[victim as usize].entry.take() {
                debug_assert!(
                    self.usage >= entry.charge,
                    "usage under-counts a live entry"
                );
                self.usage = self.usage.saturating_sub(entry.charge);
                self.index.remove(&entry.key);
            }
            self.release(victim);
            self.evictions += 1;
        }
    }

    fn get(&mut self, key: (u64, u64)) -> Option<Arc<[u8]>> {
        let Some(&slot) = self.index.get(&key) else {
            self.misses += 1;
            return None;
        };
        self.unlink(slot);
        self.push_front(slot);
        self.hits += 1;
        self.slots[slot as usize]
            .entry
            .as_ref()
            .map(|entry| Arc::clone(&entry.value))
    }

    fn insert(&mut self, key: (u64, u64), value: Arc<[u8]>, charge: usize) {
        if self.capacity == 0 {
            return;
        }

        if let Some(&slot) = self.index.get(&key) {
            if let Some(entry) = self.slots[slot as usize].entry.as_mut() {
                let old = entry.charge;
                entry.value = value;
                entry.charge = charge;
                debug_assert!(self.usage >= old, "usage under-counts a live entry");
                self.usage = self.usage.saturating_sub(old).saturating_add(charge);
            }
            self.unlink(slot);
            self.push_front(slot);
            self.evict_to_fit();
            return;
        }

        // A block larger than the whole shard would evict everything and then be evicted
        // itself. Refusing it keeps the rest of the shard warm.
        if charge > self.capacity {
            return;
        }
        let Some(slot) = self.alloc(Entry { key, value, charge }) else {
            return;
        };
        self.push_front(slot);
        self.index.insert(key, slot);
        self.usage = self.usage.saturating_add(charge);
        self.evict_to_fit();
    }
}

/// A fixed-shard-count LRU cache of uncompressed blocks.
///
/// Cloning the returned [`Arc`] is what keeps a block alive while a reader walks it, so
/// eviction never invalidates a block someone is holding.
#[derive(Debug)]
pub struct ShardedLruCache {
    shards: [Mutex<Shard>; SHARDS],
    capacity: usize,
}

impl ShardedLruCache {
    /// Builds a cache holding `capacity` bytes in total, split evenly across [`SHARDS`].
    ///
    /// Integer division means the effective capacity is `capacity - capacity % SHARDS`, and
    /// that any `capacity < SHARDS` caches nothing at all.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let per_shard = capacity / SHARDS;
        Self {
            shards: [(); SHARDS].map(|()| Mutex::new(Shard::new(per_shard))),
            capacity: per_shard * SHARDS,
        }
    }

    /// A cache of the `docs/DESIGN.md` §14 default capacity, 256 MiB.
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(defaults::BLOCK_CACHE_CAPACITY)
    }

    /// The capacity actually installed, after the rounding [`ShardedLruCache::new`] describes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes and hit counters summed over every shard. Each shard is sampled under its own
    /// lock, so the total is a smear rather than an instant, which is what a metric wants.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();
        for index in 0..SHARDS {
            let shard = self.shard(index);
            stats.hits += shard.hits;
            stats.misses += shard.misses;
            stats.evictions += shard.evictions;
            stats.usage += shard.usage;
            stats.entries += shard.index.len();
        }
        stats
    }

    /// Resident block count per shard, for asserting that hashing spreads keys out.
    #[must_use]
    pub fn entries_per_shard(&self) -> [usize; SHARDS] {
        let mut counts = [0usize; SHARDS];
        for (index, count) in counts.iter_mut().enumerate() {
            *count = self.shard(index).index.len();
        }
        counts
    }

    /// Locks one shard, treating a poisoned lock as a live one.
    ///
    /// Nothing inside a shard's methods can unwind: every index comes from that shard's own
    /// map or list, and Rust aborts rather than unwinds when an allocation fails. So a
    /// poisoned lock cannot expose a half-updated shard, while panicking here would let one
    /// unrelated thread's crash take out the cache for every other thread.
    fn shard(&self, index: usize) -> MutexGuard<'_, Shard> {
        self.shards[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Flattens a key to the two words the shard map uses, and picks its shard.
    fn locate(key: &CacheKey) -> (usize, (u64, u64)) {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&key.file_number.to_le_bytes());
        bytes[8..].copy_from_slice(&key.offset.to_le_bytes());
        // `hash64` finishes with SplitMix64, so masking the low bits is a fair split.
        let shard = usize::try_from(hash64(&bytes) & SHARD_MASK).unwrap_or(0);
        (shard, (key.file_number, key.offset))
    }
}

impl BlockCache for ShardedLruCache {
    fn lookup(&self, key: &CacheKey) -> Option<Arc<[u8]>> {
        let (shard, bits) = Self::locate(key);
        self.shard(shard).get(bits)
    }

    fn insert(&self, key: CacheKey, block: Arc<[u8]>, charge: usize) {
        let (shard, bits) = Self::locate(&key);
        self.shard(shard).insert(bits, block, charge);
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheStats, NIL, SHARD_MASK, SHARDS, Shard, ShardedLruCache};
    use crate::cache_api::{BlockCache, CacheKey};
    use std::sync::Arc;

    fn block(len: usize, fill: u8) -> Arc<[u8]> {
        Arc::from(vec![fill; len].into_boxed_slice())
    }

    fn key(file_number: u64, offset: u64) -> CacheKey {
        CacheKey {
            file_number,
            offset,
        }
    }

    /// `SHARD_MASK` is only a correct shard selector while the count is a power of two.
    #[test]
    fn shards_is_a_power_of_two() {
        assert!(SHARDS.is_power_of_two());
        assert_eq!(u64::try_from(SHARDS - 1).unwrap_or(0), SHARD_MASK);
        assert_eq!(NIL, u32::MAX);
    }

    /// The victim is the least recently *used*, not the least recently inserted: touching an
    /// entry has to save it.
    #[test]
    fn eviction_follows_use_order() {
        let mut shard = Shard::new(30);
        for i in 0..3u8 {
            shard.insert((0, u64::from(i)), block(10, i), 10);
        }
        assert_eq!(shard.usage, 30);

        // Touch the oldest, so the middle one becomes the victim.
        assert!(shard.get((0, 0)).is_some());
        shard.insert((0, 3), block(10, 3), 10);

        assert!(shard.get((0, 0)).is_some(), "touched entry was evicted");
        assert!(shard.get((0, 1)).is_none(), "LRU victim survived");
        assert!(shard.get((0, 2)).is_some());
        assert!(shard.get((0, 3)).is_some());
        assert_eq!(shard.evictions, 1);
        assert_eq!(shard.usage, 30);
    }

    /// Eviction is driven by bytes, not by entry count, and re-inserting a key re-charges it
    /// rather than double-counting.
    #[test]
    fn charge_is_accounted_in_bytes() {
        let mut shard = Shard::new(100);
        shard.insert((0, 0), block(40, 0), 40);
        shard.insert((0, 1), block(40, 1), 40);
        assert_eq!(shard.usage, 80);
        assert_eq!(shard.index.len(), 2);

        // Replacing a key adjusts usage by the difference only: 80 - 40 + 60 still fits.
        shard.insert((0, 0), block(60, 2), 60);
        assert_eq!(shard.usage, 100);
        assert_eq!(shard.index.len(), 2);

        // Now the same move overflows, and the victim is the entry the replace displaced.
        shard.insert((0, 1), block(70, 3), 70);
        assert_eq!(shard.usage, 70);
        assert_eq!(shard.index.len(), 1);
        assert!(shard.get((0, 0)).is_none(), "LRU victim survived a replace");
        assert!(shard.get((0, 1)).is_some());

        // A block that cannot fit is refused rather than flushing the shard.
        shard.insert((0, 9), block(101, 9), 101);
        assert!(shard.get((0, 9)).is_none());
        assert!(
            shard.get((0, 1)).is_some(),
            "an unfittable block flushed the shard"
        );
        assert_eq!(shard.usage, 70);
    }

    /// Freed slots come back: a shard churned far past its capacity keeps a bounded arena.
    #[test]
    fn slots_are_recycled() {
        let mut shard = Shard::new(100);
        for i in 0..1000u64 {
            shard.insert((7, i), block(10, 0), 10);
        }
        assert_eq!(shard.usage, 100);
        assert_eq!(shard.index.len(), 10);
        assert!(
            shard.slots.len() <= 11,
            "arena grew to {} slots for 10 resident blocks",
            shard.slots.len()
        );
        assert_eq!(shard.evictions, 990);
    }

    /// A zero capacity — and any capacity too small to give each shard a byte — stores
    /// nothing, and does so without panicking or dividing by zero.
    #[test]
    fn zero_capacity_caches_nothing() {
        for capacity in [0usize, 1, SHARDS - 1] {
            let cache = ShardedLruCache::new(capacity);
            assert_eq!(cache.capacity(), 0);
            cache.insert(key(1, 0), block(8, 1), 8);
            assert!(cache.lookup(&key(1, 0)).is_none());
            let stats = cache.stats();
            assert_eq!(stats.usage, 0);
            assert_eq!(stats.entries, 0);
            assert_eq!(stats.evictions, 0);
        }
    }

    /// An empty block is still a key: charge zero must not make the entry immortal or
    /// invisible.
    #[test]
    fn empty_block_round_trips() {
        let cache = ShardedLruCache::new(8 * 1024);
        cache.insert(key(3, 64), block(0, 0), 0);
        assert_eq!(cache.lookup(&key(3, 64)).map(|b| b.len()), Some(0));
    }

    /// Keys must not pile into one shard: `(file_number, offset)` pairs vary in the low bits
    /// and a weaker hash would send every 4 KiB-aligned offset to the same place.
    #[test]
    fn hashing_spreads_keys_across_shards() {
        let cache = ShardedLruCache::new(64 * 1024 * 1024);
        for file_number in 0..8u64 {
            for i in 0..256u64 {
                cache.insert(key(file_number, i * 4096), block(16, 0), 16);
            }
        }
        let counts = cache.entries_per_shard();
        let total: usize = counts.iter().sum();
        assert_eq!(total, 8 * 256);
        // A fair split gives 256 per shard; allow a wide band and still catch a hash that
        // ignores the low bits, which would leave shards empty.
        for (shard, &count) in counts.iter().enumerate() {
            assert!(
                (128..=384).contains(&count),
                "shard {shard} holds {count} of {total} keys: {counts:?}"
            );
        }
    }

    /// Eight threads hammering the same cache must leave it internally consistent: usage
    /// within capacity, and every resident key readable.
    #[test]
    fn concurrent_smoke() {
        let cache = Arc::new(ShardedLruCache::new(64 * 1024));
        std::thread::scope(|scope| {
            for thread in 0..8u8 {
                let cache = Arc::clone(&cache);
                scope.spawn(move || {
                    for round in 0..2000u64 {
                        let k = key(u64::from(thread), (round % 64) * 512);
                        if cache.lookup(&k).is_none() {
                            cache.insert(k, block(128, thread), 128);
                        }
                    }
                });
            }
        });

        let stats = cache.stats();
        assert!(
            stats.usage <= cache.capacity(),
            "usage {} exceeds capacity {}",
            stats.usage,
            cache.capacity()
        );
        assert!(stats.hits + stats.misses >= 8 * 2000);
        // Whatever survived must be readable and correctly sized.
        for thread in 0..8u8 {
            for round in 0..64u64 {
                if let Some(bytes) = cache.lookup(&key(u64::from(thread), round * 512)) {
                    assert_eq!(bytes.len(), 128);
                    assert!(bytes.iter().all(|&b| b == thread));
                }
            }
        }
    }

    /// Hits and misses are counted where callers expect them, so the metric can be trusted.
    #[test]
    fn stats_count_hits_and_misses() {
        let cache = ShardedLruCache::new(8 * 1024);
        assert_eq!(cache.stats(), CacheStats::default());
        cache.insert(key(1, 0), block(64, 1), 64);
        assert!(cache.lookup(&key(1, 0)).is_some());
        assert!(cache.lookup(&key(1, 1)).is_none());
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.entries), (1, 1, 1));
        assert_eq!(stats.usage, 64);
    }
}
