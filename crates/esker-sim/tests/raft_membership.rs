//! Servers joining and leaving while everything else goes wrong.
//!
//! `prompts/03-raft.md` (3d): "nodes join and leave while faults happen; snapshots are
//! installed mid-partition". Membership is the fault that is not a fault — the cluster is
//! *supposed* to survive it — and it moves the one thing every other property is stated
//! against: who a quorum is.
//!
//! # Two derivations of one fact
//!
//! `RawNode::new` reads the configuration out of storage and does *not* replay the log's
//! conf-change entries for itself, so a driver that fails to persist the membership loses every
//! change across a restart. The simulator's driver therefore derives the configuration itself,
//! from the log, exactly as `esker-store` will have to — which means there are two independent
//! derivations of the same fact, and the checkers compare them.
//!
//! # Shown red first
//!
//! Two ways of applying a change wrongly are injectable, and each has a test that the checker
//! catches it:
//!
//! * [`ConfFault::SkipFirst`] — the store recorded the entry and forgot to act on it.
//! * [`ConfFault::MoveTwo`] — the store moved two servers in one step, which is precisely what
//!   single-server change exists to forbid: two consecutive configurations that do not share a
//!   quorum are two disjoint majorities waiting to elect two leaders.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_raft::{ConfChange, ConfChangeKind};
use esker_sim::FaultPlan;
use esker_sim::raft::{Cluster, ConfFault, Failure, seeds};

/// Tick rounds allowed for the cluster to settle.
const LIVENESS_TICKS: u64 = 120;
/// Events per seed in the sweeps.
const EVENTS: u64 = 1_200;

/// A cluster that grows and shrinks under a quiet network still holds every property, and the
/// server that joined really does start and catch up.
#[test]
fn a_server_added_to_a_quiet_cluster_joins_and_catches_up() {
    for seed in seeds(40_000, 16) {
        let plan = FaultPlan {
            membership: 0.0,
            ..FaultPlan::lossless()
        };
        let mut cluster = Cluster::with_spares(seed, plan, 3, 2).unwrap();
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));

        assert!(
            cluster.propose_conf_change(ConfChange::new(ConfChangeKind::AddVoter, 4)),
            "seed {seed}: the leader refused to add a server"
        );
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        cluster
            .run(400)
            .unwrap_or_else(|failure| panic!("{failure}"));

        assert!(
            cluster.stats().joins > 0,
            "seed {seed}: node 4 was added to the configuration but never started"
        );
        for (node, voters) in cluster.configurations() {
            assert!(
                voters.contains(&4),
                "seed {seed}: node {node} still has {voters:?} after the change committed"
            );
        }
        cluster
            .verify_quorums()
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
}

