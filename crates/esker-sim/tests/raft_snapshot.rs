//! Snapshots under faults: a follower that fell behind the compaction boundary.
//!
//! `prompts/03-raft.md` (3d) asks for "nodes join and leave while faults happen; snapshots are
//! installed mid-partition". This is the second half. The first — membership change — is not
//! wired yet; see the note at the bottom of this file.
//!
//! The scenario only means something if the follower *cannot* be repaired by `AppendEntries`:
//! the leader must have compacted past the entries it is missing. So the plan compacts
//! aggressively, and the test asserts that snapshots were actually installed. A run in which
//! nothing was ever compacted would pass every property while testing nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::FaultPlan;
use esker_sim::raft::{Cluster, seeds};

/// The liveness bound, in tick rounds. Same reasoning as `tests/raft_sweep.rs`, with room for
/// a snapshot to be built, sent and installed on top of an election.
const LIVENESS_TICKS: u64 = 120;

/// A node is cut off, the leader compacts past it, and then the partition heals. The only way
/// back for that node is a snapshot.
#[test]
fn a_follower_cut_off_past_the_compaction_boundary_is_repaired_by_a_snapshot() {
    let mut installed = 0_u64;
    let mut compacted = 0_u64;

    for seed in seeds(7_000, 24) {
        let plan = FaultPlan {
            compact_after: 2,
            ..FaultPlan::lossless()
        };
        let mut cluster = Cluster::new(seed, plan, 3).unwrap();

        // Elect a leader and get a few entries committed with everyone present.
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));

        // Cut off a node that is *not* the leader, then keep committing without it. The leader
        // compacts as it applies, so the entries the absent node is missing stop existing.
        let leader = cluster.leader().expect("settled without a leader");
        let victim = (1..=3).find(|id| *id != leader).expect("three nodes");
        cluster.partition(&[victim]);
        for _ in 0..12 {
            cluster
                .settle(LIVENESS_TICKS)
                .unwrap_or_else(|failure| panic!("{failure}"));
        }

        // Heal, and require the cut-off node to catch up.
        cluster.heal().unwrap_or_else(|failure| panic!("{failure}"));
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        cluster
            .run(400)
            .unwrap_or_else(|failure| panic!("{failure}"));

        let stats = cluster.stats();
        installed += stats.snapshots_installed;
        compacted += stats.compactions;
    }

    assert!(
        compacted > 0,
        "no node ever compacted, so no follower could ever have needed a snapshot"
    );
    assert!(
        installed > 0,
        "{compacted} compactions but not one snapshot was installed: the cut-off follower was \
         repaired by appends, which means the scenario never actually outran the boundary"
    );
}

/// The same repair, over a network that loses messages: **every** seed must still repair the
/// follower, not just most of them.
///
/// A snapshot the network loses is the one an `AppendEntries` retry cannot cover, because the
/// leader stops sending to a peer it has offered a snapshot — `ProgressState::Snapshot` is paused
/// until the follower acknowledges, and a follower that received nothing has nothing to
/// acknowledge. The offer is a single message, so on a lossy link losing it is ordinary.
///
/// The sweeps above sum `snapshots_installed` across seeds and so pass with a few followers
/// stranded; this asserts per seed, which is what makes the retry the thing under test. The only
/// mechanism that can satisfy it here is `SNAPSHOT_TIMEOUT_TICKS`: the sim drives `RawNode`
/// directly and streams no bytes, so there is no driver to report an outcome
/// (`docs/plans/phase-4.md` §15).
#[test]
fn a_snapshot_the_network_loses_is_offered_again() {
    for seed in seeds(7_500, 16) {
        let plan = FaultPlan {
            compact_after: 2,
            ..FaultPlan::lossless()
        };
        let mut cluster = Cluster::new(seed, plan, 3).unwrap();
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));

        let leader = cluster.leader().expect("settled without a leader");
        let victim = (1..=3).find(|id| *id != leader).expect("three nodes");
        cluster.partition(&[victim]);
        for _ in 0..12 {
            cluster
                .settle(LIVENESS_TICKS)
                .unwrap_or_else(|failure| panic!("{failure}"));
        }

        // Healed, but onto a link that drops one message in three. The first offer is very likely
        // lost, and nothing but a re-offer follows it.
        cluster.heal().unwrap_or_else(|failure| panic!("{failure}"));
        cluster.calm(FaultPlan {
            drop: 0.34,
            compact_after: 2,
            ..FaultPlan::lossless()
        });
        cluster
            .run(1_200)
            .unwrap_or_else(|failure| panic!("{failure}"));

        assert!(
            cluster.stats().snapshots_installed > 0,
            "seed {seed}: the follower was offered a snapshot the network lost and was never \
             offered another, so it is stranded for the rest of the term"
        );
    }
}

/// The same faults as the main sweep, plus compaction. Every property, every event, hundreds of
/// seeds — this is where a snapshot that disagrees with what was committed would be caught.
#[test]
fn seeds_with_compaction_hold_every_safety_property() {
    let mut installed = 0_u64;
    let mut compacted = 0_u64;
    for seed in seeds(8_000, 64) {
        let mut cluster = Cluster::new(seed, FaultPlan::compacting(), 3).unwrap();
        cluster
            .run(800)
            .unwrap_or_else(|failure| panic!("{failure}"));
        let stats = cluster.stats();
        installed += stats.snapshots_installed;
        compacted += stats.compactions;
    }
    assert!(compacted > 0, "the compacting plan compacted nothing");
    assert!(
        installed > 0,
        "{compacted} compactions across the sweep and not one snapshot installed"
    );
}

/// Five nodes, compaction, and a partition that can strand a quorum on either side.
#[test]
fn five_nodes_with_compaction_hold_every_safety_property() {
    for seed in seeds(8_500, 32) {
        let mut cluster = Cluster::new(seed, FaultPlan::compacting(), 5).unwrap();
        cluster
            .run(800)
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
}

// TODO(phase-3d): membership change. `RawNode::propose_conf_change` exists, but the harness has
// no action that adds or removes a voter, and the checkers assume a fixed membership (the
// quorum they count against is the one the cluster started with). Wiring it needs the
// membership to become part of the observation so that a quorum is computed per configuration.
// Reported to the core lane rather than guessed at here.
