//! `RawClient`: the `RawKv` API application code calls (`docs/DESIGN.md` §10).
//!
//! Everything the client does around a call is here, and all of it is ordinary synchronous
//! code — no sockets, no wall clock, no threads of its own. Bytes leave through
//! [`Transport`], time enters through [`Clock`], and both are injected, which is why every
//! rule below is tested against a script rather than a cluster.
//!
//! # What one call does
//!
//! 1. Refuse it if it could not fit in a frame — better a typed error here than a connection
//!    torn down at the far end.
//! 2. Take a permit, so the number of calls in flight is bounded.
//! 3. Route: ask the region cache, and on a miss the [`RegionResolver`].
//! 4. Send, with the region's epoch and the believed leader in the header.
//! 5. On a redirectable refusal, repair the cache, back off with jitter, and go to 3 —
//!    bounded by both a retry budget and a deadline, whichever ends first.
//!
//! # The rule that protects writes
//!
//! Only errors `esker-proto` marks retryable are retried, and every one of them is a
//! *refusal*: [`ProtoError::outcome`] answers `NotApplied`, so the store provably did not
//! change anything and re-sending is safe for a write as well as a read.
//!
//! The dangerous case is the one that is not in that set. When a request goes out and no
//! usable answer comes back, the write may be in the log and nobody can tell. This client
//! does **not** re-send it: a mutation whose outcome is `Unknown` becomes
//! [`Error::AmbiguousResult`], and the caller decides whether to read the key back or to
//! fail. A read in the same situation is simply returned — re-reading is always safe, so
//! there is nothing ambiguous to report.
//!
//! # Keys are never namespaced here
//!
//! The `'r'` prefix of `docs/DESIGN.md` §3 is the *store's* job, on every path including scan
//! bounds and `DeleteRange`. A client that added it too would double-prefix, and the damage
//! would not show up until a scan came back full of keys nobody wrote.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::clock::{Clock, SystemClock};
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::region_cache::{RegionCache, RegionResolver, Route};
use crate::retry::{CALL_TIMEOUT_MS, Jitter, Redirect, RetryPolicy, Verdict, classify};
use crate::transport::Transport;
use crate::wire::{
    DEFAULT_SCAN_LIMIT, Method, ProtoError, RawKvReq, RawKvResp, Request, RequestHeader,
    RequestOutcome, payload_size, routing_key,
};

/// Most calls in flight at once, per client.
pub const MAX_IN_FLIGHT: usize = 256;

/// Most entries one scan may ask for, whatever the caller passed.
///
/// The server caps a scan too, but a client that asks for `u32::MAX` and is answered honestly
/// gets a response no frame can hold. Capping here turns that into a smaller answer rather
/// than a failed call.
pub const MAX_SCAN_LIMIT: u32 = 16_384;

/// How a [`RawClient`] behaves, all in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// Retries and their backoff schedule.
    pub retry: RetryPolicy,
    /// How long one call may take in total, retries and backoff included.
    pub call_timeout: Duration,
    /// Most calls in flight at once.
    pub max_in_flight: usize,
    /// Ceiling on a scan's `limit`.
    pub max_scan_limit: u32,
    /// Seed for the backoff jitter. `None` draws one per client, which is what production
    /// wants; a test sets it so the delays are reproducible.
    pub jitter_seed: Option<u64>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            retry: RetryPolicy::default(),
            call_timeout: Duration::from_millis(CALL_TIMEOUT_MS),
            max_in_flight: MAX_IN_FLIGHT,
            max_scan_limit: MAX_SCAN_LIMIT,
            jitter_seed: None,
        }
    }
}

/// The `RawKv` client: a region cache, bounded retries, and one method per `RawKv` verb.
#[derive(Debug)]
pub struct RawClient {
    transport: Arc<dyn Transport>,
    resolver: Arc<dyn RegionResolver>,
    clock: Arc<dyn Clock>,
    cache: RegionCache,
    jitter: Jitter,
    gate: Gate,
    options: ClientOptions,
}

