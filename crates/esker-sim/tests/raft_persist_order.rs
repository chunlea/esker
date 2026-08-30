//! The proof that the harness is honest.
//!
//! `docs/DESIGN.md` §5: the driver persists `hard_state` and entries — with fsync — **before**
//! sending any message from the same `Ready`. Violating that order breaks Raft's safety
//! argument, because a node can then tell the cluster "I have this entry" and "I voted for you"
//! and then lose both to a kill.
//!
//! Two things have to be true, and only one of them is usually tested:
//!
//! 1. With the contract kept, the properties hold. Every other test in this crate says that.
//! 2. With the contract broken, the properties *fail*. That is this file. A checker that has
//!    never been shown red, and a persistence boundary that has never been shown leaky, are
//!    both decoration — if the harness quietly let a restarted node read back what it never
//!    fsynced, every safety property here would pass vacuously and prove nothing at all.
//!
//! The violation is a race, so no single seed is guaranteed to expose it. The test sweeps seeds
//! and requires that a violation is found within them, and reports the first one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::FaultPlan;
use esker_sim::raft::{Cluster, Failure};

/// How many seeds to try before concluding the violation cannot be produced.
const SEEDS: u64 = 400;
/// Events per seed.
const EVENTS: u64 = 900;

/// Sending before persisting has to be caught. If this test ever passes silently, the
/// persistence boundary has stopped being real and every other test in this crate is worthless.
#[test]
fn sending_before_persisting_is_caught() {
    let plan = FaultPlan {
        // Crashes are the whole mechanism: the node that sent early has to die before its write
        // lands. A slow disk widens the window it dies in.
        crash: 0.05,
        restart: 0.10,
        slow_disk: 0.5,
        slow_disk_events: 20,
        ..FaultPlan::lossless()
    };

    let mut first: Option<Failure> = None;
    let mut caught = 0_u32;
    for seed in 0..SEEDS {
        let mut cluster = Cluster::new(seed, plan.clone(), 3).unwrap();
        cluster.violate_persist_order(true);
        if let Err(failure) = cluster.run(EVENTS) {
            caught += 1;
            if first.is_none() {
                first = Some(failure);
            }
        }
    }

    let failure = first.unwrap_or_else(|| {
        panic!(
            "{SEEDS} seeds of a driver that sends before it persists produced no violation at \
             all. Either the harness is letting a crashed node read back what it never fsynced, \
             or the checkers are not looking. Both make every other test in this crate vacuous."
        )
    });
    assert!(
        matches!(failure, Failure::Safety { .. }),
        "the failure was not a safety violation: {failure}"
    );
    println!("{caught}/{SEEDS} seeds caught it; the first was:\n{failure}");
}

/// The other half of the driver contract, from `docs/plans/phase-3.md` §10.2: `ready()` *moves*
/// the messages out of the core, so a driver that takes a `Ready` and drops it loses them. The
/// state is offered again and the log looks fine, which is exactly why it needs an explicit
/// rule and an explicit check rather than a reader's attention.
#[test]
fn dropping_a_taken_ready_is_caught() {
    let mut cluster = Cluster::new(1, FaultPlan::lossless(), 3).unwrap();
    cluster.discard_taken_ready(true);
    let failure = cluster
        .run(200)
        .expect_err("a driver that throws away every Ready it takes was not noticed");
    assert!(
        matches!(failure, Failure::Driver { .. }),
        "the failure was not a driver-rule failure: {failure}"
    );
    println!("{failure}");
}

/// A `Ready` that dies with its node is *not* a dropped one — the process that would have sent
/// its messages no longer exists — so a plan full of crashes must not trip the rule.
#[test]
fn a_ready_that_dies_with_its_node_is_not_a_dropped_one() {
    let plan = FaultPlan {
        crash: 0.08,
        restart: 0.10,
        slow_disk: 0.6,
        slow_disk_events: 25,
        ..FaultPlan::lossless()
    };
    for seed in 0..64 {
        let mut cluster = Cluster::new(seed, plan.clone(), 3).unwrap();
        if let Err(failure) = cluster.run(600) {
            panic!("crashes tripped the dropped-Ready rule:\n{failure}");
        }
        assert!(
            cluster.stats().readys_lost_to_crash > 0 || cluster.stats().crashes == 0,
            "seed {seed} crashed nodes but never lost a Ready with one; the accounting is not \
             exercising the path it claims to"
        );
    }
}

/// `ReadIndex` rides on `AppendEntries`'s context (§10.3), so the fault model has to carry it
/// verbatim through a duplicate and a reorder. The network carries an opaque token and resolves
/// it to the message the sender produced, so nothing can rewrite a field — this pins that, and
/// pins that contexts and duplicates actually occur rather than being asserted about in the
/// abstract.
#[test]
fn a_duplicated_message_arrives_with_the_context_it_was_sent_with() {
    let plan = FaultPlan {
        duplicate: 0.5,
        ..FaultPlan::lossless()
    };
    let mut duplicates = 0;
    let mut contexts = 0;
    let mut reads = 0;
    for seed in 0..24 {
        let mut cluster = Cluster::new(seed, plan.clone(), 3).unwrap();
        cluster
            .run(600)
            .unwrap_or_else(|failure| panic!("{failure}"));
        duplicates += cluster.stats().duplicate_deliveries;
        contexts += cluster.stats().contexts_delivered;
        reads += cluster.stats().reads_served;
    }
    assert!(duplicates > 0, "nothing was delivered twice");
    assert!(
        contexts > 0,
        "no message with a non-empty context was ever delivered, so the check that they survive \
         a duplicate never ran — the event loop is not asking for read indexes"
    );
    assert!(reads > 0, "no ReadIndex was ever answered");
}

/// The same plan, the same seeds, with the contract kept: nothing is caught. Without this, the
/// test above would also pass if the harness were simply broken for every plan.
#[test]
fn the_same_faults_with_the_contract_kept_are_clean() {
    let plan = FaultPlan {
        crash: 0.05,
        restart: 0.10,
        slow_disk: 0.5,
        slow_disk_events: 20,
        ..FaultPlan::lossless()
    };
    for seed in 0..SEEDS {
        let mut cluster = Cluster::new(seed, plan.clone(), 3).unwrap();
        if let Err(failure) = cluster.run(EVENTS) {
            panic!("a driver that keeps the contract still broke a property:\n{failure}");
        }
    }
}
