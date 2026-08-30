//! The log-structured storage engine: a write-ahead log, memtables, sorted string tables, a
//! manifest and compaction, all over byte-opaque keys ordered by a pluggable comparator.
//! One directory is one `Db` with many column families sharing a single WAL and one
//! sequence-number space (`docs/DESIGN.md` §4).
//!
//! # Invariants
//!
//! * **Log before state, fsync before ack.** A write is acknowledged only once its WAL bytes
//!   are durable, unless the caller passed `sync = false`. These steps are never reordered
//!   (`CLAUDE.md` invariant 1).
//! * **Every on-disk byte is checksummed** with [`crc32c`], and every file carries a magic
//!   number and a format version. Corruption is an error value, never a panic and never a
//!   silent skip (invariant 2).
//! * **Immutable files, atomic pointers.** SSTs and closed WAL segments are never modified in
//!   place; the only mutable pointer is `CURRENT`, replaced by fsync-then-rename (invariant 3).
//! * **Byte-opaque.** The engine never interprets a key. Tenants, tables and MVCC suffixes
//!   live in `esker-keys` and above, so nothing here may depend on that crate (invariant 7).
//! * This is the one crate where `unsafe` is a warning rather than an error, for the arena
//!   allocator and intrinsics we expect to need. Each site still needs a `// SAFETY:` comment
//!   and a test (invariant 8).
//!
//! # Module map
//!
//! The seams come first, because everything else is written against them.
//!
//! | Module | What it decides |
//! |---|---|
//! | [`error`] | the one error type; corruption is a value, never a panic |
//! | [`batch`] | the unit of atomicity, and its own serialisation |
//! | [`fs`] | every file touch, so the engine can be faulted, simulated and later tiered |
//! | [`dbformat`] | internal keys, entry kinds, and the comparator seam |
//! | [`cache_api`] | the block cache shape the read path is written against |
//! | [`options`] | knobs, prefix extraction, compression, and the §14 defaults |
//! | [`mod@format`] | byte sizes that are frozen |
//! | [`iterator`] | the cursor shape every layer iterates through |
//! | [`memfs`] | an in-memory filesystem, so damage can be injected without a disk |
//! | [`memtable`] | the sorted in-memory table every write lands in |
//! | [`filename`] | every file name, derived from a number that is never reused |
//! | [`wal`] | the write-ahead log: durability, and torn tails told from corruption |
//! | [`version`] | which files make up the database, and the manifest that records it |
//! | [`db`] | the database itself: open, group commit, reads, snapshots |
//! | [`compaction`] | moving data down the levels, and dropping what nothing can see |
//! | [`sst`], [`cache`] | the table format and the sharded LRU behind it |

#![warn(unsafe_code)]
// The engine's iterators are seekable cursors, not Rust `Iterator`s: `next()` yields nothing,
// and they must also go backwards and seek to an arbitrary key (`docs/DESIGN.md` §4.1). They
// are still called `iter()`, because that is what every caller and every LevelDB-shaped
// engine calls them.
#![allow(clippy::iter_not_returning_iterator)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod batch;
pub mod cache;
pub mod cache_api;
pub mod compaction;
pub mod db;
pub mod dbformat;
pub mod error;
pub mod filename;
pub mod fs;
pub mod iterator;
pub mod memfs;
pub mod memtable;
pub mod options;
pub mod range_del;
pub mod sst;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod version;
pub mod wal;

pub use batch::WriteBatch;
pub use cache_api::{BlockCache, CacheKey};
pub use db::checkpoint::CheckpointRange;
pub use db::iter::DbIterator;
pub use db::{ColumnFamily, Db, Snapshot};
pub use dbformat::{
    BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, MAX_SEQNO, SeqNo,
};
pub use error::{Error, Result};
pub use fs::{FileSystem, LocalFileSystem, RandomAccessFile, WritableFile};
pub use iterator::Cursor;
pub use memtable::MemTable;
pub use options::{
    CfOptions, Compression, Options, PrefixExtractor, ReadOptions, WalSyncMode, WriteOptions,
};
pub use version::{FileMeta, VersionEdit, VersionSet};
pub use wal::{LogReader, LogWriter, ReadOutcome};

