//! Knobs, and the two traits a column family is configured with.
//!
//! Every number in [`defaults`] comes from the one table in `docs/DESIGN.md` §14. They live
//! here as constants rather than as literals at their use sites so that the design document
//! and the code can be checked against each other by reading two screens, not twenty.
//!
//! The full `Options` / `CfOptions` / `ReadOptions` structures arrive with the `Db` itself
//! (step 6). What is here is what both lanes of phase 1 need to agree on now: the compression
//! codes that go on disk, the prefix extractor a bloom filter is built over, and the write
//! options that decide when a write is durable.

use std::fmt;
use std::time::Duration;

/// The per-block compression codec.
///
/// The code is a byte in every block trailer (`docs/DESIGN.md` §4.5), so these discriminants
/// are frozen. `lz4_flex` is the only codec the project will ever have: zstd and snappy are
/// banned because they are C (`docs/adr/0003-dependencies.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Compression {
    /// Store the block as it is.
    None = 0,
    /// LZ4 block format, via `lz4_flex`. The default.
    #[default]
    Lz4 = 1,
}

impl Compression {
    /// The on-disk code.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes a code. An unknown one is `None`: a corrupt trailer, or a file from a newer
    /// format. Never a panic (invariant 9).
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            _ => None,
        }
    }
}

/// Maps a key to the prefix its bloom filter is built over.
///
/// A versioned column family stores `user_key ++ enc_ts` (`docs/DESIGN.md` §3), so a filter
/// built over whole keys answers nothing useful — every version is a different key. Built over
/// the prefix, one filter answers "does this user key exist at any version", which is the
/// question a seek actually asks.
///
/// The engine stays byte-opaque: an extractor is a function on bytes, and what those bytes
/// mean is the caller's business (invariant 7).
pub trait PrefixExtractor: Send + Sync + fmt::Debug {
    /// The prefix of `key`. Only meaningful when [`in_domain`](PrefixExtractor::in_domain)
    /// holds; out of domain, implementations return the key unchanged.
    fn prefix<'a>(&self, key: &'a [u8]) -> &'a [u8];

    /// Whether `key` has a prefix at all. Keys out of the domain are not put in the filter and
    /// are not filtered by it.
    fn in_domain(&self, key: &[u8]) -> bool;

    /// A stable identifier recorded in SST properties, so a reader can tell whether a filter
    /// it finds was built over the prefixes it is about to query.
    fn name(&self) -> &str;
}

/// Strips a fixed-length suffix: the extractor for every MVCC column family, where the suffix
/// is the 8-byte encoded timestamp of `docs/DESIGN.md` §3.
#[derive(Debug, Clone)]
pub struct StripSuffix {
    suffix_len: usize,
    name: String,
}

impl StripSuffix {
    /// An extractor that removes the last `suffix_len` bytes.
    pub fn new(suffix_len: usize) -> Self {
        Self {
            suffix_len,
            // The length is part of the identity: two extractors that strip different amounts
            // produce different filters, and a reader must be able to tell them apart.
            name: format!("esker.StripSuffix.{suffix_len}"),
        }
    }

    /// The number of bytes stripped.
    pub fn suffix_len(&self) -> usize {
        self.suffix_len
    }
}

impl PrefixExtractor for StripSuffix {
    fn prefix<'a>(&self, key: &'a [u8]) -> &'a [u8] {
        match key.len().checked_sub(self.suffix_len) {
            Some(n) => &key[..n],
            None => key,
        }
    }

    fn in_domain(&self, key: &[u8]) -> bool {
        key.len() >= self.suffix_len
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// When the engine syncs the write-ahead log on its own initiative.
///
/// Orthogonal to [`WriteOptions::sync`], which is a per-write demand that is always honoured.
/// This is the policy for writes that did not ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WalSyncMode {
    /// Sync once per group commit. Every acknowledged write is durable — invariant 1's
    /// default reading, and the default here.
    #[default]
    PerWrite,
    /// Sync in the background at this interval. Writes are acknowledged before their bytes
    /// are durable, so a crash can lose up to one interval of them.
    Interval(Duration),
    /// Never sync except when a write asks. The fastest and the least durable.
    Never,
}

/// Per-write options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Whether the write's WAL bytes must be durable before it is acknowledged.
    ///
    /// **Defaults to `true`.** `CLAUDE.md` invariant 1 says a write is acknowledged only once
    /// it is durable "unless the caller explicitly passed `sync = false`" — so the opt-out is
    /// explicit, and the default is the safe one. This is deliberately the opposite of
    /// `LevelDB`'s and `RocksDB`'s default.
    pub sync: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self { sync: true }
    }
}

impl WriteOptions {
    /// Durable before acknowledgement. Same as [`WriteOptions::default`].
    pub fn synced() -> Self {
        Self { sync: true }
    }

    /// Acknowledged before the bytes are durable. A deliberate, explicit trade.
    pub fn unsynced() -> Self {
        Self { sync: false }
    }
}

/// The defaults of `docs/DESIGN.md` §14, in one place.
///
/// A change here is a change to the design document in the same commit.
pub mod defaults {
    /// Bytes in one active memtable before it is made immutable and flushed.
    pub const WRITE_BUFFER_SIZE: usize = 64 * 1024 * 1024;

    /// Immutable memtables that trigger a write slowdown (`docs/DESIGN.md` §4.4).
    pub const MEMTABLE_SLOWDOWN: usize = 2;

    /// Immutable memtables that stop writes entirely.
    pub const MEMTABLE_STOP: usize = 4;

    /// Uncompressed size of an SST data block.
    pub const BLOCK_SIZE: usize = 4 * 1024;

