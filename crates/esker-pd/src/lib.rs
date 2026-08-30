//! The placement driver: cluster membership, the region routing table, the timestamp oracle
//! and the schedulers that repair and balance replicas. It keeps its own state in an
//! `esker-engine` instance and becomes highly available by replicating itself with
//! `esker-raft` (`docs/DESIGN.md` §7).
//!
//! # Invariants
//!
//! * **Timestamps come only from here.** No node uses its wall clock for ordering
//!   (`CLAUDE.md` invariant 6), and the oracle never hands out a timestamp smaller than one
//!   it has already given: a high-water mark is persisted ahead of the physical clock so a
//!   restart cannot go backwards.
//! * **One operator per region at a time.** A scheduler never issues a second operator for a
//!   region while one is in flight, and every operator has a timeout.
//! * **The routing table is advisory to clients and authoritative here.** Clients cache it and
//!   invalidate on epoch errors; correctness never depends on a client's cache being fresh.
//!
//! # Module map
//!
//! | Module | What it decides |
//! |---|---|
//! | [`error`] | what PD refuses, and the wire error each refusal becomes |
//! | [`clock`] | the one wall clock in the system, injected so tests can break it |
//! | [`keys`] | PD's private key space, and how a key lookup becomes one seek |
//! | [`record`] | the bytes of every record PD stores, and their strict decoders |
//! | [`alloc`] | ids that are never reused, because the batch end is persisted first |
//! | [`tso`] | timestamps that never repeat, because the mark is persisted ahead |
//! | [`routing`] | the region table: epoch-guarded upserts, and where a key lives |
//! | [`pd`] | the six operations, synchronous, over one database |
//! | [`service`] | the async edge: PD behind `esker-proto`'s server |
//!
//! Phase 4a builds a **single** durable PD. High availability — three PDs replicated with
//! `esker-raft`, the oracle's mark going through the log — is 4e, and scheduling is 4b–4d
//! (`prompts/04-multiraft-pd.md`, `docs/plans/phase-4-pd.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod alloc;
pub mod balance;
pub mod clock;
pub mod error;
pub mod keys;
pub mod operator;
pub mod pd;
pub mod record;
pub mod routing;
pub mod schedule;
pub mod service;
pub mod tso;

pub use balance::Balance;
pub use clock::{Clock, SystemClock};
pub use error::{PdError, Result};
pub use operator::{Cancelled, InFlight, Observed, Progress};
pub use pd::{Bootstrapped, Pd, PdOptions, RegionRoute};
pub use record::{ClusterRecord, RegionRecord, StoreRecord, StoreStats};
pub use routing::{RegionBeat, StoreBeat, Upsert};
pub use schedule::{Cluster, LoadDelta, Repair};
pub use service::PdService;

/// Bits of the logical counter in a timestamp: `ts = physical_ms << 18 | logical`
/// (`docs/DESIGN.md` §7). Part of the wire format — every timestamp on disk uses it.
pub const TSO_LOGICAL_BITS: u32 = 18;

/// Largest logical counter that fits beside a physical millisecond.
pub const TSO_MAX_LOGICAL: u64 = (1 << TSO_LOGICAL_BITS) - 1;

/// How far ahead of the physical clock the oracle persists its high-water mark, in
/// milliseconds. A restart resumes above the persisted value, so timestamps never repeat.
pub const TSO_SAVE_INTERVAL_MS: u64 = 3_000;

/// Packs a physical millisecond and a logical counter into one ordered timestamp.
///
/// The result compares in the same order as `(physical_ms, logical)` compare as a pair, which
/// is what lets MVCC keys sort by version. A `logical` above [`TSO_MAX_LOGICAL`] is truncated
/// to its low bits rather than corrupting the physical part; callers allocate in batches and
/// roll the physical part forward before that can happen.
#[must_use]
pub fn compose_ts(physical_ms: u64, logical: u64) -> u64 {
    (physical_ms << TSO_LOGICAL_BITS) | (logical & TSO_MAX_LOGICAL)
}

/// Splits a timestamp back into `(physical_ms, logical)`.
#[must_use]
pub fn decompose_ts(ts: u64) -> (u64, u64) {
    (ts >> TSO_LOGICAL_BITS, ts & TSO_MAX_LOGICAL)
}

#[cfg(test)]
mod tests {
    use super::{TSO_MAX_LOGICAL, compose_ts, decompose_ts};

    #[test]
    fn timestamps_round_trip() {
        for physical in [0u64, 1, 1_700_000_000_000, (1 << 46) - 1] {
            for logical in [0u64, 1, TSO_MAX_LOGICAL] {
                assert_eq!(
                    decompose_ts(compose_ts(physical, logical)),
                    (physical, logical)
                );
            }
        }
    }

    /// The whole point of the layout: a later timestamp is a larger integer, so MVCC keys
    /// sort by version without decoding anything.
    #[test]
    fn timestamps_are_ordered_like_the_pairs_they_encode() {
        let mut previous = 0;
        for physical in [10u64, 11, 12] {
            for logical in [0u64, 1, 2, TSO_MAX_LOGICAL] {
                let ts = compose_ts(physical, logical);
                assert!(
                    ts > previous,
                    "{physical}/{logical} did not advance the timestamp"
                );
                previous = ts;
            }
        }
    }

    /// A logical counter that overflows must not silently add a millisecond to the physical
    /// part; that would let two batches produce the same timestamp.
    #[test]
    fn logical_overflow_does_not_bleed_into_the_physical_part() {
        let (physical, _) = decompose_ts(compose_ts(42, TSO_MAX_LOGICAL + 1));
        assert_eq!(physical, 42);
    }

    /// A millisecond has to hold a useful batch of transactions.
    #[test]
    fn logical_space_is_large_enough_for_a_batch() {
        assert!(TSO_MAX_LOGICAL >= 262_143);
    }
}
