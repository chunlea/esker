//! The block cache seam.
//!
//! The cache is declared here and implemented in `src/cache/` so that the read path depends on
//! a shape rather than on an implementation: a table reader takes an `Option<Arc<dyn
//! BlockCache>>` and works identically with a real sharded LRU, with a stub in a unit test, and
//! with nothing at all.
//!
//! The key is `(file_number, offset)` and not a path (`docs/DESIGN.md` §4.9). File numbers are
//! never reused, and SSTs are immutable, so a cached block can never be stale — there is no
//! invalidation problem to have. That is the whole reason the engine numbers its files.

use std::fmt;
use std::sync::Arc;

/// Identifies one block: which file, and where in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheKey {
    /// The engine-assigned file number. Unique for the life of the database.
    pub file_number: u64,
    /// The block's byte offset within that file.
    pub offset: u64,
}

impl CacheKey {
    /// A key for the block at `offset` of `file_number`.
    pub fn new(file_number: u64, offset: u64) -> Self {
        Self {
            file_number,
            offset,
        }
    }
}

/// A cache of decompressed data blocks, shared by every reader.
///
/// Implementations take `&self` for both operations: the cache is behind an `Arc` and used
/// concurrently by foreground reads and by compaction, so its own locking is internal.
pub trait BlockCache: Send + Sync + fmt::Debug {
    /// Stores `block` under `key`, charging `charge` bytes against the capacity. Inserting a
    /// key that is already present is allowed and must not corrupt the accounting.
    fn insert(&self, key: CacheKey, block: Arc<[u8]>, charge: usize);

    /// Returns the cached block, counting as a use for eviction purposes.
    fn lookup(&self, key: &CacheKey) -> Option<Arc<[u8]>>;
}

#[cfg(test)]
mod tests {
    use super::CacheKey;

    /// Blocks from different files must never collide, which is the point of keying by file
    /// number rather than by offset alone.
    #[test]
    fn keys_are_distinct_per_file_and_offset() {
        let a = CacheKey::new(1, 0);
        let b = CacheKey::new(2, 0);
        let c = CacheKey::new(1, 4096);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, CacheKey::new(1, 0));
    }
}
