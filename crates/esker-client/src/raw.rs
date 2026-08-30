//! `RawClient`: the `RawKv` API application code calls (`docs/DESIGN.md` §10).
//!
//! One method per `RawKv` verb over a [`Router`], which is where routing, retries, backoff and
//! the rule that protects writes live — shared with [`crate::txn`] rather than written twice.
//!
//! # Keys are never namespaced here
//!
//! The `'r'` prefix of `docs/DESIGN.md` §3 is the *store's* job, on every path including scan
//! bounds and `DeleteRange`. A client that added it too would double-prefix, and the damage
//! would not show up until a scan came back full of keys nobody wrote.

use std::sync::Arc;

use bytes::Bytes;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::region_cache::{RegionCache, RegionResolver};
use crate::router::Router;
use crate::transport::StoreTransport;
use crate::wire::{Body, DEFAULT_SCAN_LIMIT, Method, RawKvReq, RawKvResp};

pub use crate::router::{ClientOptions, MAX_IN_FLIGHT, MAX_SCAN_LIMIT};

/// The `RawKv` client: a region cache, bounded retries, and one method per `RawKv` verb.
#[derive(Debug)]
pub struct RawClient {
    router: Router,
}

impl RawClient {
    /// A client with the default options, the real clock, and one region.
    #[must_use]
    pub fn new(transport: Arc<dyn StoreTransport>, resolver: Arc<dyn RegionResolver>) -> Self {
        Self::with_options(transport, resolver, ClientOptions::default())
    }

    /// A client configured explicitly.
    #[must_use]
    pub fn with_options(
        transport: Arc<dyn StoreTransport>,
        resolver: Arc<dyn RegionResolver>,
        options: ClientOptions,
    ) -> Self {
        Self {
            router: Router::with_options(transport, resolver, options),
        }
    }

