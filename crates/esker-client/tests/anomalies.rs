//! The anomalies snapshot isolation prevents, and the one it allows — against real stores.
//!
//! `prompts/05-txn.md`: *classic anomaly tests; SI must prevent lost updates and dirty reads;
//! document that write skew is allowed*. The rules themselves are proved against three
//! `BTreeMap`s in `crates/esker-txn/tests/protocol.rs`; what these add is the whole stack —
//! two regions, two engines, the wire, the client's buffer and its lock resolution — because
//! an isolation level is a property of what a *user* observes, and every layer between the
//! decision and the user is a layer that can lose it.
//!
//! Each test is named for the anomaly and says which of `docs/txn-spec.md` §6's guarantees it
//! is exercising. The write-skew one is the odd one out: it asserts that the anomaly
//! **happens**, because a test that pins an allowance is the only thing that stops the
//! allowance quietly becoming a promise nobody meant to make.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use esker_client::wire::{Body, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
use esker_client::{Error, TxnClient};

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

/// One store per region, because nothing here is about Raft: an anomaly is a property of the
/// timestamps and the lock CF, and a leader election in the middle would only make the test
/// slower and its failures harder to read.
fn cluster(seed: u64) -> std::sync::Arc<Cluster> {
    let cluster = Cluster::start(Topology::unreplicated(seed));
    assert!(cluster.settle(Duration::from_secs(20)), "the stores start");
    cluster
}

fn client(cluster: &Cluster, seed: u64) -> TxnClient {
    cluster
        .client_within(seed, Duration::from_secs(20))
        .expect("a client")
}

fn write_one(client: &TxnClient, key: &[u8], value: &[u8]) {
    let mut txn = client.begin().unwrap();
    txn.put(key, value);
    txn.commit().unwrap().expect("it wrote something");
}

/// **Lost update, prevented.** Two transactions read the same key at their own snapshots and
/// both try to write it. First-committer-wins: exactly one commits, and the loser is refused
/// *before* it writes anything (`docs/txn-spec.md` §6).
///
/// The refusal names the key, which is what makes a unique-constraint violation distinguishable
/// from an ordinary serialization failure one layer up (§6.1).
#[test]
fn snapshot_isolation_prevents_a_lost_update() {
    let cluster = cluster(0xa0_0001);
    let client = client(&cluster, 1);
    write_one(&client, b"balance", &100u64.to_le_bytes());

    // Both read the same value, at snapshots taken before either wrote.
    let mut first = client.begin().unwrap();
    let mut second = client.begin().unwrap();
    let seen_by_first = first.get(b"balance").unwrap().unwrap();
    let seen_by_second = second.get(b"balance").unwrap().unwrap();
    assert_eq!(seen_by_first, seen_by_second, "one value, two readers");

    first.put(b"balance", &110u64.to_le_bytes());
    first.commit().unwrap().expect("the first writer commits");

    second.put(b"balance", &120u64.to_le_bytes());
    match second.commit() {
        Err(Error::TxnConflict { key, .. }) => assert_eq!(
            key,
            Some(Bytes::from_static(b"balance")),
            "the refusal names the key that lost"
        ),
        other => panic!("the second writer must not overwrite a commit it never read: {other:?}"),
    }

    // And the loser really lost: the winner's value is what is there.
    let after = client.begin().unwrap();
    assert_eq!(
        after.get(b"balance").unwrap().unwrap(),
        Bytes::copy_from_slice(&110u64.to_le_bytes())
    );

    cluster.shutdown();
}

