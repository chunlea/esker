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
use crate::router::{Router, clamp_end, repair_route};
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

    /// Walks the regions of `[start, end)`, asking each only for the keys it holds.
    ///
    /// **This sent one request for the whole range**, which a store refuses the moment the range
    /// leaves its region — `RegionMeta::check_range` requires containment, and invariant 5 is why
    /// it is right to. `Transaction::scan` was fixed for that first, because SQL reads through it
    /// and a table past the split threshold made every non-point `SELECT` answer
    /// `08006 key is not in region 1`; this is the same defect on the `RawKV` path, found while
    /// fixing that one and unreachable only because nothing scans large `RawKV` ranges today.
    ///
    /// The two directions differ in one way and it is not the clamp. A forward scan can walk
    /// lazily — ask, take the region's end, carry on from it — because the next region is found
    /// with a key it already has. A **reverse** scan needs the *last* region first, and routing
    /// only answers "who holds this key", so there is no key to ask with when `end` is empty. It
    /// therefore enumerates the regions forward and visits them backwards, which costs the walk up
    /// front and is the only order that can be right.
    fn scan_with(
        &self,
        start: &[u8],
        end: &[u8],
        limit: u32,
        reverse: bool,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        let limit = self.bounded_limit(limit);
        let mut pairs = Vec::new();
        for (from, to) in self.regions_of(start, end, reverse)? {
            if pairs.len() >= limit as usize {
                break;
            }
            pairs.extend(self.scan_region(&from, &to, limit, reverse)?);
        }
        pairs.truncate(limit as usize);
        Ok(pairs)
    }

    /// The `[from, to)` pieces of `[start, end)`, one per region, in the order the scan visits them.
    fn regions_of(&self, start: &[u8], end: &[u8], reverse: bool) -> Result<Vec<(Bytes, Bytes)>> {
        let mut pieces = Vec::new();
        let mut cursor = Bytes::copy_from_slice(start);
        for _ in 0..crate::txn::MAX_SCAN_REGIONS {
            // **The enumeration needs the repair too.** `Router::route` answers its own
            // `KeyNotInRegion` with `region_id == 0` when the driver says no region covers the key
            // — which under load is a fact about the driver being a heartbeat behind, not about the
            // cluster. That is the `08006 … key is not in region 0` a loaded gate produced while
            // the same test passed alone.
            let boundary = match self.router.route(&cursor) {
                Ok(route) => route.region.end_key,
                Err(refusal) => repair_route(&self.router, &cursor, &refusal)?,
            };
            let piece_end = clamp_end(end, &boundary);
            pieces.push((cursor.clone(), piece_end));
            if boundary.is_empty()
                || (!end.is_empty() && boundary.as_ref() >= end)
                || boundary <= cursor
            {
                break;
            }
            cursor = boundary;
        }
        if reverse {
            pieces.reverse();
        }
        Ok(pieces)
    }

    /// One region's worth, repairing a boundary the region has moved under.
    ///
    /// The refusal names the range the store actually owns, and that is believed over the region
    /// cache for the reason `Transaction::scan_region` records: a store knows about its own split
    /// at once and the placement driver at the next heartbeat, so re-asking the driver inside that
    /// window returns the same stale boundary.
    fn scan_region(
        &self,
        from: &Bytes,
        to: &Bytes,
        limit: u32,
        reverse: bool,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        let mut to = to.clone();
        let mut refreshes = 0;
        loop {
            let request = RawKvReq::Scan {
                start: from.clone(),
                end: to.clone(),
                limit,
                reverse,
            };
            match self.call(&request) {
                Ok(RawKvResp::Scan { pairs }) => return Ok(pairs),
                Ok(other) => return Err(unexpected(Method::RawScan, &other)),
                Err(Error::Store(refusal))
                    if matches!(refusal, crate::wire::ProtoError::KeyNotInRegion { .. })
                        && refreshes < crate::txn::SCAN_ROUTE_REFRESHES =>
                {
                    refreshes += 1;
                    // The same repair the transactional scan uses, and the same one the fragment
                    // dispatch must: one place, so the ordering of "believe the store" against
                    // "ask the driver" cannot differ between them.
                    to = clamp_end(&to, &repair_route(&self.router, from, &refusal)?);
                }
                Err(other) => return Err(other),
            }
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
