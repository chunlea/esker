//! The table writer: turns a sorted stream of key/value pairs into one SST file.
//!
//! # What it writes, in order
//!
//! ```text
//! data blocks     cut when the block reaches `block_size`
//! filter block    one bloom over every key, absent when bloom_bits_per_key == 0
//! index block     one entry per data block: separator key -> block handle
//! properties block
//! footer          48 bytes, last
//! ```
//!
//! The index comes before the properties so that `esker.index.size` can be recorded; the
//! properties block is the last thing before the footer for the same reason `RocksDB` puts it
//! there — it is the only block that describes the others.
//!
//! # Two blocks are deliberately not compressed
//!
//! The **filter** is a bloom bit array: high-entropy by construction, so LZ4 would spend CPU
//! to save nothing. The **properties** block is what `sst-dump` reads to explain a file that
//! may be damaged elsewhere, and a plain block is one fewer thing that can stop it. Data and
//! index blocks use `options.compression`, and even then only when it saves more than an
//! eighth (see [`encode_block`]).
//!
//! # The separator key, and a future optimisation
//!
//! An index entry's key is the *full last key* of its data block. That is correct — a seek
//! for `k` finds the first block whose separator is `>= k`, which is the first block that
//! could hold `k` — but it is not minimal. `LevelDB` shortens the separator to the smallest
//! string strictly between one block's last key and the next block's first key, so an index
//! over long keys stays small. Doing that needs the *next* key, so it means deferring the
//! index entry by one block, and it needs a comparator that can construct a separator rather
//! than only compare. Format-compatible either way: the reader does not care how short the
//! separator is, so this can change without a version bump.
//!
//! # Sequence numbers come from the engine
//!
//! An SST records the seqno range it covers, but that range lives in the key suffix and this
//! layer does not parse keys (`CLAUDE.md` invariant 7). The engine calls
//! [`TableBuilder::set_seqno_range`] with what it knows; without that call the range is
//! recorded as `0..=0`.
//!
//! [`encode_block`]: super::footer::encode_block

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use crate::dbformat::{BytewiseComparator, Comparator};
use crate::error::{Error, Result};
use crate::format::SST_FORMAT_VERSION;
use crate::fs::WritableFile;
use crate::options::{Compression, PrefixExtractor, defaults};
use crate::range_del::RangeTombstones;

use super::block::BlockBuilder;
use super::filter::{BloomBuilder, filter_key};
use super::footer::{BlockHandle, Footer, MAX_TABLE_SIZE, encode_block};
use super::props::TableProperties;

/// The label an SST-level [`Error::Io`] carries. The write path is handed an open file and
/// never learns its name, so there is nothing more specific to say here.
const CONTEXT: &str = "<sst>";

/// How a table is built and how it is read; the same knobs must be used for both, because
/// several of them (the comparator above all) are not recorded in a way that can be guessed.
#[derive(Debug, Clone)]
pub struct TableOptions {
    /// Cut a data block once it reaches this many bytes. 4 KiB by default.
    pub block_size: usize,
    /// Entries between restart points inside a block. 16 by default.
    pub restart_interval: usize,
    /// Bloom filter density. 0 writes no filter block at all.
    pub bloom_bits_per_key: usize,
    /// Builds the filter over a prefix of each key rather than the whole key.
    pub prefix_extractor: Option<Arc<dyn PrefixExtractor>>,
    /// Codec for data and index blocks.
    pub compression: Compression,
    /// The order the keys are in. Recorded by name in the properties, and checked on open.
    pub comparator: Arc<dyn Comparator>,
}

impl Default for TableOptions {
    /// The `docs/DESIGN.md` §14 defaults: 4 KiB blocks, restart every 16, 10 bits/key of
    /// bloom, LZ4, bytewise order, no prefix extractor.
    fn default() -> Self {
        Self {
            block_size: defaults::BLOCK_SIZE,
            restart_interval: defaults::RESTART_INTERVAL,
            bloom_bits_per_key: defaults::BLOOM_BITS_PER_KEY,
            prefix_extractor: None,
            compression: Compression::default(),
            comparator: Arc::new(BytewiseComparator),
        }
    }
}

