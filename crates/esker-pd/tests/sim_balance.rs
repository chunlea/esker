//! The real balance rules, run through the placement model.
//!
//! `esker_sim::mech::placement` owns the world and the checker; this file owns the one thing that
//! makes the checker mean something, which is that the decision under test is
//! **`esker_pd::balance::balance_for` itself** and not a transcription of it. Copy this file and
//! `crates/esker-sim/` into a detached worktree at `ba8ed2e` — `548dd62`'s parent — and the same
//! checker meets `is_mid_repair` as it was, counting down stores and nothing else.
//!
//! The recorded red, from that worktree — 3 of these 4 tests fail there:
//!
//! ```text
//! seed 1, round 0: balance planned AddPeer { region_id: 1, store_id: 4 } against region 1,
//! which is mid-repair (UnpromotedLearner { peer_id: 25, all_stores_live: true })
//!
//! the office moved out from under an unfinished repair
//!   left: Some(TransferLeader { region_id: 1, to_peer_id: 20 })
//!  right: None
//! ```
//!
//! The fourth is `the_checker_names_the_learner_when_it_fires`, which is a guard on the checker
//! rather than on the placement driver and passes at both revisions — as it should.
//!
//! `docs/plans/phase-11-engine.md` §10 holds the full run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_pd::balance::{Balance, balance_for};
use esker_pd::record::{RegionRecord, StoreRecord, StoreStats};
use esker_pd::schedule::Cluster;
use esker_proto::{Epoch, Peer, PeerRole, Region};
use esker_sim::mech::placement::{
    BalancePolicy, ClusterView, MidRepair, Move, RegionView, Role, Violation, World, run,
};

/// Seeds every scenario runs, the same list `esker-sim`'s own model test uses.
const SEEDS: [u64; 24] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584, 4181, 6765, 10946,
    17711, 28657, 46368, 75025,
];

/// Rounds per seed.
const ROUNDS: u64 = 400;

/// The placement driver's own rules, wearing the model's clothes.
///
/// Everything here is conversion. The one line that decides anything is the call to
/// `balance_for`, which is why this type holds no state and no opinion.
struct RealBalance;

impl BalancePolicy for RealBalance {
    fn plan(&self, region: &RegionView, cluster: &ClusterView) -> Option<Move> {
        let record = to_record(region);
        let stores = to_stores(cluster);
        let cluster = Cluster {
            stores: &stores,
            // No operator is left in flight by this model: it applies a move or it does not, so
            // the effective counts are the reported ones. A pending-operator dimension would be a
            // second model, and a worthwhile one — see `phase-11-engine.md` §7.
            pending: &[],
            now_ms: cluster.now_ms,
            max_store_down_time_ms: cluster.max_store_down_time_ms,
            target_replicas: cluster.target_replicas,
        };
        balance_for(&record, &cluster).map(from_balance)
    }
}

fn to_record(region: &RegionView) -> RegionRecord {
    RegionRecord {
        region: Region {
            id: region.region_id,
            // One region per model region and no splits, so the ranges only have to be distinct
            // and ordered. Balance reads neither.
            start_key: bytes::Bytes::from(region.region_id.to_be_bytes().to_vec()),
            end_key: bytes::Bytes::from((region.region_id + 1).to_be_bytes().to_vec()),
            peers: region
                .peers
                .iter()
                .map(|peer| Peer {
                    store_id: peer.store_id,
                    peer_id: peer.peer_id,
                    role: match peer.role {
                        Role::Voter => PeerRole::Voter,
                        Role::Learner => PeerRole::Learner,
                        Role::ColumnarLearner => PeerRole::ColumnarLearner,
                    },
                })
                .collect(),
            epoch: Epoch {
                conf_ver: region.conf_ver,
                version: region.version,
            },
        },
        leader_peer_id: region.leader_peer_id,
        term: 1,
        approximate_size: 1 << 20,
        applied_index: 1,
        last_heartbeat_ms: 0,
    }
}