/// **Dirty read, prevented.** A transaction's uncommitted write lives in the `lock` column
/// family and nowhere a reader can reach: a reader that meets the lock resolves it, and can
/// never read through it.
///
/// The writer here is abandoned rather than merely slow — its lease runs out and the reader
/// settles it — so what the reader sees is the value from *before*, and the writer's own late
/// commit is refused by the rollback marker it left (§5.4).
#[test]
fn snapshot_isolation_prevents_a_dirty_read() {
    let cluster = cluster(0xa0_0002);
    let client = client(&cluster, 2);
    write_one(&client, b"row", b"committed");

    // A writer that prewrites and stops. Driven by hand, because a `TxnClient` never leaves a
    // transaction in this state on purpose — which is exactly why it needs a test.
    let router = cluster.router(20).expect("a router");
    let start_ts = cluster.oracle().tso_one();
    let prewritten = router
        .call(&Body::Txn(TxnKvReq::Prewrite {
            start_ts,
            primary: Bytes::from_static(b"row"),
            ttl_ms: 200,
            mutations: vec![TxnMutation::Put {
                key: Bytes::from_static(b"row"),
                value: Bytes::from_static(b"dirty"),
                read_ts: None,
            }],
        }))
        .unwrap()
        .into_txn_kv()
        .unwrap();
    assert_eq!(
        prewritten,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::Ok]
        }
    );

    // Past the lease, so the reader is entitled to settle it rather than wait.
    std::thread::sleep(Duration::from_millis(400));

    let reader = client.begin().unwrap();
    assert_eq!(
        reader.get(b"row").unwrap(),
        Some(Bytes::from_static(b"committed")),
        "an uncommitted write must never be read"
    );

    // The writer was settled by the reader, so its commit is now refused for ever. A dirty read
    // avoided by *waiting* would not prove this half.
    let late = router
        .call(&Body::Txn(TxnKvReq::Commit {
            start_ts,
            commit_ts: cluster.oracle().tso_one(),
            keys: vec![Bytes::from_static(b"row")],
        }))
        .unwrap()
        .into_txn_kv()
        .unwrap();
    assert!(
        matches!(
            late,
            TxnKvResp::Commit {
                status: TxnStatus::RolledBack | TxnStatus::LockNotFound
            }
        ),
        "a transaction a reader rolled back must not commit afterwards: {late:?}"
    );

    cluster.shutdown();
}

/// **Non-repeatable read, prevented.** A transaction reads at its `start_ts` and nothing that
/// commits afterwards is visible to it, however many times it looks.
#[test]
fn a_snapshot_does_not_move_under_its_reader() {
    let cluster = cluster(0xa0_0003);
    let client = client(&cluster, 3);
    write_one(&client, b"row", b"first");

    let reader = client.begin().unwrap();
    assert_eq!(
        reader.get(b"row").unwrap(),
        Some(Bytes::from_static(b"first"))
    );

    write_one(&client, b"row", b"second");

    assert_eq!(
        reader.get(b"row").unwrap(),
        Some(Bytes::from_static(b"first")),
        "a commit above the snapshot is invisible to it"
    );
    // And a reader that starts now sees the new one, so the old answer was the snapshot and not
    // a stale cache.
    let later = client.begin().unwrap();
    assert_eq!(
        later.get(b"row").unwrap(),
        Some(Bytes::from_static(b"second"))
    );

    cluster.shutdown();
}

/// **Phantom, prevented within a snapshot.** A range scanned twice in one transaction returns
/// the same rows, even though another transaction committed a new key inside the range — and
/// even though that key spans a region boundary.
#[test]
fn a_scan_repeated_in_one_transaction_sees_no_phantoms() {
    let cluster = cluster(0xa0_0004);
    let client = client(&cluster, 4);
    // Either side of the boundary, so the scan walks both regions each time.
    write_one(&client, b"a1", b"1");
    write_one(&client, b"n1", b"2");

    let reader = client.begin().unwrap();
    let before = reader.scan(b"a", b"o", 100).unwrap();
    assert_eq!(before.len(), 2);

    write_one(&client, b"a2", b"3");
    write_one(&client, b"n2", b"4");

    let after = reader.scan(b"a", b"o", 100).unwrap();
    assert_eq!(
        after, before,
        "rows committed above the snapshot are not in it"
    );

    let later = client.begin().unwrap();
    assert_eq!(later.scan(b"a", b"o", 100).unwrap().len(), 4);

    cluster.shutdown();
}

