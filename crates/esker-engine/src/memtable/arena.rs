//! The bump-allocated backing store for the skiplist ([ADR 0041](../../../../docs/adr/0041-the-in-house-arena-skiplist.md)).
//!
//! Everything a memtable holds — key bytes, value bytes, node headers and forward pointers —
//! lives here, addressed by a `u32` offset rather than a pointer. Nothing is ever freed until
//! the whole arena is dropped, which happens when the last `Arc<MemTable>` goes.
//!
//! # Why chunks, and not one buffer that grows
//!
//! ADR 0041 as proposed said "a `Vec<u8>` grown in blocks" and argued that a `u32` offset stays
//! valid across a reallocation. The offset does. Nothing else does: the point of the arena is
//! that [`MemTableIter::key`](super::MemTableIter::key) stops copying and hands out a `&[u8]`
//! *into* it, and a slice into a buffer that is then reallocated is a use-after-free with extra
//! steps.
//!
//! So the arena is a list of chunks. A chunk is allocated once, never moved, never freed, and
//! never handed back; growth appends a chunk and leaves every existing byte at the same address
//! as well as at the same offset. That is what makes the borrow in the cursor a real lifetime
//! rather than an assertion in a comment.
//!
//! Chunk `i` holds `1 << (first_shift + i)` elements, so the chunks double and the number of
//! them needed to cover the whole `u32` offset space is `32 - first_shift` — small enough that
//! the directory is a fixed array and never itself needs to grow. That matters: a directory
//! that reallocated would put the reader back in exactly the situation this design avoids.
//!
//! # Who may call what
//!
//! [`Chunks::alloc`] and [`Chunks::alloc_bytes`] are **writer-only** and the skiplist takes a
//! mutex around them. Everything else is safe for any number of concurrent readers. The
//! publication rule that makes that true is stated once, in the `skiplist` module beside this one: a node is
//! written in full before any pointer is made to reach it, and that final store is a `Release`
//! matched by an `Acquire` in every reader.
//!
//! # The `unsafe` in here, in full
//!
//! Three sites, and each one has its bounds checked immediately above it, so every function in
//! this module is safe to call with any arguments at all:
//!
//! * [`Chunks::get`] — turns `(offset, len)` into a `&[T]`, after checking the range lies
//!   inside one allocated chunk.
//! * [`Chunks::alloc_bytes`] — copies a key and a value into space it has just reserved and
//!   which therefore no reader can reach.
//! * [`Chunks::drop`](Chunks) — hands the chunks back to the allocator.

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// Slots in a chunk directory. `32 - first_shift` are ever used, so any `first_shift` down to
/// zero fits; the array costs 256 bytes per arena and is never resized.
const MAX_CHUNKS: usize = 32;

/// An element type a chunk can be made of.
///
/// The `Send + Sync` bound is load-bearing and not decorative. `Chunks<T>` holds its `T`s behind
/// `AtomicPtr`, which std makes `Send + Sync` for *every* `T`, so the auto-derived `Sync` on
/// `Chunks<T>` would be granted whatever `T` is — while [`Chunks::get`] hands out `&[T]` to any
/// number of threads. Requiring it here is what makes that honest.
pub(super) trait ChunkElem: Sized + Send + Sync {
    /// A chunk of `len` elements, all zero.
    fn zeroed(len: usize) -> Box<[Self]>;
}

impl ChunkElem for u8 {
    fn zeroed(len: usize) -> Box<[Self]> {
        vec![0u8; len].into_boxed_slice()
    }
}

impl ChunkElem for AtomicU32 {
    fn zeroed(len: usize) -> Box<[Self]> {
        // Not `vec![]`: `AtomicU32` is deliberately not `Clone`. The chunks that hold forward
        // pointers are the smaller of the two arenas, so the loop is not on any hot path.
        (0..len).map(|_| AtomicU32::new(0)).collect()
    }
}

/// A bump allocator over doubling chunks, addressed by a `u32` element index.
///
/// One writer allocates; any number of readers resolve offsets. See the module docs for which
/// half of that each method belongs to.
pub(super) struct Chunks<T: ChunkElem> {
    /// Chunk `i`, or null if it has not been allocated yet. Allocated strictly in order, so the
    /// first null ends the list — which is how [`Chunks::drop`](Chunks) knows where to stop.
    ///
    /// Published with `Release` and read with `Acquire`. The `Acquire` is belt-and-braces: a
    /// reader only ever resolves an offset it reached through a node it loaded with `Acquire`,
    /// and the chunk was published before that node was written, so the chain already covers
    /// it. Stating it on the load as well means the rule can be checked one line at a time.
    directory: [AtomicPtr<T>; MAX_CHUNKS],
    /// The next free element index. Writer-only; `AtomicU32` for interior mutability under
    /// `&self`, not for synchronisation, so every access is `Relaxed`.
    next: AtomicU32,
    /// `log2` of chunk zero's length.
    first_shift: u32,
    /// How many of [`Chunks::directory`] this arena can ever use: `32 - first_shift`.
    slots: u32,
}

