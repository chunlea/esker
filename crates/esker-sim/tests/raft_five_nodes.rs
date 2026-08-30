//! Five nodes, where §5.4.2's interleaving is reachable.
//!
//! The stateright model next door checks three nodes exhaustively and cannot reach the
//! figure-8 case: with a quorum of two, an entry on a majority is on two of three, so every
//! later candidate must ask one of them for a vote and §5.4.1 makes it refuse. The overwrite
//! §5.4.2's term condition prevents needs a candidate that can win *while a minority holds the
//! entry*, and that needs five nodes. This is where the real implementation meets it.
//!
//! So the sweep does not merely run: it counts. `replicated_overwrites` is the number of times
//! an entry that another node had already recorded was overwritten in some node's log — which
//! is precisely "replicated beyond its leader, then outrun". A run that never produces one has
//! not tested the term condition however many seeds it burned, and the assertion says so.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::FaultPlan;
use esker_sim::raft::{Cluster, Stats, seeds};

/// Seeds in the default sweep.
const SEEDS: u64 = 240;
/// Seeds in the acceptance sweep.
const SEEDS_ACCEPTANCE: u64 = 4_000;
/// Events per seed. Longer than the three-node sweep: five nodes need more elections before a
/// minority ends up holding something alone.
const EVENTS: u64 = 1_200;

fn sweep(seeds: impl IntoIterator<Item = u64>, events: u64) -> Stats {
    let mut total = Stats::default();
    for seed in seeds {
        let mut cluster = Cluster::new(seed, FaultPlan::figure_eight(), 5).unwrap();
        if let Err(failure) = cluster.run(events) {
            panic!("{failure}");
        }
        let stats = cluster.stats();
        total.elections += stats.elections;
        total.crashes += stats.crashes;
        total.restarts += stats.restarts;
        total.partitions += stats.partitions;
        total.proposals += stats.proposals;
        total.sent += stats.sent;
        total.replicated_overwrites += stats.replicated_overwrites;
        total.committed += stats.committed;
        total.events += stats.events;
    }
    total
}

/// The default sweep: hundreds of seeds, every property after every event, and the §5.4.2
/// interleaving actually reached.
#[test]
fn five_nodes_reach_the_figure_eight_interleaving_and_stay_safe() {
    let total = sweep(seeds(30_000, SEEDS), EVENTS);
    println!(
        "5 nodes x {SEEDS} seeds: {} elections, {} partitions, {} crashes, {} restarts, \
         {} proposals, {} replicated-then-overwritten entries",
        total.elections,
        total.partitions,
        total.crashes,
        total.restarts,
        total.proposals,
        total.replicated_overwrites,
    );
    assert!(total.committed > 0, "nothing was ever committed");
    assert!(
        total.replicated_overwrites > 0,
        "not one entry was replicated beyond its leader and then overwritten. That is the \
         interleaving §5.4.2's term condition exists for, and this sweep has not tested it — \
         the plan needs retuning, not the assertion relaxing."
    );
}

/// The acceptance sweep: thousands of seeds.
///
/// ```text
/// cargo test -p esker-sim --release --test raft_five_nodes -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the thousands-of-seeds five-node run"]
fn thousands_of_five_node_seeds() {
    let total = sweep(seeds(30_000, SEEDS_ACCEPTANCE), EVENTS);
    println!(
        "5 nodes x {SEEDS_ACCEPTANCE} seeds x {EVENTS} events: {} elections, {} partitions, \
         {} crashes, {} restarts, {} slow-disk-free proposals, {} messages, \
         {} replicated-then-overwritten entries, highest committed index {}",
        total.elections,
        total.partitions,
        total.crashes,
        total.restarts,
        total.proposals,
        total.sent,
        total.replicated_overwrites,
        total.committed,
    );
    assert!(total.replicated_overwrites > 0);
    assert_eq!(
        total.events,
        SEEDS_ACCEPTANCE * EVENTS,
        "a run stopped early"
    );
}

/// The counter has to mean what it says. A healthy cluster on a perfect network replicates
/// nothing that is then overwritten — every entry it writes is the leader's and stays — so if
/// this ever counted something, the counter would be measuring truncation in general rather
/// than the §5.4.2 interleaving, and the assertion above would be worthless.
#[test]
fn a_healthy_cluster_overwrites_nothing_it_replicated() {
    for seed in seeds(32_000, 24) {
        let mut cluster = Cluster::new(seed, FaultPlan::perfect(), 5).unwrap();
        cluster
            .settle(120)
            .unwrap_or_else(|failure| panic!("{failure}"));
        cluster
            .run(400)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert_eq!(
            cluster.stats().replicated_overwrites,
            0,
            "seed {seed}: a perfect network overwrote a replicated entry"
        );
    }
}

/// The same five nodes with compaction on, so a node that comes back from a partition may need
/// a snapshot rather than an append.
#[test]
fn five_nodes_with_compaction_stay_safe() {
    for seed in seeds(31_000, 48) {
        let mut cluster = Cluster::new(seed, FaultPlan::compacting(), 5).unwrap();
        if let Err(failure) = cluster.run(EVENTS) {
            panic!("{failure}");
        }
    }
}