/// **Write skew, allowed.** The anomaly snapshot isolation does *not* prevent, pinned as a test
/// so that `docs/txn-spec.md` §6 is a stated property rather than a hope.
///
/// The classic case: two doctors, an invariant that at least one is on call. Each reads both
/// rows, sees the other is on call, and takes themselves off — writing only their own row. The
/// two transactions write **disjoint** keys, so prewrite's conflict check, which is about the
/// keys a transaction *writes*, has nothing to refuse. Both commit, and the invariant across the
/// two rows is broken.
///
/// The two fixes and what they cost are in §6: `SELECT … FOR UPDATE` prewriting the read keys
/// with `kind = Lock` — the encoding is already there, the API is not — or serializable snapshot
/// isolation, which needs the read set on the server.
#[test]
fn snapshot_isolation_allows_write_skew() {
    let cluster = cluster(0xa0_0005);
    let client = client(&cluster, 5);
    // Deliberately one either side of the boundary: it changes nothing, and saying so rules out
    // "it only happens inside one region" as an explanation of the result.
    write_one(&client, b"a-on-call", b"yes");
    write_one(&client, b"n-on-call", b"yes");

    let mut doctor_a = client.begin().unwrap();
    let mut doctor_b = client.begin().unwrap();

    // Each reads the whole invariant and finds it satisfied by the other.
    assert_eq!(
        doctor_a.get(b"n-on-call").unwrap(),
        Some(Bytes::from_static(b"yes"))
    );
    assert_eq!(
        doctor_b.get(b"a-on-call").unwrap(),
        Some(Bytes::from_static(b"yes"))
    );

    // Each writes only their own row.
    doctor_a.put(b"a-on-call", b"no");
    doctor_b.put(b"n-on-call", b"no");
    doctor_a.commit().unwrap().expect("the first commits");
    doctor_b
        .commit()
        .expect("the second commits too — this is the anomaly")
        .expect("it wrote something");

    let after = client.begin().unwrap();
    assert_eq!(
        after.get(b"a-on-call").unwrap(),
        Some(Bytes::from_static(b"no"))
    );
    assert_eq!(
        after.get(b"n-on-call").unwrap(),
        Some(Bytes::from_static(b"no"))
    );
    // Nobody is on call. No serial order of the two transactions produces this, and snapshot
    // isolation permits it — see this test's header for what would not.

    cluster.shutdown();
}

/// **Check-then-insert on one key, safe.** The composition `esker-sql` builds unique indexes on
/// (`docs/txn-spec.md` §6.1): two transactions each read a key, find nothing, and write it.
/// Exactly one commits, and the loser is told which key it lost.
#[test]
fn two_inserts_of_one_new_key_leave_one_winner() {
    let cluster = cluster(0xa0_0006);
    let client = client(&cluster, 6);

    let mut first = client.begin().unwrap();
    let mut second = client.begin().unwrap();
    assert_eq!(first.get(b"unique").unwrap(), None);
    assert_eq!(second.get(b"unique").unwrap(), None);

    first.put(b"unique", b"first");
    second.put(b"unique", b"second");
    first.commit().unwrap().expect("the winner commits");

    match second.commit() {
        Err(Error::TxnConflict { key, .. }) => {
            assert_eq!(key, Some(Bytes::from_static(b"unique")));
        }
        other => panic!("both inserts must not succeed: {other:?}"),
    }

    let after = client.begin().unwrap();
    assert_eq!(
        after.get(b"unique").unwrap(),
        Some(Bytes::from_static(b"first"))
    );

    cluster.shutdown();
}

