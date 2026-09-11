//! Reading the database as it was: `begin_at`, and the three refusals around it.
//!
//! [ADR 0021](../../../docs/adr/0021-time-machine.md) decision 1 — *a historical read is a read
//! timestamp, and nothing else*. The storage layer has always been a time machine: every
//! version is filed under `commit_ts` and a read at `T` is "the newest write with
//! `commit_ts ≤ T`", which is what `TxnKv` already does with whatever `start_ts` it was given.
//! What was missing was a surface and a **bound**, and the bound is the interesting half —
//! the machinery would happily answer questions it cannot answer correctly.
//!
//! So most of this file is about the refusals:
//!
//! * a write at a past snapshot, which Percolator's conflict check cannot catch;
//! * a read below the safepoint, where some versions are collected and some are not, so the
//!   answer would be a state that never existed;
//! * a read above the oracle's high-water mark, which would see a prefix of an instant and
//!   call it complete.
//!
//! Each is a refusal rather than a clamp. A clamp answers a question the caller did not ask.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use esker_client::wire::{Body, TxnKvReq, TxnKvResp};
use esker_client::{Error, TxnClient, physical_ms, ts_at_ms};

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

fn cluster(seed: u64) -> std::sync::Arc<Cluster> {
    let cluster = Cluster::start(Topology::unreplicated(seed));
    assert!(cluster.settle(Duration::from_secs(20)), "the stores start");
    cluster
}

fn write_one(client: &TxnClient, key: &[u8], value: &[u8]) -> u64 {
    let mut txn = client.begin().unwrap();
    txn.put(key, value);
    txn.commit().unwrap().expect("it wrote something")
}

/// The feature itself: a value written, overwritten, and both readable — each at its own
/// timestamp, from one client, with no snapshot machinery anywhere.
#[test]
fn a_read_at_a_past_timestamp_sees_what_was_there_then() {
    let cluster = cluster(0x71_0001);
    let client = cluster.client_within(1, Duration::from_secs(20)).unwrap();

    let first = write_one(&client, b"a-row", b"before");
    let second = write_one(&client, b"a-row", b"after");
    assert!(second > first, "the oracle moves forward");

    // Now.
    assert_eq!(
        client.begin().unwrap().get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"after"))
    );
    // As of the first commit: the old value, and only it.
    assert_eq!(
        client.begin_at(first).unwrap().get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"before")),
        "a read at a past timestamp answers with the version that was newest then"
    );
    // As of one tick before the first commit: the row does not exist yet.
    assert_eq!(
        client.begin_at(first - 1).unwrap().get(b"a-row").unwrap(),
        None,
        "before its first commit the key is absent, not empty"
    );

    cluster.shutdown();
}

/// A scan travels too, which is what makes the feature usable for anything larger than a key.
#[test]
fn a_scan_at_a_past_timestamp_sees_the_rows_of_that_moment() {
    let cluster = cluster(0x71_0002);
    let client = cluster.client_within(2, Duration::from_secs(20)).unwrap();

    write_one(&client, b"a1", b"1");
    let after_one = write_one(&client, b"a2", b"2");
    write_one(&client, b"a3", b"3");

    let then = client.begin_at(after_one).unwrap();
    let rows: Vec<Bytes> = then
        .scan(b"a", b"b", 100)
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        rows,
        vec![Bytes::from_static(b"a1"), Bytes::from_static(b"a2")],
        "the third row had not been written at that timestamp"
    );

    assert_eq!(
        client.begin().unwrap().scan(b"a", b"b", 100).unwrap().len(),
        3
    );
    cluster.shutdown();
}

