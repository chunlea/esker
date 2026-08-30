//! The client: a region cache keyed by key range, routing to the right store, bounded retries
//! on redirectable errors, and the `RawKv` and `TxnKv` APIs application code actually calls
//! (`docs/DESIGN.md` §10).
//!
//! # Invariants
//!
//! * **The cache is a hint, never an authority.** Every request carries a region epoch and the
//!   server checks it; a stale cache costs a redirect, never a wrong answer
//!   (`CLAUDE.md` invariant 5).
//! * **Retries are bounded and backed off.** Only errors that carry a redirect hint —
//!   `NotLeader`, `EpochNotMatch`, `ServerIsBusy` — are retried, and never forever.
//! * **Timestamps come from the oracle** (invariant 6); the client never invents one.
//!
//! # Layout
//!
//! [`wire`] is the message layer — a stand-in for `esker-proto` until that crate lands;
//! [`transport`] is the one seam through which bytes leave the process; [`clock`] is the one
//! seam through which time enters. Everything else — the region cache, the retry loop,
//! `RawClient` — is ordinary synchronous code with neither, which is what lets
//! [`testing::FakeTransport`] drive all of it.
//!
//! The transaction client is phase 5 (`prompts/05-txn.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod clock;
mod error;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transport;
pub mod wire;

pub use error::{Error, Result};

/// How many times a redirectable error is retried before it is returned to the caller.
pub const MAX_RETRIES: u32 = 8;

/// Delay before the first retry, in milliseconds.
pub const BACKOFF_BASE_MS: u64 = 10;

/// Ceiling on the backoff, in milliseconds. Without it, exponential growth turns a brief
/// leader election into a multi-minute stall.
pub const BACKOFF_MAX_MS: u64 = 2_000;

/// Delay before retry number `attempt`, counting from zero.
///
/// Exponential with a hard ceiling. The caller adds jitter from its own
/// `esker_base::rng::Pcg32` so that a herd of clients does not retry in lockstep; that draw
/// is not made here, because this function must stay deterministic and testable.
#[must_use]
pub fn backoff_ms(attempt: u32) -> u64 {
    // `checked_shl` only guards the shift width, not the value, so `x << 63` would silently
    // shift every bit out and return a zero delay. Build the factor first, then multiply.
    1u64.checked_shl(attempt)
        .and_then(|factor| BACKOFF_BASE_MS.checked_mul(factor))
        .unwrap_or(BACKOFF_MAX_MS)
        .min(BACKOFF_MAX_MS)
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_BASE_MS, BACKOFF_MAX_MS, MAX_RETRIES, backoff_ms};

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
        }
    }

    /// Retrying the full budget must not take so long that a client looks hung.
    #[test]
    fn total_retry_budget_is_bounded() {
        let total: u64 = (0..MAX_RETRIES).map(backoff_ms).sum();
        assert!(total < 10_000, "a full retry budget waits {total} ms");
    }
}