/// **A range check meets a lock, waits for it, and is refused when it commits** — and never
/// wounds it ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md)
/// §1).
///
/// The store answers `Locked` for a lock inside a checked range, because a transaction that has
/// prewritten into the range but not committed is a phantom in flight. What the client owes it is
/// a wait, not a wound: a range check **acquires nothing**, so it cannot be half of a cycle, and
/// killing the holder to make "this range did not move" true would abort a transaction that did
/// nothing wrong. The holder here is *younger* than the checker, which is exactly the case the
/// wound rule would kill.
///
/// The assertion is in two halves and both are needed: the checker is refused, **and the holder's
/// lock is still there afterwards**. Only the second one can tell a wait from a wound.
#[test]
fn a_range_check_waits_for_a_lock_it_may_not_wound() {
    let cluster = cluster(0xa0_0104);
    let client = client(&cluster, 104);
    let router = cluster.router(104).expect("a router");

    // The checker's snapshot is taken first, so the holder below is younger than it.
    let mut checker = client.begin().unwrap();
    let holder_ts = cluster.oracle().tso_one();
    assert!(
        holder_ts > checker.start_ts(),
        "the holder must be the younger of the two for the wound rule to have an opinion"
    );

    let held = router
        .call(&Body::Txn(TxnKvReq::Prewrite {
            start_ts: holder_ts,
            primary: Bytes::from_static(b"a7"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: Bytes::from_static(b"a7"),
                value: Bytes::from_static(b"in flight"),
                read_ts: None,
            }],
        }))
        .unwrap()
        .into_txn_kv()
        .unwrap();
    assert_eq!(held, TxnKvResp::prewrite_ok(1));

    // `a7`, not something past `m`: the topology's boundary is `b"m"`, and a range check is
    // answered by the region its **lower bound** falls in and no further (ADR 0067 §3). A holder
    // on the far side of the boundary is a limitation this test must not accidentally measure.
    checker.put(b"a5", b"mine");
    checker.checking(
        [],
        vec![(Bytes::from_static(b"a"), Bytes::from_static(b"z"))],
    );
    let refused = checker
        .commit()
        .expect_err("a lock inside the read range is a phantom this transaction cannot rule out");
    assert!(
        matches!(refused, Error::LockNotCleared { start_ts, .. } if start_ts == holder_ts),
        "{refused}"
    );

    // **The half that proves it waited rather than wounded.** A wound would have left a rollback
    // marker on the holder's primary and taken its lock away.
    let still = router
        .call(&Body::Txn(TxnKvReq::Prewrite {
            start_ts: cluster.oracle().tso_one(),
            primary: Bytes::from_static(b"a"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::CheckRange {
                start: Bytes::from_static(b"a"),
                end: Bytes::from_static(b"z"),
            }],
        }))
        .unwrap()
        .into_txn_kv()
        .unwrap();
    let TxnKvResp::Prewrite { keys } = still else {
        panic!("a prewrite answers a prewrite")
    };
    assert!(
        matches!(&keys[..], [TxnStatus::Locked(lock)] if lock.start_ts == holder_ts),
        "the holder is untouched: {keys:?}"
    );

    cluster.shutdown();
}

/// **And when the lock turns out to be nobody's, the check is asked again and passes.**
///
/// The counterfactual of the test above, and the reason the store answers `Locked` rather than
/// `Conflict`: a lock is a question, not a verdict. This holder's lease runs out, so the checker
/// settles it — leaving a rollback marker, which ADR 0078 already says is not a phantom — asks the
/// range again, and commits. A client that read `Locked` as "you lost" would refuse this
/// transaction for a row that never existed.
#[test]
fn a_range_check_that_outlives_the_lock_commits() {
    let cluster = cluster(0xa0_0105);
    let client = client(&cluster, 105);
    let router = cluster.router(105).expect("a router");

    // A lease short enough that the resolution loop outlives it, on this side of the `b"m"`
    // boundary so that the range check actually reaches it.
    let abandoned = router
        .call(&Body::Txn(TxnKvReq::Prewrite {
            start_ts: cluster.oracle().tso_one(),
            primary: Bytes::from_static(b"a7"),
            ttl_ms: 1,
            mutations: vec![TxnMutation::Put {
                key: Bytes::from_static(b"a7"),
                value: Bytes::from_static(b"abandoned"),
                read_ts: None,
            }],
        }))
        .unwrap()
        .into_txn_kv()
        .unwrap();
    assert_eq!(abandoned, TxnKvResp::prewrite_ok(1));

    let mut checker = client.begin().unwrap();
    checker.put(b"a5", b"mine");
    checker.checking(
        [],
        vec![(Bytes::from_static(b"a"), Bytes::from_static(b"z"))],
    );
    checker
        .commit()
        .expect("the lock was settled and left a marker, which is not a phantom");

    cluster.shutdown();
}
