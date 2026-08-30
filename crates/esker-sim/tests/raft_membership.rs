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
use esker_sim::raft::{Census, Cluster, ConfFault, Failure, seeds};

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
/// # It runs every seed, and reports a census
///
/// A sweep that stops at its first failing seed reports one bug and hides the rest. That is how
/// a red acceptance run stays red one reason at a time: each fix reveals the next, and nobody
/// ever finds out how many there are. So this one runs all 3000 seeds and prints a
/// [`Census`] — the classes, the seeds in each, and one whole failure per class to start
/// from — and fails at the end if any of them failed.
///
/// It earned that shape the hard way. `ESKER_SIM_SEED=41213`, the membership finding this note
/// was opened for, was fixed in `esker-raft` by 987d472 — and turned out to have been standing in
/// front of four other classes the sweep had never reached. The reading recorded here at the time
/// was also wrong, instructively: index 2 and index 5 held the same conf change because they were
/// two proposals with a `remove` between them, not because one was truncated and re-appended.
/// Nothing on node 1 was ever truncated. What broke was §4.1 applied to an entry that had not
/// been appended.
///
/// # What it took to get clean
///
/// Twenty-seven of these 3000 seeds failed when the census was first taken, and they were never
/// twenty-seven bugs. They were five, and the classes fell in groups because one cause is seen by
/// several checkers at once:
///
/// * A configuration change re-applied from a *duplicate* append, which dropped every later
///   change off the tracker's stack while the entries stayed in the log (987d472). Thirteen
///   seeds, all of them `membership: core against its log`.
/// * A leader proposing a configuration change before it had committed an entry of its own term
///   (b668af3). That is how `ESKER_SIM_SEED=42650` got two leaders in term 12 — node 2 under
///   `[1, 2, 3]`, node 3 under `[1, 3, 5]`, one server either side of a common `[1, 2, 3, 5]` and
///   so two servers, and no shared quorum, from each other. It took election safety, all three
///   leader-completeness seeds, the snapshot-metadata seed and four committed-twice seeds with
///   it. The first version of that rule tested the wrong thing — whether the *inherited* tail was
///   committed, which is satisfied on the spot by a leader whose log was already fully committed
///   when it won — and `ESKER_SIM_SEED=53017`, past the 3000, walked straight through it. The
///   test is now the leader's own entry, which is the thing that actually settles the branch.
/// * This harness reading a compaction boundary out of storage against a commit index out of the
///   core, which are two different logs for as long as a snapshot the core has accepted has not
///   been written (15f34fc). Three committed-twice seeds and one membership seed.
/// * A joining server seeded with the configuration that named it rather than the one at index 0,
///   so its own derivation started by asserting a membership its log does not justify (0dec4ba).
/// * A restarted node holding a configuration it could not revert, because `RawNode::new` did not
///   replay the log's conf-change tail onto an anchor (570d455, a08883d). `ESKER_SIM_SEED=42705`:
///   the change was truncated away, the configuration stayed, and node 2 won a term with two of
///   its three imagined voters. The anchor is what a restart replays *from*, and there was only
///   one to hand at first — a snapshot's metadata, which says which index its membership is as
///   of. That left every node that restarted before it had ever compacted still holding the hole,
///   which `ESKER_SIM_SEED=114249` walked into at 100000 seeds. So the anchor is now what
///   `InitialState::conf_state` means: the membership as of the index the log begins after,
///   rather than as of the last entry. A driver persists what predates the log it is keeping, and
///   the core derives the rest from the entries — which is the same rule the rest of this system
///   already follows, since a snapshot has always carried its membership that way.
///
/// It is green now, over the 3000 and over `ESKER_SIM_SEEDS=100000`. The census is what keeps
/// that honest: a sweep that stops at its first failing seed would have reported each of these as
/// "the" bug in turn.
///
/// # Past the gate
///
/// `ESKER_SIM_SEEDS` widens the sweep, and widening it is how the last two of those were found at
/// all: 3000 seeds is what CI can afford, not a claim about where the bugs stop. The last five
/// only showed up past 60000, and every one of them was the same restart hole seen through a
/// different checker.
#[test]
#[ignore = "the thousands-of-seeds membership run; minutes, not seconds"]
fn thousands_of_membership_seeds() {
    let mut changes = 0;
    let mut joins = 0;
    let mut census = Census::default();
    let sweep = seeds(41_000, 3_000);
    let count = sweep.len();
    for seed in sweep {
        let mut cluster = Cluster::with_spares(seed, FaultPlan::reconfiguring(), 3, 2).unwrap();
        census.record(cluster.run(EVENTS));
        changes += cluster.stats().conf_changes;
        joins += cluster.stats().joins;
    }
    println!("{count} seeds x {EVENTS} events: {changes} conf changes, {joins} servers started");
    assert!(changes > 0 && joins > 0);
    assert!(census.is_clean(), "{census}");
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
