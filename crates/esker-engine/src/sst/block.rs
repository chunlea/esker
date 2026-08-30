//! The block: the unit every SST is built from, and the unit the cache stores.
//!
//! Data, index and properties blocks all use this one layout. Only the meaning of the values
//! differs, which is why the reader below knows nothing about what it is iterating.
//!
//! # Layout (format v1)
//!
//! ```text
//! +-----------------------------------------------------------+
//! | entry 0                                                    |
//! | entry 1                                                    |
//! | ...                                                        |
//! +-----------------------------------------------------------+
//! | restart[0]: u32 LE   offset of the 1st entry of group 0    |
//! | restart[1]: u32 LE                                         |
//! | ...                                                        |
//! +-----------------------------------------------------------+
//! | num_restarts: u32 LE                                       |
//! +-----------------------------------------------------------+
//! ```
//!
//! One entry is
//! `shared:varint ++ non_shared:varint ++ value_len:varint ++ key_delta ++ value`, where
//! `shared` is the number of leading bytes this key has in common with the previous one.
//! Every `restart_interval`-th entry stores `shared = 0`, so its key stands alone: those are
//! the restart points, and they are what makes a block searchable rather than only readable
//! front to back.
//!
//! The prefix compression is purely byte-level. It never looks at what a key *means*
//! (`CLAUDE.md` invariant 7), and it is independent of the comparator — order comes from the
//! comparator, sharing comes from `memcmp`. A block whose keys are not in comparator order is
//! still decodable; it is only [`BlockIter::seek`] that would return nonsense, which is why
//! the table builder rejects out-of-order keys rather than leaving it to be discovered here.
//!
//! # Cost of `prev`
//!
//! Entries are variable-length and prefix-compressed, so a block cannot be walked backwards.
//! [`BlockIter::prev`] therefore re-seeks to the restart point before the current entry and
//! scans forward to the one preceding it: O(`restart_interval`) decodes per step, 16 by
//! default. Reverse iteration over a whole block is still linear overall, because each restart
//! group is re-scanned once per group rather than once per entry.

use std::ops::Range;
use std::sync::Arc;

use esker_base::varint;

use crate::dbformat::Comparator;
use crate::error::{Error, Result};

/// Bytes of restart array overhead per restart point, plus the trailing count.
const RESTART_ENTRY_SIZE: usize = 4;

/// Builds one block, prefix-compressing each key against the one before it.
///
/// The builder is reusable: [`BlockBuilder::reset`] clears it without freeing its buffers, so
/// a table writer cutting thousands of blocks allocates once.
#[derive(Debug)]
pub struct BlockBuilder {
    restart_interval: usize,
    buffer: Vec<u8>,
    restarts: Vec<u32>,
    /// Entries since the last restart point.
    counter: usize,
    last_key: Vec<u8>,
    entries: usize,
    finished: bool,
}

impl BlockBuilder {
    /// A builder that starts a new restart group every `restart_interval` entries.
    ///
    /// An interval of 0 is read as 1: every entry becomes a restart point, which costs 4 bytes
    /// and the shared prefix per entry but makes every entry binary-searchable.
    #[must_use]
    pub fn new(restart_interval: usize) -> Self {
        Self {
            restart_interval: restart_interval.max(1),
            buffer: Vec::new(),
            restarts: vec![0],
            counter: 0,
            last_key: Vec::new(),
            entries: 0,
            finished: false,
        }
    }

    /// Appends one entry. Keys must arrive in the caller's sort order; nothing here checks it,
    /// because only the caller has the comparator that defines it.
    ///
    /// Fails when a key or value does not fit in a `u32`, which the entry header encodes.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        debug_assert!(!self.finished, "add after finish");
        let non_shared_len = u32::try_from(key.len()).map_err(|_| {
            Error::InvalidArgument(format!("key of {} bytes is too large", key.len()))
        })?;
        let value_len = u32::try_from(value.len()).map_err(|_| {
            Error::InvalidArgument(format!("value of {} bytes is too large", value.len()))
        })?;