    /// Replaces the clock. Tests hand it a [`crate::clock::FakeClock`] so a full retry budget
    /// runs in microseconds and the backoff sequence can be asserted exactly.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.router = self.router.with_clock(clock);
        self
    }

    /// The region cache, for inspection.
    #[must_use]
    pub fn cache(&self) -> &RegionCache {
        self.router.cache()
    }

    /// The options this client was built with.
    #[must_use]
    pub fn options(&self) -> &ClientOptions {
        self.router.options()
    }

    /// The routing and retry machinery underneath.
    #[must_use]
    pub fn router(&self) -> &Router {
        &self.router
    }

    // -- the RawKv surface ---------------------------------------------------------------

    /// Reads one key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        match self.call(&RawKvReq::get(Bytes::copy_from_slice(key)))? {
            RawKvResp::Get { value } => Ok(value),
            other => Err(unexpected(Method::RawGet, &other)),
        }
    }

    /// Reads several keys, answered in the order they were asked.
    pub fn batch_get<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<Vec<Option<Bytes>>> {
        let keys: Vec<Bytes> = keys
            .iter()
            .map(|key| Bytes::copy_from_slice(key.as_ref()))
            .collect();
        match self.call(&RawKvReq::BatchGet { keys })? {
            RawKvResp::BatchGet { values } => Ok(values),
            other => Err(unexpected(Method::RawBatchGet, &other)),
        }
    }

    /// Writes one key, durably.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_with(key, value, true)
    }

    /// Writes one key, waiting for durability only when `sync`.
    ///
    /// `sync = false` is the caller explicitly accepting that an acknowledged write may be
    /// lost by a crash — `CLAUDE.md` invariant 1 makes that an opt-out, never a default.
    pub fn put_with(&self, key: &[u8], value: &[u8], sync: bool) -> Result<()> {
        let request = RawKvReq::Put {
            key: Bytes::copy_from_slice(key),
            value: Bytes::copy_from_slice(value),
            sync,
        };
        match self.call(&request)? {
            RawKvResp::Put => Ok(()),
            other => Err(unexpected(Method::RawPut, &other)),
        }
    }

    /// Writes several keys atomically — one engine write batch.
    pub fn batch_put(&self, pairs: Vec<(Bytes, Bytes)>) -> Result<()> {
        self.batch_put_with(pairs, true)
    }

    /// [`RawClient::batch_put`], with durability under the caller's control.
    pub fn batch_put_with(&self, pairs: Vec<(Bytes, Bytes)>, sync: bool) -> Result<()> {
        match self.call(&RawKvReq::BatchPut { pairs, sync })? {
            RawKvResp::BatchPut => Ok(()),
            other => Err(unexpected(Method::RawBatchPut, &other)),
        }
    }

    /// Removes one key. Removing an absent key is not an error.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.delete_with(key, true)
    }

    /// [`RawClient::delete`], with durability under the caller's control.
    pub fn delete_with(&self, key: &[u8], sync: bool) -> Result<()> {
        let request = RawKvReq::Delete {
            key: Bytes::copy_from_slice(key),
            sync,
        };
        match self.call(&request)? {
            RawKvResp::Delete => Ok(()),
            other => Err(unexpected(Method::RawDelete, &other)),
        }
    }

    /// Removes everything in `[start, end)`; an empty `end` means the end of the key space.
    /// Answers with how many keys went.
    pub fn delete_range(&self, start: &[u8], end: &[u8]) -> Result<u64> {
        let request = RawKvReq::DeleteRange {
            start: Bytes::copy_from_slice(start),
            end: Bytes::copy_from_slice(end),
            sync: true,
        };
        match self.call(&request)? {
            RawKvResp::DeleteRange { deleted } => Ok(deleted),
            other => Err(unexpected(Method::RawDeleteRange, &other)),
        }
    }

    /// Reads `[start, end)` in key order, at most `limit` pairs.
    ///
    /// A `limit` of zero means the protocol's default rather than "unlimited"; anything above
    /// [`ClientOptions::max_scan_limit`] is capped, because a scan has to fit in one frame.
    pub fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        self.scan_with(start, end, limit, false)
    }

    /// [`RawClient::scan`], walking from the high end of the range towards the low one.
    pub fn scan_reverse(
        &self,
        start: &[u8],
        end: &[u8],
        limit: u32,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        self.scan_with(start, end, limit, true)
    }

    fn scan_with(
        &self,
        start: &[u8],
        end: &[u8],
        limit: u32,
        reverse: bool,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        let request = RawKvReq::Scan {
            start: Bytes::copy_from_slice(start),
            end: Bytes::copy_from_slice(end),
            limit: self.bounded_limit(limit),
            reverse,
        };
        match self.call(&request)? {
            RawKvResp::Scan { pairs } => Ok(pairs),
            other => Err(unexpected(Method::RawScan, &other)),
        }
    }

    /// Writes `value` only if the key currently holds `expected`; `None` means "absent".
    ///
    /// Answers `(swapped, previous)`. This is the one method a retry could change the meaning
    /// of, which is why an ambiguous failure of it is never re-sent — see
    /// [`crate::wire::is_idempotent`].
    pub fn compare_and_swap(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<(bool, Option<Bytes>)> {
        let request = RawKvReq::CompareAndSwap {
            key: Bytes::copy_from_slice(key),
            expected: expected.map(Bytes::copy_from_slice),
            value: value.map(Bytes::copy_from_slice),
            sync: true,
        };
        match self.call(&request)? {
            RawKvResp::CompareAndSwap { swapped, previous } => Ok((swapped, previous)),
            other => Err(unexpected(Method::RawCompareAndSwap, &other)),
        }
    }

    /// A scan limit the transport can actually answer.
    fn bounded_limit(&self, limit: u32) -> u32 {
        self.router.bounded_limit(limit, DEFAULT_SCAN_LIMIT)
    }

    /// Sends one request, retrying redirectable refusals until the budget or the deadline runs
    /// out ([`Router::call`]).
    ///
    /// Takes the request by reference because a retry re-sends it: the loop clones it per
    /// attempt rather than consuming it, so the caller's copy is still there to log.
    pub fn call(&self, request: &RawKvReq) -> Result<RawKvResp> {
        let method = request.method();
        match self.router.call(&Body::Raw(request.clone()))? {
            crate::wire::Response::RawKv(response) => Ok(response),
            other => Err(Error::UnexpectedResponse {
                expected: method,
                actual: other.method(),
            }),
        }
    }
}

fn unexpected(expected: Method, response: &RawKvResp) -> Error {
    Error::UnexpectedResponse {
        expected,
        actual: response.method(),
    }
}