/// **Read-only, and the refusal names the key.** A historical transaction that could commit
/// would be a lost update the conflict check cannot see: the competing writer committed after
/// the old snapshot and before this write, which is the one window snapshot isolation leaves
/// open (ADR 0021 decision 1).
#[test]
fn a_transaction_in_the_past_cannot_write() {
    let cluster = cluster(0x71_0003);
    let client = cluster.client_within(3, Duration::from_secs(20)).unwrap();
    let before = write_one(&client, b"a-row", b"original");

    let mut past = client.begin_at(before).unwrap();
    assert!(past.is_read_only());
    past.put(b"a-row", b"rewritten");
    past.delete(b"another");

    match past.commit() {
        Err(Error::ReadOnlyTransaction { start_ts, key }) => {
            assert_eq!(start_ts, before);
            assert_eq!(
                key,
                Bytes::from_static(b"a-row"),
                "the first key that tried"
            );
        }
        other => panic!("a past transaction must not commit: {other:?}"),
    }

    // And nothing of it reached anybody: the value is what it was.
    assert_eq!(
        client.begin().unwrap().get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"original"))
    );

    cluster.shutdown();
}

/// The dropped write is dropped *from the read too*, which is the point: a historical read that
/// answered with the caller's own phantom write would be lying about the one thing it exists to
/// be honest about.
#[test]
fn a_refused_write_does_not_become_a_phantom_read() {
    let cluster = cluster(0x71_0004);
    let client = cluster.client_within(4, Duration::from_secs(20)).unwrap();
    let before = write_one(&client, b"a-row", b"original");

    let mut past = client.begin_at(before).unwrap();
    past.put(b"a-row", b"phantom");
    assert_eq!(
        past.get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"original")),
        "read-your-writes does not apply to a write that was refused"
    );
    assert!(past.is_empty(), "nothing was buffered");

    cluster.shutdown();
}

/// **Not in the future.** A timestamp above the oracle's high-water mark names an instant that
/// has not happened.
#[test]
fn a_read_in_the_future_is_refused() {
    let cluster = cluster(0x71_0005);
    let client = cluster.client_within(5, Duration::from_secs(20)).unwrap();
    write_one(&client, b"a-row", b"now");

    let now = cluster.oracle().tso_one();
    let later = ts_at_ms(physical_ms(now) + 60_000);
    match client.begin_at(later) {
        Err(Error::SnapshotInTheFuture { requested, now: at }) => {
            assert_eq!(requested, later);
            assert!(at <= later, "the mark it was compared against");
        }
        other => panic!("a read in the future must be refused: {other:?}"),
    }

    cluster.shutdown();
}

/// **Not below the safepoint** — unit 4's retention-floor interaction, both sides of the edge.
///
/// The floor is the collector's, not a second number invented for this feature: how far back a
/// read may go *is* how far back the collector has not swept (ADR 0021 decision 2). So the test
/// moves the real safepoint and asks either side of it.
///
/// At the floor **exactly** the read succeeds, and that is not an off-by-one to tidy away: the
/// collector keeps the newest version at or below the safepoint precisely because that is what
/// a read at the safepoint returns (`docs/txn-spec.md` §7). One tick below, versions are gone.
#[test]
fn a_read_below_the_retention_floor_is_refused_and_at_it_is_not() {
    let cluster = cluster(0x71_0006);
    let client = cluster.client_within(6, Duration::from_secs(20)).unwrap();

    let old = write_one(&client, b"a-row", b"ancient");
    let recent = write_one(&client, b"a-row", b"recent");
    assert_eq!(client.safepoint().unwrap(), 0, "nothing is collected yet");

    // Publish a safepoint between the two commits: everything below it may be collected, so
    // nothing below it can be answered for.
    let floor = old + 1;
    let router = cluster.router(60).expect("a router");
    match router
        .call(&Body::Txn(TxnKvReq::GcSafepoint { safepoint: floor }))
        .unwrap()
        .into_txn_kv()
        .unwrap()
    {
        TxnKvResp::GcSafepoint { safepoint } => assert_eq!(safepoint, floor),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.safepoint().unwrap(), floor, "the client sees it");

    // Below the floor: refused, and the refusal says how far back the caller may ask.
    match client.begin_at(old) {
        Err(Error::SnapshotTooOld {
            requested,
            floor: named,
        }) => {
            assert_eq!(requested, old);
            assert_eq!(named, floor, "the error names the window");
        }
        other => panic!("a read below the safepoint must be refused: {other:?}"),
    }

    // At the floor exactly: answered.
    let at_edge = client
        .begin_at(floor)
        .expect("the floor itself is readable");
    assert_eq!(
        at_edge.get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"ancient")),
        "the newest version at or below the safepoint is the one kept, and it answers"
    );
    // And a read above it is unaffected.
    assert_eq!(
        client.begin_at(recent).unwrap().get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"recent"))
    );

    cluster.shutdown();
}