        let shared = if self.counter < self.restart_interval {
            shared_prefix_len(&self.last_key, key)
        } else {
            // Start a new restart group: this key is stored whole.
            let offset = u32::try_from(self.buffer.len()).map_err(|_| {
                Error::InvalidArgument(format!(
                    "block grew past 4 GiB at {} bytes",
                    self.buffer.len()
                ))
            })?;
            self.restarts.push(offset);
            self.counter = 0;
            0
        };

        // `shared <= key.len()`, so both casts below are exact.
        varint::put_u32(shared, &mut self.buffer);
        varint::put_u32(non_shared_len - shared, &mut self.buffer);
        varint::put_u32(value_len, &mut self.buffer);
        self.buffer.extend_from_slice(&key[shared as usize..]);
        self.buffer.extend_from_slice(value);

        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
        self.entries += 1;
        Ok(())
    }

    /// Bytes the block would occupy if finished now, restart array included. The table builder
    /// uses this to decide when to cut a block.
    #[must_use]
    pub fn size_estimate(&self) -> usize {
        self.buffer.len() + (self.restarts.len() + 1) * RESTART_ENTRY_SIZE
    }

    /// Entries added since the last reset.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// True when no entry has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// The last key added, which a table builder needs as a block's separator key.
    #[must_use]
    pub fn last_key(&self) -> &[u8] {
        &self.last_key
    }

    /// Appends the restart array and returns the finished block. Call [`BlockBuilder::reset`]
    /// before adding to it again.
    pub fn finish(&mut self) -> &[u8] {
        for &restart in &self.restarts {
            self.buffer.extend_from_slice(&restart.to_le_bytes());
        }
        // A `u32` count is enough: the offsets it indexes are themselves `u32`.
        let count = u32::try_from(self.restarts.len()).unwrap_or(u32::MAX);
        self.buffer.extend_from_slice(&count.to_le_bytes());
        self.finished = true;
        &self.buffer
    }

    /// Empties the builder, keeping its allocations.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.restarts.clear();
        self.restarts.push(0);
        self.counter = 0;
        self.entries = 0;
        self.last_key.clear();
        self.finished = false;
    }
}

/// Length of the longest common prefix of `a` and `b`, in bytes.
fn shared_prefix_len(a: &[u8], b: &[u8]) -> u32 {
    let limit = a.len().min(b.len());
    let mut i = 0;
    while i < limit && a[i] == b[i] {
        i += 1;
    }
    // Bounded by `b.len()`, which the caller has already proved fits in a `u32`.
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// A parsed, immutable block, ready to be iterated.
///
/// Holds its bytes as an [`Arc`] so the block cache and every iterator over it share one copy.
#[derive(Debug, Clone)]
pub struct Block {
    data: Arc<[u8]>,
    /// Offset of the restart array: also the end of the entry region.
    restart_offset: usize,
    num_restarts: usize,
}

impl Block {
    /// Validates the trailer and restart array of `data`.
    ///
    /// The block's checksum has already been verified by the reader. This rejects the shapes
    /// a correct checksum cannot rule out — a truncated block, a restart count that does not
    /// fit in the bytes present — as corruption rather than as a panic (invariant 2).
    pub fn new(data: Arc<[u8]>) -> Result<Self> {
        let len = data.len();
        if len < RESTART_ENTRY_SIZE {
            return Err(Error::corruption(
                "sst block",
                format!("block of {len} bytes is shorter than its restart count"),
            ));
        }
        let num_restarts = read_u32(&data, len - RESTART_ENTRY_SIZE) as usize;
        if num_restarts == 0 {
            return Err(Error::corruption(
                "sst block",
                "block declares zero restart points; every block has at least one",
            ));
        }
        let array_bytes = num_restarts
            .checked_mul(RESTART_ENTRY_SIZE)
            .and_then(|bytes| bytes.checked_add(RESTART_ENTRY_SIZE))
            .ok_or_else(|| {
                Error::corruption(
                    "sst block",
                    format!("restart count {num_restarts} overflows"),
                )
            })?;
        let restart_offset = len.checked_sub(array_bytes).ok_or_else(|| {
            Error::corruption(
                "sst block",
                format!("{num_restarts} restart points do not fit in {len} bytes"),
            )
        })?;
        Ok(Self {
            data,
            restart_offset,
            num_restarts,
        })
    }

    /// Restart points in this block.
    #[must_use]
    pub fn num_restarts(&self) -> usize {
        self.num_restarts
    }

    /// Total size of the block in bytes, restart array included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True when the block holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.restart_offset == 0
    }

    /// An iterator over the block, ordered by `comparator`.
    ///
    /// Not a `std::iter::Iterator`: the engine's cursor is the `seek / seek_for_prev / next /
    /// prev / key / value / valid` shape of `docs/DESIGN.md` §4.1, which a forward-only
    /// `Iterator` cannot express, and every layer above expects that shape.
    #[allow(clippy::iter_not_returning_iterator)]
    #[must_use]
    pub fn iter(&self, comparator: Arc<dyn Comparator>) -> BlockIter {
        BlockIter {
            data: Arc::clone(&self.data),
            comparator,
            restart_offset: self.restart_offset,
            num_restarts: self.num_restarts,
            current: self.restart_offset,
            restart_index: self.num_restarts,
            key: Vec::new(),
            value: 0..0,
            status: None,
        }
    }
}

