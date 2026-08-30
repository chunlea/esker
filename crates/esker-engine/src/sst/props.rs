//! The properties block: what a reader can learn about a table without reading its data.
//!
//! Compaction pickers, `sst-dump` and the read path all want facts about a file — how many
//! entries, which key range, which comparator built it — and none of them should have to
//! decompress a data block to get one. The properties block answers all of it in one small
//! read at open time.
//!
//! # Layout
//!
//! An ordinary [block](super::block), so it needs no second decoder, with a restart interval
//! of 1: property names share almost nothing, and standing entries alone keeps the block
//! readable by a tool that has lost the rest of the file. Names are ASCII and stored in
//! bytewise order, as the block format requires. Values are either a LEB128 `u64` or raw
//! bytes, per the table in [`TableProperties`].
//!
//! **An unknown name is ignored and a missing one keeps its default.** That is what lets a
//! later format version add a property without making every older reader fail, and it is the
//! only place in this crate where unrecognised on-disk bytes are not an error — they are
//! names, not data, and every value the reader actually depends on is checked by the caller.
//!
//! # Two names that are safety checks, not statistics
//!
//! `esker.comparator` and `esker.prefix_extractor` record *how* the file was built. A table
//! sorted by one comparator and searched with another returns wrong answers rather than
//! errors, and a filter built over prefixes and probed with whole keys reports present keys as
//! absent. [`TableReader`] refuses the first and disables the filter on the second.
//!
//! [`TableReader`]: super::reader::TableReader

use std::sync::Arc;

use esker_base::varint;

use crate::dbformat::{BytewiseComparator, Comparator};
use crate::error::{Error, Result};
use crate::options::Compression;

use super::block::{Block, BlockBuilder};

/// Property names, in the bytewise order the block stores them.
mod name {
    pub(super) const BLOOM_BITS_PER_KEY: &[u8] = b"esker.bloom.bits_per_key";
    pub(super) const COMPARATOR: &[u8] = b"esker.comparator";
    pub(super) const COMPRESSION: &[u8] = b"esker.compression";
    pub(super) const DATA_BLOCK_COUNT: &[u8] = b"esker.data.block_count";
    pub(super) const DATA_SIZE: &[u8] = b"esker.data.size";
    pub(super) const ENTRY_COUNT: &[u8] = b"esker.entry_count";
    pub(super) const FILTER_SIZE: &[u8] = b"esker.filter.size";
    pub(super) const FORMAT_VERSION: &[u8] = b"esker.format_version";
    pub(super) const INDEX_SIZE: &[u8] = b"esker.index.size";
    pub(super) const KEY_LARGEST: &[u8] = b"esker.key.largest";
    pub(super) const KEY_SMALLEST: &[u8] = b"esker.key.smallest";
    pub(super) const PREFIX_EXTRACTOR: &[u8] = b"esker.prefix_extractor";
    pub(super) const RAW_KEY_BYTES: &[u8] = b"esker.raw_key_bytes";
    pub(super) const RAW_VALUE_BYTES: &[u8] = b"esker.raw_value_bytes";
    pub(super) const SEQNO_LARGEST: &[u8] = b"esker.seqno.largest";
    pub(super) const SEQNO_SMALLEST: &[u8] = b"esker.seqno.smallest";

    /// Every name, in the order they must be written. Only the test that pins that
    /// order reads it; the encoder spells the order out in code.
    #[cfg(test)]
    pub(super) const ALL: [&[u8]; 16] = [
        BLOOM_BITS_PER_KEY,
        COMPARATOR,
        COMPRESSION,
        DATA_BLOCK_COUNT,
        DATA_SIZE,
        ENTRY_COUNT,
        FILTER_SIZE,
        FORMAT_VERSION,
        INDEX_SIZE,
        KEY_LARGEST,
        KEY_SMALLEST,
        PREFIX_EXTRACTOR,
        RAW_KEY_BYTES,
        RAW_VALUE_BYTES,
        SEQNO_LARGEST,
        SEQNO_SMALLEST,
    ];
}

