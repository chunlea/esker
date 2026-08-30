//! Percolator-style distributed transactions: optimistic, two-phase, snapshot isolation, over
//! the `lock`, `write` and `default` column families. Timestamps come from the placement
//! driver's oracle and are encoded into keys, so ordering never depends on a node's clock
//! (`docs/DESIGN.md` §8, and `docs/txn-spec.md` for the bytes).
//!
//! # Invariants
//!
//! * **The primary key decides.** A transaction is committed exactly when its primary's
//!   `write` record exists. Every reader that meets a lock resolves it by inspecting the
//!   primary — rolling forward if it committed, back if its TTL expired — and never by
//!   guessing. A secondary committed before its primary leaves a state the resolution rules
//!   classify *wrongly*, so [`commit_secondary`] cannot be called without a
//!   [`PrimaryCommitted`] token, and the only source of one is the primary's applied plan.
//! * **Prewrite is atomic per key.** The `lock` entry and the `default` value are one
//!   [`Mutations`] list, and so are the `write` entry and the lock's removal at commit; the
//!   store turns each into one `WriteBatch` through Raft.
//! * **Prewrite checks both column families.** `write` for a commit newer than the snapshot
//!   *and* `lock` for any lock. Either one alone is a silent isolation break.
//! * **Timestamps come only from the oracle** (`CLAUDE.md` invariant 6). Even lock expiry is
//!   measured between two timestamps, never against a node's wall clock — see [`is_expired`].
//! * **Garbage collection never removes a visible version.** Below the safepoint the newest
//!   visible version of each key survives; everything older may go, except a rollback marker,
//!   which lives until the safepoint passes its `start_ts` (`docs/txn-spec.md` §7).
//! * **Nothing here panics on stored bytes.** A malformed record is a [`TxnError::Corrupt`]
//!   (invariant 9).
//!
//! # Layout
//!
//! This crate is a **library of decisions, not a service**. [`key`] builds the engine keys,
//! [`codec`] the record bytes, [`snapshot`] is the five-question trait the store implements
//! over an engine snapshot, and [`percolator`] is every rule of the protocol as a pure
//! function from a snapshot to a [`Mutations`] list. There are no threads, no sockets and no
//! clock in here; the `esker-store` handler that phase 4 unblocks is `decode → call → write
//! batch through Raft`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod codec;
pub mod error;
pub mod key;
pub mod mutation;
pub mod percolator;
pub mod snapshot;

pub use codec::{Kind, LOCK_TTL_MS, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};
pub use error::{Result, TxnError};
pub use mutation::{Cf, Mutation, Mutations};
pub use percolator::{
    CommitDecision, Op, Prewrite, PrewriteDecision, PrimaryCommit, PrimaryCommitted, PrimaryState,
    ReadOutcome, Resolution, check_prewrite, commit_primary, commit_secondary, primary_state, read,
    resolve, rollback,
};
pub use snapshot::{TxnSnapshot, Version};

/// Bits of the logical counter in a timestamp: `ts = physical_ms << 18 | logical`.
///
/// A copy of `esker_pd::TSO_LOGICAL_BITS`, which is where the oracle defines it. It is copied
/// rather than imported because `esker-txn` sits above the placement driver in
/// `CLAUDE.md`'s layer table and must not link it; `tests/tso_format.rs` asserts the two are
/// still equal, so the copy cannot drift silently — and a silent drift here would scale every
/// lock TTL by 262,144.
pub const TSO_LOGICAL_BITS: u32 = 18;

/// The physical millisecond a timestamp was minted in.
///
/// The only thing in Esker that reads a wall-clock quantity out of a timestamp, and it is used
/// for exactly one purpose: deciding whether a lock's TTL has run out. Both operands are
/// timestamps from the oracle, so `CLAUDE.md` invariant 6 holds — no node consults its own
/// clock to decide that another node's transaction is dead.
#[must_use]
pub fn physical_ms(ts: u64) -> u64 {
    ts >> TSO_LOGICAL_BITS
}

/// Whether a lock minted at `start_ts` with `ttl_ms` to live is expired as of `now_ts`.
///
/// Both timestamps come from the oracle. The comparison is strict, so a lock is live for
/// exactly `ttl_ms` milliseconds: a resolver at the boundary waits rather than rolling back a
/// transaction that is still inside its lease, which is the direction that costs latency
/// instead of correctness.
#[must_use]
pub fn is_expired(start_ts: u64, ttl_ms: u64, now_ts: u64) -> bool {
    physical_ms(now_ts) > physical_ms(start_ts).saturating_add(ttl_ms)
}

#[cfg(test)]
mod tests {
    use super::{LOCK_TTL_MS, TSO_LOGICAL_BITS, is_expired, physical_ms};

    /// The TTL has to outlast a heartbeat interval by enough that an ordinary pause does not
    /// look like a crashed client.
    #[test]
    fn lock_ttl_leaves_room_for_a_heartbeat() {
        assert!(LOCK_TTL_MS >= 1_000);
    }

    fn ts(physical_ms: u64, logical: u64) -> u64 {
        (physical_ms << TSO_LOGICAL_BITS) | logical
    }

    /// The logical counter must not leak into the physical reading, or two timestamps from the
    /// same millisecond would look milliseconds apart.
    #[test]
    fn the_logical_counter_is_not_time() {
        let max_logical = (1 << TSO_LOGICAL_BITS) - 1;
        assert_eq!(
            physical_ms(ts(1_700_000_000_000, max_logical)),
            1_700_000_000_000
        );
        assert_eq!(physical_ms(ts(0, max_logical)), 0);
    }

    /// The boundary: a lock is live for exactly its TTL and expires the millisecond after.
    /// Waiting a millisecond too long costs latency; rolling back a millisecond too early
    /// aborts a healthy transaction, so the comparison leans the way it does.
    #[test]
    fn a_lock_expires_the_millisecond_after_its_ttl() {
        let start = ts(1_000, 0);
        assert!(!is_expired(start, 3_000, ts(1_000, 0)), "at the start");
        assert!(!is_expired(start, 3_000, ts(3_999, 7)), "just inside");
        assert!(
            !is_expired(start, 3_000, ts(4_000, 0)),
            "exactly at the TTL"
        );
        assert!(is_expired(start, 3_000, ts(4_001, 0)), "one ms past");
        // A logical counter alone never expires a lock.
        assert!(!is_expired(
            start,
            3_000,
            ts(4_000, (1 << TSO_LOGICAL_BITS) - 1)
        ));
    }

    /// A TTL big enough to overflow must read as "not expired", not wrap into the past.
    #[test]
    fn an_absurd_ttl_does_not_wrap() {
        assert!(!is_expired(ts(1, 0), u64::MAX, u64::MAX));
    }
}
