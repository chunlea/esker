//! Asking a columnar replica to evaluate a plan fragment.
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decisions 3 and 4,
//! `docs/plans/phase-10-routing.md` U1. Phase 8 built the format, the service and the learner that
//! answers; it closed saying *"nothing on a real cluster can ask a fragment"*. This is the caller.
//!
//! # Three ways this is not [`Router::call`]
//!
//! **It addresses a learner, not a leader.** `Route::target()` answers with the believed leader,
//! which is the one peer a fragment must *not* go to: a voter holds rows and answers
//! [`RefusalReason::NotColumnar`]. So the peer is chosen by role, here, and everything else the
//! router does — the region cache, the resolver, the clock, the in-flight gate, the backoff
//! jitter — is shared with it rather than copied.
//!
//! **A refusal is an answer.** [`FragmentAnswer::Refused`] is `Ok`. A node that does not implement
//! an operator, or that cannot catch up in time, is answering correctly, and the answer means
//! *fall back to a row scan* — a path the planner already has. Putting it on the error path would
//! make every rolling upgrade look like a fault.
//!
//! **The retry set is deliberately narrower.** [`Router::call`] retries every error `esker-proto`
//! marks retryable, because for a key-value request a redirect is a redirect. A fragment is
//! evaluated over *the whole of a region's columnar copy* — `esker_columnar`'s `KeyRange`
//! is refused by the evaluator, because a columnar file records no key range — so "which region"
//! is not a hint here, it is the definition of what the answer covers. A region that split under
//! this call answers about a different set of rows than the caller planned for, and retrying into
//! it would return a **partial answer that looks complete**. So an epoch change is not retried: it
//! repairs the cache and surfaces, and the caller falls back to rows inside the same snapshot,
//! which is the one fallback that cannot change an answer.
//!
//! What *is* retried is the pair that costs nothing to repeat: a busy store, and an answer that
//! never came back. A fragment is a read at a fixed `ts`, so asking again cannot change what the
//! first attempt did — the rule [`crate::retry::may_ask_again`] already draws by method.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::region_cache::Route;
use crate::retry::{Verdict, classify};
use crate::router::{Router, terminal};
use crate::wire::{Method, Peer, PeerRole, ProtoError, Request, RequestHeader, Response};

pub use esker_proto::fragment::{FragmentReq, RefusalReason, ScanStats};

/// What a columnar replica answered.
///
/// A refusal is a variant of this rather than an `Err`, for the reason the module docs give: it is
/// a normal answer meaning *read the rows*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentAnswer {
    /// The fragment was evaluated. `result` is `esker_proto::fragment::result`'s format —
    /// versioned and checksummed of its own, decoded by whoever asked.
    Answered {
        /// The answer.
        result: Bytes,
        /// What it cost, for `EXPLAIN`.
        stats: ScanStats,
    },
    /// It was not, and this is normal. See [`RefusalReason`].
    Refused {
        /// What the planner should do about it.
        reason: RefusalReason,
        /// For a human. Never matched on.
        detail: String,
    },
}

impl FragmentAnswer {
    /// The refusal reason, or `None` for an answer.
    #[must_use]
    pub fn refusal(&self) -> Option<RefusalReason> {
        match self {
            FragmentAnswer::Answered { .. } => None,
            FragmentAnswer::Refused { reason, .. } => Some(*reason),
        }
    }
}

/// One region of a key range, and the replica in it that can answer a fragment.
///
/// **The epoch is part of it.** A shard is a promise about *which rows a fragment covers*, and the
/// epoch is what the store checks that promise against (`CLAUDE.md` invariant 5). A shard built
/// before a split and used after one is refused by the store rather than answered partially.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// The region.
    pub region_id: u64,
    /// Its epoch when the shard was built.
    pub epoch: crate::wire::Epoch,
    /// Inclusive start of the region's range.
    pub start: Bytes,
    /// Exclusive end, or empty for unbounded.
    pub end: Bytes,
    /// The columnar learner holding it, or `None` when the region has none — in which case
    /// nothing here can answer a fragment and the caller reads the rows.
    pub columnar: Option<Peer>,
}

impl Shard {
    /// Whether a fragment can be asked of this shard at all.
    #[must_use]
    pub fn is_columnar(&self) -> bool {
        self.columnar.is_some()
    }
}