impl RawClient {
    /// A client with the default options, the real clock, and one region.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>, resolver: Arc<dyn RegionResolver>) -> Self {
        Self::with_options(transport, resolver, ClientOptions::default())
    }

    /// A client configured explicitly.
    #[must_use]
    pub fn with_options(
        transport: Arc<dyn Transport>,
        resolver: Arc<dyn RegionResolver>,
        options: ClientOptions,
    ) -> Self {
        let jitter = match options.jitter_seed {
            Some(seed) => Jitter::seeded(seed),
            None => Jitter::from_entropy(),
        };
        Self {
            transport,
            resolver,
            clock: Arc::new(SystemClock),
            cache: RegionCache::new(),
            jitter,
            gate: Gate::new(options.max_in_flight),
            options,
        }
    }

    /// Replaces the clock. Tests hand it a [`crate::clock::FakeClock`] so a full retry budget
    /// runs in microseconds and the backoff sequence can be asserted exactly.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The region cache, for inspection.
    #[must_use]
    pub fn cache(&self) -> &RegionCache {
        &self.cache
    }

    /// The options this client was built with.
    #[must_use]
    pub fn options(&self) -> &ClientOptions {
        &self.options
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
        let limit = if limit == 0 {
            DEFAULT_SCAN_LIMIT
        } else {
            limit
        };
        limit.min(self.options.max_scan_limit.max(1))
    }

    // -- the call loop -------------------------------------------------------------------

    /// Sends one request, retrying redirectable refusals until the budget or the deadline
    /// runs out.
    ///
    /// Takes the request by reference because a retry re-sends it: the loop clones it per
    /// attempt rather than consuming it, so the caller's copy is still there to log.
    pub fn call(&self, request: &RawKvReq) -> Result<RawKvResp> {
        let method = request.method();
        let started = self.clock.now();
        let deadline = started + self.options.call_timeout;

        let limit = self.transport.max_frame_size();
        let size = payload_size(request);
        if size >= limit {
            return Err(Error::RequestTooLarge { bytes: size, limit });
        }

        let Some(_permit) = self.gate.acquire(self.options.call_timeout) else {
            return Err(Error::DeadlineExceeded {
                attempts: 0,
                source: None,
            });
        };

        let mut attempts: u32 = 0;
        loop {
            if self.clock.now() >= deadline {
                return Err(Error::DeadlineExceeded {
                    attempts,
                    source: None,
                });
            }

            let route = self.route(routing_key(request))?;
            let target = route.target().ok_or_else(|| Error::NoRegion {
                key: Bytes::copy_from_slice(routing_key(request)),
            })?;
            let wire = Request::raw_kv(
                RequestHeader::new(route.region.id, route.region.epoch, target.peer_id),
                request.clone(),
            );

            attempts += 1;
            let error = match self.transport.call(target.store_id, &wire, deadline) {
                Ok(response) if response.method() == method => return Ok(response),
                Ok(response) => return Err(unexpected(method, &response)),
                Err(error) => error,
            };

            match classify(&error) {
                Verdict::Surface => {
                    self.on_terminal(&error, route.region.id, routing_key(request));
                    return Err(terminal(error, method));
                }
                Verdict::Retry(redirect) => {
                    self.repair(&redirect, route.region.id);
                    if attempts > self.options.retry.max_retries {
                        return Err(Error::RetriesExhausted {
                            attempts,
                            source: Box::new(error),
                        });
                    }
                    let delay = self.jitter.apply(self.options.retry.backoff(attempts - 1));
                    // Sleeping past the deadline only delays the same answer, so stop now and
                    // say which failure the caller was waiting on.
                    if self.clock.now() + delay >= deadline {
                        return Err(Error::DeadlineExceeded {
                            attempts,
                            source: Some(Box::new(error)),
                        });
                    }
                    self.clock.sleep(delay);
                }
            }
        }
    }

    /// The cached route for `key`, or a fresh one from the resolver.
    fn route(&self, key: &[u8]) -> Result<Route> {
        if let Some(route) = self.cache.lookup(key) {
            return Ok(route);
        }
        // TODO(phase-4): this is `GetRegion(key)` over the wire to the placement driver; today
        // it answers from a constant. Nothing above it changes when that lands.
        let route = self.resolver.locate(key).ok_or_else(|| Error::NoRegion {
            key: Bytes::copy_from_slice(key),
        })?;
        self.cache.insert(route.clone());
        Ok(route)
    }

    /// Applies what a redirectable refusal said to fix.
    fn repair(&self, redirect: &Redirect, region_id: u64) {
        match redirect {
            Redirect::Leader { hint } => self.cache.set_leader(region_id, *hint),
            Redirect::Epoch { replacements } if replacements.is_empty() => {
                // No replacements offered, so there is nothing to learn from the error: drop
                // the entry and let the resolver answer again.
                self.cache.invalidate(region_id);
            }
            Redirect::Epoch { replacements } => {
                self.cache
                    .insert_all(replacements.iter().cloned().map(|region| Route {
                        region,
                        leader: None,
                    }));
            }
            Redirect::Refresh => self.cache.invalidate(region_id),
            Redirect::Busy => {}
        }
    }

    /// Drops a cache entry that a terminal error proved wrong.
    ///
    /// `KeyNotInRegion` is not retryable — waiting cannot fix a routing mistake — but it does
    /// prove the cached region is a lie, and leaving it in place would make the caller's next
    /// call fail the same way.
    fn on_terminal(&self, error: &ProtoError, region_id: u64, key: &[u8]) {
        if matches!(error, ProtoError::KeyNotInRegion { .. }) {
            self.cache.invalidate(region_id);
            self.cache.invalidate_key(key);
        }
    }
}

/// Turns a terminal protocol error into a client error, naming the ambiguous case.
///
/// A mutation whose outcome is `Unknown` is the case the whole retry story is built around:
/// it went out, no usable answer came back, and it may or may not be in the log. A read in
/// the same position is not ambiguous — nothing changed either way — so it is returned plainly
/// and the caller may simply ask again.
fn terminal(error: ProtoError, method: Method) -> Error {
    if method.is_mutation() && error.outcome() == RequestOutcome::Unknown {
        return Error::AmbiguousResult {
            method,
            source: Box::new(error),
        };
    }
    Error::Store(error)
}

fn unexpected(expected: Method, response: &RawKvResp) -> Error {
    Error::UnexpectedResponse {
        expected,
        actual: response.method(),
    }
}
