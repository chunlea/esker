//! When to try again, how long to wait, and what to fix in between.
//!
//! Which errors are retryable is `esker-proto`'s answer, not this module's:
//! [`ProtoError::is_retryable`] names them, and asking it rather than keeping a second list
//! here is what stops the client and the store from drifting apart about the same error. This
//! module decides the rest — what to *repair* before trying again, and how long to wait.
//!
//! The retryable set is the redirect-hint errors of `docs/DESIGN.md` §9: the peer is not the
//! leader, the epoch has moved on, the store does not host the region, or the store is
//! shedding load. Every one is a **refusal** — the store rejected the request before changing
//! anything, so [`ProtoError::outcome`] answers `NotApplied` — and that is what makes
//! re-sending safe for a write as well as a read. Everything else surfaces at once.
//!
//! `RegionNotFound` is a fourth alongside the three the phase prompt names. It belongs with
//! them for the same reason `EpochNotMatch` does: it says the client's routing is stale and
//! nothing else, and its repair is a cache refresh.
//!
//! # The other half of the rule: a lost answer to a question
//!
//! [`is_retryable`](ProtoError::is_retryable) is a property of the error alone, and it has to
//! be — see [`classify`]. But the rule it comes from is about *writes*: a write may be re-sent
//! only when the previous attempt provably did not commit. The mirror of that rule needs the
//! method, and [`may_ask_again`] is where it lives: **a read may always be re-sent**, because
//! asking again cannot change what the first attempt did.
//!
//! Without it, a read whose answer was lost — the peer stopped mid-call, the connection went —
//! fails the caller outright, even though the region has two other replicas that could answer
//! and the request could not have changed anything. That is a refusal manufactured out of a
//! dropped packet.
//!
//! # The rule the retry set is derived from
//!
//! > A write may be re-sent only when the previous attempt provably did not commit.
//!
//! Under last-write-wins, re-sending a `Put` that *did* commit is harmless — the same bytes
//! land twice. The danger is the case nobody can rule out: the request went out and the
//! connection died, so the write may be in the log and the client cannot tell. That is not a
//! refusal at all: `esker-proto` marks it [`RequestOutcome::Unknown`], and this module
//! deliberately does not retry it. It becomes [`crate::Error::AmbiguousResult`] and the
//! caller decides. `prompts/05-txn.md` builds on that distinction, which is why it is drawn
//! here rather than left to a comment.
//!
//! # Jitter is not decoration
//!
//! A leader election makes every client of a region fail at the same instant. Retrying them
//! all on the same exponential schedule reconverges the herd on the new leader in lockstep,
//! which is how a recovered cluster gets knocked over again. Each client therefore draws its
//! own delay from [`Jitter`], seeded per client.

use std::sync::Mutex;
use std::time::Duration;

use esker_base::rng::Pcg32;

use crate::wire::{Method, ProtoError, Region, RequestOutcome};

/// How many times a redirectable error is retried before it is returned to the caller.
///
/// A budget of eight is nine calls in total: the first attempt plus eight retries.
pub const MAX_RETRIES: u32 = 8;

/// Delay before the first retry, in milliseconds.
pub const BACKOFF_BASE_MS: u64 = 10;

/// Ceiling on the backoff, in milliseconds. Without it, exponential growth turns a brief
/// leader election into a multi-minute stall.
pub const BACKOFF_MAX_MS: u64 = 2_000;

/// How long a call may take in total, in milliseconds, retries and backoff included.
pub const CALL_TIMEOUT_MS: u64 = 10_000;

/// Delay before retry number `attempt`, counting from zero.
///
/// Exponential with a hard ceiling. The caller adds jitter from its own
/// [`Jitter`] so that a herd of clients does not retry in lockstep; that draw is not made
/// here, because this function must stay deterministic and testable.
#[must_use]
pub fn backoff_ms(attempt: u32) -> u64 {
    // `checked_shl` only guards the shift width, not the value, so `x << 63` would silently
    // shift every bit out and return a zero delay. Build the factor first, then multiply.
    1u64.checked_shl(attempt)
        .and_then(|factor| BACKOFF_BASE_MS.checked_mul(factor))
        .unwrap_or(BACKOFF_MAX_MS)
        .min(BACKOFF_MAX_MS)
}

