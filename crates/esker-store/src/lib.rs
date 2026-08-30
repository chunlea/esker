//! One process, one store id, many regions: the apply loop, region splits, snapshot transfer
//! and the driver that turns a `Ready` from `esker-raft` into durable bytes in `esker-engine`.
//! This is the layer that owns the ordering rules the two pure cores cannot enforce for
//! themselves (`docs/DESIGN.md` §6).
//!
//! # Invariants
//!
//! * **Persist before send.** For every `Ready`: write `hard_state` and the new entries in one
//!   `WriteBatch` with `sync = true`, *then* send the messages, *then* apply committed
//!   entries, *then* `advance()`. This is `CLAUDE.md` invariant 1 at the consensus layer.
//! * **Every request carries a region epoch.** A stale epoch is rejected with a redirect hint;
//!   a store never serves a request for a range it no longer owns (invariant 5).
//! * **Regions tile the key space.** They are contiguous and non-overlapping, and the first
//!   region is `["", "")`. A split preserves that property atomically on every peer.
//! * **Apply is deterministic.** Every peer applying the same committed entry produces the
//!   same `WriteBatch`, including the `apply_index` written with it.
//!
//! # Module map
//!
//! | Module | What it decides |
//! |---|---|
//! | [`error`] | the store's failures, and which of them a client may safely retry |
//! | [`region`] | the epoch and key-range checks every request runs |
//! | [`regions`] | every region this store hosts, indexed by id and by range |
//! | [`apply`] | what a Raft entry carries, and what applying one does to the data |
//! | [`heartbeat`] | when a store talks to the placement driver, counted in ticks |
//! | [`meta`] | the `'m' ++ region_id` record: which regions this store hosts, on disk |
//! | [`pd`] | the placement driver's five store-facing methods, behind a trait |
//! | [`pd_remote`] | that trait over a socket: the one bridge between sync and async here |
//! | [`peer`] | one region's `RawNode`, its driver thread, and the `Ready` loop |
//! | [`raft_log`] | the Raft log and the peer's persistent state, on the `raft` column family |
//! | [`rawkv`] | the eight `RawKv` methods, over the engine, synchronously |
//! | [`server`] | opening the database and its column families, and the wire service |
//! | [`snapshot`] | shipping a region to a peer the log cannot catch up |
//! | [`split`] | when a region is split, and where |
//! | [`transport`] | one connection per store pair, carrying every region's messages per tick |
//!
//! Phase 2 builds the single-region store (`prompts/02-single-node-server.md`); the apply
//! loop, splits and snapshots are phase 4 (`prompts/04-multiraft-pd.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod apply;
pub mod error;
pub mod heartbeat;
pub mod meta;
pub mod pd;
pub mod pd_remote;
pub mod peer;
pub mod raft_log;
pub mod rawkv;
pub mod region;
pub mod regions;
pub mod server;
pub mod snapshot;
pub mod split;
pub mod transport;

pub use apply::Command;
pub use error::{Result, StoreError, engine_to_proto};
pub use heartbeat::{Heartbeats, RegionReport, StoreReport};
pub use pd::{Bootstrapped, PdClient, RegionHeartbeat, RegionRoute, StoreHeartbeat, StoreInfo};
pub use pd_remote::RemotePd;
pub use peer::{
    Applied, DiscardTransport, LogCompaction, NoHost, PeerOptions, RaftPeer, RaftTransport,
    RegionHost,
};
pub use raft_log::{PersistedState, RaftLogStorage};
pub use rawkv::Limits;
pub use region::{RegionMeta, request_range};
pub use regions::{RegionMap, RegionState};
pub use server::{Store, StoreOptions, StoreService};
pub use split::{SplitOptions, choose_split_key};
pub use transport::{PeerAddress, RegionTransport, StoreAddress, StoreTransport};