/// Appends `payload` as a framed block and returns its handle.
///
/// Free-standing rather than a method so that the caller can hold a borrow of the block
/// builder producing the payload while this borrows the file and the offset.
fn write_block(
    file: &mut dyn WritableFile,
    offset: &mut u64,
    payload: &[u8],
    compression: Compression,
) -> Result<BlockHandle> {
    let framed = encode_block(payload, compression);
    let handle = BlockHandle::new(
        *offset,
        (framed.len() - crate::format::BLOCK_TRAILER_SIZE) as u64,
    );
    file.append(&framed).map_err(|e| Error::io(CONTEXT, e))?;
    *offset += framed.len() as u64;
    if *offset > MAX_TABLE_SIZE {
        return Err(Error::InvalidArgument(format!(
            "sst grew to {offset} bytes, past the {MAX_TABLE_SIZE}-byte cap that keeps the \
             footer a fixed 48 bytes"
        )));
    }
    Ok(handle)
}

/// Writes one sorted string table.
///
/// Keys must arrive in `options.comparator` order, strictly increasing;
/// [`TableBuilder::add`] rejects anything else rather than writing a file whose index would
/// lie. A table with no entries at all is legal and well-defined: no data blocks, no filter,
/// an empty index, and properties saying so.
pub struct TableBuilder {
    options: TableOptions,
    file: Box<dyn WritableFile>,
    data_block: BlockBuilder,
    index_block: BlockBuilder,
    filter: Option<BloomBuilder>,
    offset: u64,
    props: TableProperties,
    last_key: Vec<u8>,
    separator: Vec<u8>,
    handle_bytes: Vec<u8>,
    range_tombstones: RangeTombstones,
}

impl fmt::Debug for TableBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Non-exhaustive on purpose: `WritableFile` is a file, not a value, and is not
        // `Debug`; the pending block builders are scratch buffers nobody wants printed.
        f.debug_struct("TableBuilder")
            .field("options", &self.options)
            .field("offset", &self.offset)
            .field("entries", &self.props.entry_count)
            .field("data_blocks", &self.props.data_block_count)
            .finish_non_exhaustive()
    }
}

impl TableBuilder {
    /// A builder writing to `file`, which it owns until [`TableBuilder::finish`].
    #[must_use]
    pub fn new(options: TableOptions, file: Box<dyn WritableFile>) -> Self {
        let filter =
            (options.bloom_bits_per_key > 0).then(|| BloomBuilder::new(options.bloom_bits_per_key));
        let props = TableProperties {
            format_version: SST_FORMAT_VERSION,
            comparator_name: options.comparator.name().to_string(),
            prefix_extractor_name: options
                .prefix_extractor
                .as_ref()
                .map(|extractor| extractor.name().to_string()),
            bloom_bits_per_key: options.bloom_bits_per_key as u64,
            compression: options.compression,
            ..TableProperties::default()
        };
        Self {
            data_block: BlockBuilder::new(options.restart_interval),
            index_block: BlockBuilder::new(options.restart_interval),
            filter,
            options,
            file,
            offset: 0,
            props,
            last_key: Vec::new(),
            separator: Vec::new(),
            handle_bytes: Vec::new(),
            range_tombstones: RangeTombstones::new(),
        }
    }

    /// Records the range of sequence numbers this table covers. See the module docs for why
    /// the engine has to supply it.
    pub fn set_seqno_range(&mut self, smallest: u64, largest: u64) {
        self.props.smallest_seqno = smallest;
        self.props.largest_seqno = largest;
    }

    /// Attaches the range tombstones this table carries.
    ///
    /// They are written as a block of their own, and the table's key bounds are widened to
    /// span them: a read for a key inside a deleted range has to open the file that says so,
    /// and files are picked by their bounds
    /// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)). Call before
    /// [`TableBuilder::finish`]; calling twice replaces the set.
    pub fn set_range_tombstones(&mut self, tombstones: RangeTombstones) {
        self.range_tombstones = tombstones;
    }