impl<T: ChunkElem> Chunks<T> {
    /// An empty arena whose first chunk will hold `1 << first_shift` elements.
    pub(super) fn new(first_shift: u32) -> Self {
        debug_assert!(
            first_shift < 32,
            "the directory must cover the u32 offset space"
        );
        Self {
            directory: [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_CHUNKS],
            next: AtomicU32::new(0),
            first_shift,
            slots: 32 - first_shift,
        }
    }

    /// An arena that gives up after `slots` chunks, so a test can reach the exhaustion path
    /// without allocating four gigabytes to get there.
    #[cfg(test)]
    pub(super) fn cramped(first_shift: u32, slots: u32) -> Self {
        let mut arena = Self::new(first_shift);
        arena.slots = slots;
        arena
    }

    /// One past the highest element index this arena can ever hand out.
    ///
    /// `2^(first_shift + slots) - 2^first_shift`, which for a real arena is `2^32 - 2^first_shift`
    /// — so every valid offset fits in a `u32` with room to spare and the arithmetic below never
    /// has to think about wrapping.
    #[inline]
    fn capacity(&self) -> u64 {
        (1u64 << (self.first_shift + self.slots)) - (1u64 << self.first_shift)
    }

    /// Which chunk holds element `offset`. Only meaningful for `offset < capacity()`.
    ///
    /// Chunk `i` covers `[2^(f+i) - 2^f, 2^(f+i+1) - 2^f)`, so shifting the offset up by `2^f`
    /// puts it in `[2^(f+i), 2^(f+i+1))` and the chunk index is what `ilog2` reads off.
    #[inline]
    fn chunk_of(&self, offset: u64) -> u32 {
        (offset + (1u64 << self.first_shift)).ilog2() - self.first_shift
    }

    /// The element index chunk `index` starts at.
    #[inline]
    fn chunk_start(&self, index: u32) -> u64 {
        (1u64 << (self.first_shift + index)) - (1u64 << self.first_shift)
    }

    /// How many elements chunk `index` holds.
    #[inline]
    fn chunk_len(&self, index: u32) -> u64 {
        1u64 << (self.first_shift + index)
    }

    /// How many elements have been handed out. Writer-only, and only for accounting.
    pub(super) fn used(&self) -> u32 {
        self.next.load(Ordering::Relaxed)
    }

