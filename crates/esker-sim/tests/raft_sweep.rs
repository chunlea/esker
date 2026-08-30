//! Seed sweeps: hundreds of runs per fault mix by default, thousands behind `--ignored`.
//!
//! Every failure prints `ESKER_SIM_SEED=n` first, and setting that variable runs that one seed,
//! so a red line in CI is a command to paste rather than a mystery to reconstruct.
//!
//! ```text
//! ESKER_SIM_SEED=4171 cargo test -p esker-sim --test raft_sweep
//! ```
//!
//! The sweeps assert three things, and the third is the one that is easy to forget: the
//! properties hold, the cluster still makes progress once the faults stop, and **the faults
//! actually happened**. A sweep that injected nothing is a sweep that proves nothing, so every
//! run's counters are summed and the totals are asserted to be non-zero.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::FaultPlan;
use esker_sim::raft::{Cluster, Stats, seeds};

/// Events per seed in the default sweep. Long enough for several elections and a few hundred
/// replicated entries, short enough that a few hundred seeds stay under a second each.
const EVENTS: u64 = 800;

/// The liveness bound, in tick rounds after every fault has been healed.
///
/// A tick is 100 ms (`esker_raft::TICK_MS`) and the randomised election timeout is 10–20 ticks
/// (`ELECTION_TIMEOUT_MIN_TICKS`/`MAX`). A worst case is: the current leader has just been
/// killed, so up to 20 ticks pass before anyone campaigns; a split vote costs another full
/// timeout; the winner then needs a round trip to replicate and commit. Three timeouts plus
/// slack is 80, and the bound is set at **120 ticks**.
///
/// Measured, on a lossless network over 32 seeds: the worst settle takes **19 ticks** — one
/// election timeout and a round trip, which is what the argument above predicts.
/// `the_liveness_bound_is_not_tight` prints that number and fails if it ever climbs past half
/// the bound, so the bound cannot quietly become a lie.
const LIVENESS_TICKS: u64 = 120;

/// Adds up the counters over a sweep so the "did the faults actually bite" assertion can be
/// made once at the end rather than per seed.
#[derive(Default)]
struct Totals {
    stats: Vec<Stats>,
}

impl Totals {
    fn add(&mut self, stats: Stats) {
        self.stats.push(stats);
    }

    fn sum(&self, field: impl Fn(&Stats) -> u64) -> u64 {
        self.stats.iter().map(field).sum()
    }
}

/// Runs one seed of one plan and returns its counters.
fn sweep_one(seed: u64, plan: &FaultPlan, nodes: usize, events: u64) -> Stats {
    let mut cluster = Cluster::new(seed, plan.clone(), nodes).unwrap_or_else(|error| {
        panic!("{error}");
    });
    if let Err(failure) = cluster.run(events) {
        panic!("{failure}");
    }
    cluster.stats()
}

/// A run is a function of its seed and nothing else. Two clusters built the same way must do
/// exactly the same things — otherwise a printed seed is not a reproduction.
#[test]
fn the_same_seed_replays_exactly() {
    for seed in seeds(9_000, 12) {
        let plan = FaultPlan::chaotic();
        let mut first = Cluster::new(seed, plan.clone(), 5).unwrap();
        let mut second = Cluster::new(seed, plan, 5).unwrap();
        for event in 0..300 {
            let left = first.step();
            let right = second.step();
            assert_eq!(
                left.is_ok(),
                right.is_ok(),
                "ESKER_SIM_SEED={seed}: the two runs diverged at event {event}"
            );
            assert_eq!(
                first.trace_report(),
                second.trace_report(),
                "ESKER_SIM_SEED={seed}: the two runs took different actions at event {event}"
            );
        }
        assert_eq!(first.stats(), second.stats(), "ESKER_SIM_SEED={seed}");
    }
}

/// A cluster with nothing wrong with it elects a leader and commits, and does so quickly.
#[test]
fn a_healthy_cluster_elects_a_leader_and_commits() {
    for seed in seeds(100, 32) {
        let mut cluster = Cluster::new(seed, FaultPlan::perfect(), 3).unwrap();
        let settled = cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!(
            settled.ticks < LIVENESS_TICKS,
            "ESKER_SIM_SEED={seed}: a healthy cluster needed {} ticks",
            settled.ticks
        );
        assert!(settled.index > 0);
    }
}

