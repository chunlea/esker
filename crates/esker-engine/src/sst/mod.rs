//! Sorted string tables: block builder, index, bloom filter, properties and footer
//! (`docs/DESIGN.md` §4.5, format version 1).
//!
//! The contract with the rest of the engine, all of it declared outside this module:
//!
//! * keys and order — [`crate::dbformat`] ([`Comparator`](crate::dbformat::Comparator),
//!   [`InternalKeyComparator`](crate::dbformat::InternalKeyComparator))
//! * files — [`crate::fs`] ([`WritableFile`](crate::fs::WritableFile),
//!   [`RandomAccessFile`](crate::fs::RandomAccessFile))
//! * caching — [`crate::cache_api`] ([`BlockCache`](crate::cache_api::BlockCache),
//!   [`CacheKey`](crate::cache_api::CacheKey))
//! * knobs — [`crate::options`] ([`Compression`](crate::options::Compression),
//!   [`PrefixExtractor`](crate::options::PrefixExtractor), [`defaults`](crate::options::defaults))
//! * errors — [`crate::error`]
//! * frozen sizes — [`crate::format`]
//!
//! # File layout
//!
//! ```text
//! +--------------------------------+  offset 0
//! | data block 0     | trailer     |
//! | data block 1     | trailer     |
//! | ...                            |
//! +--------------------------------+
//! | filter block     | trailer     |   absent when bloom_bits_per_key == 0
//! +--------------------------------+
//! | properties block | trailer     |
//! +--------------------------------+
//! | index block      | trailer     |
//! +--------------------------------+
//! | footer, exactly 48 bytes       |   ends at end-of-file
//! +--------------------------------+
//! ```
//!
//! Every block is followed by a 5-byte trailer, `compression_type:u8 ++ crc32c:u32` (LE), and
//! the checksum covers the block payload **and** the type byte — so flipping a stored `Lz4`
//! code to `None` on disk is caught rather than decoded as garbage. A block handle names a
//! block by its `{offset, size}`, where `size` excludes the trailer.
//!
//! The footer is last so a reader can find it from the file size alone, and it is a fixed 48
//! bytes forever.
//!
//! # Keys are opaque
//!
//! Nothing here parses a key (`CLAUDE.md` invariant 7). Order comes only from the injected
//! [`Comparator`](crate::dbformat::Comparator); the prefix compression inside a block is
//! byte-level, never semantic; and the seqno range recorded in the properties is *given* to
//! the builder by the engine, because reading it out of a key would mean understanding one.
//!
//! # What the rest of the engine gets, from step 6 onwards
//!
//! `TableBuilder::new(TableOptions, Box<dyn WritableFile>)` with sorted `add(key, value)` and
//! `finish() -> TableProperties`, and `TableReader::open(Box<dyn RandomAccessFile>,
//! file_number, TableOptions, Option<Arc<dyn BlockCache>>)` with `get` and an iterator
//! supporting `seek`, `seek_for_prev`, `next` and `prev`.

pub mod block;
pub mod builder;
pub mod filter;
pub mod footer;
pub mod props;
pub mod reader;

pub use block::{Block, BlockBuilder, BlockIter};
pub use builder::{TableBuilder, TableOptions};
pub use filter::{BloomBuilder, BloomFilter, DEFAULT_BITS_PER_KEY, filter_key};
pub use footer::{BlockHandle, Footer, MAX_TABLE_SIZE};
pub use props::TableProperties;
pub use reader::{TableIter, TableReader};
