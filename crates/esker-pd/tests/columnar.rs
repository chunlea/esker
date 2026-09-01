//! Columnar placement at the heartbeat: PD acts on what a SQL node reported, and on nothing else.
//!
//! [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md) Decision 5. PD **cannot read** the
//! catalog setting these come from — it links neither `esker-sql` nor a client, and every method
//! on its service is inbound — so a SQL node reports key ranges and PD schedules from the report.
//!
//! The four claims here are the ones that would be expensive to get wrong, and two of them are
//! about *not* acting: a columnar replica must never be confused with a voter in either
//! direction, because each confusion produces a cluster that looks healthy and is not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_pd::clock::TestClock;
use esker_pd::{Clock, Pd, PdOptions, RegionBeat, StoreBeat, StoreStats};
use esker_proto::pd::ColumnarWish;
use esker_proto::{Epoch, Operator, Peer, PeerRole, Region};

fn open() -> (tempfile::TempDir, Arc<TestClock>, Arc<Pd>) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    // Balance off. These tests are about what PD does with a *report*, and a balance move
    // arriving in the same slot would make "PD asked for nothing" and "PD asked for something
    // else" indistinguishable — which is the assertion half of them rest on. Repair still runs,
    // which is what the last two need.
    let pd = Pd::open(
        dir.path(),
        PdOptions {
            balance: false,
            ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        },
    )
    .unwrap();
    (dir, clock, pd)
}

fn alive(pd: &Pd, store_id: u64) {
    pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
        .unwrap();
    pd.store_heartbeat(&StoreBeat {
        store_id,
        stats: StoreStats {
            region_count: 1,
            ..StoreStats::default()
        },
    })
    .unwrap();
}

/// A region over the whole key space, so every wish overlaps it.
fn region(peers: Vec<Peer>) -> Region {
    Region {
        id: 1,
        start_key: bytes::Bytes::new(),
        end_key: bytes::Bytes::new(),
        peers,
        epoch: Epoch::new(3, 4),
    }
}

fn ask(pd: &Pd, region: Region) -> Option<Operator> {
    pd.region_heartbeat(&RegionBeat {
        region,
        leader_peer_id: 10,
        term: 4,
        approximate_size: 0,
        applied_index: 0,
    })
    .unwrap()
    .operator
}

/// Reports that the whole key space wants `replicas` columnar copies.
fn wants(pd: &Pd, replicas: u8) {
    pd.report_columnar(vec![ColumnarWish {
        start_key: bytes::Bytes::new(),
        end_key: bytes::Bytes::new(),
        replicas,
    }])
    .unwrap();
}

/// Three voters on three live stores, and two more stores free to take columnar copies.
fn healthy(pd: &Pd) -> Vec<Peer> {
    for store in 1..=5 {
        alive(pd, store);
    }
    vec![Peer::voter(1, 10), Peer::voter(2, 11), Peer::voter(3, 12)]
}

// --- (1) The flag asks, and a learner appears ---------------------------------------------------

/// A reported wish produces an `AddLearner` on a store that has no peer of the region, and it is
/// an `AddLearner` rather than an `AddPeer` — which is the difference between a replica that
/// stays a learner and one on its way to voting.
#[test]
fn a_reported_wish_places_a_columnar_learner() {
    let (_dir, _clock, pd) = open();
    let peers = healthy(&pd);

    // Nothing reported: a healthy region is asked for nothing at all.
    assert_eq!(ask(&pd, region(peers.clone())), None);

    wants(&pd, 1);
    let operator = ask(&pd, region(peers.clone())).expect("the wish was not acted on");
    let Operator::AddLearner {
        region_id,
        store_id,
        ..
    } = operator
    else {
        panic!("expected AddLearner, got {operator:?}");
    };
    assert_eq!(region_id, 1);
    assert!(
        store_id == 4 || store_id == 5,
        "the columnar copy went to store {store_id}, which already holds a peer of the region"
    );
}

/// And once it is there, PD stops asking. A placement that kept being re-issued would grow the
/// region by a replica per operator timeout.
#[test]
fn a_placed_columnar_learner_is_not_asked_for_twice() {
    let (_dir, _clock, pd) = open();
    let mut peers = healthy(&pd);
    wants(&pd, 1);
    let operator = ask(&pd, region(peers.clone())).expect("first ask");
    let Operator::AddLearner {
        store_id, peer_id, ..
    } = operator
    else {
        panic!("expected AddLearner, got {operator:?}");
    };

    // The store applies it: a learner that stays one.
    peers.push(Peer {
        store_id,
        peer_id,
        role: PeerRole::ColumnarLearner,
    });
    assert_eq!(
        ask(&pd, region(peers)),
        None,
        "PD asked for a second columnar copy of a region that has the one it wanted"
    );
}