/// A query, not a write: asking for the safepoint publishes zero, and a store's safepoint only
/// ever rises — so the question cannot lower the answer. This is why the client needs no verb
/// of its own for it.
#[test]
fn asking_for_the_safepoint_does_not_move_it() {
    let cluster = cluster(0x71_0007);
    let client = cluster.client_within(7, Duration::from_secs(20)).unwrap();
    let router = cluster.router(70).expect("a router");

    let floor = ts_at_ms(1_000);
    let _ = router
        .call(&Body::Txn(TxnKvReq::GcSafepoint { safepoint: floor }))
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            client.safepoint().unwrap(),
            floor,
            "asking again must not reset it to the zero the question carries"
        );
    }

    cluster.shutdown();
}

// -- checkpoints ---------------------------------------------------------------------------

/// A checkpoint names the present and a later transaction reads it back — and what makes it a
/// checkpoint rather than a handle is that it outlives the session that took it, which is the
/// one place this diverges from `pg_export_snapshot()` (ADR 0021 decision 3).
#[test]
fn a_named_snapshot_is_read_back_at_the_moment_it_named() {
    let cluster = cluster(0x71_0009);
    let client = cluster.client_within(9, Duration::from_secs(20)).unwrap();

    write_one(&client, b"a-row", b"at the checkpoint");
    let exported = client.export_snapshot(b"named/before-the-change").unwrap();
    write_one(&client, b"a-row", b"after the change");

    assert_eq!(
        client.snapshot_at(b"named/before-the-change").unwrap(),
        exported,
        "the name resolves to the timestamp it was exported at"
    );
    let then = client
        .begin_at_snapshot(b"named/before-the-change")
        .unwrap();
    assert_eq!(
        then.get(b"a-row").unwrap(),
        Some(Bytes::from_static(b"at the checkpoint")),
        "reading at a checkpoint is reading at its timestamp, and nothing more"
    );
    assert!(then.is_read_only(), "a checkpoint is a past instant");

    // A *different* client, of the kind that did not take it: the record is in the database,
    // not in the exporting session.
    let stranger = cluster.client_within(99, Duration::from_secs(20)).unwrap();
    assert_eq!(
        stranger
            .begin_at_snapshot(b"named/before-the-change")
            .unwrap()
            .get(b"a-row")
            .unwrap(),
        Some(Bytes::from_static(b"at the checkpoint"))
    );

    cluster.shutdown();
}

/// A name that was never exported is a refusal, not an empty answer: PostgreSQL's
/// `42704 snapshot "..." does not exist`.
#[test]
fn a_snapshot_that_was_never_exported_is_refused() {
    let cluster = cluster(0x71_000a);
    let client = cluster.client_within(10, Duration::from_secs(20)).unwrap();

    match client.begin_at_snapshot(b"named/never-taken") {
        Err(Error::NoSuchSnapshot { name }) => {
            assert_eq!(name, Bytes::from_static(b"named/never-taken"));
        }
        other => panic!("an unknown name must be refused: {other:?}"),
    }

    cluster.shutdown();
}