/// The checksum every engine format uses, re-exported so callers can write
/// `esker_engine::crc32c::checksum(..)` as `docs/DESIGN.md` §4.5 describes. The
/// implementation lives in `esker-base` because `esker-proto` needs it too.
pub use esker_base::crc32c;

/// Names of the column families every store creates at bootstrap (`docs/DESIGN.md` §4.8).
pub mod cf {
    /// User data, and the long values of transactions (`docs/DESIGN.md` §8).
    pub const DEFAULT: &str = "default";
    /// Percolator locks, one live entry per locked key.
    pub const LOCK: &str = "lock";
    /// Percolator commit records, keyed by user key and commit timestamp.
    pub const WRITE: &str = "write";
    /// Raft logs, hard state and region metadata (`docs/DESIGN.md` §6).
    pub const RAFT: &str = "raft";

    /// All built-in column families, in creation order.
    pub const BUILTIN: [&str; 4] = [DEFAULT, LOCK, WRITE, RAFT];
}

/// Byte layouts that are frozen. Changing any of these is a format change: it needs an ADR,
/// a format-version bump and a migration story (`docs/DESIGN.md` §4).
pub mod format {
    /// Size of one write-ahead-log block. Records are split to never straddle a block.
    pub const WAL_BLOCK_SIZE: usize = 32 * 1024;

    /// `crc32c:u32 ++ len:u16 ++ type:u8`. A block tail shorter than this is zero-filled.
    pub const WAL_HEADER_SIZE: usize = 7;

    /// Trailing magic of a sorted string table: the ASCII bytes `ESKERSST1`.
    pub const SST_MAGIC: [u8; 9] = *b"ESKERSST1";

    /// Fixed footer size: index handle, filter handle, properties handle, version, magic.
    pub const SST_FOOTER_SIZE: usize = 48;

    /// Version of the SST layout this build reads and writes.
    pub const SST_FORMAT_VERSION: u32 = 1;

    /// Every block ends with `compression_type:u8 ++ crc32c:u32`.
    pub const BLOCK_TRAILER_SIZE: usize = 5;
}

#[cfg(test)]
mod tests {
    use super::{cf, format};

    #[test]
    fn column_family_names_are_distinct() {
        let unique: std::collections::BTreeSet<&str> = cf::BUILTIN.into_iter().collect();
        assert_eq!(
            unique.len(),
            cf::BUILTIN.len(),
            "two built-in column families share a name"
        );
        assert!(unique.iter().all(|name| !name.is_empty()));
    }

    /// The magic is written into every SST footer, so its bytes are part of the format.
    #[test]
    fn sst_magic_is_frozen() {
        assert_eq!(&format::SST_MAGIC, b"ESKERSST1");
        assert_eq!(format::SST_MAGIC.len(), 9);
        assert!(format::SST_FOOTER_SIZE > format::SST_MAGIC.len());
    }

    /// A WAL record needs its header plus at least one payload byte, so a block has to be
    /// meaningfully larger than a header for the format to make sense.
    #[test]
    fn wal_block_can_hold_records() {
        assert!(format::WAL_BLOCK_SIZE > format::WAL_HEADER_SIZE * 16);
        assert_eq!(format::WAL_BLOCK_SIZE % 1024, 0);
    }

    /// The re-export is what keeps `docs/DESIGN.md` §4.5 true; if it disappears the design
    /// document and the code have drifted.
    #[test]
    fn crc32c_is_reachable_through_the_engine() {
        assert_eq!(crate::crc32c::checksum(b"123456789"), 0xE306_9283);
    }
}