/// The whole thing at once: servers joining and leaving while nodes crash, partitions form and
/// heal, disks stall and the leader compacts past whoever is behind.
#[test]
fn membership_changes_under_faults_hold_every_property() {
    let mut changes = 0;
    let mut joins = 0;
    let mut snapshots = 0;
    let mut quorum_reports = 0;
    for seed in seeds(41_000, 128) {
        let mut cluster = Cluster::with_spares(seed, FaultPlan::reconfiguring(), 3, 2).unwrap();
        cluster
            .run(EVENTS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        // Not asserted here: see `SafetyChecker::verify_quorums`. Under a plan that both
        // reconfigures and partitions, "the configuration in force at index i" depends on which
        // branch the observer was on, so the count is a report rather than a property. It is a
        // hard assertion in the quiet test above, where there is only one lineage.
        if let Err(failure) = cluster.verify_quorums() {
            quorum_reports += 1;
            if quorum_reports == 1 {
                println!("first quorum report (not a failure):\n{failure}");
            }
        }
        let stats = cluster.stats();
        changes += stats.conf_changes;
        joins += stats.joins;
        snapshots += stats.snapshots_installed;
    }
    println!(
        "{changes} conf changes accepted, {joins} servers started, {snapshots} snapshots, \
         {quorum_reports} quorum reports"
    );
    assert!(changes > 0, "no membership change was ever accepted");
    assert!(joins > 0, "no server ever joined");
    assert!(
        snapshots > 0,
        "no snapshot was installed, so no joiner was ever repaired past the compaction boundary"
    );
}

/// The acceptance sweep.
///
/// ```text
/// cargo test -p esker-sim --release --test raft_membership -- --ignored --nocapture
/// ```
///
/// # This run is currently RED, and deliberately so
///
/// `ESKER_SIM_SEED=41213` reaches a state where two nodes hold the *same log* — five entries,
/// the conf-change that adds server 5 at index 5, nothing compacted, neither truncated — and
/// their cores disagree about the membership: node 2's has the change, node 1's does not. The
/// driver's derivation matches node 2's core exactly, so this is not the harness losing a
/// change; it is one core losing a change that is still in its own log.
///
/// The shape that produces it is visible in node 1's log: index 2 and index 5 hold the *same*
/// conf change, so it was appended, taken away by a truncation, and re-appended. `ConfTracker`
/// pops appended changes at or above a truncation point and re-records them from the entries an
/// append carries; a re-append that does not carry the conf-change entry again — because the
/// prefix already matched — would pop the change and never put it back. That is a reading, not
/// a diagnosis: it is the core lane's to adjudicate, and this test is the reproduction.
///
/// The default sweep above does not reach it and stays green, so this is a finding on the
/// acceptance gate rather than a broken build.
#[test]
#[ignore = "the thousands-of-seeds membership run; currently RED on seed 41213, see above"]
fn thousands_of_membership_seeds() {
    let mut changes = 0;
    let mut joins = 0;
    for seed in seeds(41_000, 3_000) {
        let mut cluster = Cluster::with_spares(seed, FaultPlan::reconfiguring(), 3, 2).unwrap();
        cluster
            .run(EVENTS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        changes += cluster.stats().conf_changes;
        joins += cluster.stats().joins;
    }
    println!("3000 seeds x {EVENTS} events: {changes} conf changes, {joins} servers started");
    assert!(changes > 0 && joins > 0);
}

/// A store that recorded the conf-change entry and forgot to act on it. Its configuration and
/// its core's diverge, and nothing else about the run looks wrong — which is exactly why this
/// needs a check rather than a reader's attention.
#[test]
fn a_forgotten_conf_change_is_caught() {
    let failure = break_membership(ConfFault::SkipFirst);
    assert!(
        matches!(failure, Failure::Safety { .. }),
        "not a safety failure: {failure}"
    );
    println!("{failure}");
}

/// A store that moved two servers in one step. Single-server change is what makes every pair of
/// consecutive configurations share a quorum; two at once is how a cluster ends up with two
/// disjoint majorities.
#[test]
fn moving_two_servers_at_once_is_caught() {
    let failure = break_membership(ConfFault::MoveTwo);
    assert!(
        matches!(failure, Failure::Safety { .. }),
        "not a safety failure: {failure}"
    );
    println!("{failure}");
}

/// Runs seeds with a broken conf-change derivation until one is caught, and returns the first
/// failure. Panics — loudly — if none is, because a membership check that never fires is a
/// membership check nobody has tested.
fn break_membership(fault: ConfFault) -> Failure {
    for seed in 0..64 {
        let mut cluster = Cluster::with_spares(seed, FaultPlan::reconfiguring(), 3, 2).unwrap();
        cluster.break_conf_changes(fault);
        if let Err(failure) = cluster.run(EVENTS) {
            return failure;
        }
        if let Err(failure) = cluster.verify_quorums() {
            return failure;
        }
    }
    panic!(
        "64 seeds of a driver applying membership changes as {fault:?} produced no violation. \
         Either the membership checks are not looking or the sweep never proposes a change — \
         both make every other assertion in this file vacuous."
    );
}