/// A checkpoint is a claim, and this is the claim failing: named, then collected past.
///
/// It is the composition that matters — the name still resolves, and the *read* is what
/// refuses, with the window in it. A checkpoint that promised its data would be a promise the
/// storage layer never made.
#[test]
fn a_checkpoint_older_than_the_window_names_history_that_is_gone() {
    let cluster = cluster(0x71_000b);
    let client = cluster.client_within(11, Duration::from_secs(20)).unwrap();

    write_one(&client, b"a-row", b"ancient");
    let named = client.export_snapshot(b"named/too-old").unwrap();
    let recent = write_one(&client, b"a-row", b"recent");

    let router = cluster.router(61).expect("a router");
    let _ = router
        .call(&Body::Txn(TxnKvReq::GcSafepoint { safepoint: recent }))
        .unwrap();

    assert_eq!(
        client.snapshot_at(b"named/too-old").unwrap(),
        named,
        "the name still resolves — it is the read that cannot be answered"
    );
    match client.begin_at_snapshot(b"named/too-old") {
        Err(Error::SnapshotTooOld { requested, floor }) => {
            assert_eq!(requested, named);
            assert_eq!(floor, recent);
        }
        other => panic!("a checkpoint past the window must refuse: {other:?}"),
    }

    cluster.shutdown();
}

/// A value that is not one of these records is a typed refusal rather than a number read out
/// of whatever bytes were there (`CLAUDE.md` invariant 2).
#[test]
fn a_name_holding_something_else_is_not_read_as_a_timestamp() {
    let cluster = cluster(0x71_000c);
    let client = cluster.client_within(12, Duration::from_secs(20)).unwrap();

    // An ordinary value at the name, as an application that reused the key would leave.
    write_one(&client, b"named/not-a-snapshot", b"just a value");
    assert!(
        matches!(
            client.snapshot_at(b"named/not-a-snapshot"),
            Err(Error::Store(esker_client::wire::ProtoError::Corrupt { .. }))
        ),
        "a nine-byte record is the whole format; anything else is not one"
    );

    // And a record of a version this build does not read.
    let mut txn = client.begin().unwrap();
    txn.put(b"named/from-the-future", &[9u8, 0, 0, 0, 0, 0, 0, 0, 0]);
    txn.commit().unwrap();
    assert!(matches!(
        client.snapshot_at(b"named/from-the-future"),
        Err(Error::Store(esker_client::wire::ProtoError::Corrupt { .. }))
    ));

    cluster.shutdown();
}

/// `ts_ago` is measured from a timestamp the **oracle** handed out, never from this machine's
/// clock (`CLAUDE.md` invariant 6), and it saturates rather than wrapping.
#[test]
fn a_timestamp_from_a_duration_comes_from_the_oracle() {
    let cluster = cluster(0x71_0008);
    let client = cluster.client_within(8, Duration::from_secs(20)).unwrap();

    let now = cluster.oracle().tso_one();
    let ago = client.ts_ago(Duration::from_millis(500)).unwrap();
    assert!(ago < now, "the past is below the present");
    // **500 is the ceiling here, not the floor**, and the bound was on the wrong side of it.
    // `ts_ago` takes its *own* TSO read — `self.oracle.timestamp()` — which happens after the
    // `now` above, so `ago = later - 500` and `gap = 500 - (later - now)`. The two reads cannot
    // run in the other order, so `gap` reaches 500 only when both land in the same millisecond
    // and is below it by however long the second call took. A range starting at 500 therefore
    // fails on any machine slow enough to cross a millisecond boundary between two TSO reads,
    // which is not load-sensitivity to be waited out: it is the assertion asking for a value the
    // code cannot produce.
    //
    // **Measured at exactly 500, fifteen runs, quiet and under sixty-four spinning threads.** The
    // two TSO reads always land in the same millisecond, so the 400 floor is a hundred
    // milliseconds of slack that has never been touched: this range is `== 500` in practice, and
    // the floor is there for the boundary crossing the paragraph above describes rather than for
    // anything observed (`docs/plans/debt-c6.md` §13).
    let gap = physical_ms(now).saturating_sub(physical_ms(ago));
    assert!(
        (400..=500).contains(&gap),
        "half a second back, less the time the second TSO read took: {gap} ms"
    );

    // Longer than the oracle's clock has run: the bottom of the space, refused as too old
    // rather than wrapped into the future.
    let far = client
        .ts_ago(Duration::from_secs(60 * 60 * 24 * 365 * 100))
        .unwrap();
    assert_eq!(far, 0);

    cluster.shutdown();
}
