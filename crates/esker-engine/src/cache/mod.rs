//! The sharded LRU block cache (`docs/DESIGN.md` §4.9).
//!
//! It implements [`BlockCache`](crate::cache_api::BlockCache), declared in
//! [`crate::cache_api`], keyed by [`CacheKey`](crate::cache_api::CacheKey). Capacity and shard
//! count default to [`BLOCK_CACHE_CAPACITY`](crate::options::defaults::BLOCK_CACHE_CAPACITY)
//! and [`BLOCK_CACHE_SHARDS`](crate::options::defaults::BLOCK_CACHE_SHARDS). No `lru` crate:
//! the doubly-linked list and hash map are written here (`CLAUDE.md`, "Dependency policy"),
//! over indices rather than pointers so that none of it needs `unsafe`.
//!
//! What is cached is the *uncompressed* block. Decompression happens once, on the read that
//! misses; every later hit is a pointer clone.

pub mod lru;

pub use lru::{CacheStats, SHARDS, ShardedLruCache};
