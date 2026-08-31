//! A client that dies at each boundary of a two-phase commit, and the reader that cleans up.
//!
//! `crates/esker-txn/tests/protocol.rs::a_crash_at_every_step_resolves_the_way_the_primary_says`
//! is the same claim proved against three `BTreeMap`s. This is it against real stores, real
//! sockets, two Raft groups and a leader killed in between — which is where the claim can fail
//! for reasons a `BTreeMap` has no way to show:
//!
//! * the primary's record and the secondary's lock are in **different Raft groups**, so
//!   "committed" and "still locked" are two separate facts and the roll-forward is a real
//!   round trip to a different set of machines;
//! * the resolver is a *client*, and the judgement about whose lease has expired is its
//!   (`docs/plans/phase-5.md` §10.2), so the rule being tested lives in code that a
//!   single-process model never runs;
//! * a leader change between the crash and the resolution has to leave both facts intact.
//!
//! # The four boundaries, and which one is the commit point
//!
//! In the order a transaction crosses them: after the primary's prewrite, after the
//! secondaries', after the primary's commit, after each secondary's. **Only the third is the
//! commit point.** A crash before it must roll back; a crash after it must roll forward. That
//! asymmetry is the whole of Percolator, and each boundary below is one row of it.
//!
//! # Why the phases are driven by hand
//!
//! `TxnClient::commit` crosses all four in one call and offers nowhere to die in between. So
//! these tests send the `TxnKv` verbs themselves, through the same router a client uses — the
//! client's own commit is what `tests/txn.rs` and `tests/txn_multi_region.rs` cover, and what
//! is under test here is what a *half-finished* transaction leaves behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_client::wire::{Body, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
use esker_client::{Router, TxnClient};

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

/// The primary: an account key, so it lands in the low region.
const PRIMARY: &[u8] = b"acct/01";
/// The secondary: a witness key, so it lands in the *other* region — a different Raft group,
/// which is what makes "committed on the primary, lost on the secondary" reachable at all.
const SECONDARY: &[u8] = b"wit/01";

/// Short enough that a test can outlive a lease without sleeping for seconds; long enough that
/// a slow machine does not expire a lock while the test is still writing it.
const TTL_MS: u64 = 300;

/// How far a transaction got before its client died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// The primary is prewritten and nothing else.
    PrimaryPrewritten,
    /// Both keys are prewritten; no commit record exists anywhere.
    SecondariesPrewritten,
    /// The primary is committed. **The transaction has committed**; the secondary is not
    /// finished and its lock is still there.
    PrimaryCommitted,
    /// Everything is committed and the client died before it could tell anyone.
    SecondariesCommitted,
}

impl Boundary {
    /// Whether a transaction that stopped here committed.
    fn committed(self) -> bool {
        matches!(self, Self::PrimaryCommitted | Self::SecondariesCommitted)
    }

    fn name(self) -> &'static str {
        match self {
            Self::PrimaryPrewritten => "after the primary's prewrite",
            Self::SecondariesPrewritten => "after the secondaries' prewrite",
            Self::PrimaryCommitted => "after the primary's commit",
            Self::SecondariesCommitted => "after each secondary's commit",
        }
    }
}

fn call(router: &Router, request: TxnKvReq) -> TxnKvResp {
    router
        .call(&Body::Txn(request))
        .expect("the call reaches a store")
        .into_txn_kv()
        .expect("a TxnKv answer")
}

fn key(bytes: &[u8]) -> Bytes {
    Bytes::copy_from_slice(bytes)
}