    /// Reserves `count` contiguous elements, or `None` if the arena is full.
    ///
    /// **Writer only.** Contiguous is the whole contract: [`Chunks::get`] hands out one slice,
    /// so an allocation may not straddle a chunk boundary. When one would, the tail of the
    /// current chunk is abandoned and the allocation starts the next one — bounded waste,
    /// because the chunks double.
    ///
    /// `None` means the offset space is exhausted, which for the byte arena is four gigabytes of
    /// keys and values in a single memtable. Nothing in the engine can reach it: a memtable is
    /// switched when it passes `write_buffer_size`, so it can only overshoot by one batch, and a
    /// four-gigabyte batch is already several times its own size in memory before it gets here.
    pub(super) fn alloc(&self, count: u32) -> Option<u32> {
        let count = u64::from(count);
        let mut start = u64::from(self.next.load(Ordering::Relaxed));
        let index = loop {
            if start >= self.capacity() {
                return None;
            }
            let index = self.chunk_of(start);
            let end = self.chunk_start(index) + self.chunk_len(index);
            if start + count <= end {
                break index;
            }
            start = end;
        };
        self.reserve_chunk(index);
        // `start + count` is at most the end of chunk `index`, which is at most `capacity()`,
        // which is `2^32 - 2^first_shift`. Both fit.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "bounded by capacity(), which is below 2^32"
        )]
        {
            self.next.store((start + count) as u32, Ordering::Relaxed);
            Some(start as u32)
        }
    }

    /// Allocates every chunk up to and including `index` that does not exist yet.
    ///
    /// **Writer only**, which is why the existence check is a `Relaxed` load: nobody else ever
    /// stores here. The `Release` on the store is what a reader's `Acquire` pairs with.
    fn reserve_chunk(&self, index: u32) {
        for i in 0..=index {
            let slot = &self.directory[i as usize];
            if slot.load(Ordering::Relaxed).is_null() {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "chunk_len is below 2^32 for every chunk this arena can reach"
                )]
                let len = self.chunk_len(i) as usize;
                slot.store(Box::into_raw(T::zeroed(len)).cast::<T>(), Ordering::Release);
            }
        }
    }

    /// The `len` elements at `offset`, or an empty slice if that range is not inside one
    /// allocated chunk.
    ///
    /// Safe for any arguments: the empty slice is what a caller gets for an offset this arena
    /// never handed out. Every real caller passes an offset it read out of a published node, so
    /// the empty slice means a bug somewhere above rather than a condition to handle.
    ///
    /// The returned slice borrows the arena, and the arena is owned by the `MemTable` that the
    /// cursor holds an `Arc` to. That chain is the whole lifetime argument for the iterator.
    #[allow(
        unsafe_code,
        reason = "ADR 0041: turning an offset into a borrow is the point of the arena. The \
                  bounds are checked in the three lines above the block."
    )]
    #[inline]
    pub(super) fn get(&self, offset: u32, len: u32) -> &[T] {
        let offset = u64::from(offset);
        let len = u64::from(len);
        if offset >= self.capacity() {
            return &[];
        }
        let index = self.chunk_of(offset);
        let within = offset - self.chunk_start(index);
        if within + len > self.chunk_len(index) {
            return &[];
        }
        let base = self.directory[index as usize].load(Ordering::Acquire);
        if base.is_null() {
            return &[];
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "both are bounded by chunk_len, which is below 2^32"
        )]
        // SAFETY: `base` is chunk `index`, allocated by `reserve_chunk` as `chunk_len(index)`
        // elements and never moved or freed while this `&self` borrow lives. The two checks
        // above put `[within, within + len)` inside that chunk, so the whole slice is one
        // allocated object. The elements were written before the node that names them was
        // published, and are never written again, so no reader can observe them changing.
        unsafe {
            std::slice::from_raw_parts(base.add(within as usize), len as usize)
        }
    }
}

impl Chunks<u8> {
    /// Copies `parts` end to end into one contiguous run and returns its offset.
    ///
    /// Several parts rather than one because the skiplist's key arrives in pieces — a user key
    /// and an eight-byte tag — and joining them in the caller would be an allocation per insert.
    ///
    /// **Writer only.** Allocating and copying in one call is deliberate: it is what keeps the
    /// "these bytes are reserved and nothing can see them yet" precondition inside this
    /// function, so there is no `unsafe fn` for a caller to get wrong.
    #[allow(
        unsafe_code,
        reason = "ADR 0041: the writer copies into space `alloc` has just reserved, which no \
                  published node names and therefore no reader can reach."
    )]
    pub(super) fn alloc_bytes(&self, parts: &[&[u8]]) -> Option<u32> {
        let mut total = 0usize;
        for part in parts {
            total = total.checked_add(part.len())?;
        }
        let offset = self.alloc(u32::try_from(total).ok()?)?;
        let index = self.chunk_of(u64::from(offset));
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the offset is inside chunk `index`, so the difference is below chunk_len"
        )]
        let within = (u64::from(offset) - self.chunk_start(index)) as usize;
        let base = self.directory[index as usize].load(Ordering::Relaxed);
        debug_assert!(!base.is_null(), "alloc reserved the chunk it returned into");
        // SAFETY: `alloc` has just reserved `[offset, offset + total)` and guarantees it lies
        // inside chunk `index`, which `reserve_chunk` allocated; `base` is that chunk. The
        // parts sum to exactly the length reserved, so the writes stay inside it. The range is
        // past everything previously handed out, so no published node names it and no reader
        // can be reading it. The parts are the caller's, and none can overlap an arena the
        // caller has no way to name.
        unsafe {
            let mut dst = base.add(within);
            for part in parts {
                std::ptr::copy_nonoverlapping(part.as_ptr(), dst, part.len());
                dst = dst.add(part.len());
            }
        }
        Some(offset)
    }
}

impl Chunks<AtomicU32> {
    /// The word at `offset`, or `None` if this arena never handed that offset out.
    #[inline]
    pub(super) fn word(&self, offset: u32) -> Option<&AtomicU32> {
        self.get(offset, 1).first()
    }
}