/// What one table knows about itself.
///
/// | Property | Encoding | Meaning |
/// |---|---|---|
/// | `esker.entry_count` | varint | entries added to the table |
/// | `esker.data.block_count` | varint | data blocks written |
/// | `esker.raw_key_bytes` | varint | key bytes before prefix compression |
/// | `esker.raw_value_bytes` | varint | value bytes before compression |
/// | `esker.data.size` | varint | stored bytes of data blocks, trailers included |
/// | `esker.index.size` | varint | stored bytes of the index block |
/// | `esker.filter.size` | varint | stored bytes of the filter block, 0 when absent |
/// | `esker.key.smallest` / `esker.key.largest` | bytes | first and last key, verbatim |
/// | `esker.seqno.smallest` / `esker.seqno.largest` | varint | supplied by the engine |
/// | `esker.format_version` | varint | the layout version that wrote this |
/// | `esker.comparator` | bytes | comparator name — a correctness check |
/// | `esker.prefix_extractor` | bytes | extractor name, empty when none — a correctness check |
/// | `esker.bloom.bits_per_key` | varint | filter density, 0 when there is no filter |
/// | `esker.compression` | varint | the codec the builder was configured with |
///
/// `file_size` is deliberately not in the block: the block is written before the footer, so
/// the size is not known yet. The builder fills it in from what it wrote, and the reader from
/// the file itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableProperties {
    /// Entries in the table.
    pub entry_count: u64,
    /// Data blocks written.
    pub data_block_count: u64,
    /// Key bytes as handed to the builder, before prefix compression.
    pub raw_key_bytes: u64,
    /// Value bytes as handed to the builder, before compression.
    pub raw_value_bytes: u64,
    /// Stored size of all data blocks, trailers included.
    pub data_size: u64,
    /// Stored size of the index block, trailer included.
    pub index_size: u64,
    /// Stored size of the filter block, trailer included; 0 when there is no filter.
    pub filter_size: u64,
    /// First key in the table.
    pub smallest_key: Vec<u8>,
    /// Last key in the table.
    pub largest_key: Vec<u8>,
    /// Lowest sequence number the table holds, as declared by the engine.
    pub smallest_seqno: u64,
    /// Highest sequence number the table holds, as declared by the engine.
    pub largest_seqno: u64,
    /// Layout version that wrote the table.
    pub format_version: u32,
    /// Name of the comparator the keys are ordered by.
    pub comparator_name: String,
    /// Name of the prefix extractor the filter was built with, if any.
    pub prefix_extractor_name: Option<String>,
    /// Bits per key the filter was built at; 0 when there is no filter.
    pub bloom_bits_per_key: u64,
    /// Codec the builder was configured with. Individual blocks may still be stored
    /// uncompressed when compression did not pay — each block's trailer is authoritative.
    pub compression: Compression,
    /// Total size of the file. Not stored in the block; see the type's docs.
    pub file_size: u64,
}

impl TableProperties {
    /// Serialises to the properties block payload.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut builder = BlockBuilder::new(1);
        let mut scratch = Vec::with_capacity(varint::MAX_LEN_U64);
        let mut put_u64 = |builder: &mut BlockBuilder, key: &[u8], value: u64| {
            scratch.clear();
            varint::put_u64(value, &mut scratch);
            builder.add(key, &scratch)
        };

        // Written in `name::ALL` order, which a test pins as bytewise sorted.
        put_u64(
            &mut builder,
            name::BLOOM_BITS_PER_KEY,
            self.bloom_bits_per_key,
        )?;
        builder.add(name::COMPARATOR, self.comparator_name.as_bytes())?;
        put_u64(
            &mut builder,
            name::COMPRESSION,
            u64::from(self.compression.as_u8()),
        )?;
        put_u64(&mut builder, name::DATA_BLOCK_COUNT, self.data_block_count)?;
        put_u64(&mut builder, name::DATA_SIZE, self.data_size)?;
        put_u64(&mut builder, name::ENTRY_COUNT, self.entry_count)?;
        put_u64(&mut builder, name::FILTER_SIZE, self.filter_size)?;
        put_u64(
            &mut builder,
            name::FORMAT_VERSION,
            u64::from(self.format_version),
        )?;
        put_u64(&mut builder, name::INDEX_SIZE, self.index_size)?;
        builder.add(name::KEY_LARGEST, &self.largest_key)?;
        builder.add(name::KEY_SMALLEST, &self.smallest_key)?;
        builder.add(
            name::PREFIX_EXTRACTOR,
            self.prefix_extractor_name
                .as_deref()
                .unwrap_or("")
                .as_bytes(),
        )?;
        put_u64(&mut builder, name::RAW_KEY_BYTES, self.raw_key_bytes)?;
        put_u64(&mut builder, name::RAW_VALUE_BYTES, self.raw_value_bytes)?;
        put_u64(&mut builder, name::SEQNO_LARGEST, self.largest_seqno)?;
        put_u64(&mut builder, name::SEQNO_SMALLEST, self.smallest_seqno)?;