/// Drives a transaction up to `boundary` and stops, the way a client that died would.
///
/// Answers with the `start_ts` it used, so a later assertion can name the transaction that was
/// abandoned rather than infer it.
fn abandon_at(cluster: &Cluster, router: &Router, boundary: Boundary, value: &[u8]) -> u64 {
    let start_ts = cluster.oracle().tso_one();

    let prewrite = |keys: Vec<TxnMutation>| {
        let response = call(
            router,
            TxnKvReq::Prewrite {
                start_ts,
                primary: key(PRIMARY),
                ttl_ms: TTL_MS,
                mutations: keys,
            },
        );
        match response {
            TxnKvResp::Prewrite { keys } => {
                assert!(
                    keys.iter().all(|status| *status == TxnStatus::Ok),
                    "the prewrite of an uncontended key was refused: {keys:?}"
                );
            }
            other => panic!("expected a Prewrite answer, got {other:?}"),
        }
    };

    prewrite(vec![TxnMutation::Put {
        key: key(PRIMARY),
        value: key(value),
    }]);
    if boundary == Boundary::PrimaryPrewritten {
        return start_ts;
    }

    prewrite(vec![TxnMutation::Put {
        key: key(SECONDARY),
        value: key(value),
    }]);
    if boundary == Boundary::SecondariesPrewritten {
        return start_ts;
    }

    let commit_ts = cluster.oracle().tso_one();
    let commit = |keys: Vec<Bytes>| match call(
        router,
        TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys,
        },
    ) {
        TxnKvResp::Commit { status } => assert_eq!(status, TxnStatus::Ok, "the commit was refused"),
        other => panic!("expected a Commit answer, got {other:?}"),
    };

    commit(vec![key(PRIMARY)]);
    if boundary == Boundary::PrimaryCommitted {
        return start_ts;
    }

    commit(vec![key(SECONDARY)]);
    start_ts
}

/// What a reader that arrives afterwards sees on both keys.
fn read_both(client: &TxnClient) -> (Option<Bytes>, Option<Bytes>) {
    let txn = client.begin().expect("a snapshot");
    let primary = txn.get(PRIMARY).expect("the primary reads");
    let secondary = txn.get(SECONDARY).expect("the secondary reads");
    (primary, secondary)
}

/// One boundary, end to end: abandon there, change the leadership under it, and read.
fn crash_at(boundary: Boundary, kill: bool) {
    let cluster = Cluster::start(Topology::two_regions(0x5c_0000 + boundary as u64));
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never elected a leader to begin with"
    );
    let router = cluster.router(7).expect("a router");

    let start_ts = abandon_at(&cluster, &router, boundary, b"transferred");

    if kill {
        // The records the resolution will read have to survive a leadership change: the
        // primary's `write` record and the secondary's lock are on different groups, and a
        // resolver reads one and writes the other.
        for group in 0..cluster.regions() {
            if let Some(at) = cluster.leader_of(group) {
                cluster.kill(at);
                cluster.start_node(at);
            }
        }
        assert!(
            cluster.settle(Duration::from_secs(30)),
            "the cluster never came back after both leaders were killed"
        );
    }

    // Past the abandoned transaction's lease, so a resolver is entitled to declare it dead.
    // The oracle's physical half is real milliseconds, which is what makes this a wait rather
    // than a fiction (`docs/txn-spec.md` §5.5).
    std::thread::sleep(Duration::from_millis(TTL_MS * 2));

    let client = cluster
        .client_within(11, Duration::from_secs(20))
        .expect("a client");
    let (primary, secondary) = read_both(&client);

    if boundary.committed() {
        assert_eq!(
            primary.as_deref(),
            Some(b"transferred".as_slice()),
            "{}: the primary's record is the commit point and it exists",
            boundary.name()
        );
        assert_eq!(
            secondary.as_deref(),
            Some(b"transferred".as_slice()),
            "{}: the transaction committed, so a reader must roll the secondary FORWARD — \
             rolling it back loses a key of a committed transaction (start_ts {start_ts})",
            boundary.name()
        );
    } else {
        assert_eq!(
            primary,
            None,
            "{}: nothing committed, so nothing is visible (start_ts {start_ts})",
            boundary.name()
        );
        assert_eq!(
            secondary,
            None,
            "{}: nothing committed, so nothing is visible (start_ts {start_ts})",
            boundary.name()
        );
    }

    // And the resolution was permanent rather than per-reader: a second reader, at a later
    // snapshot, sees the same thing without doing the work again.
    let (again_primary, again_secondary) = read_both(&client);
    assert_eq!(again_primary, primary, "{}", boundary.name());
    assert_eq!(again_secondary, secondary, "{}", boundary.name());

    cluster.shutdown();
}