/// The bounded exponential schedule a client backs off on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt. Zero means one attempt and no retry.
    pub max_retries: u32,
    /// Delay before the first retry.
    pub backoff_base: Duration,
    /// Ceiling on any single delay.
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            backoff_base: Duration::from_millis(BACKOFF_BASE_MS),
            backoff_max: Duration::from_millis(BACKOFF_MAX_MS),
        }
    }
}

impl RetryPolicy {
    /// The un-jittered delay before retry number `attempt`, counting from zero.
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        self.backoff_base
            .checked_mul(factor)
            .unwrap_or(self.backoff_max)
            .min(self.backoff_max)
    }
}

/// A per-client source of backoff jitter.
///
/// Equal jitter: the delay is drawn uniformly from the top half of the schedule's value, so
/// it is never less than half the intended wait — a herd is spread out without any client
/// hammering. The generator is `esker-base`'s seeded PCG32, because `rand` is banned and
/// because a seeded generator is what makes a jittered retry test reproducible.
#[derive(Debug)]
pub struct Jitter {
    rng: Mutex<Pcg32>,
}

impl Jitter {
    /// A generator that will produce the same sequence every run.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self {
            rng: Mutex::new(Pcg32::new(seed, seed ^ 0x9E37_79B9_7F4A_7C15)),
        }
    }

    /// A generator seeded from the process id and the wall clock.
    ///
    /// The clock is used to make two clients in one process differ, never for ordering
    /// (`CLAUDE.md` invariant 6 is about timestamps that decide who wins, not about a seed).
    #[must_use]
    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let nanos = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0);
        Self::seeded(nanos ^ (u64::from(std::process::id()) << 32))
    }

    /// Draws a delay in `[base / 2, base]`.
    #[must_use]
    pub fn apply(&self, base: Duration) -> Duration {
        let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
        if millis == 0 {
            return base;
        }
        let half = millis / 2;
        let mut rng = self
            .rng
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Duration::from_millis(half + rng.range_inclusive(0, millis - half))
    }
}

/// What the client must repair before trying again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// The peer is not the leader. Point the cache at the hinted peer, or forget the leader
    /// entirely when the peer did not offer a hint — an unknown leader still routes, because
    /// asking a follower is how the next hint is obtained.
    Leader {
        /// The peer the store says leads, by peer id.
        hint: Option<u64>,
    },
    /// The epoch moved on. Drop what is cached for the region and take the replacements the
    /// store sent; when it sent none, the next lookup goes back to the resolver.
    Epoch {
        /// The regions that now cover the range.
        replacements: Vec<Region>,
    },
    /// The store does not host the region at all. Nothing to take from the error, so drop the
    /// cached entry and let the resolver answer again.
    Refresh,
    /// Nothing to repair; the store is shedding load. Wait and ask again.
    Busy,
}

/// Whether a failed call is worth making again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Fix routing as described, back off, and try again.
    Retry(Redirect),
    /// Give the caller the error now.
    Surface,
}