fn to_stores(cluster: &ClusterView) -> Vec<StoreRecord> {
    cluster
        .stores
        .iter()
        .map(|store| StoreRecord {
            store_id: store.store_id,
            address: format!("127.0.0.1:{}", 20_160 + store.store_id),
            started_ms: 0,
            last_heartbeat_ms: store.last_heartbeat_ms,
            stats: StoreStats {
                capacity: 1 << 40,
                available: 1 << 39,
                region_count: store.region_count,
                leader_count: store.leader_count,
                applied_bytes: 0,
            },
        })
        .collect()
}

fn from_balance(balance: Balance) -> Move {
    match balance {
        Balance::TransferLeader {
            region_id,
            to_peer_id,
            ..
        } => Move::TransferLeader {
            region_id,
            to_peer_id,
        },
        Balance::AddPeer {
            region_id,
            store_id,
            ..
        } => Move::AddPeer {
            region_id,
            store_id,
        },
        Balance::RemovePeer {
            region_id, peer_id, ..
        } => Move::RemovePeer { region_id, peer_id },
    }
}

#[test]
fn balance_never_touches_a_mid_repair_region() {
    let mut reached_the_live_learner = 0_u64;
    for seed in SEEDS {
        match run(seed, ROUNDS, &RealBalance) {
            Ok(report) => reached_the_live_learner += report.declined_learner_all_live,
            Err(violation) => panic!(
                "{violation}\n\nRerun with just this seed. DESIGN.md §7 and \
                 `esker_pd::balance::is_mid_repair`: a region is mid-repair while it holds a peer \
                 on a down store *or* a plain Learner, and neither may be balanced."
            ),
        }
    }

    println!(
        "{} seeds x {ROUNDS} rounds; reached a region with an unpromoted learner and every store \
         up {reached_the_live_learner} times",
        SEEDS.len()
    );

    // A green run over a model that never reached the interesting state would be worthless, so
    // the floor is asserted here too rather than only in `esker-sim`'s own test: this file is the
    // one that runs in a detached worktree, and it has to be able to say for itself that it put
    // the real rules in front of the state they were widened for.
    assert!(
        reached_the_live_learner > 1_000,
        "the run never reached a region holding an unpromoted learner with every store up \
         ({reached_the_live_learner} times). That is the state `548dd62` widened `is_mid_repair` \
         to cover; a green run that never reaches it says nothing."
    );
}

#[test]
fn a_columnar_learner_is_not_a_repair() {
    // The other half of `548dd62`, and the reason `is_mid_repair` is a match rather than a
    // `!= Voter`: ADR 0022 says a columnar learner is never promoted, so counting it as a repair
    // would freeze every region holding one out of balance for ever. Constructed rather than
    // seeded, because it is an assertion about one shape.
    let world = World::new(1);
    let cluster = world.cluster();
    let mut region = world.regions().into_iter().next().unwrap();

    let baseline = RealBalance.plan(&region, &cluster);
    assert!(
        baseline.is_some(),
        "the fixture is not out of balance to begin with, so the rest of this test proves \
         nothing: {region:?}"
    );

    // The quietest store, which is where balance wants to put a replica, now holds a columnar
    // learner of this region instead.
    let quiet = cluster
        .stores
        .iter()
        .min_by_key(|store| (store.region_count, store.store_id))
        .unwrap()
        .store_id;
    region.peers.push(esker_sim::mech::placement::PeerView {
        peer_id: 9_001,
        store_id: quiet,
        role: Role::ColumnarLearner,
    });
    region.conf_ver += 1;

    assert!(
        RealBalance.plan(&region, &cluster).is_some(),
        "a region holding a columnar learner was frozen out of balance. ADR 0022 Decision 1: it \
         is never promoted, so it is never a repair to wait for — and a region that holds one for \
         the life of the cluster would never be balanced again."
    );

    // And the same region with a *plain* learner is refused, so the assertion above is about the
    // role and not about the extra peer.
    let mut with_plain = region.clone();
    with_plain.peers.last_mut().unwrap().role = Role::Learner;
    assert_eq!(
        RealBalance.plan(&with_plain, &cluster),
        None,
        "a plain learner must still stop balance; otherwise the test above is passing for the \
         wrong reason"
    );
}

