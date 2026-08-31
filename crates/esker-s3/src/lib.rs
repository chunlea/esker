//! A minimal S3 client, written here rather than taken from crates.io.
//!
//! Phase 6b puts SSTs in object storage (`docs/DESIGN.md` §13). That needs four calls —
//! `PutObject`, ranged `GetObject`, `ListObjectsV2`, `DeleteObject` — one signing algorithm,
//! and enough HTTP/1.1 to carry them. `CLAUDE.md` bans the AWS SDK, `hyper` and `reqwest`, and
//! the alternative is about a thousand lines, so it is here.
//!
//! # What this crate costs
//!
//! `esker-base`, `thiserror`, `tracing`. Nothing else, at any depth. That is deliberate: the
//! 40-crate budget in `deny.toml` is the resource TLS will need
//! ([ADR 0025](../../../docs/adr/0025-s3-transport-and-tls.md)), and spending it on an HTTP
//! client would be spending it on the easy half.
//!
//! # What it does not do
//!
//! No TLS — [`transport::TcpTransport`] speaks plain HTTP, [`Endpoint::parse`] refuses
//! `https://`, and ADR 0025 says when that changes and what has to be true first. No multipart
//! upload; an object larger than [`MAX_SINGLE_PUT`] is an error rather than a truncation. No
//! bucket lifecycle, versioning or encryption — those are bucket configuration and not a
//! client's business.
//!
//! # The shape
//!
//! [`ObjectStore`] is the trait the engine depends on, and it is deliberately smaller than S3:
//! four methods over opaque byte ranges, with no notion of a signature, a region or a status
//! code. [`S3Client`] implements it over [`transport::Transport`], and [`MemoryStore`]
//! implements it over a `BTreeMap` so that the engine's own tests need no container.

pub mod client;
pub mod error;
pub mod http;
pub mod memory;
pub mod sigv4;
pub mod transport;
mod xml;

pub use client::{Config, Endpoint, S3Client};
pub use error::{Error, Result};
pub use memory::MemoryStore;
pub use sigv4::Credentials;

use std::fmt;

/// The largest object this client will `PutObject` in one request.
///
/// S3's own limit for a single `PutObject` is 5 GiB; ours is lower because an SST defaults to
/// 8 MiB (`docs/DESIGN.md` §14) and a request body is held in memory. Anything approaching this
/// is a compaction configured far outside its defaults, and an error naming the limit is a
/// better outcome than a 5 GiB allocation.
pub const MAX_SINGLE_PUT: usize = 256 * 1024 * 1024;

/// What one `ListObjectsV2` page reports about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSummary {
    /// The object's key, relative to nothing — the full key as stored.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// The server's entity tag, quotes stripped. For a single-part upload this is the MD5 of
    /// the body, but nothing here relies on that: it is an opaque version marker, compared for
    /// equality and never computed.
    pub etag: String,
}

/// The bytes and the metadata one `GetObject` returned.
#[derive(Debug, Clone)]
pub struct GetResponse {
    /// The bytes read.
    pub body: Vec<u8>,
    /// The object's entity tag, if the server sent one.
    pub etag: Option<String>,
    /// The object's total size, when the response said — a `Content-Range` gives it, a plain
    /// `200` gives it as `Content-Length`.
    pub total_size: Option<u64>,
}

/// The object-storage operations SST tiering needs.
///
/// Smaller than S3 on purpose. The engine's tiered filesystem is written against this, so a
/// test can supply [`MemoryStore`] and a real deployment can supply [`S3Client`] without the
/// engine knowing which. Every method takes `&self`: one store is shared by the uploader
/// thread and every reader.
pub trait ObjectStore: Send + Sync + fmt::Debug {
    /// Stores `body` under `key`, replacing whatever was there.
    ///
    /// Must be idempotent: the uploader retries, and a retry of a `PutObject` whose response
    /// was lost has to be harmless. Returns the stored object's `ETag` when the server gives
    /// one, so a later ranged read can check it (ADR 0024 decision 6).
    fn put(&self, key: &str, body: &[u8]) -> Result<Option<String>>;

    /// Reads `len` bytes of `key` starting at `offset`.
    ///
    /// A range that runs past the end of the object returns what remains rather than an error,
    /// matching `RandomAccessFile::read_at`'s short read at EOF. `expected_etag`, when given,
    /// must match what the server reports or the read fails rather than returning bytes from
    /// some other version of the object.
    fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        expected_etag: Option<&str>,
    ) -> Result<GetResponse>;

    /// Reads the whole of `key`.
    fn get(&self, key: &str) -> Result<GetResponse>;

    /// Every object whose key starts with `prefix`, in the server's order, following
    /// continuation tokens until the listing is complete.
    fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>>;

    /// Removes `key`. Deleting something that is not there succeeds — S3 says so, and the
    /// sweep that calls this retries.
    fn delete(&self, key: &str) -> Result<()>;
}