        Ok(builder.finish().to_vec())
    }

    /// Parses a properties block. Unknown names are skipped; absent ones keep their defaults.
    pub fn decode(payload: Arc<[u8]>) -> Result<Self> {
        let block = Block::new(payload)?;
        let comparator: Arc<dyn Comparator> = Arc::new(BytewiseComparator);
        let mut iter = block.iter(comparator);
        let mut props = Self::default();

        iter.seek_to_first();
        while iter.valid() {
            let (key, value) = (iter.key().to_vec(), iter.value());
            match key.as_slice() {
                name::BLOOM_BITS_PER_KEY => props.bloom_bits_per_key = read_u64(value, &key)?,
                name::COMPARATOR => props.comparator_name = read_string(value, &key)?,
                name::COMPRESSION => {
                    let code = read_u64(value, &key)?;
                    props.compression = u8::try_from(code)
                        .ok()
                        .and_then(Compression::from_u8)
                        .ok_or_else(|| {
                            Error::corruption(
                                "sst properties",
                                format!("unknown compression code {code}"),
                            )
                        })?;
                }
                name::DATA_BLOCK_COUNT => props.data_block_count = read_u64(value, &key)?,
                name::DATA_SIZE => props.data_size = read_u64(value, &key)?,
                name::ENTRY_COUNT => props.entry_count = read_u64(value, &key)?,
                name::FILTER_SIZE => props.filter_size = read_u64(value, &key)?,
                name::FORMAT_VERSION => {
                    let version = read_u64(value, &key)?;
                    props.format_version = u32::try_from(version).map_err(|_| {
                        Error::corruption(
                            "sst properties",
                            format!("format version {version} does not fit in 32 bits"),
                        )
                    })?;
                }
                name::INDEX_SIZE => props.index_size = read_u64(value, &key)?,
                name::KEY_LARGEST => props.largest_key = value.to_vec(),
                name::KEY_SMALLEST => props.smallest_key = value.to_vec(),
                name::PREFIX_EXTRACTOR => {
                    let named = read_string(value, &key)?;
                    props.prefix_extractor_name = (!named.is_empty()).then_some(named);
                }
                name::RAW_KEY_BYTES => props.raw_key_bytes = read_u64(value, &key)?,
                name::RAW_VALUE_BYTES => props.raw_value_bytes = read_u64(value, &key)?,
                name::SEQNO_LARGEST => props.largest_seqno = read_u64(value, &key)?,
                name::SEQNO_SMALLEST => props.smallest_seqno = read_u64(value, &key)?,
                // A property this build does not know. See the module docs.
                _ => {}
            }
            iter.next();
        }
        iter.status()?;
        Ok(props)
    }
}

fn read_u64(value: &[u8], name: &[u8]) -> Result<u64> {
    varint::get_u64(value).map(|(v, _)| v).map_err(|e| {
        Error::corruption(
            "sst properties",
            format!("{}: {e}", String::from_utf8_lossy(name)),
        )
    })
}