/// Decides what to do about `error`.
///
/// The method the request carried is deliberately **not** an input. Every retryable variant
/// is a refusal, which is safe to re-send whatever the method was, so the method would change
/// nothing here; the one place it does matter — an answer that never came back, which is safe
/// to repeat for a read and not for a write — is not retryable at all, and the call site
/// turns it into [`crate::Error::AmbiguousResult`]. Keeping the method out of this function
/// is what makes "retryable" a property of the error alone.
///
/// Anything not named below surfaces, which is the safe default when `esker-proto` grows a
/// variant this list has not heard of.
#[must_use]
pub fn classify(error: &ProtoError) -> Verdict {
    // Belt and braces: a variant that names a repair below but that the protocol does not
    // consider retryable must not be retried. The protocol crate is the authority.
    if !error.is_retryable() {
        return Verdict::Surface;
    }
    debug_assert_eq!(
        error.outcome(),
        RequestOutcome::NotApplied,
        "a retryable error must be one the store provably did not apply"
    );
    match error {
        ProtoError::NotLeader { leader_hint, .. } => {
            Verdict::Retry(Redirect::Leader { hint: *leader_hint })
        }
        ProtoError::EpochNotMatch { current_regions } => Verdict::Retry(Redirect::Epoch {
            replacements: current_regions.clone(),
        }),
        ProtoError::RegionNotFound { .. } => Verdict::Retry(Redirect::Refresh),
        ProtoError::ServerIsBusy { .. } => Verdict::Retry(Redirect::Busy),
        // **The store this attempt was routed to could not be reached**, and the request never
        // left this process, so re-sending it is safe for a write as well as a read. Forgetting
        // the leader is the repair: a store that is not answering is certainly not leading, and an
        // unknown leader routes to a peer chosen by the attempt number — so the next try asks a
        // different one rather than the same corpse (`Route::target_at`).
        ProtoError::NotSent { .. } => Verdict::Retry(Redirect::Leader { hint: None }),
        // **Never retried, and said so here rather than left to the catch-all.** The history the
        // read wanted is below the store's garbage-collection safepoint and may be collected;
        // asking again cannot bring a version back, and asking a *different* store is worse —
        // a peer whose safepoint has not caught up would answer, which is the wrong answer this
        // refusal exists to prevent ([ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md)
        // decision 5). The caller starts a new transaction at a fresh timestamp.
        //
        // The catch-all below would already surface it — `outcome()` files it as `NotApplied`, so
        // `may_ask_again` is false for a read as well. It is written out because a default that
        // happens to be right is not a decision, and the next variant added beside it will not be
        // this lucky.
        #[allow(
            clippy::match_same_arms,
            reason = "identical to the catch-all on purpose: the arm records the decision, and                       a default that happens to be right is not one"
        )]
        ProtoError::SnapshotTooOld { .. } => Verdict::Surface,
        // `KeyNotInRegion` lands here: this client's routing is wrong, and no amount of
        // waiting fixes that. The caller sees it; the cache entry that produced it is dropped
        // by the call site so the next attempt starts from the resolver rather than the same
        // lie.
        _ => Verdict::Surface,
    }
}