#[test]
fn a_crash_after_the_primary_prewrite_rolls_back() {
    crash_at(Boundary::PrimaryPrewritten, false);
}

#[test]
fn a_crash_after_the_secondary_prewrite_rolls_back() {
    crash_at(Boundary::SecondariesPrewritten, false);
}

/// The one that matters: the transaction **committed**, and the only place that says so is a
/// `write` record in the *other* region. A resolver that reads its own region and finds a lock
/// with nothing behind it has to go and ask, and roll forward.
#[test]
fn a_crash_after_the_primary_commit_rolls_forward() {
    crash_at(Boundary::PrimaryCommitted, false);
}

#[test]
fn a_crash_after_the_secondary_commit_is_already_done() {
    crash_at(Boundary::SecondariesCommitted, false);
}

/// Every boundary again, with both regions' leaders killed and restarted in between.
///
/// Slower, and the reason it is worth its seconds: the resolution reads a record that was
/// written by a leader that no longer exists, on a peer that had to learn it from the log.
#[test]
#[ignore = "four boundaries, each with two leader kills; tens of seconds"]
fn every_boundary_survives_a_leader_change() {
    for boundary in [
        Boundary::PrimaryPrewritten,
        Boundary::SecondariesPrewritten,
        Boundary::PrimaryCommitted,
        Boundary::SecondariesCommitted,
    ] {
        crash_at(boundary, true);
    }
}

/// A lock **inside** its lease is not a dead transaction, and a reader that kills it anyway
/// breaks the transaction it belonged to.
///
/// This is the other half of the rule the four boundaries test: a resolver decides by the
/// lease, and it must be late rather than early (`docs/plans/phase-5.md` §10.2). The owner
/// here is alive and about to commit; the reader has to wait for it and then see its value,
/// not roll it back and read nothing.
#[test]
fn a_live_lock_is_waited_for_rather_than_killed() {
    let cluster = Cluster::start(Topology::two_regions(0x5c_1000));
    assert!(cluster.settle(Duration::from_secs(30)), "no leader");
    let router = cluster.router(3).expect("a router");

    // A lease long enough that it cannot expire while the reader is looking.
    let start_ts = cluster.oracle().tso_one();
    match call(
        &router,
        TxnKvReq::Prewrite {
            start_ts,
            primary: key(PRIMARY),
            ttl_ms: 30_000,
            mutations: vec![TxnMutation::Put {
                key: key(PRIMARY),
                value: key(b"alive"),
            }],
        },
    ) {
        TxnKvResp::Prewrite { keys } => assert_eq!(keys, vec![TxnStatus::Ok]),
        other => panic!("{other:?}"),
    }

    // The owner commits shortly after the reader has started looking.
    let owner = {
        let router = Arc::clone(&router);
        let commit_ts = cluster.oracle().tso_one();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            match call(
                &router,
                TxnKvReq::Commit {
                    start_ts,
                    commit_ts,
                    keys: vec![key(PRIMARY)],
                },
            ) {
                TxnKvResp::Commit { status } => assert_eq!(
                    status,
                    TxnStatus::Ok,
                    "a live transaction's commit was refused, so a reader killed it"
                ),
                other => panic!("{other:?}"),
            }
        })
    };

    let client = cluster.client_within(5, Duration::from_secs(20)).unwrap();
    let txn = client.begin().unwrap();
    let seen = txn.get(PRIMARY).expect("the read waits out a live lease");
    owner.join().expect("the owner commits");
    assert_eq!(
        seen.as_deref(),
        Some(b"alive".as_slice()),
        "a reader that meets a live lock must wait for its owner, not roll it back"
    );

    cluster.shutdown();
}
