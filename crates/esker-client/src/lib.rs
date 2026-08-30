//! The client: a region cache keyed by key range, routing to the right store, bounded retries
//! on redirectable errors, and the `RawKv` and `TxnKv` APIs application code actually calls
//! (`docs/DESIGN.md` §10).
//!
//! # Invariants
//!
//! * **The cache is a hint, never an authority.** Every request carries a region epoch and the
//!   server checks it; a stale cache costs a redirect, never a wrong answer
//!   (`CLAUDE.md` invariant 5).
//! * **Retries are bounded and backed off.** Only the errors `esker-proto` marks retryable —
//!   the ones carrying a redirect hint — are retried, and never forever.
//! * **A write is never re-sent unless it provably did not commit.** Every retried error is a
//!   refusal; an answer that never came back becomes [`Error::AmbiguousResult`] and stops.
//! * **Timestamps come from the oracle** (invariant 6); the client never invents one.
//!
//! # Layout
//!
//! [`wire`] is the message layer: `esker-proto`'s routing and error types re-exported, plus
//! the `RawKv` bodies until that crate's `messages` module lands. [`transport`] is the one
//! seam through which bytes leave the process; [`clock`] is the one seam through which time
//! enters. Everything else — [`region_cache`], [`retry`], [`gate`], `RawClient` — is ordinary
//! synchronous code with neither, which is what lets [`testing::FakeTransport`] drive all of
//! it without a socket or a wall clock.
//!
//! The transaction client is phase 5 (`prompts/05-txn.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod clock;
mod error;
pub mod gate;
pub mod region_cache;
pub mod retry;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transport;
pub mod wire;

pub use error::{Error, Result};
pub use region_cache::{RegionCache, RegionResolver, Route, StaticRegion};
pub use retry::{BACKOFF_BASE_MS, BACKOFF_MAX_MS, MAX_RETRIES, RetryPolicy, backoff_ms};
pub use transport::Transport;