#[test]
fn the_checker_names_the_learner_when_it_fires() {
    // A guard on the checker itself, run against the real rules. `MidRepair::UnpromotedLearner`
    // is the reason a failure here would be about `548dd62`; if a future change made the checker
    // fire on `PeerOnDownStore` instead, that would be a different bug wearing this test's name.
    let world = World::new(1);
    for region in world.regions() {
        if let Some(why) = world.mid_repair(region.region_id) {
            assert!(
                matches!(why, MidRepair::PeerOnDownStore { .. }),
                "a fresh world should have no learners: {why:?}"
            );
        }
    }
    // `Violation` is an error type a failure is printed through; hold it to that.
    let violation = Violation::BalancedMidRepair {
        seed: 1,
        round: 3,
        region_id: 1,
        why: MidRepair::UnpromotedLearner {
            peer_id: 25,
            all_stores_live: true,
        },
        planned: Move::AddPeer {
            region_id: 1,
            store_id: 5,
        },
    };
    let rendered = violation.to_string();
    assert!(
        rendered.contains("seed 1") && rendered.contains("mid-repair"),
        "a violation has to print its seed and say what it is: {rendered}"
    );
}

#[test]
fn leader_balance_has_a_repair_guard_of_its_own() {
    // `548dd62`'s trace is a `TransferLeader`, not a replica move, and the commit says why:
    // "leader_balance had no repair guard at all beyond 'the leader's store is down'". The seeded
    // run above cannot isolate that, because `balance_for` asks `region_balance` first and it
    // answers first. So this is the shape constructed: region counts dead level, so the replica
    // rule has nothing to say, and leader counts far apart, so the office rule does.
    //
    // The cost of getting it wrong is not one extra membership change. A leader with a transfer
    // in flight refuses proposals, and the promotion that would finish the repair is proposed by
    // that leader — so the transfer waits for a target the promotion would have caught up, and
    // the promotion waits for the transfer.
    let stores: Vec<esker_sim::mech::placement::StoreView> = (1..=5)
        .map(|store_id| esker_sim::mech::placement::StoreView {
            store_id,
            last_heartbeat_ms: 100_000,
            region_count: 3,
            leader_count: if store_id == 1 { 5 } else { 0 },
        })
        .collect();
    let cluster = ClusterView {
        stores,
        now_ms: 100_000,
        max_store_down_time_ms: 30_000,
        target_replicas: 3,
    };

    let voters: Vec<esker_sim::mech::placement::PeerView> = (1..=3)
        .map(|store_id| esker_sim::mech::placement::PeerView {
            peer_id: store_id * 10,
            store_id,
            role: Role::Voter,
        })
        .collect();
    let settled = RegionView {
        region_id: 1,
        conf_ver: 4,
        version: 1,
        leader_peer_id: 10,
        peers: voters.clone(),
    };

    // The office is worth moving when nothing is being repaired, or the shape proves nothing.
    assert!(
        matches!(
            RealBalance.plan(&settled, &cluster),
            Some(Move::TransferLeader { region_id: 1, .. })
        ),
        "the fixture is not leader-imbalanced: {:?}",
        RealBalance.plan(&settled, &cluster)
    );

    let mut repairing = settled.clone();
    repairing.peers.push(esker_sim::mech::placement::PeerView {
        peer_id: 40,
        store_id: 4,
        role: Role::Learner,
    });
    repairing.conf_ver += 1;
    assert_eq!(
        RealBalance.plan(&repairing, &cluster),
        None,
        "the office moved out from under an unfinished repair. The transfer blocks the \
         `AddVoter` that would finish it, and the two then wait for each other until the operator \
         times out (548dd62)."
    );

    // And the columnar case again on this path: a region holding one is not being repaired, so
    // its office is still balance's to move.
    let mut columnar = settled;
    columnar.peers.push(esker_sim::mech::placement::PeerView {
        peer_id: 41,
        store_id: 4,
        role: Role::ColumnarLearner,
    });
    columnar.conf_ver += 1;
    assert!(
        RealBalance.plan(&columnar, &cluster).is_some(),
        "a columnar learner froze the office in place; ADR 0022 says it is never promoted, so \
         there is nothing to wait for"
    );
}
