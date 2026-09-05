//! Routing, retries and backoff: everything a client does *around* a call, for both services.
//!
//! Extracted from `RawClient` when `TxnClient` arrived, because the two want exactly the same
//! five steps and a second copy of them would be a second place for the rules of
//! `docs/DESIGN.md` §10 to drift:
//!
//! 1. Refuse the request if it could not fit in a frame — better a typed error here than a
//!    connection torn down at the far end.
//! 2. Take a permit, so the number of calls in flight is bounded.
//! 3. Route: ask the region cache, and on a miss the [`RegionResolver`].
//! 4. Send, with the region's epoch and the believed leader in the header.
//! 5. On a redirectable refusal, repair the cache, back off with jitter, and go to 3 —
//!    bounded by both a retry budget and a deadline, whichever ends first.
//!
//! All of it is ordinary synchronous code with no sockets and no wall clock: bytes leave
//! through [`StoreTransport`] and time enters through [`Clock`], both injected, which is what
//! lets every rule be tested against a script rather than a cluster.
//!
//! # The rule that protects writes
//!
//! Only errors `esker-proto` marks retryable are retried, and every one of them is a
//! *refusal*: [`ProtoError::outcome`] answers `NotApplied`, so the store provably did not
//! change anything and re-sending is safe for a write as well as a read.
//!
//! The dangerous case is the one that is not in that set. When a request goes out and no
//! usable answer comes back, the write may be in the log and nobody can tell. This router does
//! **not** re-send it: a mutation whose outcome is `Unknown` becomes
//! [`Error::AmbiguousResult`], and the caller decides. A read in the same situation is simply
//! returned — re-reading is always safe.
//!
//! `esker-txn` is the caller that has an answer for that: a `Prewrite` which may or may not
//! have landed is **resolvable**, because the transaction's primary says which. See
//! [`crate::txn`].

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::clock::{Clock, SystemClock};
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::region_cache::{RegionCache, RegionResolver, Route};
use crate::retry::{
    CALL_TIMEOUT_MS, Jitter, Redirect, RetryPolicy, Verdict, classify, may_ask_again,
};
use crate::transport::StoreTransport;
use crate::wire::{Body, Epoch, Method, ProtoError, RequestHeader, RequestOutcome, Response};

/// Most calls in flight at once, per client.
pub const MAX_IN_FLIGHT: usize = 256;

/// Most entries one scan may ask for, whatever the caller passed.
///
/// The server caps a scan too, but a client that asks for `u32::MAX` and is answered honestly
/// gets a response no frame can hold. Capping here turns that into a smaller answer rather
/// than a failed call.
pub const MAX_SCAN_LIMIT: u32 = 16_384;

/// How a client behaves, all in one place.
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

/// The routing and retry machinery, shared by every client in this crate.
#[derive(Debug)]
pub struct Router {
    transport: Arc<dyn StoreTransport>,
    resolver: Arc<dyn RegionResolver>,
    clock: Arc<dyn Clock>,
    cache: RegionCache,
    jitter: Jitter,
    gate: Gate,
    options: ClientOptions,
}

impl Router {
    /// A router with the default options and the real clock.
    #[must_use]
    pub fn new(transport: Arc<dyn StoreTransport>, resolver: Arc<dyn RegionResolver>) -> Self {
        Self::with_options(transport, resolver, ClientOptions::default())
    }