    /// Entries between restart points in a data block.
    pub const RESTART_INTERVAL: usize = 16;

    /// Bloom filter bits per key.
    pub const BLOOM_BITS_PER_KEY: usize = 10;

    /// L0 files that trigger a compaction.
    pub const L0_COMPACTION_TRIGGER: usize = 4;

    /// L0 files at which writers are slowed down.
    pub const L0_SLOWDOWN: usize = 8;

    /// L0 files at which writers are stopped.
    pub const L0_STOP: usize = 12;

    /// Target total bytes of L1.
    pub const MAX_BYTES_FOR_LEVEL_BASE: u64 = 64 * 1024 * 1024;

    /// Size ratio between consecutive levels.
    pub const MAX_BYTES_FOR_LEVEL_MULTIPLIER: u64 = 10;

    /// Number of levels, L0 included.
    pub const NUM_LEVELS: usize = 7;

    /// Block cache capacity in bytes.
    pub const BLOCK_CACHE_CAPACITY: usize = 256 * 1024 * 1024;

    /// Block cache shards. More shards, less contention, coarser eviction.
    pub const BLOCK_CACHE_SHARDS: usize = 8;

    /// Threads in the compaction pool.
    pub const COMPACTION_THREADS: usize = 2;

    /// Bytes a group-commit leader will drain from the queue before writing.
    pub const GROUP_COMMIT_MAX_BYTES: usize = 1024 * 1024;

    /// Batches a group-commit leader will drain before writing.
    pub const GROUP_COMMIT_MAX_BATCHES: usize = 128;
}

#[cfg(test)]
mod tests {
    use super::{Compression, PrefixExtractor, StripSuffix, WalSyncMode, WriteOptions, defaults};

    #[test]
    fn compression_codes_are_frozen() {
        assert_eq!(Compression::None.as_u8(), 0);
        assert_eq!(Compression::Lz4.as_u8(), 1);
        assert_eq!(Compression::from_u8(0), Some(Compression::None));
        assert_eq!(Compression::from_u8(1), Some(Compression::Lz4));
        for byte in 2..=u8::MAX {
            assert_eq!(Compression::from_u8(byte), None, "byte {byte}");
        }
        assert_eq!(Compression::default(), Compression::Lz4);
    }

    /// Invariant 1: durable-then-acknowledge is the default, and skipping it is explicit.
    #[test]
    fn writes_are_synced_unless_asked_otherwise() {
        assert!(WriteOptions::default().sync);
        assert!(!WriteOptions::unsynced().sync);
        assert_eq!(WalSyncMode::default(), WalSyncMode::PerWrite);
    }

    #[test]
    fn strip_suffix_removes_exactly_the_suffix() {
        let extractor = StripSuffix::new(8);
        let key = b"user-key\x00\x00\x00\x00\x00\x00\x00\x01";
        assert!(extractor.in_domain(key));
        assert_eq!(extractor.prefix(key), b"user-key");
        assert_eq!(extractor.name(), "esker.StripSuffix.8");
    }

    /// A short key has no version suffix to strip. It must be reported out of domain rather
    /// than silently yielding a nonsense prefix.
    #[test]
    fn short_keys_are_out_of_domain() {
        let extractor = StripSuffix::new(8);
        assert!(!extractor.in_domain(b"abc"));
        assert_eq!(extractor.prefix(b"abc"), b"abc");
        assert!(extractor.in_domain(b"12345678"));
        assert_eq!(extractor.prefix(b"12345678"), b"");
    }

    /// Two extractors that strip different lengths build different filters, so a reader has
    /// to be able to tell them apart by name alone.
    #[test]
    fn extractor_names_carry_the_length() {
        assert_ne!(StripSuffix::new(8).name(), StripSuffix::new(4).name());
    }

    /// These are the numbers `docs/DESIGN.md` §14 promises. If one changes, the table does.
    #[test]
    fn defaults_match_the_design_document() {
        assert_eq!(defaults::WRITE_BUFFER_SIZE, 64 << 20);
        assert_eq!(defaults::MEMTABLE_STOP, 4);
        assert_eq!(defaults::BLOCK_SIZE, 4 << 10);
        assert_eq!(defaults::RESTART_INTERVAL, 16);
        assert_eq!(defaults::BLOOM_BITS_PER_KEY, 10);
        assert_eq!(defaults::L0_COMPACTION_TRIGGER, 4);
        assert_eq!(defaults::L0_SLOWDOWN, 8);
        assert_eq!(defaults::L0_STOP, 12);
        assert_eq!(defaults::MAX_BYTES_FOR_LEVEL_BASE, 64 << 20);
        assert_eq!(defaults::MAX_BYTES_FOR_LEVEL_MULTIPLIER, 10);
        assert_eq!(defaults::NUM_LEVELS, 7);
        assert_eq!(defaults::BLOCK_CACHE_CAPACITY, 256 << 20);
        assert_eq!(defaults::BLOCK_CACHE_SHARDS, 8);
        assert_eq!(defaults::COMPACTION_THREADS, 2);
        assert_eq!(defaults::GROUP_COMMIT_MAX_BYTES, 1 << 20);
        assert_eq!(defaults::GROUP_COMMIT_MAX_BATCHES, 128);
        // The stall thresholds have to be ordered for the policy to mean anything.
        assert!(defaults::MEMTABLE_SLOWDOWN < defaults::MEMTABLE_STOP);
        assert!(defaults::L0_COMPACTION_TRIGGER < defaults::L0_SLOWDOWN);
        assert!(defaults::L0_SLOWDOWN < defaults::L0_STOP);
    }
}