/// Asks columnar replicas to evaluate fragments, through the region cache the rest of the client
/// already routes with.
#[derive(Debug)]
pub struct FragmentClient {
    router: Arc<Router>,
}

impl FragmentClient {
    /// A fragment client over a router. The router is shared with the transactional client on the
    /// same node, so a cache warmed by a row read is warm for a fragment.
    #[must_use]
    pub fn new(router: Arc<Router>) -> Self {
        Self { router }
    }

    /// The router underneath, for a caller that wants its options or its clock.
    #[must_use]
    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }

    /// Every region covering `[start, end)`, in key order, each with its columnar learner.
    ///
    /// `GetRegion` is the only routing question the placement driver answers, so this walks:
    /// resolve `start`, take the region's `end_key`, resolve that, and so on — the walk
    /// `esker-cli region ls` makes for the same reason. An empty `end_key` is the last region of
    /// the cluster and ends the walk.
    ///
    /// A region that does not reach past the key it was found with would loop forever, so it is a
    /// typed error rather than a hang: a routing answer that does not advance is malformed, and
    /// the caller falls back to rows.
    pub fn shards(&self, start: &[u8], end: &[u8]) -> Result<Vec<Shard>> {
        let mut shards = Vec::new();
        let mut key = Bytes::copy_from_slice(start);
        loop {
            let mut route = self
                .router
                .route(&key)
                .map_err(|error| terminal(error, Method::FragmentEvaluate))?;
            // **One confirmation from the authority when the cache lists no learner.** The region
            // cache is a hint repaired by the refusals it causes, and a *missing learner* causes
            // none: a columnar replica joins through a conf change, and a client holding an entry
            // from before it joined would keep planning on rows for ever and never be told
            // otherwise. Paid only by a table whose catalog record asks for a copy — the planner
            // checks that before it asks for shards — so an ordinary table costs nothing.
            if columnar_peer(&route).is_none()
                && let Ok(Some(fresh)) = self.router.locate(&key)
            {
                route = fresh;
            }
            let region_end = route.region.end_key.clone();
            shards.push(Shard {
                region_id: route.region.id,
                epoch: route.region.epoch,
                start: route.region.start_key.clone(),
                end: region_end.clone(),
                columnar: columnar_peer(&route).copied(),
            });
            if region_end.is_empty() {
                return Ok(shards);
            }
            if region_end <= key {
                return Err(Error::Internal(format!(
                    "region {} ends at or before the key it was found with; the routing answer \
                     does not advance",
                    route.region.id
                )));
            }
            if !end.is_empty() && region_end.as_ref() >= end {
                return Ok(shards);
            }
            key = region_end;
        }
    }

    /// Sends one fragment to `shard`'s columnar learner.
    ///
    /// The header carries the shard's own epoch rather than whatever the cache believes now, so a
    /// region that moved is refused by the store instead of answered about a different range.
    pub fn evaluate(&self, shard: &Shard, request: &FragmentReq) -> Result<FragmentAnswer> {
        let Some(peer) = shard.columnar.as_ref() else {
            // Not an error and not a wire round trip: a region with no columnar replica is
            // exactly what `NotColumnar` means, answered without asking anybody.
            return Ok(FragmentAnswer::Refused {
                reason: RefusalReason::NotColumnar,
                detail: format!("region {} has no columnar learner", shard.region_id),
            });
        };

        let options = self.router.options();
        let clock = self.router.clock();
        let deadline = clock.now() + options.call_timeout;
        let Some(_permit) = self.router.gate().acquire(options.call_timeout) else {
            return Err(Error::DeadlineExceeded {
                attempts: 0,
                source: None,
            });
        };

        let wire = Request::Fragment {
            header: RequestHeader::new(shard.region_id, shard.epoch, peer.peer_id),
            request: request.clone(),
        };

        let mut attempts: u32 = 0;
        loop {
            if clock.now() >= deadline {
                return Err(Error::DeadlineExceeded {
                    attempts,
                    source: None,
                });
            }
            attempts += 1;
            let error = match self.router.transport().call(peer.store_id, &wire, deadline) {
                Ok(Response::Fragment(answer)) => return Ok(answer.into()),
                Ok(other) => {
                    return Err(Error::UnexpectedResponse {
                        expected: Method::FragmentEvaluate,
                        actual: other.method(),
                    });
                }
                Err(error) => error,
            };

            // The cache is repaired whatever happens next, so that the *next* statement routes
            // better even when this one is about to fall back to rows.
            if let Verdict::Retry(redirect) = classify(&error) {
                self.router.repair(&redirect, shard.region_id);
            }
            if !retryable(&error) {
                return Err(terminal(error, Method::FragmentEvaluate));
            }
            if attempts > options.retry.max_retries {
                return Err(Error::RetriesExhausted {
                    attempts,
                    source: Box::new(error),
                });
            }
            let delay = self
                .router
                .jitter()
                .apply(options.retry.backoff(attempts - 1));
            if clock.now() + delay >= deadline {
                return Err(Error::DeadlineExceeded {
                    attempts,
                    source: Some(Box::new(error)),
                });
            }
            clock.sleep(delay);
        }
    }
}