/// Whether an error [`classify`] said to surface is merely a **lost answer** that this method
/// may ask for again.
///
/// This is the one rule that needs the method, and it is the mirror of the one the retryable
/// set is derived from. A write may be re-sent only when the previous attempt provably did not
/// commit, so an [`RequestOutcome::Unknown`] answer to a write is never re-sent: it becomes
/// [`crate::Error::AmbiguousResult`] and the caller decides. A **read** is the opposite case
/// and the reasoning is the whole of it: whatever the first attempt did or did not do, asking
/// again does not change it, and the answer is the same answer. So it is asked again, of
/// whichever peer the repaired route leads to.
///
/// The repair is [`Redirect::Leader`] with no hint. The peer that lost the answer is the peer
/// that stopped, and forgetting the leader is what sends the next attempt somewhere else; an
/// unknown leader still routes, because asking a follower is how the next hint is obtained.
///
/// It matters exactly when the cluster is losing nodes, which is when a client most needs its
/// reads to work: `esker-store` answers a proposal stranded by a stopping peer with
/// [`ProtoError::Closed`], and every read in flight to that peer gets the same treatment. One
/// of those two is genuinely ambiguous and one is not, and this is what tells them apart.
#[must_use]
pub fn may_ask_again(method: Method, error: &ProtoError) -> bool {
    !method.is_mutation() && error.outcome() == RequestOutcome::Unknown
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::{
        BACKOFF_BASE_MS, BACKOFF_MAX_MS, Jitter, MAX_RETRIES, Redirect, RetryPolicy, Verdict,
        backoff_ms, classify,
    };
    use crate::wire::ProtoError;

    /// **Every variant, one line each, and the compiler counts them.**
    ///
    /// `classify` ends in a catch-all, so a variant added tomorrow is classified by whatever that
    /// happens to be rather than by anybody's decision — and the catch-all is *right* often enough
    /// that the omission is silent. This table is where the decision is written down, and the
    /// count below is what fails when a variant is added without a line here.
    ///
    /// The table is the rule, not a description of the code: a change that moves a variant from
    /// one column to the other has to change this file, which is how a reviewer sees it happen.
    /// The table itself, so the test that reads it stays short enough to read.
    fn classification_table() -> Vec<(ProtoError, bool)> {
        use crate::wire::Region;

        // (the error, whether the client retries it)
        vec![
            // Retried: each one carries, or implies, somewhere else to ask.
            (
                ProtoError::NotLeader {
                    region_id: 1,
                    leader_hint: Some(2),
                },
                true,
            ),
            (
                ProtoError::EpochNotMatch {
                    current_regions: Vec::<Region>::new(),
                },
                true,
            ),
            (ProtoError::RegionNotFound { region_id: 1 }, true),
            (
                ProtoError::ServerIsBusy {
                    reason: "busy".to_owned(),
                },
                true,
            ),
            // The bytes never left, so there is nothing to have half-happened.
            (ProtoError::not_sent("connection refused"), true),
            // Surfaced: asking again, or asking elsewhere, cannot help.
            (
                ProtoError::SnapshotTooOld {
                    start_ts: 1,
                    safepoint: 2,
                },
                false,
            ),
            (
                ProtoError::KeyNotInRegion {
                    key: bytes::Bytes::from_static(b"k"),
                    region_id: 1,
                    start_key: bytes::Bytes::new(),
                    end_key: bytes::Bytes::new(),
                },
                false,
            ),
            (
                ProtoError::Locked {
                    lock_info: bytes::Bytes::from_static(b"lock"),
                },
                false,
            ),
            (ProtoError::invalid("nonsense"), false),
            (
                ProtoError::Unsupported {
                    detail: "later".to_owned(),
                },
                false,
            ),
            (ProtoError::corrupt("a frame", "bad bytes"), false),
            (
                ProtoError::Io {
                    detail: "disk".to_owned(),
                },
                false,
            ),
            // **Unknown outcome, and a write may not be re-sent on one.** A read may ask again —
            // that is `may_ask_again`, which needs the method and so is not this table's business.
            (
                ProtoError::Closed {
                    detail: "the peer closed the connection".to_owned(),
                },
                false,
            ),
            (
                ProtoError::Timeout {
                    detail: "no answer".to_owned(),
                },
                false,
            ),
            (ProtoError::internal("a bug"), false),
            (ProtoError::DuplicateRequestId { request_id: 1 }, false),
            (ProtoError::NotBootstrapped, false),
            (
                ProtoError::ClusterMismatch {
                    expected: 1,
                    actual: 2,
                },
                false,
            ),
            // **Surfaced here, and retried by `PdConn`.** This client talks to *stores*; a
            // placement driver's redirect reaching it is a routing mistake, and following it is
            // the SQL node's business rather than this loop's. The table is where that stops
            // being a surprise.
            (
                ProtoError::PdNotLeader {
                    leader_id: 1,
                    leader_address: String::new(),
                },
                false,
            ),
            (
                ProtoError::WireVersion {
                    expected: 1,
                    actual: 2,
                },
                false,
            ),
        ]
    }

    #[test]
    fn every_error_is_classified_on_purpose() {
        let table = classification_table();
        for (error, retried) in &table {
            let verdict = classify(error);
            assert_eq!(
                matches!(verdict, Verdict::Retry(_)),
                *retried,
                "{error:?} is classified as {verdict:?}, which this table does not say"
            );
        }

        // **The count is the guard.** `code::ALL` has one entry per variant, so a variant added
        // without a line above makes this fail — which is the whole point of writing the table.
        assert_eq!(
            table.len(),
            esker_proto::error::code::ALL.len(),
            "a `ProtoError` variant has no line in this table, so nobody decided how the client \
             treats it"
        );
    }

    #[test]
    fn backoff_grows_then_flattens() {
        assert_eq!(backoff_ms(0), BACKOFF_BASE_MS);
        assert_eq!(backoff_ms(1), BACKOFF_BASE_MS * 2);
        assert_eq!(backoff_ms(2), BACKOFF_BASE_MS * 4);

        let mut previous = 0;
        for attempt in 0..MAX_RETRIES {
            let delay = backoff_ms(attempt);
            assert!(
                delay >= previous,
                "backoff went backwards at attempt {attempt}"
            );
            assert!(delay <= BACKOFF_MAX_MS, "backoff blew past its ceiling");
            previous = delay;
        }
    }

    /// A shift wide enough to overflow must saturate at the ceiling, not wrap to a tiny delay
    /// or panic. This is `CLAUDE.md` invariant 9 in miniature.
    #[test]
    fn absurd_attempt_counts_saturate() {
        for attempt in [63u32, 64, 1_000, u32::MAX] {
            assert_eq!(backoff_ms(attempt), BACKOFF_MAX_MS);
            assert_eq!(
                RetryPolicy::default().backoff(attempt),
                Duration::from_millis(BACKOFF_MAX_MS)
            );
        }
    }

    /// Retrying the full budget must not take so long that a client looks hung.
    #[test]
    fn total_retry_budget_is_bounded() {
        let total: u64 = (0..MAX_RETRIES).map(backoff_ms).sum();
        assert!(total < 10_000, "a full retry budget waits {total} ms");
    }

    /// The struct and the free function are two spellings of one schedule, and they are used
    /// from different places; they must not drift apart.
    #[test]
    fn the_policy_and_the_free_function_agree() {
        let policy = RetryPolicy::default();
        for attempt in 0..MAX_RETRIES {
            assert_eq!(
                policy.backoff(attempt),
                Duration::from_millis(backoff_ms(attempt)),
                "attempt {attempt}"
            );
        }
    }

    /// Never less than half the schedule, never more than all of it, and not the same number
    /// every time — a jitter that always returned the midpoint would spread no herd at all.
    #[test]
    fn jitter_stays_in_the_top_half_of_the_delay() {
        let jitter = Jitter::seeded(7);
        let base = Duration::from_millis(100);
        let mut seen = BTreeSet::new();
        for _ in 0..200 {
            let delay = jitter.apply(base);
            assert!(delay >= base / 2, "{delay:?} is less than half of {base:?}");
            assert!(delay <= base, "{delay:?} exceeds {base:?}");
            seen.insert(delay);
        }
        assert!(
            seen.len() > 10,
            "jitter produced only {} values",
            seen.len()
        );
    }

    /// A seeded generator is what makes a jittered retry test reproducible.
    #[test]
    fn the_same_seed_draws_the_same_delays() {
        let base = Duration::from_millis(64);
        let first: Vec<_> = {
            let jitter = Jitter::seeded(99);
            (0..8).map(|_| jitter.apply(base)).collect()
        };
        let second: Vec<_> = {
            let jitter = Jitter::seeded(99);
            (0..8).map(|_| jitter.apply(base)).collect()
        };
        assert_eq!(first, second);

        let other: Vec<_> = {
            let jitter = Jitter::seeded(100);
            (0..8).map(|_| jitter.apply(base)).collect()
        };
        assert_ne!(first, other, "two clients drew the same schedule");
    }

    /// A sub-millisecond delay has no room to jitter, and dividing it must not produce zero.
    #[test]
    fn a_delay_too_small_to_jitter_is_returned_as_it_is() {
        let jitter = Jitter::seeded(1);
        assert_eq!(jitter.apply(Duration::ZERO), Duration::ZERO);
        let tiny = Duration::from_micros(400);
        assert_eq!(jitter.apply(tiny), tiny);
    }

    #[test]
    fn each_redirectable_error_says_what_to_repair() {
        assert_eq!(
            classify(&ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(22),
            }),
            Verdict::Retry(Redirect::Leader { hint: Some(22) })
        );
        assert_eq!(
            classify(&ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            }),
            Verdict::Retry(Redirect::Leader { hint: None })
        );
        assert_eq!(
            classify(&ProtoError::EpochNotMatch {
                current_regions: vec![],
            }),
            Verdict::Retry(Redirect::Epoch {
                replacements: vec![]
            })
        );
        assert_eq!(
            classify(&ProtoError::RegionNotFound { region_id: 1 }),
            Verdict::Retry(Redirect::Refresh)
        );
        assert_eq!(
            classify(&ProtoError::ServerIsBusy {
                reason: "l0 stall".to_owned(),
            }),
            Verdict::Retry(Redirect::Busy)
        );
    }

    /// The negative half of the same rule, and the one that protects data: nothing else is
    /// retried, whatever the method was.
    #[test]
    fn everything_else_surfaces_at_once() {
        let others = [
            ProtoError::KeyNotInRegion {
                key: bytes::Bytes::from_static(b"k"),
                region_id: 1,
                start_key: bytes::Bytes::new(),
                end_key: bytes::Bytes::new(),
            },
            ProtoError::Locked {
                lock_info: bytes::Bytes::from_static(b"lock"),
            },
            ProtoError::Closed {
                detail: "reset".to_owned(),
            },
            ProtoError::corrupt("response", "bad crc"),
            ProtoError::internal("disk on fire"),
            ProtoError::invalid("empty key"),
        ];
        for error in &others {
            assert_eq!(classify(error), Verdict::Surface, "{error:?} was retried");
        }
    }

    /// **`NotSent` is retried, and the repair is to stop believing in the leader.**
    ///
    /// It was in the list above until run 124 showed what that cost: a leader-store kill refused
    /// 184 statements in 0.695 s, with a largest gap of 367 ms — no budget spent, because the set
    /// had no entry for *"the store I was routed to is not reachable"*. Re-sending is safe for a
    /// write as well as a read: the request never left the process.
    #[test]
    fn a_request_that_never_left_is_retried_somewhere_else() {
        assert_eq!(
            classify(&ProtoError::not_sent("connection refused")),
            Verdict::Retry(Redirect::Leader { hint: None }),
            "a store that is not answering is certainly not leading"
        );
    }

    /// And the one it must **not** take with it. `Closed` says the request went out and the answer
    /// was lost, so a write may have committed; a read in that position is re-asked by
    /// `may_ask_again`, which needs the method and therefore cannot live in `classify`.
    #[test]
    fn a_lost_answer_is_still_not_retried_by_the_classifier() {
        assert_eq!(
            classify(&ProtoError::Closed {
                detail: "reset".to_owned(),
            }),
            Verdict::Surface,
            "an ambiguous outcome must not be re-sent on the strength of the error alone"
        );
    }

    /// The protocol crate owns the retryable set; this module owns the repair for each one.
    /// If they disagree — a retryable error with no repair, or a repair for something the
    /// protocol will not retry — a request either spins without being fixed or is refused
    /// when it should have been redirected.
    #[test]
    fn retryable_variants_all_have_a_repair() {
        let every_variant = [
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            },
            ProtoError::EpochNotMatch {
                current_regions: vec![],
            },
            ProtoError::RegionNotFound { region_id: 1 },
            ProtoError::ServerIsBusy {
                reason: String::new(),
            },
            ProtoError::KeyNotInRegion {
                key: bytes::Bytes::new(),
                region_id: 1,
                start_key: bytes::Bytes::new(),
                end_key: bytes::Bytes::new(),
            },
            ProtoError::Locked {
                lock_info: bytes::Bytes::new(),
            },
            ProtoError::not_sent("x"),
            ProtoError::Closed {
                detail: String::new(),
            },
            ProtoError::corrupt("x", "y"),
            ProtoError::internal("x"),
            ProtoError::invalid("x"),
        ];
        for error in &every_variant {
            let retried = matches!(classify(error), Verdict::Retry(_));
            assert_eq!(
                retried,
                error.is_retryable(),
                "{error:?}: classify says retry={retried}, the protocol says {}",
                error.is_retryable()
            );
            if retried {
                assert_eq!(
                    error.outcome(),
                    crate::wire::RequestOutcome::NotApplied,
                    "{error:?} is retried but may have been applied"
                );
            }
        }
    }
}
