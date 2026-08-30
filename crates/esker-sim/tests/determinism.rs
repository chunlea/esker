//! The property the simulator exists for: a run is a function of its seed.
//!
//! Every simulator failure prints its seed, and that seed has to be enough to replay the
//! failure exactly (`docs/DESIGN.md` §11). If it is not — because something iterated a
//! `HashMap`, read a real clock, or broke a tie by address — then a failing seed is just a
//! story about a bug rather than a way to find it.
//!
//! Two runs of the same scenario with the same seed must produce identical traces. Two runs
//! with *different* seeds must not, otherwise this test would pass just as well against a
//! network that ignored its seed entirely.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_sim::net::{Network as _, NetworkError, TraceEvent};
use esker_sim::{FaultPlan, Millis, NodeId, SimNetwork, scenario_rng};

const NODES: [NodeId; 4] = [NodeId(1), NodeId(2), NodeId(3), NodeId(4)];

/// A scenario with enough going on to expose an ordering bug: every node sends to a randomly
/// chosen peer, deliveries and sends interleave, and the payloads differ so that a trace is
/// sensitive to which message went where.
///
/// The scenario's own choices come from [`scenario_rng`], a different stream from the one the
/// network draws faults from, so a change to one does not shift the other.
fn run(seed: u64, plan: FaultPlan) -> Vec<TraceEvent> {
    let mut net = SimNetwork::new(seed, plan, &NODES);
    let mut rng = scenario_rng(seed);

    for round in 0..40u64 {
        for (index, &from) in NODES.iter().enumerate() {
            let to = NODES[rng.below(u32::try_from(NODES.len()).unwrap()) as usize];
            if to == from {
                continue;
            }
            let payload = Bytes::from(format!("round {round} from {index}").into_bytes());
            match net.node(from).send(to, payload) {
                Ok(()) => {}
                Err(NetworkError::Unroutable(node)) => panic!("unroutable node {node:?}"),
            }
        }

        // Interleave delivery with sending, so ordering bugs have somewhere to hide.
        net.run_until(Millis(round * 20));

        for &node in &NODES {
            while net.recv(node).is_some() {}
        }
    }

    net.run_to_quiescence();
    net.trace().to_vec()
}

#[test]
fn the_same_seed_replays_exactly() {
    for seed in [1u64, 42, 9_999] {
        let first = run(seed, FaultPlan::hostile());
        let second = run(seed, FaultPlan::hostile());

        assert_eq!(
            first.len(),
            second.len(),
            "seed {seed} produced traces of different lengths"
        );
        for (index, (a, b)) in first.iter().zip(&second).enumerate() {
            assert_eq!(a, b, "seed {seed} diverged at event {index}");
        }
    }
}

/// Without this, a network that ignored its seed would pass the test above.
#[test]
fn different_seeds_produce_different_runs() {
    let a = run(1, FaultPlan::hostile());
    let b = run(2, FaultPlan::hostile());
    assert_ne!(
        a, b,
        "two seeds produced the same trace; the seed is not being used"
    );
}

/// A trace of nothing is identical to another trace of nothing. The scenario has to actually
/// exercise every kind of event, or "deterministic" is a claim about an empty set.
#[test]
fn the_scenario_exercises_every_event_kind() {
    let trace = run(42, FaultPlan::hostile());
    assert!(trace.len() > 200, "only {} events", trace.len());

    let count = |predicate: fn(&TraceEvent) -> bool| trace.iter().filter(|e| predicate(e)).count();
    assert!(
        count(|e| matches!(e, TraceEvent::Sent { .. })) > 0,
        "nothing was sent"
    );
    assert!(
        count(|e| matches!(e, TraceEvent::Dropped { .. })) > 0,
        "nothing was dropped"
    );
    assert!(
        count(|e| matches!(e, TraceEvent::Duplicated { .. })) > 0,
        "nothing was duplicated"
    );
    assert!(
        count(|e| matches!(e, TraceEvent::Delivered { .. })) > 0,
        "nothing was delivered"
    );
}

/// The fault plan changes what happens; the seed alone does not determine a run.
#[test]
fn a_different_fault_plan_changes_the_run() {
    let hostile = run(42, FaultPlan::hostile());
    let perfect = run(42, FaultPlan::perfect());
    assert_ne!(hostile, perfect);
    assert!(
        !perfect
            .iter()
            .any(|e| matches!(e, TraceEvent::Dropped { .. })),
        "a perfect network dropped a message"
    );
}

/// Determinism has to survive a differently shaped heap, so it must not depend on anything
/// the allocator chose. Re-running after unrelated allocations have moved the heap around is
/// a cheap way to notice a trace that depends on an address.
#[test]
fn the_trace_does_not_depend_on_allocation_addresses() {
    let baseline = run(7, FaultPlan::hostile());

    let mut ballast: Vec<Vec<u8>> = (0..256u32)
        .map(|i| vec![u8::try_from(i % 251).unwrap(); 977])
        .collect();
    ballast.retain(|block| block.len() % 2 == 1);
    assert!(!ballast.is_empty());

    assert_eq!(run(7, FaultPlan::hostile()), baseline);
}
