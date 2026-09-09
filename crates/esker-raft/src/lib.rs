//! Raft as a pure state machine, modelled on the `RawNode`/`Ready` split: time enters through
//! `tick()`, messages through `step()`, and every effect leaves through a `Ready` that the
//! caller must persist before acting on. Leader election, log replication, pre-vote,
//! check-quorum, `ReadIndex`, snapshots and single-server membership change live here
//! (`docs/DESIGN.md` §5).
//!
//! # Invariants
//!
//! * **No threads, no timers, no sockets, no file I/O.** This is what makes the algorithm
//!   simulatable and model-checkable, and it is not traded away for convenience
//!   (`CLAUDE.md` invariant 4).
//! * **Every decision is a function of `(state, message | tick)`.** No wall clock, no ambient
//!   randomness: the election timeout is drawn from an injected `esker_base::rng::Pcg32`.
//! * **The driver contract is part of correctness.** The caller persists `hard_state` and
//!   entries — with fsync — *before* sending any message from the same `Ready`. Violating the
//!   order breaks Raft's safety guarantee, so the simulator tests it explicitly.
//! * **Byte-opaque.** Proposals are opaque payloads; nothing here interprets a key.
//!
//! The plan for what is built here, in what order, and what each lane owns, is
//! `docs/plans/phase-3.md`; `docs/raft-spec.md` maps every rule of the dissertation's Figure 3.1
//! to the function that implements it.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod conf;
mod config;
mod core;
mod election;
mod error;
mod log;
mod message;
mod progress;
#[cfg(test)]
mod proptests;
mod raw_node;
mod readonly;
mod replication;
mod snapshot;
mod storage;
#[cfg(test)]
mod testkit;
mod transfer;
mod types;

pub use crate::config::{Config, MAX_SIZE_PER_MSG};
pub use crate::core::{Role, Status};
pub use crate::error::{RaftError, Result};
pub use crate::message::Message;
pub use crate::raw_node::{RawNode, Ready};
pub use crate::storage::{InitialState, LogStorage, MemStorage};
pub use crate::types::{
    ConfChange, ConfChangeKind, ConfState, Counters, Entry, EntryKind, HardState, Index, NodeId,
    PeerProgress, ReadState, Snapshot, SnapshotMeta, SnapshotStatus, Term,
};

/// Wall-clock duration a caller should map onto one `tick()`, in milliseconds. The core
/// counts ticks and never reads a clock itself (`docs/DESIGN.md` §14).
pub const TICK_MS: u64 = 100;

/// Lower bound of the randomised election timeout, in ticks.
pub const ELECTION_TIMEOUT_MIN_TICKS: u64 = 10;

/// Upper bound of the randomised election timeout, in ticks. The spread is what stops two
/// followers from campaigning in lockstep forever.
pub const ELECTION_TIMEOUT_MAX_TICKS: u64 = 20;

/// Ticks between heartbeats from a leader.
pub const HEARTBEAT_TICKS: u64 = 2;

/// How many append messages may be in flight to one follower before the leader stops sending.
pub const MAX_INFLIGHT_MSGS: usize = 256;

/// Ticks a leader waits for a snapshot in flight before offering it again.
///
/// The **second** line, not the first: a driver that finishes or fails a transfer says so with
/// [`RawNode::report_snapshot`] in milliseconds. This covers what no report can — the driver that
/// never got to say anything, and, the case that actually stranded a replica, the announcement
/// that was lost before any transfer began, where nobody owes a report at all.
///
/// Ten seconds at [`TICK_MS`], which is one store-heartbeat interval (`docs/DESIGN.md` §14): a
/// lost announcement is repaired before a scheduler could notice the stall. Cutting short a
/// transfer genuinely in progress is not the hazard it looks like — the leader re-*offers*, and an
/// offer carries no data, so the cost is one message every ten seconds and a receiver that is
/// already fetching ignores it.
pub const SNAPSHOT_TIMEOUT_TICKS: u64 = 100;

#[cfg(test)]
mod tests {
    use super::{
        ELECTION_TIMEOUT_MAX_TICKS, ELECTION_TIMEOUT_MIN_TICKS, HEARTBEAT_TICKS, MAX_INFLIGHT_MSGS,
        TICK_MS,
    };

    /// Raft's liveness argument requires the election timeout to be a comfortable multiple of
    /// the broadcast interval. If a heartbeat cannot cross the network and be processed
    /// several times over before a follower gives up, the cluster churns leaders instead of
    /// making progress.
    #[test]
    fn election_timeout_dominates_the_heartbeat_interval() {
        assert!(ELECTION_TIMEOUT_MIN_TICKS >= HEARTBEAT_TICKS * 5);
        assert!(ELECTION_TIMEOUT_MAX_TICKS > ELECTION_TIMEOUT_MIN_TICKS);
    }

    /// The randomised window has to be wide enough that split votes are unlikely; a window of
    /// one tick is not randomisation.
    #[test]
    fn election_timeout_window_is_wide_enough_to_break_ties() {
        let window = ELECTION_TIMEOUT_MAX_TICKS - ELECTION_TIMEOUT_MIN_TICKS;
        assert!(window >= ELECTION_TIMEOUT_MIN_TICKS / 2);
    }

    #[test]
    fn timing_constants_are_usable() {
        assert!(TICK_MS > 0);
        assert!(MAX_INFLIGHT_MSGS > 0);
        // A default election timeout of one to two seconds at 100 ms per tick.
        assert_eq!(ELECTION_TIMEOUT_MIN_TICKS * TICK_MS, 1_000);
        assert_eq!(ELECTION_TIMEOUT_MAX_TICKS * TICK_MS, 2_000);
    }
}