// --- (2) The flag is cleared, and the learner is retired ----------------------------------------

/// `ALTER TABLE ... SET (columnar_replicas = 0)` arrives as a report that no longer names the
/// range, and PD gives the replica back.
#[test]
fn clearing_the_flag_retires_the_columnar_learner() {
    let (_dir, _clock, pd) = open();
    let mut peers = healthy(&pd);
    peers.push(Peer {
        store_id: 4,
        peer_id: 20,
        role: PeerRole::ColumnarLearner,
    });

    // Still wanted: nothing to do.
    wants(&pd, 1);
    assert_eq!(ask(&pd, region(peers.clone())), None);

    // Cleared. A range that wants none is simply absent from the report.
    pd.report_columnar(Vec::new()).unwrap();
    let operator = ask(&pd, region(peers)).expect("the cleared flag was not acted on");
    let Operator::RemovePeer { peer_id, .. } = operator else {
        panic!("expected RemovePeer, got {operator:?}");
    };
    assert_eq!(peer_id, 20, "the wrong peer was retired");
}

// --- (3) Under-replication is repaired only when the flag asks ----------------------------------

/// A region with no columnar copy is not under-replicated unless somebody asked for one — and a
/// region that asked for two and has one is.
///
/// This is the claim that keeps a columnar copy a *feature* rather than a default: a cluster
/// nobody has reported anything for must never grow a replica, however many stores are free.
#[test]
fn columnar_under_replication_is_repaired_only_when_the_flag_asks() {
    let (_dir, _clock, pd) = open();
    let peers = healthy(&pd);

    // Never reported: no columnar copies, and PD asks for none however long it beats.
    for _ in 0..3 {
        assert_eq!(
            ask(&pd, region(peers.clone())),
            None,
            "PD placed a columnar replica nobody asked for"
        );
    }

    // Asked for two, holding one: that is under-replication, and now it is repaired.
    wants(&pd, 2);
    let mut with_one = peers.clone();
    with_one.push(Peer {
        store_id: 4,
        peer_id: 20,
        role: PeerRole::ColumnarLearner,
    });
    let operator = ask(&pd, region(with_one)).expect("a short columnar placement was not repaired");
    assert!(
        matches!(operator, Operator::AddLearner { .. }),
        "expected AddLearner, got {operator:?}"
    );
}

// --- (4) Never confused with voter repair -------------------------------------------------------

/// A columnar learner does not count towards the voter target, in either direction.
///
/// **This is the one that hides.** A region of two voters and a columnar copy has three replicas
/// and cannot elect a leader if it loses one; a repair that counted the columnar copy would see
/// three-of-three and report a healthy cluster. The failure surfaces the next time a store dies,
/// which is the moment it is least affordable.
#[test]
fn a_columnar_learner_does_not_stand_in_for_a_voter() {
    let (_dir, _clock, pd) = open();
    for store in 1..=5 {
        alive(&pd, store);
    }
    wants(&pd, 1);

    // Two voters, and a columnar copy already placed. The target is three voters.
    let peers = vec![
        Peer::voter(1, 10),
        Peer::voter(2, 11),
        Peer {
            store_id: 4,
            peer_id: 20,
            role: PeerRole::ColumnarLearner,
        },
    ];
    let operator = ask(&pd, region(peers)).expect("an under-replicated region was left alone");
    assert!(
        matches!(operator, Operator::AddPeer { .. }),
        "the columnar copy was counted as a voter: expected AddPeer, got {operator:?}"
    );
}

/// And the repair comes **first**: a region short of voters spends its one operator on the voter,
/// not on the columnar copy it also lacks.
#[test]
fn voter_repair_outranks_columnar_placement() {
    let (_dir, _clock, pd) = open();
    for store in 1..=5 {
        alive(&pd, store);
    }
    wants(&pd, 1);

    // Short a voter AND short its columnar copy. Both are wanted; only one can be asked for.
    let peers = vec![Peer::voter(1, 10), Peer::voter(2, 11)];
    let operator = ask(&pd, region(peers)).expect("nothing was asked for");
    assert!(
        matches!(operator, Operator::AddPeer { .. }),
        "columnar placement took the slot a repair needed: got {operator:?}"
    );
}