/// Key prefixes inside the `raft` column family (`docs/DESIGN.md` §6).
///
/// They share one column family, so they must not collide: a log entry key must never be
/// mistaken for a hard-state key by a range scan.
pub mod raft_cf {
    /// `'l' ++ region_id:u64 ++ index:u64` → one Raft log entry.
    pub const LOG_ENTRY: u8 = b'l';
    /// `'s' ++ region_id:u64` → hard state and apply state.
    pub const STATE: u8 = b's';
    /// `'m' ++ region_id:u64` → region metadata.
    pub const METADATA: u8 = b'm';
    /// `'p' ++ region_id:u64` → a snapshot is part-way into this region.
    ///
    /// Written before a receive touches anything and removed when the region is adopted, so a
    /// restart can tell a region that is complete from one that is half-built and must not be
    /// served (`docs/plans/phase-4.md` §13.1). A **separate prefix** rather than a field on the
    /// `'m'` record: that format has a golden test, and an addition beside it costs nothing while
    /// a change to it would need an ADR and a version bump.
    pub const PENDING_SNAPSHOT: u8 = b'p';

    /// Every prefix this column family uses.
    pub const ALL: [u8; 4] = [LOG_ENTRY, STATE, METADATA, PENDING_SNAPSHOT];
}

/// A region is split once it grows past this many bytes (`docs/DESIGN.md` §14).
pub const REGION_SPLIT_SIZE: u64 = 96 * 1024 * 1024;

/// Interval between store heartbeats to the placement driver, in milliseconds.
pub const STORE_HEARTBEAT_MS: u64 = 10_000;

/// Interval between region heartbeats from a leader, in milliseconds.
pub const REGION_HEARTBEAT_MS: u64 = 60_000;

/// Size of one chunk of a streamed Raft snapshot, in bytes.
pub const SNAPSHOT_CHUNK_SIZE: usize = 1024 * 1024;

/// How long a store waits for one of the placement driver's operators to commit.
///
/// A proposal is answered when it *applies*, and a membership change that cannot reach a quorum
/// never does. The wait is bounded so that the heartbeat round carrying the operator cannot stop —
/// which would close the only channel PD has to correct whatever it got wrong.
pub const OPERATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How many snapshot chunks may be queued for the network before the walk waits.
///
/// Small on purpose: a snapshot is megabytes and the point of streaming it is that neither end
/// holds a region in memory. Four chunks is enough to keep the socket busy while the next is read.
pub const SNAPSHOT_STREAM_DEPTH: usize = 4;

/// How far the log may run past its truncation point before it is compacted.
///
/// A log is kept because a follower that falls behind can be caught up from it, which is far
/// cheaper than shipping the region's files. Past this, the entries are more likely to be paid for
/// in disk than spent on a follower, and the region's data is the better answer.
pub const RAFT_LOG_COMPACT_THRESHOLD: u64 = 4096;

/// How many applied entries a compaction leaves behind it.
///
/// Not zero, and that is the point: a follower one entry behind the leader must not need a
/// snapshot. The tail is the window in which a brief network stall is repaired by a few
/// `AppendEntries` rather than by a megabyte of SSTs.
pub const RAFT_LOG_KEEP_ENTRIES: u64 = 1024;

#[cfg(test)]
mod tests {
    use super::{
        REGION_HEARTBEAT_MS, REGION_SPLIT_SIZE, SNAPSHOT_CHUNK_SIZE, STORE_HEARTBEAT_MS, raft_cf,
    };

    /// The three prefixes share one column family and one keyspace. A collision would let a
    /// scan over log entries read a region's hard state as if it were an entry.
    #[test]
    fn raft_cf_prefixes_are_distinct() {
        let unique: std::collections::BTreeSet<u8> = raft_cf::ALL.into_iter().collect();
        assert_eq!(
            unique.len(),
            raft_cf::ALL.len(),
            "two raft-CF prefixes collide"
        );
    }

    /// A region heartbeat is much rarer than a store heartbeat, because there are far more
    /// regions than stores; if that ordering inverts, the placement driver drowns.
    #[test]
    fn heartbeat_intervals_are_ordered() {
        assert!(REGION_HEARTBEAT_MS > STORE_HEARTBEAT_MS);
    }

    /// A snapshot has to be streamed in many chunks, otherwise chunking buys nothing.
    #[test]
    fn a_full_region_is_many_snapshot_chunks() {
        assert!(REGION_SPLIT_SIZE / SNAPSHOT_CHUNK_SIZE as u64 >= 32);
    }
}