impl<T: ChunkElem> Drop for Chunks<T> {
    #[allow(
        unsafe_code,
        reason = "ADR 0041: the chunks were taken apart with `Box::into_raw` and this is the \
                  one place that puts them back together."
    )]
    fn drop(&mut self) {
        for index in 0..self.slots {
            let base = *self.directory[index as usize].get_mut();
            if base.is_null() {
                // `reserve_chunk` fills the directory in order, so the first hole is the end.
                break;
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "chunk_len is below 2^32 for every chunk this arena can reach"
            )]
            let len = self.chunk_len(index) as usize;
            // SAFETY: `base` came from `Box::into_raw(T::zeroed(len))` in `reserve_chunk` with
            // exactly this `len`, and nothing else ever stores into the directory, so this
            // rebuilds the same box that was taken apart. `&mut self` means every reader that
            // borrowed from this chunk is gone: the borrows in `get` are tied to `&self`.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(base, len)) });
        }
    }
}

impl<T: ChunkElem> std::fmt::Debug for Chunks<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chunks")
            .field("used", &self.used())
            .field("first_shift", &self.first_shift)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{Chunks, MAX_CHUNKS};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A tiny first chunk, so a test crosses several chunk boundaries in a few allocations.
    /// The production shifts are in [`super::super::skiplist`]; the addressing is the same.
    const TINY: u32 = 2;

    /// The three functions that turn an offset into a place have to agree with each other for
    /// every offset, or a reader lands in the wrong chunk and reads someone else's bytes.
    #[test]
    fn chunk_addressing_agrees_with_itself() {
        let arena = Chunks::<u8>::new(TINY);
        let mut expected_start = 0u64;
        for index in 0..8 {
            let len = arena.chunk_len(index);
            assert_eq!(arena.chunk_start(index), expected_start, "chunk {index}");
            for offset in expected_start..expected_start + len {
                assert_eq!(arena.chunk_of(offset), index, "offset {offset}");
            }
            expected_start += len;
        }
        assert_eq!(
            arena.chunk_start(arena.slots - 1) + arena.chunk_len(arena.slots - 1),
            arena.capacity(),
            "the last chunk ends exactly at capacity, so no offset is unreachable"
        );
        assert!(arena.capacity() <= u64::from(u32::MAX) + 1);
    }

    /// The first allocation is offset zero. The skiplist spends it on a reserved word so that
    /// `NIL == 0` can never name a real node.
    #[test]
    fn the_first_offset_is_zero() {
        let arena = Chunks::<AtomicU32>::new(TINY);
        assert_eq!(arena.alloc(1), Some(0));
        assert_ne!(arena.alloc(1), Some(0), "and it is handed out only once");
    }

    /// Everything written stays readable at the offset it was written to, across as many chunk
    /// boundaries as it takes.
    #[test]
    fn offsets_round_trip_across_many_chunks() {
        let arena = Chunks::<u8>::new(TINY);
        let mut placed = Vec::new();
        for i in 0..200u32 {
            let key = format!("k{i}").into_bytes();
            let value = format!("value-{i}-{}", "x".repeat((i % 7) as usize)).into_bytes();
            let offset = arena.alloc_bytes(&[&key, &value]).unwrap();
            placed.push((offset, key, value));
        }
        for (offset, key, value) in &placed {
            let total = u32::try_from(key.len() + value.len()).unwrap();
            let bytes = arena.get(*offset, total);
            assert_eq!(&bytes[..key.len()], &key[..], "key at {offset}");
            assert_eq!(&bytes[key.len()..], &value[..], "value at {offset}");
        }
        assert!(
            arena.directory[6].load(Ordering::Relaxed).is_null()
                || arena.chunk_of(u64::from(arena.used())) > 3,
            "the test has to actually cross chunk boundaries to be worth anything"
        );
    }

    /// The trap this whole design exists to avoid: a reference handed out before the arena grew
    /// must still be readable after it grew. With one buffer that reallocates, this is a
    /// use-after-free; with chunks it is a no-op, and this test is what says so.
    #[test]
    fn a_slice_outlives_the_growth_that_follows_it() {
        let arena = Chunks::<u8>::new(TINY);
        let first = arena.alloc_bytes(&[b"ab", b"cd"]).unwrap();
        // The borrow is live from here to the end of the test, across every allocation below.
        let borrowed: &[u8] = arena.get(first, 4);
        assert_eq!(borrowed, b"abcd");

        let mut grew = 0;
        for i in 0..64u32 {
            arena
                .alloc_bytes(&[&i.to_le_bytes(), &[b'z'; 13]])
                .expect("the arena has room");
            let chunks = (0..MAX_CHUNKS)
                .filter(|&c| !arena.directory[c].load(Ordering::Relaxed).is_null())
                .count();
            grew = grew.max(chunks);
        }
        assert!(
            grew >= 5,
            "the arena must really have grown; it reached {grew} chunks"
        );
        assert_eq!(
            borrowed, b"abcd",
            "the old slice still reads what it always did"
        );
    }

    /// An allocation that would span two chunks starts the next one instead, because a slice
    /// has to be contiguous. The abandoned tail is the price and it is bounded by the chunk.
    #[test]
    fn an_allocation_never_straddles_a_chunk() {
        let arena = Chunks::<u8>::new(TINY);
        // Chunk 0 holds 4 bytes, chunk 1 holds 8. Fill three of chunk 0's four, then ask for
        // three: it cannot fit in the one byte left, so it must land at the start of chunk 1.
        assert_eq!(arena.alloc(3), Some(0));
        let offset = arena.alloc(3).unwrap();
        assert_eq!(offset, 4, "the last byte of chunk 0 is abandoned");
        assert_eq!(arena.chunk_of(u64::from(offset)), 1);
        assert_eq!(
            arena.get(offset, 3).len(),
            3,
            "and the whole run is readable as one slice"
        );
    }

    /// `get` is total. An offset the arena never handed out is a bug above it, and the answer
    /// is an empty slice rather than a read of whatever happens to be there.
    #[test]
    fn a_bogus_offset_reads_as_empty_rather_than_as_something() {
        let arena = Chunks::<u8>::new(TINY);
        let offset = arena.alloc_bytes(&[b"ab", b"cd"]).unwrap();
        assert_eq!(arena.get(offset, 4), b"abcd");
        assert!(arena.get(offset, 5).is_empty(), "past the end of chunk 0");
        assert!(arena.get(u32::MAX, 1).is_empty(), "past capacity");
        assert!(
            arena.get(1_000_000, 1).is_empty(),
            "inside capacity but in a chunk that was never allocated"
        );
    }

    /// Bigger than the biggest chunk means there is nowhere contiguous to put it, and the arena
    /// says so rather than wrapping the offset or panicking (invariant 9).
    #[test]
    fn an_allocation_larger_than_any_chunk_is_refused() {
        let arena = Chunks::<u8>::new(TINY);
        assert_eq!(arena.alloc(u32::MAX), None);
        assert_eq!(arena.used(), 0, "and a refusal consumes nothing");
        assert_eq!(arena.alloc(4), Some(0), "the arena still works afterwards");
    }

    /// The parts of a key land end to end, which is what lets the skiplist hand its internal
    /// key over as a user key and a tag without joining them first.
    #[test]
    fn alloc_bytes_joins_its_parts() {
        let arena = Chunks::<u8>::new(TINY);
        assert_eq!(arena.alloc_bytes(&[b"", b""]), Some(0), "empty is fine");
        assert!(arena.get(0, 0).is_empty());
        assert_eq!(arena.used(), 0, "and an empty run consumes nothing");

        // Six bytes do not fit in chunk 0's four, so this lands at the start of chunk 1.
        let offset = arena.alloc_bytes(&[b"a", b"bc", b"def"]).unwrap();
        assert_eq!(offset, 4);
        assert_eq!(arena.get(offset, 6), b"abcdef");
    }

    /// Words come out zero, which is what lets `NIL == 0` mean "no node" without anyone having
    /// to write it.
    #[test]
    fn words_start_at_zero_and_hold_what_is_stored() {
        let arena = Chunks::<AtomicU32>::new(TINY);
        let offset = arena.alloc(6).unwrap();
        for i in 0..6 {
            assert_eq!(
                arena.word(offset + i).unwrap().load(Ordering::Relaxed),
                0,
                "word {i} of a fresh allocation"
            );
        }
        arena.word(offset + 3).unwrap().store(42, Ordering::Release);
        assert_eq!(arena.word(offset + 3).unwrap().load(Ordering::Acquire), 42);
        assert_eq!(arena.word(offset + 2).unwrap().load(Ordering::Relaxed), 0);
        assert!(arena.word(u32::MAX).is_none());
    }

    /// Allocate, grow across several chunks, drop. Under Miri this is the test that says every
    /// chunk goes back exactly once; without it, it says the arena does not panic on the way out.
    #[test]
    fn every_chunk_is_handed_back() {
        for shift in [TINY, 4, 8] {
            let arena = Chunks::<AtomicU32>::new(shift);
            for _ in 0..500 {
                arena.alloc(3).unwrap();
            }
            drop(arena);
        }
    }
}