    /// Bytes written so far. The flush job uses this to decide when to start a new file.
    #[must_use]
    pub fn file_size(&self) -> u64 {
        self.offset
    }

    /// Entries added so far.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.props.entry_count
    }

    /// Appends one entry.
    ///
    /// Fails if `key` is not strictly greater than the previous key under the configured
    /// comparator. Catching it here rather than trusting the caller matters: an out-of-order
    /// key produces a file whose index is wrong, and the reader has no way to tell.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.props.entry_count > 0 {
            match self.options.comparator.cmp(&self.last_key, key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(Error::InvalidArgument(format!(
                        "duplicate key added to an sst under {}",
                        self.options.comparator.name()
                    )));
                }
                Ordering::Greater => {
                    return Err(Error::InvalidArgument(format!(
                        "keys added to an sst out of {} order",
                        self.options.comparator.name()
                    )));
                }
            }
        }

        if let Some(filter) = self.filter.as_mut()
            && let Some(bytes) = filter_key(self.options.prefix_extractor.as_deref(), key)
        {
            filter.add(bytes);
        }

        self.data_block.add(key, value)?;

        if self.props.entry_count == 0 {
            self.props.smallest_key = key.to_vec();
        }
        self.props.entry_count += 1;
        self.props.raw_key_bytes += key.len() as u64;
        self.props.raw_value_bytes += value.len() as u64;
        self.last_key.clear();
        self.last_key.extend_from_slice(key);

        if self.data_block.size_estimate() >= self.options.block_size {
            self.flush_data_block()?;
        }
        Ok(())
    }

    /// Writes the pending data block and its index entry, if there is one.
    fn flush_data_block(&mut self) -> Result<()> {
        if self.data_block.is_empty() {
            return Ok(());
        }
        self.separator.clear();
        self.separator.extend_from_slice(self.data_block.last_key());

        let compression = self.options.compression;
        let payload = self.data_block.finish();
        let handle = write_block(self.file.as_mut(), &mut self.offset, payload, compression)?;
        self.data_block.reset();

        self.props.data_block_count += 1;
        self.props.data_size += handle.total_len();

        self.handle_bytes.clear();
        handle.encode_to(&mut self.handle_bytes);
        self.index_block.add(&self.separator, &self.handle_bytes)
    }

    /// Writes the remaining blocks and the footer, makes the file durable, and returns what
    /// the table now knows about itself.
    ///
    /// The file is synced here rather than by the caller because the builder owns it and the
    /// caller cannot reach it afterwards. An SST must be durable before the manifest edit that
    /// references it (`CLAUDE.md` invariant 1), so syncing is the only safe default.
    pub fn finish(mut self) -> Result<TableProperties> {
        self.flush_data_block()?;
        self.props.largest_key = std::mem::take(&mut self.last_key);

        // The range-deletion block, uncompressed and before the filter. Its handle goes in the
        // properties rather than the footer, which is 48 bytes forever and has no room for a
        // fourth ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)).
        if !self.range_tombstones.is_empty() {
            self.widen_bounds_for_tombstones();
            let payload = self.range_tombstones.encode();
            let handle = write_block(
                self.file.as_mut(),
                &mut self.offset,
                &payload,
                Compression::None,
            )?;
            self.props.range_del_offset = handle.offset;
            self.props.range_del_size = handle.size;
            self.props.range_del_count = self.range_tombstones.len() as u64;
        }

        // The filter, uncompressed. An empty table gets none: its handle would sit at offset
        // 0 with size 0, which is exactly how the footer spells "absent".
        let filter_handle = match self.filter.take() {
            Some(bloom) if self.props.entry_count > 0 => {
                let payload = bloom.finish();
                write_block(
                    self.file.as_mut(),
                    &mut self.offset,
                    &payload,
                    Compression::None,
                )?
            }
            _ => BlockHandle::NONE,
        };
        self.props.filter_size = if filter_handle.is_none() {
            0
        } else {
            filter_handle.total_len()
        };

        let compression = self.options.compression;
        let payload = self.index_block.finish();
        let index_handle = write_block(self.file.as_mut(), &mut self.offset, payload, compression)?;
        self.props.index_size = index_handle.total_len();

        // Properties last, so they can describe every block above; uncompressed, so a tool
        // can read them out of a file whose data blocks are damaged.
        let props_payload = self.props.encode()?;
        let props_handle = write_block(
            self.file.as_mut(),
            &mut self.offset,
            &props_payload,
            Compression::None,
        )?;

        let footer = Footer::new(index_handle, filter_handle, props_handle).encode()?;
        self.file
            .append(&footer)
            .map_err(|e| Error::io(CONTEXT, e))?;
        self.offset += footer.len() as u64;
        self.file.sync_data().map_err(|e| Error::io(CONTEXT, e))?;

        self.props.file_size = self.offset;
        Ok(self.props)
    }

    /// Grows the table's key bounds to span its tombstones.
    ///
    /// `Version::overlapping` picks files by `smallest_key`/`largest_key`, so a tombstone
    /// reaching outside them would be invisible to exactly the reads that need it — and the
    /// failure would be silent: the read returns the value from a lower level with nothing
    /// reporting an error.
    ///
    /// The bounds are *user* keys here, and a tombstone's `end` is exclusive; recording it as
    /// the largest key overstates the table's reach by one key, which costs an extra file
    /// opened and never a wrong answer. An empty table takes the tombstones' bounds outright.
    fn widen_bounds_for_tombstones(&mut self) {
        let comparator = self.options.comparator.as_ref();
        let Some((low, high)) = self.range_tombstones.key_bounds(comparator) else {
            return;
        };
        let (low, high) = (low.to_vec(), high.to_vec());
        if self.props.entry_count == 0 {
            self.props.smallest_key = low;
            self.props.largest_key = high;
            return;
        }
        if comparator.cmp(&low, &self.props.smallest_key) == Ordering::Less {
            self.props.smallest_key = low;
        }
        if comparator.cmp(&high, &self.props.largest_key) == Ordering::Greater {
            self.props.largest_key = high;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TableBuilder, TableOptions};
    use crate::dbformat::Comparator;
    use crate::fs::FileSystem;
    use crate::memfs::MemFileSystem;
    use crate::options::{Compression, StripSuffix};
    use crate::sst::footer::{BlockHandle, Footer};
    use crate::sst::props::TableProperties;
    use std::cmp::Ordering;
    use std::path::Path;
    use std::sync::Arc;

    /// Reverse bytewise order, to prove nothing here assumes `memcmp`.
    #[derive(Debug)]
    struct ReverseComparator;

    impl Comparator for ReverseComparator {
        fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
            b.cmp(a)
        }
        fn name(&self) -> &'static str {
            "test.ReverseComparator"
        }
    }

    fn build(
        options: TableOptions,
        entries: &[(Vec<u8>, Vec<u8>)],
    ) -> (MemFileSystem, TableProperties) {
        let fs = MemFileSystem::new();
        let file = fs.create(Path::new("/t.sst")).unwrap();
        let mut builder = TableBuilder::new(options, file);
        for (key, value) in entries {
            builder.add(key, value).unwrap();
        }
        let props = builder.finish().unwrap();
        (fs, props)
    }

    fn kv(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..count)
            .map(|i| {
                (
                    format!("key{i:06}").into_bytes(),
                    format!("value-{i}").into_bytes(),
                )
            })
            .collect()
    }

    fn footer_of(bytes: &[u8]) -> Footer {
        Footer::decode(&bytes[bytes.len() - crate::format::SST_FOOTER_SIZE..]).unwrap()
    }

    /// The file the builder writes must be exactly as long as it says, end in a footer, and
    /// have that footer point at blocks that lie inside it.
    #[test]
    fn the_file_is_self_describing() {
        let entries = kv(500);
        let (fs, props) = build(TableOptions::default(), &entries);
        let bytes = fs.contents("/t.sst").unwrap();

        assert_eq!(props.file_size, bytes.len() as u64);
        assert_eq!(props.entry_count, 500);
        assert!(props.data_block_count > 1, "500 entries fit in one block?");
        assert_eq!(props.smallest_key, entries[0].0);
        assert_eq!(props.largest_key, entries[499].0);

        let footer = footer_of(&bytes);
        for handle in [footer.index, footer.filter, footer.properties] {
            assert!(!handle.is_none());
            assert!(
                handle.offset + handle.total_len() <= bytes.len() as u64,
                "handle {handle:?} points past the end of a {}-byte file",
                bytes.len()
            );
        }
        // Blocks are laid out in the documented order, without gaps or overlap.
        assert_eq!(footer.filter.offset, props.data_size);
        assert_eq!(
            footer.index.offset,
            footer.filter.offset + footer.filter.total_len()
        );
        assert_eq!(
            footer.properties.offset,
            footer.index.offset + footer.index.total_len()
        );
        assert_eq!(
            footer.properties.offset + footer.properties.total_len() + 48,
            bytes.len() as u64
        );
    }

    /// A key that is not strictly after the last one would make the index lie, so it is
    /// refused — under whatever comparator was configured, not under `memcmp`.
    #[test]
    fn out_of_order_keys_are_refused() {
        let fs = MemFileSystem::new();
        let mut builder = TableBuilder::new(
            TableOptions::default(),
            fs.create(Path::new("/t.sst")).unwrap(),
        );
        builder.add(b"b", b"1").unwrap();
        assert!(builder.add(b"a", b"2").is_err(), "went backwards");
        assert!(builder.add(b"b", b"3").is_err(), "repeated a key");
        builder.add(b"c", b"4").unwrap();

        let fs = MemFileSystem::new();
        let options = TableOptions {
            comparator: Arc::new(ReverseComparator),
            ..TableOptions::default()
        };
        let mut builder = TableBuilder::new(options, fs.create(Path::new("/r.sst")).unwrap());
        builder.add(b"c", b"1").unwrap();
        builder.add(b"b", b"2").unwrap();
        assert!(builder.add(b"z", b"3").is_err(), "ascending under reverse");
        let props = builder.finish().unwrap();
        assert_eq!(props.comparator_name, "test.ReverseComparator");
        assert_eq!(props.smallest_key, b"c");
        assert_eq!(props.largest_key, b"b");
    }

    /// A table with no entries is legal. It has no filter, because a `{0, 0}` handle is how
    /// the footer says "absent" and an empty table's filter would land exactly there.
    #[test]
    fn an_empty_table_is_well_defined() {
        let (fs, props) = build(TableOptions::default(), &[]);
        let bytes = fs.contents("/t.sst").unwrap();

        assert_eq!(props.entry_count, 0);
        assert_eq!(props.data_block_count, 0);
        assert_eq!(props.data_size, 0);
        assert_eq!(props.filter_size, 0);
        assert!(props.smallest_key.is_empty());
        assert_eq!(props.file_size, bytes.len() as u64);

        let footer = footer_of(&bytes);
        assert_eq!(footer.filter, BlockHandle::NONE);
        assert_eq!(footer.index.offset, 0, "the index is the first block");
        assert!(!footer.index.is_none(), "an empty index is still a block");
        assert!(!footer.properties.is_none());
    }

    /// Setting `bloom_bits_per_key` to 0 must leave the filter out entirely, not write an
    /// empty one.
    #[test]
    fn no_filter_when_bits_per_key_is_zero() {
        let options = TableOptions {
            bloom_bits_per_key: 0,
            ..TableOptions::default()
        };
        let (fs, props) = build(options, &kv(50));
        assert_eq!(props.filter_size, 0);
        assert_eq!(props.bloom_bits_per_key, 0);
        assert_eq!(
            footer_of(&fs.contents("/t.sst").unwrap()).filter,
            BlockHandle::NONE
        );
    }

    /// The extractor's name is recorded so a reader can tell whether the filter it found was
    /// built over the prefixes it is about to probe with.
    #[test]
    fn the_prefix_extractor_is_named_in_the_properties() {
        let options = TableOptions {
            prefix_extractor: Some(Arc::new(StripSuffix::new(4))),
            ..TableOptions::default()
        };
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
            .map(|i| (format!("k{i:04}ts{i:02}").into_bytes(), b"v".to_vec()))
            .collect();
        let (_, props) = build(options, &entries);
        assert_eq!(
            props.prefix_extractor_name.as_deref(),
            Some("esker.StripSuffix.4")
        );
        assert!(props.filter_size > 0);
    }

    /// Blocks are cut at `block_size`, so a smaller block size means more blocks and a bigger
    /// index — the trade the knob exists to make.
    #[test]
    fn block_size_controls_how_many_blocks_are_cut() {
        let entries = kv(400);
        let (_, small) = build(
            TableOptions {
                block_size: 256,
                ..TableOptions::default()
            },
            &entries,
        );
        let (_, large) = build(
            TableOptions {
                block_size: 64 * 1024,
                ..TableOptions::default()
            },
            &entries,
        );
        assert!(
            small.data_block_count > large.data_block_count,
            "{} vs {}",
            small.data_block_count,
            large.data_block_count
        );
        assert_eq!(large.data_block_count, 1);
        assert!(small.index_size > large.index_size);
        assert_eq!(small.entry_count, large.entry_count);
        assert_eq!(small.raw_key_bytes, large.raw_key_bytes);
    }

    /// Compressed and uncompressed tables hold the same entries and the same properties apart
    /// from the sizes, so nothing about the codec leaks into the logical content.
    #[test]
    fn compression_changes_only_sizes() {
        let entries = kv(300);
        let (_, lz4) = build(
            TableOptions {
                compression: Compression::Lz4,
                ..TableOptions::default()
            },
            &entries,
        );
        let (_, none) = build(
            TableOptions {
                compression: Compression::None,
                ..TableOptions::default()
            },
            &entries,
        );
        assert_eq!(lz4.entry_count, none.entry_count);
        assert_eq!(lz4.raw_key_bytes, none.raw_key_bytes);
        assert_eq!(lz4.raw_value_bytes, none.raw_value_bytes);
        assert_eq!(lz4.data_block_count, none.data_block_count);
        assert_eq!(lz4.compression, Compression::Lz4);
        assert!(
            lz4.data_size < none.data_size,
            "lz4 {} vs none {}",
            lz4.data_size,
            none.data_size
        );
    }

    /// The seqno range is whatever the engine declared, and 0..=0 when it declared nothing.
    #[test]
    fn the_seqno_range_comes_from_the_caller() {
        let (_, props) = build(TableOptions::default(), &kv(5));
        assert_eq!((props.smallest_seqno, props.largest_seqno), (0, 0));

        let fs = MemFileSystem::new();
        let mut builder = TableBuilder::new(
            TableOptions::default(),
            fs.create(Path::new("/t.sst")).unwrap(),
        );
        builder.set_seqno_range(17, 9_000_000_000);
        builder.add(b"k", b"v").unwrap();
        let props = builder.finish().unwrap();
        assert_eq!(
            (props.smallest_seqno, props.largest_seqno),
            (17, 9_000_000_000)
        );
    }

    /// Building the same entries twice must produce identical bytes. Anything that leaks a
    /// hash-map iteration order or a timestamp into the file breaks this.
    #[test]
    fn the_writer_is_deterministic() {
        let entries = kv(200);
        let (first, _) = build(TableOptions::default(), &entries);
        let (second, _) = build(TableOptions::default(), &entries);
        assert_eq!(
            first.contents("/t.sst").unwrap(),
            second.contents("/t.sst").unwrap()
        );
    }

    /// The builder syncs before returning, because the caller cannot reach the file
    /// afterwards and an SST must be durable before the manifest names it.
    #[test]
    fn finish_makes_the_file_durable() {
        let fs = MemFileSystem::new();
        let mut builder = TableBuilder::new(
            TableOptions::default(),
            fs.create(Path::new("/t.sst")).unwrap(),
        );
        for (key, value) in &kv(100) {
            builder.add(key, value).unwrap();
        }
        let props = builder.finish().unwrap();

        // Everything the builder wrote survives losing every unsynced byte.
        fs.lose_unsynced().unwrap();
        assert_eq!(
            fs.contents("/t.sst").unwrap().len() as u64,
            props.file_size,
            "finish() returned before the file was durable"
        );
    }
}