    /// A router configured explicitly.
    #[must_use]
    pub fn with_options(
        transport: Arc<dyn StoreTransport>,
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

    /// The options this router was built with.
    #[must_use]
    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    /// The clock, for callers that measure their own deadlines against the same one.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Largest frame the transport underneath will carry.
    #[must_use]
    pub fn max_frame_size(&self) -> usize {
        self.transport.max_frame_size()
    }

    /// The transport, for a caller in this crate that addresses a **peer** rather than a region.
    ///
    /// [`Router::call`] sends to [`Route::target`], which is the believed *leader*; a fragment
    /// goes to a columnar learner, which is the one peer that is never it
    /// (`crates/esker-client/src/fragment.rs`). Everything else that call does — the cache, the
    /// resolver, the clock, the in-flight gate — is shared through the three accessors here
    /// rather than copied, so there is still one region cache and one bound on calls in flight.
    pub(crate) fn transport(&self) -> &Arc<dyn StoreTransport> {
        &self.transport
    }

    /// The in-flight bound, so a caller that does not go through [`Router::call`] is still
    /// counted by it.
    pub(crate) fn gate(&self) -> &Gate {
        &self.gate
    }

    /// The backoff jitter, so two loops in this crate draw from one generator and a seeded
    /// client stays reproducible whichever loop ran.
    pub(crate) fn jitter(&self) -> &Jitter {
        &self.jitter
    }

    /// A scan limit the transport can actually answer.
    #[must_use]
    pub fn bounded_limit(&self, limit: u32, default: u32) -> u32 {
        let limit = if limit == 0 { default } else { limit };
        limit.min(self.options.max_scan_limit.max(1))
    }

    /// The route for `key` as the cache believes it, without asking the resolver.
    ///
    /// Used to group a transaction's keys by region before sending anything. A miss is not an
    /// error here: the caller resolves it through an ordinary call, because a group of one is
    /// as correct as a group of ten and grouping is only ever an optimisation.
    #[must_use]
    pub fn cached_route(&self, key: &[u8]) -> Option<Route> {
        self.cache.lookup(key)
    }

    /// Sends one request, retrying redirectable refusals until the budget or the deadline runs
    /// out.
    ///
    /// Takes the body by reference because a retry re-sends it: the loop clones it per attempt
    /// rather than consuming it, so the caller's copy is still there to log.
    pub fn call(&self, body: &Body) -> Result<Response> {
        let method = body.method();
        let started = self.clock.now();
        let deadline = started + self.options.call_timeout;

        let limit = self.transport.max_frame_size();
        let size = body.payload_size();
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
        // Attempts **in a row** that taught this client nothing, which is what the retry budget
        // counts. See `learned_a_newer_epoch`: a refusal that hands back fresher routing than the
        // one it refused is progress, and spending a budget on progress is how a client gives up
        // on a call that was going to succeed.
        let mut fruitless: u32 = 0;
        // The region the last attempt was addressed to, so a refusal repairs the entry that
        // produced it. Zero until one is routed: a resolver that failed named no region, and
        // there is nothing cached to invalidate.
        let mut region_id: u64 = 0;
        // The epoch that attempt carried, so the repair that follows can be asked whether it
        // moved. `None` until one is routed, for the same reason `region_id` is zero.
        let mut sent_epoch: Option<Epoch> = None;
        loop {
            if self.clock.now() >= deadline {
                return Err(Error::DeadlineExceeded {
                    attempts,
                    source: None,
                });
            }

            attempts += 1;
            // A resolver failure is not a routing answer: the placement driver could not say,
            // which is usually momentary. It goes through the same classifier as a store's
            // refusal so that "retryable" is decided in one place, by the protocol crate.
            let error = match self.route(body.routing_key()) {
                Err(error) => error,
                Ok(route) => {
                    let target = route.target().ok_or_else(|| Error::NoRegion {
                        key: Bytes::copy_from_slice(body.routing_key()),
                    })?;
                    let header =
                        RequestHeader::new(route.region.id, route.region.epoch, target.peer_id);
                    let wire = body.clone().into_request(header);
                    match self.transport.call(target.store_id, &wire, deadline) {
                        Ok(response) if response.method() == method => return Ok(response),
                        Ok(response) => {
                            return Err(Error::UnexpectedResponse {
                                expected: method,
                                actual: response.method(),
                            });
                        }
                        Err(error) => {
                            region_id = route.region.id;
                            sent_epoch = Some(route.region.epoch);
                            error
                        }
                    }
                }
            };

            let repair = match classify(&error) {
                Verdict::Retry(redirect) => Some(redirect),
                // The one rule `classify` cannot state, because it needs the method: an answer
                // that never came back may be asked for again when asking cannot change what
                // the first attempt did. See `retry::may_ask_again` — a read, never a write.
                Verdict::Surface if may_ask_again(method, &error) => {
                    Some(Redirect::Leader { hint: None })
                }
                Verdict::Surface => None,
            };
            let Some(redirect) = repair else {
                self.on_terminal(&error, region_id, body.routing_key());
                return Err(terminal(error, method));
            };
            self.repair(&redirect, region_id);
            // **The budget counts failures, and a refusal that taught this client where the
            // region went is not one.** Reset rather than decremented: a call that keeps being
            // given fresher routing keeps its full budget for the moment it stops being given
            // any, and the call deadline above is what bounds it either way.
            if self.learned_a_newer_epoch(body.routing_key(), sent_epoch) {
                fruitless = 0;
            } else {
                fruitless += 1;
            }
            if fruitless > self.options.retry.max_retries {
                return Err(Error::RetriesExhausted {
                    attempts,
                    source: Box::new(error),
                });
            }
            let delay = self.jitter.apply(self.options.retry.backoff(attempts - 1));
            // Sleeping past the deadline only delays the same answer, so stop now and say
            // which failure the caller was waiting on.
            if self.clock.now() + delay >= deadline {
                return Err(Error::DeadlineExceeded {
                    attempts,
                    source: Some(Box::new(error)),
                });
            }
            self.clock.sleep(delay);
        }
    }

    /// The route the **resolver** answers with, cached over whatever was there.
    ///
    /// [`Router::route`] prefers the cache, which is right for a request that only needs to know
    /// where to send bytes: a stale entry costs a redirect and never a wrong answer. It is not
    /// right for a caller that needs the region's *membership* — a columnar learner joins through
    /// a conf change, and a cached entry taken before it joined lists no learner and causes no
    /// refusal to repair itself with. Such a caller asks the authority once
    /// (`crates/esker-client/src/fragment.rs`).
    pub(crate) fn locate(&self, key: &[u8]) -> std::result::Result<Option<Route>, ProtoError> {
        let route = self.resolver.locate(key)?;
        if let Some(route) = &route {
            self.cache.insert(route.clone());
        }
        Ok(route)
    }

    /// The cached route for `key`, or a fresh one from the resolver.
    ///
    /// The three outcomes are three different things, and flattening any pair of them would
    /// cost the caller something: a hit, a `GetRegion` that says no region covers the key —
    /// terminal, because waiting does not create one — and a `GetRegion` that could not be
    /// answered, which is the caller's to classify and usually to retry.
    pub(crate) fn route(&self, key: &[u8]) -> std::result::Result<Route, ProtoError> {
        if let Some(route) = self.cache.lookup(key) {
            return Ok(route);
        }
        let route = self.resolver.locate(key)?.ok_or_else(|| {
            // Not retryable, and the classifier agrees: `KeyNotInRegion` says this key belongs
            // to no region the cluster admits to, which is what "no region covers it" is.
            ProtoError::KeyNotInRegion {
                key: Bytes::copy_from_slice(key),
                region_id: 0,
                start_key: Bytes::new(),
                end_key: Bytes::new(),
            }
        })?;
        self.cache.insert(route.clone());
        Ok(route)
    }

    /// Applies what a redirectable refusal said to fix.
    pub(crate) fn repair(&self, redirect: &Redirect, region_id: u64) {
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

    /// Whether the repair just made left this client holding a **newer** epoch for `key` than the
    /// attempt that failed was addressed with.
    ///
    /// This is the difference between a client that is chasing a moving region and one that is
    /// hammering a dead one, and until it existed the retry budget could not tell them apart. A
    /// saturated cluster splits and rebalances, so a region's epoch really does move between one
    /// attempt and the next; each refusal carries the regions that replaced the one asked about,
    /// so each attempt leaves the cache **more correct than it found it** and the next one is
    /// aimed better. Counting those against the same budget as a store that will not answer is how
    /// `Backend::begin` came back as `gave up after 9 attempts: region epoch does not match` with
    /// most of its ten-second deadline unspent — 2.27 s of it, in the repro this note comes from
    /// (`docs/plans/debt-c3.md` §3).
    ///
    /// [`Epoch::is_stale_against`] is the comparison, and it is the same one the *store* uses to
    /// decide the request was stale in the first place: the two counters move independently, so
    /// "newer" is not one comparison. Asking it here rather than writing `>` is what keeps the
    /// client's idea of progress and the store's idea of staleness from drifting apart.
    ///
    /// Only the epoch counts. A `NotLeader` hint moves no epoch and does not reset the budget,
    /// which is the conservative half of this: chasing a leader around a region that is not
    /// changing is exactly the loop the budget was put there to stop.
    fn learned_a_newer_epoch(&self, key: &[u8], sent: Option<Epoch>) -> bool {
        let Some(sent) = sent else {
            // Nothing was sent, so nothing can have been learned: the resolver refused before an
            // attempt was addressed at all.
            return false;
        };
        self.cache
            .lookup(key)
            .is_some_and(|route| sent.is_stale_against(route.region.epoch))
    }

    /// Drops a cache entry that a terminal error proved wrong.
    ///
    /// `KeyNotInRegion` is not retryable — waiting cannot fix a routing mistake — but it does
    /// prove the cached region is a lie, and leaving it in place would make the caller's next
    /// call fail the same way.
    ///
    /// `region_id == 0` is the resolver's own refusal: nothing was cached, so there is nothing
    /// to drop, and only the key is swept.
    fn on_terminal(&self, error: &ProtoError, region_id: u64, key: &[u8]) {
        if matches!(error, ProtoError::KeyNotInRegion { .. }) {
            if region_id != 0 {
                self.cache.invalidate(region_id);
            }
            self.cache.invalidate_key(key);
        }
    }
}

/// Where the region holding `key` ends, after a store refused a request for it.
///
/// **One repair, in one place.** A scan learns a boundary has moved from the refusal it causes,
/// and so does a fragment dispatch — [ADR 0040](../../../docs/adr/0040-the-engine-a-query-runs-on.md)'s
/// path has the same problem for the same reason. Two copies of this would be two chances to get
/// the ordering of "believe the store" and "ask the driver" the wrong way round, and only one of
/// them would be under test.
///
/// # The two cases, and why the order between them matters
///
/// * **The refusal's own bounds contain `key`.** Then the store has just told us the truth about
///   itself and it is *newer than the driver*: a store knows about its own split the instant it
///   happens and the placement driver learns at the next heartbeat. Believe it, and return.
/// * **They do not.** Then this is stale *routing* rather than a stale *boundary* — the request
///   went to a store that never held the key — and only the authority can fix it.
///
/// Getting that order wrong is not academic: asking the driver first returns the same too-wide
/// boundary inside the heartbeat window, and the retries burn out in microseconds
/// (`Transaction::scan_region`, measured 2026-09-05).
///
/// # Why the driver is asked in a loop
///
/// Under load the driver stays behind for longer than one round trip, and a `GetRegion` that
/// answers "no region covers this key" — `region_id == 0`, the resolver's own refusal — is *not*
/// a fact about the cluster, it is a fact about the driver's knowledge at that instant. That is
/// what surfaced as `08006 … key is not in region 0` in a loaded gate while passing alone. So it
/// waits the driver out on the router's own jittered schedule, and gives up with the refusal it
/// was handed rather than one it invented.
pub(crate) fn repair_route(
    router: &Router,
    key: &Bytes,
    refusal: &ProtoError,
) -> std::result::Result<Bytes, Error> {
    if let ProtoError::KeyNotInRegion {
        start_key, end_key, ..
    } = refusal
        && owns(start_key, end_key, key)
    {
        return Ok(end_key.clone());
    }
    let policy = RetryPolicy::default();
    for attempt in 0..ROUTE_REPAIR_ATTEMPTS {
        match router.locate(key) {
            // The authority knows the key, and its answer replaces whatever the cache held.
            Ok(Some(route)) if owns(&route.region.start_key, &route.region.end_key, key) => {
                return Ok(route.region.end_key);
            }
            // Either it answered and still does not cover the key, or it could not answer at
            // all. **The two are one case here**: both mean the driver does not yet know where
            // this key lives, which is a fact about its knowledge and not about the cluster.
            Ok(_) | Err(_) => {}
        }
        router
            .clock()
            .sleep(router.jitter().apply(policy.backoff(attempt)));
    }
    Err(terminal(refusal.clone(), Method::PdGetRegion))
}

/// How many times the driver is asked before a refusal is believed as final.
///
/// Small and fixed rather than a deadline: each attempt is a round trip to the authority and the
/// thing being waited for is one heartbeat, not an unbounded queue.
const ROUTE_REPAIR_ATTEMPTS: u32 = 5;

/// Whether `[start_key, end_key)` — a region, as the store named it — contains `key`.
///
/// Beside the router rather than beside either scan, because **both** scans need it and a second
/// copy of a range rule is a second chance to get an empty key's two meanings backwards.
///
/// An empty `end_key` is the end of the key space; an empty `start_key` is its beginning.
pub(crate) fn owns(start_key: &Bytes, end_key: &Bytes, key: &Bytes) -> bool {
    start_key.as_ref() <= key.as_ref() && (end_key.is_empty() || key.as_ref() < end_key.as_ref())
}

/// A scan page's end: the caller's, or the region's, whichever comes first.
///
/// **An empty key means "the end of the key space" on both sides**, and they mean it in opposite
/// directions here — an empty `boundary` is the last region, so the caller's own `end` stands; an
/// empty `end` is an unbounded scan, so the region's boundary is what bounds this page. Reading
/// either one as a literal empty string would clamp every page to nothing.
pub(crate) fn clamp_end(end: &[u8], boundary: &Bytes) -> Bytes {
    if boundary.is_empty() {
        return Bytes::copy_from_slice(end);
    }
    if end.is_empty() || boundary.as_ref() < end {
        return boundary.clone();
    }
    Bytes::copy_from_slice(end)
}

/// Turns a terminal protocol error into a client error, naming the ambiguous case.
///
/// A mutation whose outcome is `Unknown` is the case the whole retry story is built around: it
/// went out, no usable answer came back, and it may or may not be in the log. A read in the
/// same position is not ambiguous — nothing changed either way — so it is returned plainly and
/// the caller may simply ask again.
pub(crate) fn terminal(error: ProtoError, method: Method) -> Error {
    if method.is_mutation() && error.outcome() == RequestOutcome::Unknown {
        return Error::AmbiguousResult {
            method,
            source: Box::new(error),
        };
    }
    Error::Store(error)
}

/// Runs one closure per group, on its own thread when there is more than one group.
///
/// The transaction client prewrites and commits secondaries **per region, in parallel**
/// (`docs/DESIGN.md` §8): a transaction touching five regions should cost one round trip, not
/// five. `StoreTransport::call` blocks, so parallel here means threads — scoped ones, so
/// nothing is spawned that outlives the call and no `'static` bound leaks into the transport.
///
/// A single group is answered on the calling thread, which is the overwhelmingly common case
/// and the one every test exercises: a thread per region is worth it at five regions and
/// absurd at one. Results come back **in group order** whichever path ran, so a caller — and a
/// test — sees the same answer either way.
pub(crate) fn fan_out<T, F>(groups: usize, run: F) -> Vec<Result<T>>
where
    T: Send,
    F: Fn(usize) -> Result<T> + Send + Sync,
{
    if groups <= 1 {
        return (0..groups).map(&run).collect();
    }
    let run = &run;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..groups)
            .map(|index| scope.spawn(move || run(index)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    // A panicked worker is a bug in this crate, not a failed request, and
                    // saying so is better than a `resume_unwind` that loses which group it was.
                    Err(Error::Internal(
                        "a request worker panicked; this is a bug in esker-client".to_owned(),
                    ))
                })
            })
            .collect()
    })
}