/// The bound is documented as generous. If the observed worst case ever climbs into it, either
/// the algorithm has regressed or the bound is now a lie — and this test says which.
#[test]
fn the_liveness_bound_is_not_tight() {
    let mut worst = 0;
    for seed in seeds(500, 32) {
        let mut cluster = Cluster::new(seed, FaultPlan::lossless(), 3).unwrap();
        let settled = cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));
        worst = worst.max(settled.ticks);
    }
    println!("worst settle over the sample: {worst} ticks (bound {LIVENESS_TICKS})");
    assert!(
        worst * 2 < LIVENESS_TICKS,
        "the worst observed settle took {worst} ticks against a bound of {LIVENESS_TICKS}; \
         the bound is no longer comfortable"
    );
}

/// The default sweep: every fault mix, hundreds of seeds, every property checked after every
/// event.
#[test]
fn hundreds_of_seeds_hold_every_safety_property() {
    let plans = [
        ("lossless", FaultPlan::lossless()),
        ("hostile", FaultPlan::hostile()),
        ("crashy", FaultPlan::crashy()),
        ("chaotic", FaultPlan::chaotic()),
    ];
    let mut totals = Totals::default();
    for (name, plan) in &plans {
        for seed in seeds(1, 64) {
            let stats = sweep_one(seed, plan, 3, EVENTS);
            assert!(
                stats.events == EVENTS,
                "ESKER_SIM_SEED={seed} ({name}): the run stopped early"
            );
            totals.add(stats);
        }
    }
    assert_faults_actually_bit(&totals);
}

/// Five nodes, where a partition can leave a quorum on either side.
#[test]
fn five_nodes_hold_every_safety_property() {
    let mut totals = Totals::default();
    for seed in seeds(2_000, 48) {
        totals.add(sweep_one(seed, &FaultPlan::chaotic(), 5, EVENTS));
    }
    assert_faults_actually_bit(&totals);
}

/// After the chaos stops, the cluster has to come back — a cluster that is merely *safe* while
/// wedged has proved nothing about Raft.
#[test]
fn a_healed_cluster_makes_progress_again() {
    for seed in seeds(3_000, 32) {
        let mut cluster = Cluster::new(seed, FaultPlan::chaotic(), 3).unwrap();
        if let Err(failure) = cluster.run(EVENTS) {
            panic!("{failure}");
        }
        // Heal everything, then hold the plan quiet so nothing new breaks.
        cluster.calm(FaultPlan::lossless());
        cluster.heal().unwrap_or_else(|failure| panic!("{failure}"));
        cluster
            .settle(LIVENESS_TICKS)
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
}

/// The acceptance run from `prompts/03-raft.md`: 10,000 seeds with faults, zero violations.
///
/// ```text
/// cargo test -p esker-sim --release --test raft_sweep -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the 10,000-seed acceptance run; minutes, not seconds"]
fn ten_thousand_seeds_with_faults() {
    let plans = [
        FaultPlan::hostile(),
        FaultPlan::crashy(),
        FaultPlan::chaotic(),
    ];
    let mut totals = Totals::default();
    let per_plan = 10_000 / plans.len() as u64 + 1;
    for plan in &plans {
        for seed in seeds(1, per_plan) {
            totals.add(sweep_one(seed, plan, 3, EVENTS));
        }
    }
    let runs = totals.stats.len();
    println!(
        "{runs} seeds: {} elections, {} crashes, {} restarts, {} partitions, {} slow writes, \
         {} messages, {} proposals",
        totals.sum(|s| s.elections),
        totals.sum(|s| s.crashes),
        totals.sum(|s| s.restarts),
        totals.sum(|s| s.partitions),
        totals.sum(|s| s.slow_writes),
        totals.sum(|s| s.sent),
        totals.sum(|s| s.proposals),
    );
    assert!(runs >= 10_000, "only {runs} seeds ran");
    assert_faults_actually_bit(&totals);
}

/// A sweep that injected nothing proves nothing. This is the assertion that would have caught
/// a harness where, say, `crash` never fired because the draw order shifted.
fn assert_faults_actually_bit(totals: &Totals) {
    for (name, count) in [
        ("elections", totals.sum(|s| s.elections)),
        ("crashes", totals.sum(|s| s.crashes)),
        ("restarts", totals.sum(|s| s.restarts)),
        ("partitions", totals.sum(|s| s.partitions)),
        ("slow writes", totals.sum(|s| s.slow_writes)),
        ("messages sent", totals.sum(|s| s.sent)),
        ("messages delivered", totals.sum(|s| s.delivered)),
        ("proposals accepted", totals.sum(|s| s.proposals)),
        // Not a fault, but the assertion that stops two of the four properties from being
        // switched off by accident: they are only evaluated on a node whose disk is idle.
        ("committed entries", totals.sum(|s| s.committed)),
        ("applied entries", totals.sum(|s| s.applied)),
    ] {
        assert!(count > 0, "the sweep produced no {name} at all");
    }
}
