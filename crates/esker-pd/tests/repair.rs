//! Repair at the heartbeat, for the shape the phase-4 acceptance run left behind: a region
//! **under its replica target with every store alive**.
//!
//! `src/schedule.rs` holds the rule and its unit tests; these two drive the whole public path —
//! heartbeat in, operator out, in-flight set and allocator moving with it — because the rule
//! being right and PD acting on it are different claims. They live here rather than beside
//! their siblings in `src/pd/repair.rs` only because that file is at its size budget
//! (`CLAUDE.md`), and everything they touch is public API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_pd::clock::TestClock;
use esker_pd::{Clock, Pd, PdOptions, RegionBeat, StoreBeat, StoreStats};
use esker_proto::{Epoch, Operator, Peer, Region};

fn open() -> (tempfile::TempDir, Arc<TestClock>, Arc<Pd>) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let pd = Pd::open(
        dir.path(),
        PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
    )
    .unwrap();
    (dir, clock, pd)
}

/// Registers `store_id` and beats for it, which is what a live store does.
fn alive(pd: &Pd, store_id: u64, region_count: u64) {
    pd.store_heartbeat(&StoreBeat {
        store_id,
        stats: StoreStats {
            region_count,
            ..StoreStats::default()
        },
    })
    .unwrap();
}

fn region(id: u64, peers: Vec<Peer>) -> Region {
    Region {
        id,
        start_key: bytes::Bytes::new(),
        end_key: bytes::Bytes::new(),
        peers,
        epoch: Epoch::new(3, 4),
    }
}

fn ask(pd: &Pd, region: Region, leader: u64) -> Option<Operator> {
    pd.region_heartbeat(&RegionBeat {
        region,
        leader_peer_id: leader,
        term: 4,
        approximate_size: 0,
        applied_index: 0,
    })
    .unwrap()
    .operator
}

/// Region 27 of `docs/bench/phase-4.md` Run 4, over the same path a real leader's beat takes.
/// Two voters, target three, **nothing down** — the state PD watched for 270 seconds and never
/// acted on, until the death of one of the two voters put 1,410 keys out of reach for good
/// (`docs/adr/0026-the-quorum-loss-boundary.md`).
///
/// The one-in-flight discipline is asserted with it, because the new trigger fires on a state
/// rather than on an event and a state does not go away while the operator works: a rule that
/// re-derived a *second* `AddPeer` on the next beat would grow the region by a replica per
/// heartbeat.
///
/// Mutation check: restore the old first line of `schedule::repair_for` — `None` when no peer
/// is on a down store — and the first `expect` here is what fails.
#[test]
fn a_region_at_two_voters_with_every_store_alive_earns_an_add_peer() {
    let (_dir, _clock, pd) = open();
    for store_id in 1..=3 {
        pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
            .unwrap();
    }
    // Every store healthy, and store 3 the emptiest.
    alive(&pd, 1, 8);
    alive(&pd, 2, 8);
    alive(&pd, 3, 1);

    let two_voters = || region(27, vec![Peer::voter(1, 28), Peer::voter(3, 29)]);
    let operator = ask(&pd, two_voters(), 28).expect("a region short of the target is repaired");
    let Operator::AddPeer {
        region_id,
        epoch,
        store_id,
        peer_id,
    } = operator
    else {
        panic!("expected an AddPeer, got {operator:?}");
    };
    assert_eq!(region_id, 27);
    assert_eq!(epoch, Epoch::new(3, 4), "addressed to the epoch PD holds");
    assert_eq!(store_id, 2, "the emptiest live store with no peer of it");

    // Asked again before anything has happened: the same operator, never a second one.
    for _ in 0..3 {
        assert_eq!(ask(&pd, two_voters(), 28), Some(operator.clone()));
    }
    assert_eq!(pd.in_flight().unwrap().len(), 1);

    // The replacement lands and can vote. Repair restores the target; it does not grow past it.
    let three_voters = region(
        27,
        vec![
            Peer::voter(1, 28),
            Peer::voter(3, 29),
            Peer::voter(2, peer_id),
        ],
    );
    assert_eq!(ask(&pd, three_voters, 28), None, "the region is whole");
    assert!(pd.in_flight().unwrap().is_empty());
}

/// Repair is a sweep now — a store dying is not the only thing that starts one — so a round of
/// it must spread. Three regions short of the target decide in one round, before any store has
/// reported again, and each replica goes somewhere else: the operator issued for the first
/// region is already counted against its destination when the second one decides — the same
/// effective counts balance reads (`docs/adr/0018-balance-moves-the-spread-by-two.md`).
///
/// Mutation check: place on `store.stats.region_count` instead of `Cluster::effective_regions`
/// and all three land on store 3.
#[test]
fn a_round_of_repairs_spreads_across_the_empty_stores() {
    let (_dir, _clock, pd) = open();
    for store_id in 1..=5 {
        pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
            .unwrap();
    }
    for store_id in 1..=2 {
        alive(&pd, store_id, 3);
    }
    for store_id in 3..=5 {
        alive(&pd, store_id, 0);
    }

    // Three regions, each two voters of a target of three, each deciding on its own beat with
    // no store report in between.
    let mut chosen: Vec<u64> = Vec::new();
    for id in 1..=3 {
        let short = region(
            id,
            vec![Peer::voter(1, id * 10), Peer::voter(2, id * 10 + 1)],
        );
        match ask(&pd, short, id * 10) {
            Some(Operator::AddPeer { store_id, .. }) => chosen.push(store_id),
            other => panic!("region {id} expected an AddPeer, got {other:?}"),
        }
    }
    assert_eq!(
        chosen,
        vec![3, 4, 5],
        "a round of repairs piled onto one store"
    );
}