/// Reads a little-endian `u32`. The caller has already bounds-checked `offset`.
fn read_u32(data: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&data[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}

/// One decoded entry header: where its key delta and value live, and where the next entry
/// starts.
struct EntryHeader {
    shared: usize,
    key_delta: Range<usize>,
    value: Range<usize>,
    next: usize,
}

/// A cursor over one block's entries.
///
/// Invalid until positioned. Every method that can meet corrupt bytes records the error in
/// [`BlockIter::status`] and becomes invalid rather than panicking, so a caller that ignores
/// the status sees an empty range rather than wrong data — but the table reader checks it.
#[derive(Debug)]
pub struct BlockIter {
    data: Arc<[u8]>,
    comparator: Arc<dyn Comparator>,
    restart_offset: usize,
    num_restarts: usize,
    /// Offset of the current entry, or `restart_offset` when invalid.
    current: usize,
    /// Index of the restart group holding `current`, or `num_restarts` when invalid.
    restart_index: usize,
    key: Vec<u8>,
    value: Range<usize>,
    status: Option<Error>,
}

impl BlockIter {
    /// Whether the cursor is on an entry.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.current < self.restart_offset
    }

    /// The current key. Empty when invalid.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The current value. Empty when invalid.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.data[self.value.clone()]
    }

    /// The first corruption this cursor met, if any. A caller that has finished iterating must
    /// check this before trusting that it saw every entry.
    pub fn status(&self) -> Result<()> {
        match &self.status {
            None => Ok(()),
            Some(Error::Corruption { context, detail }) => {
                Err(Error::corruption(context.clone(), detail.clone()))
            }
            Some(other) => Err(Error::InvalidArgument(other.to_string())),
        }
    }

    /// Offset of restart point `index`.
    fn restart_point(&self, index: usize) -> usize {
        read_u32(&self.data, self.restart_offset + index * RESTART_ENTRY_SIZE) as usize
    }

    fn invalidate(&mut self) {
        self.current = self.restart_offset;
        self.restart_index = self.num_restarts;
        self.key.clear();
        self.value = 0..0;
    }

    fn fail(&mut self, detail: impl Into<String>) {
        if self.status.is_none() {
            self.status = Some(Error::corruption("sst block", detail));
        }
        self.invalidate();
    }

    /// Decodes the entry header at `offset`, bounded by the restart array.
    fn decode_at(&self, offset: usize) -> Result<EntryHeader> {
        let limit = self.restart_offset;
        if offset > limit {
            return Err(Error::corruption(
                "sst block",
                format!("entry offset {offset} is past the {limit}-byte entry region"),
            ));
        }
        let bytes = &self.data[offset..limit];
        let (shared, n1) = varint::get_u32(bytes)
            .map_err(|e| Error::corruption("sst block", format!("shared prefix: {e}")))?;
        let (non_shared, n2) = varint::get_u32(&bytes[n1..])
            .map_err(|e| Error::corruption("sst block", format!("key length: {e}")))?;
        let (value_len, n3) = varint::get_u32(&bytes[n1 + n2..])
            .map_err(|e| Error::corruption("sst block", format!("value length: {e}")))?;

        let header_len = n1 + n2 + n3;
        let key_start = offset + header_len;
        let value_start = key_start + non_shared as usize;
        let next = value_start + value_len as usize;
        if next > limit {
            return Err(Error::corruption(
                "sst block",
                format!(
                    "entry at {offset} runs {} bytes past the entry region",
                    next - limit
                ),
            ));
        }
        Ok(EntryHeader {
            shared: shared as usize,
            key_delta: key_start..value_start,
            value: value_start..next,
            next,
        })
    }

    /// Reads the entry at `self.current`, rebuilding the key from `shared` bytes of the
    /// previous one. The caller guarantees `self.key` already holds that previous key.
    fn parse_current(&mut self) -> bool {
        if self.current >= self.restart_offset {
            self.invalidate();
            return false;
        }
        let header = match self.decode_at(self.current) {
            Ok(header) => header,
            Err(error) => {
                self.status.get_or_insert(error);
                self.invalidate();
                return false;
            }
        };
        if header.shared > self.key.len() {
            self.fail(format!(
                "entry at {} shares {} bytes with a {}-byte key",
                self.current,
                header.shared,
                self.key.len()
            ));
            return false;
        }
        self.key.truncate(header.shared);
        self.key
            .extend_from_slice(&self.data[header.key_delta.clone()]);
        self.value = header.value;
        true
    }

    /// Positions on the first entry of restart group `index`.
    fn seek_to_restart(&mut self, index: usize) -> bool {
        if index >= self.num_restarts {
            self.invalidate();
            return false;
        }
        let offset = self.restart_point(index);
        if offset > self.restart_offset {
            self.fail(format!(
                "restart point {index} points past the entry region"
            ));
            return false;
        }
        self.restart_index = index;
        self.current = offset;
        // A restart entry shares nothing, so no previous key is needed.
        self.key.clear();
        self.parse_current()
    }

    /// Positions on the first entry of the block.
    pub fn seek_to_first(&mut self) {
        self.seek_to_restart(0);
    }

    /// Positions on the last entry of the block.
    pub fn seek_to_last(&mut self) {
        if !self.seek_to_restart(self.num_restarts - 1) {
            return;
        }
        // Walk to the end of the final restart group.
        loop {
            let next = match self.decode_at(self.current) {
                Ok(header) => header.next,
                Err(error) => {
                    self.status.get_or_insert(error);
                    self.invalidate();
                    return;
                }
            };
            if next >= self.restart_offset {
                return;
            }
            self.current = next;
            if !self.parse_current() {
                return;
            }
        }
    }

    /// Advances to the next entry.
    pub fn next(&mut self) {
        if !self.valid() {
            return;
        }
        let next = match self.decode_at(self.current) {
            Ok(header) => header.next,
            Err(error) => {
                self.status.get_or_insert(error);
                self.invalidate();
                return;
            }
        };
        self.current = next;
        // Keep `restart_index` in step so `prev` knows which group to rewind to.
        while self.restart_index + 1 < self.num_restarts
            && self.restart_point(self.restart_index + 1) <= self.current
        {
            self.restart_index += 1;
        }
        self.parse_current();
    }

    /// Steps back one entry, by re-scanning from the restart point before it. See the module
    /// docs for why this is not a constant-time operation.
    pub fn prev(&mut self) {
        if !self.valid() {
            return;
        }
        let target = self.current;

        // Find the restart group that starts strictly before the current entry.
        let mut index = self.restart_index;
        while index > 0 && self.restart_point(index) >= target {
            index -= 1;
        }
        if self.restart_point(index) >= target {
            // The current entry is the first of the block; there is nothing before it.
            self.invalidate();
            return;
        }
        if !self.seek_to_restart(index) {
            return;
        }
        // Scan forward, stopping on the entry whose successor is the one we started on.
        loop {
            let next = match self.decode_at(self.current) {
                Ok(header) => header.next,
                Err(error) => {
                    self.status.get_or_insert(error);
                    self.invalidate();
                    return;
                }
            };
            if next >= target {
                return;
            }
            self.current = next;
            if !self.parse_current() {
                return;
            }
        }
    }

    /// Positions on the first entry whose key is `>= target`, or invalidates if there is none.
    ///
    /// Binary search over the restart points, then a linear scan of at most one restart group.
    pub fn seek(&mut self, target: &[u8]) {
        // Invariant: restart `low` has a first key <= target (or low == 0 and none does),
        // and restart `high` has a first key > target.
        let mut low = 0usize;
        let mut high = self.num_restarts;
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            let offset = self.restart_point(mid);
            if offset > self.restart_offset {
                self.fail(format!("restart point {mid} points past the entry region"));
                return;
            }
            let first_key = match self.decode_at(offset) {
                Ok(header) => &self.data[header.key_delta],
                Err(error) => {
                    self.status.get_or_insert(error);
                    self.invalidate();
                    return;
                }
            };
            if self.comparator.cmp(first_key, target).is_lt() {
                low = mid;
            } else {
                high = mid;
            }
        }

        if !self.seek_to_restart(low) {
            return;
        }
        while self.valid() && self.comparator.cmp(&self.key, target).is_lt() {
            self.next();
        }
    }

    /// Positions on the last entry whose key is `<= target`, or invalidates if there is none.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.seek(target);
        if self.status.is_some() {
            return;
        }
        if !self.valid() {
            self.seek_to_last();
            // The block's last key may still be greater than the target only if the block is
            // empty, in which case `seek_to_last` already invalidated.
            if self.valid() && self.comparator.cmp(&self.key, target).is_gt() {
                self.invalidate();
            }
            return;
        }
        if self.comparator.cmp(&self.key, target).is_gt() {
            self.prev();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, BlockBuilder, BlockIter, shared_prefix_len};
    use crate::dbformat::{BytewiseComparator, Comparator};
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn comparator() -> Arc<dyn Comparator> {
        Arc::new(BytewiseComparator)
    }

    fn build(restart_interval: usize, entries: &[(Vec<u8>, Vec<u8>)]) -> Block {
        let mut builder = BlockBuilder::new(restart_interval);
        for (key, value) in entries {
            builder.add(key, value).expect("entries fit in a block");
        }
        let bytes = builder.finish().to_vec();
        Block::new(Arc::from(bytes.into_boxed_slice())).expect("a freshly built block parses")
    }

    fn collect(iter: &mut BlockIter) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        iter.seek_to_first();
        while iter.valid() {
            out.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.next();
        }
        out
    }

    fn collect_reverse(iter: &mut BlockIter) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        iter.seek_to_last();
        while iter.valid() {
            out.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.prev();
        }
        out
    }

    fn pairs(keys: &[&[u8]]) -> Vec<(Vec<u8>, Vec<u8>)> {
        keys.iter()
            .enumerate()
            .map(|(i, k)| (k.to_vec(), format!("v{i}").into_bytes()))
            .collect()
    }

    #[test]
    fn shared_prefix_is_byte_level() {
        assert_eq!(shared_prefix_len(b"", b"abc"), 0);
        assert_eq!(shared_prefix_len(b"abc", b"abd"), 2);
        assert_eq!(shared_prefix_len(b"abc", b"abc"), 3);
        assert_eq!(shared_prefix_len(b"abcdef", b"abc"), 3);
        assert_eq!(shared_prefix_len(b"\xff\xff", b"\xff\xfe"), 1);
    }

    /// The block bytes are on disk, so this layout is frozen. Decoded by hand here so a
    /// change to the entry encoding fails with a readable diff.
    #[test]
    fn golden_block_bytes() {
        let entries = pairs(&[b"aaaa", b"aaab", b"bbbb"]);
        let mut builder = BlockBuilder::new(2);
        for (key, value) in &entries {
            builder.add(key, value).unwrap();
        }
        let bytes = builder.finish().to_vec();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // entry 0: restart, shared=0 non_shared=4 value_len=2 "aaaa" "v0"
            0x00, 0x04, 0x02, b'a', b'a', b'a', b'a', b'v', b'0',
            // entry 1: shared=3 non_shared=1 value_len=2 "b" "v1"
            0x03, 0x01, 0x02, b'b', b'v', b'1',
            // entry 2: restart (counter hit the interval), shared=0 non_shared=4 "bbbb" "v2"
            0x00, 0x04, 0x02, b'b', b'b', b'b', b'b', b'v', b'2',
            // restart array: offsets 0 and 15, then the count
            0x00, 0x00, 0x00, 0x00,
            0x0f, 0x00, 0x00, 0x00,
            0x02, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            bytes, expected,
            "the block layout changed; that is a format change (ADR + version bump)"
        );

        let block = Block::new(Arc::from(bytes.into_boxed_slice())).unwrap();
        assert_eq!(block.num_restarts(), 2);
        assert_eq!(collect(&mut block.iter(comparator())), entries);
    }

    /// An interval of 1 makes every entry a restart point; an interval larger than the block
    /// makes exactly one. Both are boundary cases the table builder can produce.
    #[test]
    fn restart_intervals_at_both_extremes() {
        let entries = pairs(&[b"a", b"b", b"c", b"d"]);
        for (interval, expected_restarts) in [(0usize, 4usize), (1, 4), (3, 2), (99, 1)] {
            let block = build(interval, &entries);
            assert_eq!(
                block.num_restarts(),
                expected_restarts,
                "interval {interval}"
            );
            let mut iter = block.iter(comparator());
            assert_eq!(collect(&mut iter), entries, "interval {interval}");
            let mut reversed = collect_reverse(&mut iter);
            reversed.reverse();
            assert_eq!(reversed, entries, "interval {interval} reversed");
            for (key, value) in &entries {
                iter.seek(key);
                assert!(iter.valid());
                assert_eq!(iter.key(), &key[..]);
                assert_eq!(iter.value(), &value[..]);
            }
        }
    }

    /// An empty block is well-defined: it parses, and every cursor operation on it is invalid
    /// rather than a panic.
    #[test]
    fn empty_block_is_iterable_and_empty() {
        let block = build(16, &[]);
        assert!(block.is_empty());
        assert_eq!(block.num_restarts(), 1);
        let mut iter = block.iter(comparator());
        assert_eq!(collect(&mut iter), vec![]);
        assert_eq!(collect_reverse(&mut iter), vec![]);
        iter.seek(b"anything");
        assert!(!iter.valid());
        iter.seek_for_prev(b"anything");
        assert!(!iter.valid());
        iter.next();
        iter.prev();
        assert!(!iter.valid());
        iter.status().expect("an empty block is not corrupt");
    }

    /// A single entry exercises every boundary at once: it is the first, the last, and the
    /// only restart point.
    #[test]
    fn single_entry_block() {
        let entries = pairs(&[b"only"]);
        let block = build(16, &entries);
        let mut iter = block.iter(comparator());
        assert_eq!(collect(&mut iter), entries);
        assert_eq!(collect_reverse(&mut iter), entries);

        iter.seek(b"aaa");
        assert_eq!(iter.key(), b"only");
        iter.seek(b"zzz");
        assert!(!iter.valid(), "seek past the last key must invalidate");
        iter.seek_for_prev(b"zzz");
        assert_eq!(iter.key(), b"only");
        iter.seek_for_prev(b"aaa");
        assert!(!iter.valid(), "no key at or before the first");
    }

    /// Seek lands on the first key >= target, and `seek_for_prev` on the last key <= target,
    /// including between, before and after every entry.
    #[test]
    fn seek_lands_on_the_right_side_of_a_gap() {
        let entries = pairs(&[b"b", b"d", b"f"]);
        let block = build(2, &entries);
        let mut iter = block.iter(comparator());

        for (target, expected) in [
            (b"a".as_slice(), Some(b"b".as_slice())),
            (b"b", Some(b"b")),
            (b"c", Some(b"d")),
            (b"f", Some(b"f")),
            (b"g", None),
        ] {
            iter.seek(target);
            assert_eq!(
                iter.valid().then(|| iter.key()),
                expected,
                "seek {target:?}"
            );
        }

        for (target, expected) in [
            (b"a".as_slice(), None),
            (b"b", Some(b"b".as_slice())),
            (b"c", Some(b"b")),
            (b"f", Some(b"f")),
            (b"g", Some(b"f")),
        ] {
            iter.seek_for_prev(target);
            assert_eq!(
                iter.valid().then(|| iter.key()),
                expected,
                "seek_for_prev {target:?}"
            );
        }
    }

    /// Walking off either end and then back must work: an invalid cursor is a position, not a
    /// broken state.
    #[test]
    fn stepping_past_the_ends() {
        let entries = pairs(&[b"a", b"b", b"c"]);
        let block = build(2, &entries);
        let mut iter = block.iter(comparator());

        iter.seek_to_last();
        iter.next();
        assert!(!iter.valid());
        iter.next();
        assert!(!iter.valid(), "next on an invalid cursor must be a no-op");
        iter.seek_to_first();
        assert_eq!(iter.key(), b"a");
        iter.prev();
        assert!(!iter.valid());
        iter.prev();
        assert!(!iter.valid(), "prev on an invalid cursor must be a no-op");
        iter.seek_to_last();
        assert_eq!(iter.key(), b"c");
        iter.status().unwrap();
    }

    /// Mixing directions must not lose the cursor's place.
    #[test]
    fn alternating_next_and_prev() {
        let entries = pairs(&[b"k0", b"k1", b"k2", b"k3", b"k4", b"k5"]);
        let block = build(2, &entries);
        let mut iter = block.iter(comparator());
        iter.seek(b"k3");
        assert_eq!(iter.key(), b"k3");
        iter.prev();
        assert_eq!(iter.key(), b"k2");
        iter.next();
        assert_eq!(iter.key(), b"k3");
        iter.next();
        assert_eq!(iter.key(), b"k4");
        iter.prev();
        assert_eq!(iter.key(), b"k3");
        iter.prev();
        assert_eq!(iter.key(), b"k2");
        iter.prev();
        assert_eq!(iter.key(), b"k1");
        iter.status().unwrap();
    }

    /// A block whose trailer or restart array cannot be believed must be rejected at parse
    /// time, not walked into.
    #[test]
    fn malformed_blocks_are_rejected() {
        let arc = |v: Vec<u8>| Arc::from(v.into_boxed_slice());
        assert!(Block::new(arc(vec![])).is_err(), "no trailer");
        assert!(Block::new(arc(vec![0, 0, 0])).is_err(), "short trailer");
        assert!(Block::new(arc(vec![0, 0, 0, 0])).is_err(), "zero restarts");
        // Claims 1000 restart points in 8 bytes.
        assert!(Block::new(arc(vec![0, 0, 0, 0, 0xe8, 0x03, 0, 0])).is_err());
        // Claims a restart count that overflows when multiplied out.
        assert!(Block::new(arc(vec![0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])).is_err());
    }

    /// A restart point aimed into the restart array, and an entry claiming to share more
    /// bytes than the previous key has: both are corruption a checksum would not catch if the
    /// bytes were rewritten wholesale, and both must surface as a status, never a panic.
    #[test]
    fn corrupt_entries_surface_as_status() {
        let entries = pairs(&[b"aaaa", b"aaab", b"aaac"]);
        let mut builder = BlockBuilder::new(16);
        for (key, value) in &entries {
            builder.add(key, value).unwrap();
        }
        let mut bytes = builder.finish().to_vec();

        // Entry 1 shares 3 bytes with "aaaa"; claim it shares 200.
        let entry1 = 3 + 4 + 2;
        bytes[entry1] = 200;
        let block = Block::new(Arc::from(bytes.into_boxed_slice())).unwrap();
        let mut iter = block.iter(comparator());
        iter.seek_to_first();
        assert_eq!(iter.key(), b"aaaa");
        iter.next();
        assert!(!iter.valid());
        assert!(iter.status().is_err(), "an impossible shared prefix passed");
    }

    proptest! {
        /// Build from a sorted map, read every key back, forwards and backwards, at every
        /// restart interval. Keys are drawn to collide heavily in their prefixes and to
        /// include 0xFF bytes, which is where a length-prefixed decoder goes wrong.
        #[test]
        fn round_trips_over_arbitrary_sorted_entries(
            raw in prop::collection::btree_map(
                prop_oneof![
                    prop::collection::vec(prop_oneof![Just(0u8), Just(0xffu8), any::<u8>()], 0..24),
                    (0..8usize, 0..8usize).prop_map(|(a, b)| {
                        let mut k = vec![0xffu8; a];
                        k.extend(std::iter::repeat_n(b'x', b));
                        k
                    }),
                ],
                prop::collection::vec(any::<u8>(), 0..40),
                0..64,
            ),
            interval in 1usize..20,
        ) {
            let entries: Vec<(Vec<u8>, Vec<u8>)> = raw.into_iter().collect();
            let block = build(interval, &entries);
            let mut iter = block.iter(comparator());

            prop_assert_eq!(collect(&mut iter), entries.clone());

            let mut reversed = collect_reverse(&mut iter);
            reversed.reverse();
            prop_assert_eq!(reversed, entries.clone());

            // Every key is found exactly, and every key seeks to itself.
            for (key, value) in &entries {
                iter.seek(key);
                prop_assert!(iter.valid());
                prop_assert_eq!(iter.key(), &key[..]);
                prop_assert_eq!(iter.value(), &value[..]);

                iter.seek_for_prev(key);
                prop_assert!(iter.valid());
                prop_assert_eq!(iter.key(), &key[..]);
            }
            prop_assert!(iter.status().is_ok());
        }

        /// Seeking to a key that is not in the block must land where a `BTreeMap` says it
        /// should: the model is the definition of correct here.
        #[test]
        fn seek_matches_a_btree_model(
            keys in prop::collection::btree_set(prop::collection::vec(any::<u8>(), 0..6), 0..40),
            probes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..6), 1..30),
            interval in 1usize..8,
        ) {
            let model: BTreeMap<Vec<u8>, Vec<u8>> =
                keys.iter().map(|k| (k.clone(), k.clone())).collect();
            let entries: Vec<(Vec<u8>, Vec<u8>)> = model.clone().into_iter().collect();
            let block = build(interval, &entries);
            let mut iter = block.iter(comparator());

            for probe in &probes {
                iter.seek(probe);
                let expected = model.range(probe.clone()..).next().map(|(k, _)| k.clone());
                prop_assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);

                iter.seek_for_prev(probe);
                let expected = model.range(..=probe.clone()).next_back().map(|(k, _)| k.clone());
                prop_assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);
            }
            prop_assert!(iter.status().is_ok());
        }
    }
}