fn read_string(value: &[u8], name: &[u8]) -> Result<String> {
    String::from_utf8(value.to_vec()).map_err(|_| {
        Error::corruption(
            "sst properties",
            format!("{} is not utf-8", String::from_utf8_lossy(name)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{TableProperties, name};
    use crate::format::SST_FORMAT_VERSION;
    use crate::options::Compression;
    use std::sync::Arc;

    fn sample() -> TableProperties {
        TableProperties {
            entry_count: 1234,
            data_block_count: 7,
            raw_key_bytes: 40_000,
            raw_value_bytes: 900_000,
            data_size: 28_672,
            index_size: 260,
            filter_size: 1_546,
            smallest_key: b"aaaa\x00\xff".to_vec(),
            largest_key: b"zzzz".to_vec(),
            smallest_seqno: 5,
            largest_seqno: 9_000_000_000,
            format_version: SST_FORMAT_VERSION,
            comparator_name: "esker.BytewiseComparator".to_string(),
            prefix_extractor_name: Some("esker.StripSuffix.8".to_string()),
            bloom_bits_per_key: 10,
            compression: Compression::Lz4,
            file_size: 0,
        }
    }

    /// The block format requires sorted keys, so the write order is part of correctness.
    #[test]
    fn property_names_are_in_bytewise_order() {
        let mut sorted = name::ALL;
        sorted.sort_unstable();
        assert_eq!(name::ALL, sorted, "property names are written out of order");
        let unique: std::collections::BTreeSet<&[u8]> = name::ALL.into_iter().collect();
        assert_eq!(unique.len(), name::ALL.len(), "duplicate property name");
    }

    #[test]
    fn round_trips() {
        let props = sample();
        let bytes = props.encode().unwrap();
        let decoded = TableProperties::decode(Arc::from(bytes.into_boxed_slice())).unwrap();
        // `file_size` is not carried in the block; everything else is.
        assert_eq!(
            decoded,
            TableProperties {
                file_size: 0,
                ..props
            }
        );
    }

    /// Keys can hold any bytes, including none; the properties block must not assume text.
    #[test]
    fn empty_and_binary_keys_round_trip() {
        let props = TableProperties {
            smallest_key: Vec::new(),
            largest_key: vec![0xff; 300],
            prefix_extractor_name: None,
            compression: Compression::None,
            ..TableProperties::default()
        };
        let bytes = props.encode().unwrap();
        let decoded = TableProperties::decode(Arc::from(bytes.into_boxed_slice())).unwrap();
        assert_eq!(decoded, props);
        assert_eq!(decoded.prefix_extractor_name, None);
    }

    /// A property block from a newer build carries names this one does not know. Skipping them
    /// is what makes the format extensible, so it is pinned here.
    #[test]
    fn unknown_properties_are_ignored() {
        use super::super::block::BlockBuilder;
        let mut builder = BlockBuilder::new(1);
        // Sorted: "esker.entry_count" < "esker.zzz.from_the_future".
        let mut count = Vec::new();
        esker_base::varint::put_u64(42, &mut count);
        builder.add(name::ENTRY_COUNT, &count).unwrap();
        builder
            .add(b"esker.zzz.from_the_future", b"whatever this means")
            .unwrap();
        let bytes = builder.finish().to_vec();

        let decoded = TableProperties::decode(Arc::from(bytes.into_boxed_slice())).unwrap();
        assert_eq!(decoded.entry_count, 42);
        assert_eq!(
            decoded.comparator_name, "",
            "absent names keep their default"
        );
    }

    /// Values that cannot mean what their name promises are corruption, not defaults.
    #[test]
    fn malformed_values_are_corruption() {
        use super::super::block::BlockBuilder;

        let mut builder = BlockBuilder::new(1);
        builder.add(name::ENTRY_COUNT, &[0x80]).unwrap(); // truncated varint
        let bytes = builder.finish().to_vec();
        assert!(TableProperties::decode(Arc::from(bytes.into_boxed_slice())).is_err());

        let mut builder = BlockBuilder::new(1);
        builder.add(name::COMPARATOR, &[0xff, 0xfe]).unwrap(); // not utf-8
        let bytes = builder.finish().to_vec();
        assert!(TableProperties::decode(Arc::from(bytes.into_boxed_slice())).is_err());

        let mut builder = BlockBuilder::new(1);
        let mut code = Vec::new();
        esker_base::varint::put_u64(9, &mut code);
        builder.add(name::COMPRESSION, &code).unwrap(); // no such codec
        let bytes = builder.finish().to_vec();
        assert!(TableProperties::decode(Arc::from(bytes.into_boxed_slice())).is_err());
    }
}