/// Whether asking again can change the answer.
///
/// **Narrower than [`crate::retry::classify`], on purpose**, and the difference is not a drift:
/// there, a redirectable refusal is retried because a key-value request re-routed to the right
/// region is the same request. Here it is not. A fragment covers *the whole of a region's columnar
/// copy*, so re-routing after a split asks about a different set of rows and returns a partial
/// answer that looks complete — the one failure this feature must never have. An epoch change and
/// a region that has gone therefore surface, and the caller falls back to rows in the same
/// snapshot.
///
/// What is left is the pair that costs nothing to repeat, because a fragment is a read at a fixed
/// `ts` and asking twice cannot change what the first attempt did:
///
/// * a store shedding load ([`ProtoError::ServerIsBusy`]) — wait and ask again, which is what the
///   refusal says to do;
/// * an answer that never came back — a timeout, a torn connection, a leader that stepped down
///   with the request in flight. `docs/plans/phase-9-rails.md` §8 measured that last one on this
///   very call: an election gap under a saturated machine, reported immediately and with the
///   deadline untouched, which a retry turns back into an answer.
///
/// A `NotLeader` is neither, and it is a shape this call should not see at all: a fragment is
/// addressed to a learner, which never claims to lead. Retrying it would chase a leader this
/// request does not want.
fn retryable(error: &ProtoError) -> bool {
    // `ServerIsBusy` is the refusal that says to wait; the other three are the shapes of "nobody
    // said whether it happened", which is safe for a read and only for a read —
    // `RequestOutcome::Unknown` is exactly the case `Router::call` refuses to repeat for a
    // mutation.
    matches!(
        error,
        ProtoError::ServerIsBusy { .. }
            | ProtoError::Timeout { .. }
            | ProtoError::Closed { .. }
            | ProtoError::Io { .. }
    )
}

/// The columnar learner among a route's peers, if it has one.
///
/// The first, not the best: a region has at most a handful, and phase 8 places one. Choosing
/// between several — by load, by lag, by locality — is a scheduling question this build does not
/// have the inputs for, and picking arbitrarily is honest where picking cleverly would be
/// pretending.
fn columnar_peer(route: &Route) -> Option<&Peer> {
    route
        .region
        .peers
        .iter()
        .find(|peer| peer.role == PeerRole::ColumnarLearner)
}

impl From<esker_proto::fragment::FragmentResp> for FragmentAnswer {
    fn from(response: esker_proto::fragment::FragmentResp) -> Self {
        match response {
            esker_proto::fragment::FragmentResp::Result { result, stats } => {
                FragmentAnswer::Answered { result, stats }
            }
            esker_proto::fragment::FragmentResp::Refused { reason, detail } => {
                FragmentAnswer::Refused { reason, detail }
            }
        }
    }
}

/// How long a caller should let a fragment run before giving up on it.
///
/// Longer than a row read's, and it is not a guess: a fragment scans a whole region's columnar
/// copy and the `ReadIndex` round in front of it is a round trip to the leader. A caller that
/// wants a different one sets [`crate::ClientOptions::call_timeout`] on the router it builds.
pub const FRAGMENT_TIMEOUT: Duration = Duration::from_secs(60);
