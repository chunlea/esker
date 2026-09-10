//! The seven `TxnKv` methods, over a real socket against a real engine.
//!
//! `esker-txn`'s own matrix proves the *rules* against three `BTreeMap`s
//! (`crates/esker-txn/tests/protocol.rs`); what this file proves is that the store asks them the
//! right questions and writes down the answers — the snapshot over three real column families,
//! the `'x'` namespace applied once and only once, and every verb reaching apply and coming back
//! as a response.
//!
//! Nothing here is stubbed. The bytes go through the kernel and the records land on a disk,
//! which is where a layering mistake — a prefix applied twice, a decision made on the wrong
//! snapshot, a lock left behind by a refused batch — actually shows up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_proto::txn::{LockInfo, TxnStatus};
use esker_proto::{
    Epoch, ProtoError, Request, RequestHeader, Server, ServerHandle, TcpTransport, Transport,
    TransportConfig, TxnKvReq, TxnKvResp, TxnMutation,
};
use esker_store::{Store, StoreOptions, StoreService};
use tempfile::TempDir;

struct Running {
    handle: ServerHandle,
    store: Arc<Store>,
    #[allow(dead_code)]
    dir: TempDir,
}

async fn start() -> Running {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let handle = Server::bind(
        "127.0.0.1:0",
        StoreService::new(Arc::clone(&store)),
        TransportConfig::new(),
    )
    .await
    .unwrap()
    .spawn()
    .unwrap();
    Running { handle, store, dir }
}

fn header() -> RequestHeader {
    RequestHeader::new(1, Epoch::INITIAL, 0)
}

async fn call(transport: &TcpTransport, request: TxnKvReq) -> Result<TxnKvResp, ProtoError> {
    transport
        .call(Request::txn_kv(header(), request))
        .await?
        .into_txn_kv()
}

fn key(bytes: &'static [u8]) -> Bytes {
    Bytes::from_static(bytes)
}

fn put(k: &'static [u8], v: &'static [u8]) -> TxnMutation {
    TxnMutation::Put {
        key: key(k),
        value: key(v),
        read_ts: None,
    }
}

/// Prewrite then commit one key, as a client would.
async fn commit_one(
    transport: &TcpTransport,
    k: &'static [u8],
    v: &'static [u8],
    start_ts: u64,
    commit_ts: u64,
) {
    let prewrite = call(
        transport,
        TxnKvReq::Prewrite {
            start_ts,
            primary: key(k),
            ttl_ms: 3_000,
            mutations: vec![put(k, v)],
        },
    )
    .await
    .unwrap();
    assert_eq!(prewrite, TxnKvResp::prewrite_ok(1), "prewrite of {k:?}");

    let commit = call(
        transport,
        TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![key(k)],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        commit,
        TxnKvResp::Commit {
            status: TxnStatus::Ok
        }
    );
}

async fn get(transport: &TcpTransport, k: &'static [u8], ts: u64) -> Option<Bytes> {
    match call(transport, TxnKvReq::Get { key: key(k), ts })
        .await
        .unwrap()
    {
        TxnKvResp::Get { value } => value,
        other => panic!("{other:?}"),
    }
}

// -- the whole protocol, over the wire ------------------------------------------------------

/// A transaction's life, end to end: nothing visible before the commit, everything after it,
/// and the snapshot boundary exactly where `docs/txn-spec.md` §5.1 says.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transaction_is_invisible_until_it_commits() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    assert_eq!(get(&transport, b"k", 100).await, None, "nothing yet");

    let prewrite = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"k"),
            ttl_ms: 3_000,
            mutations: vec![put(b"k", b"v")],
        },
    )
    .await
    .unwrap();
    assert_eq!(prewrite, TxnKvResp::prewrite_ok(1));

    // A prewritten key is *locked*, not visible: a reader at a snapshot above the lock meets it
    // and is told so, rather than reading through it.
    let blocked = call(
        &transport,
        TxnKvReq::Get {
            key: key(b"k"),
            ts: 50,
        },
    )
    .await
    .unwrap_err();
    let lock = LockInfo::from_error(&blocked)
        .expect("a locked key answers with a lock")
        .expect("the lock decodes");
    assert_eq!(lock.key, key(b"k"));
    assert_eq!(lock.primary, key(b"k"));
    assert_eq!(lock.start_ts, 10);
    assert_eq!(lock.ttl_ms, 3_000);

    call(
        &transport,
        TxnKvReq::Commit {
            start_ts: 10,
            commit_ts: 20,
            keys: vec![key(b"k")],
        },
    )
    .await
    .unwrap();

    assert_eq!(get(&transport, b"k", 19).await, None, "below the commit");
    assert_eq!(
        get(&transport, b"k", 20).await,
        Some(key(b"v")),
        "a commit at exactly the read timestamp is visible"
    );
    assert_eq!(get(&transport, b"k", 1_000).await, Some(key(b"v")));
}

/// A value over the inline cutoff travels through the `default` column family and comes back
/// whole — the one path where a read needs two column families rather than one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_value_round_trips_through_the_default_cf() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();
    let long = Bytes::from(vec![7u8; 4096]);

    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"big"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"big"),
                value: long.clone(),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    call(
        &transport,
        TxnKvReq::Commit {
            start_ts: 10,
            commit_ts: 20,
            keys: vec![key(b"big")],
        },
    )
    .await
    .unwrap();

    assert_eq!(get(&transport, b"big", 20).await, Some(long));
}

/// Write-write conflict, over the wire: the loser is refused and nothing of it is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_after_the_snapshot_refuses_the_second_writer() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"k", b"winner", 30, 40).await;

    let refused = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"k"),
            ttl_ms: 3_000,
            mutations: vec![put(b"k", b"loser")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        refused,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::Conflict { commit_ts: 40 }]
        }
    );
    assert_eq!(
        get(&transport, b"k", 1_000).await,
        Some(key(b"winner")),
        "the winner's value is what stands"
    );
}

/// A batch is **one decision**: a prewrite refused on any key writes nothing at all, so the keys
/// that would have succeeded are not left locked for a transaction that has been told it lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_prewrite_leaves_no_lock_behind() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"b", b"winner", 30, 40).await;

    // `a` would have succeeded; `b` conflicts.
    let refused = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"1"), put(b"b", b"2")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        refused,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::Ok, TxnStatus::Conflict { commit_ts: 40 }]
        },
        "per key, and the one that would have worked says so"
    );

    // The proof that nothing was staged: `a` is readable by anyone, because no lock is on it.
    assert_eq!(
        get(&transport, b"a", 1_000).await,
        None,
        "a refused batch left no value"
    );
    // And a *different* transaction can take it, which a leftover lock would have prevented.
    let taken = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 50,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"mine")],
        },
    )
    .await
    .unwrap();
    assert_eq!(taken, TxnKvResp::prewrite_ok(1), "no lock was left behind");
}

/// A prewrite that meets another transaction's lock reports it **per key**, with everything a
/// resolver needs ([ADR 0016](../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_conflict_comes_back_per_key_with_the_lock() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"primary"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"1"), put(b"b", b"2")],
        },
    )
    .await
    .unwrap();

    let refused = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 20,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"x"), put(b"b", b"y")],
        },
    )
    .await
    .unwrap();
    let TxnKvResp::Prewrite { keys: statuses } = refused else {
        panic!("not a prewrite answer");
    };
    let locks: Vec<&LockInfo> = statuses.iter().filter_map(TxnStatus::lock).collect();
    assert_eq!(locks.len(), 2, "both collisions, in one answer");
    for lock in locks {
        assert_eq!(lock.start_ts, 10);
        assert_eq!(
            lock.primary,
            key(b"primary"),
            "a resolver is pointed at the primary, which may be in another region"
        );
    }
}

/// `Rollback` leaves a marker, and the marker is what makes a late `Prewrite` fail
/// (`docs/txn-spec.md` §5.4). That is the one thing here that is deliberately *not* revisitable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rollback_marker_refuses_a_late_prewrite() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let rolled = call(
        &transport,
        TxnKvReq::Rollback {
            start_ts: 10,
            keys: vec![key(b"k")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rolled,
        TxnKvResp::Rollback {
            status: TxnStatus::Ok
        }
    );

    let late = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"k"),
            ttl_ms: 3_000,
            mutations: vec![put(b"k", b"too late")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        late,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::RolledBack]
        },
        "the marker is what a late prewrite meets"
    );
    // A different transaction is unaffected: the marker is about one `start_ts`.
    let other = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 11,
            primary: key(b"k"),
            ttl_ms: 3_000,
            mutations: vec![put(b"k", b"fine")],
        },
    )
    .await
    .unwrap();
    assert_eq!(other, TxnKvResp::prewrite_ok(1));
}

/// `ResolveLock` applies the caller's verdict. The store does **not** classify the transaction:
/// its primary may be in another region entirely (`docs/plans/phase-5.md` §10.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve_lock_applies_the_verdict_it_is_given() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    // Two secondaries of a transaction whose primary is elsewhere.
    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"far-away"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"1"), put(b"b", b"2")],
        },
    )
    .await
    .unwrap();

    // The caller read the primary's region and found it committed at 20.
    let resolved = call(
        &transport,
        TxnKvReq::ResolveLock {
            start_ts: 10,
            commit_ts: 20,
            keys: vec![key(b"a"), key(b"b")],
        },
    )
    .await
    .unwrap();
    assert_eq!(resolved, TxnKvResp::ResolveLock { resolved: 2 });
    assert_eq!(get(&transport, b"a", 20).await, Some(key(b"1")));
    assert_eq!(get(&transport, b"b", 20).await, Some(key(b"2")));

    // Asking again is a no-op rather than an error: the caller wanted the locks gone and they
    // are gone, which is a success and not a race lost.
    let again = call(
        &transport,
        TxnKvReq::ResolveLock {
            start_ts: 10,
            commit_ts: 20,
            keys: vec![key(b"a"), key(b"b")],
        },
    )
    .await
    .unwrap();
    assert_eq!(again, TxnKvResp::ResolveLock { resolved: 0 });
}

/// The other verdict: zero means the caller judged the lease expired, and the keys roll back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_commit_ts_rolls_the_keys_back() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"far-away"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"1")],
        },
    )
    .await
    .unwrap();
    let resolved = call(
        &transport,
        TxnKvReq::ResolveLock {
            start_ts: 10,
            commit_ts: 0,
            keys: vec![key(b"a")],
        },
    )
    .await
    .unwrap();
    assert_eq!(resolved, TxnKvResp::ResolveLock { resolved: 1 });
    assert_eq!(get(&transport, b"a", 1_000).await, None, "rolled back");

    // And the marker is there: the dead transaction cannot come back.
    let late = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![put(b"a", b"zombie")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        late,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::RolledBack]
        }
    );
}

/// A heartbeat extends a lease and never shortens one — a heartbeat that arrived out of order
/// behind a longer one would otherwise pull the lease in under a resolver that already read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heartbeat_extends_a_lease_and_never_shortens_it() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 10,
            primary: key(b"p"),
            ttl_ms: 3_000,
            mutations: vec![put(b"p", b"v")],
        },
    )
    .await
    .unwrap();

    let extended = call(
        &transport,
        TxnKvReq::Heartbeat {
            start_ts: 10,
            primary: key(b"p"),
            ttl_ms: 60_000,
        },
    )
    .await
    .unwrap();
    assert_eq!(extended, TxnKvResp::Heartbeat { ttl_ms: 60_000 });

    let stale = call(
        &transport,
        TxnKvReq::Heartbeat {
            start_ts: 10,
            primary: key(b"p"),
            ttl_ms: 5_000,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        stale,
        TxnKvResp::Heartbeat { ttl_ms: 60_000 },
        "a shorter TTL does not pull the lease in"
    );

    // A heartbeat for a transaction with no lock here answers zero rather than failing: it may
    // have committed while the heartbeat was in flight, and what a client acts on is the TTL it
    // now has.
    let gone = call(
        &transport,
        TxnKvReq::Heartbeat {
            start_ts: 999,
            primary: key(b"p"),
            ttl_ms: 60_000,
        },
    )
    .await
    .unwrap();
    assert_eq!(gone, TxnKvResp::Heartbeat { ttl_ms: 0 });
}

/// A scan reads at a timestamp, collapses a key's versions to one row, and honours deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scan_reads_one_row_per_key_at_its_timestamp() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"a", b"1", 10, 20).await;
    commit_one(&transport, b"b", b"2", 10, 20).await;
    commit_one(&transport, b"c", b"3", 10, 20).await;
    // `b` is rewritten, so it has two versions and must still be one row.
    commit_one(&transport, b"b", b"2-again", 30, 40).await;

    let scanned = call(
        &transport,
        TxnKvReq::Scan {
            start: key(b"a"),
            end: key(b"c"),
            limit: 100,
            ts: 1_000,
            reverse: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        scanned,
        TxnKvResp::Scan {
            pairs: vec![(key(b"a"), key(b"1")), (key(b"b"), key(b"2-again")),],
        },
        "the end is exclusive and a rewritten key is one row"
    );

    // At an older snapshot, the older version.
    let older = call(
        &transport,
        TxnKvReq::Scan {
            start: key(b"a"),
            end: key(b"c"),
            limit: 100,
            ts: 20,
            reverse: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        older,
        TxnKvResp::Scan {
            pairs: vec![(key(b"a"), key(b"1")), (key(b"b"), key(b"2"))],
        }
    );
}

/// A key that is **only locked** is part of a scan's answer, not something it walks past.
///
/// The lock CF is the whole of a prewritten key's existence: there is no `write` record until
/// it commits. So a scan built from the `write` column family alone would answer without it —
/// with no lock reported, nothing for the client to resolve, and no way to tell the difference
/// between a row that is not there and a row nobody looked in the right place for.
///
/// The case that makes it a lost row rather than a slow one: a transaction that has **committed
/// its primary** in another region and not yet its secondary. It is committed; its row exists;
/// and until someone resolves that lock the only trace of it here is the lock. That is the
/// state the bank test's crashed clients leave behind constantly, and it is how this was found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scan_reports_a_key_that_is_only_locked() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"a", b"1", 10, 20).await;
    // `b` is prewritten and never committed here: its primary is `a`, which in a real cluster
    // is where the transaction's fate is written and may well be another region.
    call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 30,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![put(b"b", b"pending")],
        },
    )
    .await
    .unwrap();

    // A scan at a timestamp above the lock must refuse rather than answer: the row may be
    // there, and only resolving the lock can say.
    let refused = call(
        &transport,
        TxnKvReq::Scan {
            start: key(b"a"),
            end: Bytes::new(),
            limit: 100,
            ts: 1_000,
            reverse: false,
        },
    )
    .await
    .expect_err("a scan that meets a lock refuses");
    match refused {
        ProtoError::Locked { lock_info } => {
            let lock = LockInfo::decode(&lock_info).expect("a lock the client can read");
            assert_eq!(lock.key, key(b"b"), "the key that is in the way");
            assert_eq!(lock.start_ts, 30);
        }
        other => panic!("expected a Locked refusal, got {other:?}"),
    }

    // A scan *below* the lock is unaffected: a lock above the snapshot is not in its way.
    let older = call(
        &transport,
        TxnKvReq::Scan {
            start: key(b"a"),
            end: Bytes::new(),
            limit: 100,
            ts: 25,
            reverse: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        older,
        TxnKvResp::Scan {
            pairs: vec![(key(b"a"), key(b"1"))]
        }
    );

    // Resolved forward — which is what a reader does once it finds the primary committed — and
    // the row is there.
    call(
        &transport,
        TxnKvReq::ResolveLock {
            start_ts: 30,
            commit_ts: 40,
            keys: vec![key(b"b")],
        },
    )
    .await
    .unwrap();
    let after = call(
        &transport,
        TxnKvReq::Scan {
            start: key(b"a"),
            end: Bytes::new(),
            limit: 100,
            ts: 1_000,
            reverse: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        after,
        TxnKvResp::Scan {
            pairs: vec![(key(b"a"), key(b"1")), (key(b"b"), key(b"pending"))],
        },
        "the row a committed transaction wrote is in the scan"
    );
}

/// The safepoint rises and never falls. A number that moved backwards would promise a reader
/// history that has already been collected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_safepoint_only_rises() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    assert_eq!(running.store.safepoint(), 0);
    let raised = call(&transport, TxnKvReq::GcSafepoint { safepoint: 100 })
        .await
        .unwrap();
    assert_eq!(raised, TxnKvResp::GcSafepoint { safepoint: 100 });
    assert_eq!(running.store.safepoint(), 100);

    let stale = call(&transport, TxnKvReq::GcSafepoint { safepoint: 50 })
        .await
        .unwrap();
    assert_eq!(
        stale,
        TxnKvResp::GcSafepoint { safepoint: 100 },
        "a stale message overtaking a fresh one is not a decision"
    );
    assert_eq!(running.store.safepoint(), 100);
}

/// Everything a transaction wrote survives a restart, because everything it wrote went through
/// the log before it was acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_transaction_survives_a_restart() {
    let dir = TempDir::new().unwrap();
    {
        let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
        let handle = Server::bind(
            "127.0.0.1:0",
            StoreService::new(Arc::clone(&store)),
            TransportConfig::new(),
        )
        .await
        .unwrap()
        .spawn()
        .unwrap();
        let transport = TcpTransport::connect(handle.local_addr()).await.unwrap();
        commit_one(&transport, b"kept", b"v", 10, 20).await;
        // A lock left mid-transaction has to come back too, or a resolver would find nothing to
        // resolve and a half-finished transaction would vanish.
        call(
            &transport,
            TxnKvReq::Prewrite {
                start_ts: 30,
                primary: key(b"pending"),
                ttl_ms: 3_000,
                mutations: vec![put(b"pending", b"half")],
            },
        )
        .await
        .unwrap();
        handle.shutdown().await.unwrap();
        store.flush().unwrap();
    }

    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let handle = Server::bind(
        "127.0.0.1:0",
        StoreService::new(Arc::clone(&store)),
        TransportConfig::new(),
    )
    .await
    .unwrap()
    .spawn()
    .unwrap();
    let transport = TcpTransport::connect(handle.local_addr()).await.unwrap();

    assert_eq!(get(&transport, b"kept", 1_000).await, Some(key(b"v")));
    let still_locked = call(
        &transport,
        TxnKvReq::Get {
            key: key(b"pending"),
            ts: 1_000,
        },
    )
    .await
    .unwrap_err();
    assert!(
        LockInfo::from_error(&still_locked).is_some(),
        "the lock survived the restart"
    );
}

// -- garbage collection --------------------------------------------------------------------

/// `prompts/05-txn.md`'s GC acceptance, at the number it asks for: **a thousand versions** of
/// one key, a safepoint, a compaction — and the visible read is unchanged while the SST holds
/// one version.
///
/// Two halves, and the second is why the count is taken rather than inferred: a read answers
/// with one value however many versions are behind it, so a collector that ran and a collector
/// that did not look identical through the front door. What is asserted is that the versions
/// left *on disk* are one — after a flush, so nothing is in a memtable, and after a compaction,
/// which is the only thing that applies the filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_safepoint_collects_every_version_but_the_visible_one() {
    // Enough that the collector's work is visible in a count, small enough that `just check`
    // stays seconds. The prompt's thousand is the run below, which is the same code.
    collect_after_versions(200).await;
}

/// The acceptance criterion at the number `prompts/05-txn.md` states: a thousand versions.
///
/// Behind `--ignored` because a thousand committed versions is a thousand `fsync`ed round
/// trips — half a minute of them — and the two hundred above prove the same property on every
/// change.
///
/// ```text
/// cargo test -p esker-store --release --test txnkv -- --ignored a_thousand_versions
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a thousand committed versions; tens of seconds"]
async fn a_thousand_versions_collect_to_one() {
    collect_after_versions(1_000).await;
}

/// Commits `versions` versions of one key, publishes a safepoint above all of them, compacts,
/// and checks both halves: the visible read is unchanged, and one version is left on disk.
async fn collect_after_versions(versions: u64) {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    for round in 1..=versions {
        let start_ts = round * 10;
        call(
            &transport,
            TxnKvReq::Prewrite {
                start_ts,
                primary: key(b"hot"),
                ttl_ms: 3_000,
                mutations: vec![TxnMutation::Put {
                    key: key(b"hot"),
                    value: Bytes::from(format!("v{round}")),
                    read_ts: None,
                }],
            },
        )
        .await
        .unwrap();
        call(
            &transport,
            TxnKvReq::Commit {
                start_ts,
                commit_ts: start_ts + 1,
                keys: vec![key(b"hot")],
            },
        )
        .await
        .unwrap();
    }

    let newest = Bytes::from(format!("v{versions}"));
    assert_eq!(
        get(&transport, b"hot", u64::MAX).await,
        Some(newest.clone())
    );

    // A safepoint above every version, so all but the newest are collectable. The collector's
    // window is the cluster default, and these timestamps have a physical half of zero — so
    // the default retention pushes nothing back and the published safepoint is the line.
    call(
        &transport,
        TxnKvReq::GcSafepoint {
            safepoint: versions * 10 + 5,
        },
    )
    .await
    .unwrap();
    running.store.flush().unwrap();
    running.store.compact_write_cf().unwrap();

    // The visible read is unchanged — which is the half that matters, and the half a collector
    // that dropped the newest version below the safepoint would break.
    assert_eq!(get(&transport, b"hot", u64::MAX).await, Some(newest));

    // And the versions really went: one survivor rather than every one of them.
    let left = running.store.write_records(b"hot").unwrap();
    assert_eq!(
        left, 1,
        "the newest version below the safepoint survives and the rest are collected"
    );

    // On *disk*, and not merely in the answer: the `write` memtable is empty after the flush,
    // so the one record the count above found is in an SST and nowhere else. Without this the
    // test would pass on a collector that never ran, as long as reads went to a memtable that
    // happened to hold the newest version.
    let in_memory = running
        .store
        .property("esker.mem-table-size.write")
        .and_then(|size| size.parse::<u64>().ok())
        .expect("the write column family reports its memtable size");
    assert_eq!(
        in_memory, 0,
        "the flush left nothing in memory, so the surviving version is the one in the SST"
    );
    let files: u64 = (0..7)
        .filter_map(|level| {
            running
                .store
                .property(&format!("esker.num-files-at-level{level}.write"))
        })
        .filter_map(|count| count.parse::<u64>().ok())
        .sum();
    assert!(
        files >= 1,
        "every committed version was flushed, so at least one SST is behind the answer"
    );
}

/// A reader below the safepoint still sees what it is entitled to, because the safepoint is a
/// floor PD sets from the oldest active read — the collector never collects above it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_above_the_safepoint_is_not_collected() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"k", b"old", 10, 20).await;
    commit_one(&transport, b"k", b"new", 30, 40).await;

    // Below both commits: nothing is collectable.
    call(&transport, TxnKvReq::GcSafepoint { safepoint: 15 })
        .await
        .unwrap();
    running.store.flush().unwrap();
    running.store.compact_write_cf().unwrap();

    assert_eq!(
        running.store.write_records(b"k").unwrap(),
        2,
        "a safepoint below a version does not collect it"
    );
    assert_eq!(get(&transport, b"k", 20).await, Some(key(b"old")));
    assert_eq!(get(&transport, b"k", 40).await, Some(key(b"new")));
}

/// A table with a longer retention keeps more, which is the whole of the time-machine hook
/// ([ADR 0021](../../docs/adr/0021-time-machine.md)): the window a reader may travel back
/// through *is* the distance the collector leaves alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_longer_retention_keeps_more_of_one_table() {
    use esker_store::gc::RetentionPolicy;

    let running = start().await;
    // One table keeps an hour where the cluster keeps ten minutes.
    running
        .store
        .collector()
        .set_policy(RetentionPolicy::uniform(10 * 60 * 1_000).with_table(1, 7, 60 * 60 * 1_000));

    let published = (2 * 60 * 60 * 1_000u64) << esker_store::TSO_LOGICAL_BITS;
    let policy = running.store.collector().policy();

    let mut kept = esker_keys::prefix::table_row_prefix(1, 7);
    kept.extend_from_slice(b"row");
    let mut ordinary = esker_keys::prefix::table_row_prefix(1, 9);
    ordinary.extend_from_slice(b"row");

    let kept_at = policy
        .effective_safepoint(&esker_txn::key::write(&kept, 1), published)
        .unwrap();
    let ordinary_at = policy
        .effective_safepoint(&esker_txn::key::write(&ordinary, 1), published)
        .unwrap();
    assert!(
        kept_at < ordinary_at,
        "the table with the longer window has its safepoint pushed further back"
    );
    assert_eq!(
        ordinary_at, published,
        "a table with no override takes the cluster default, not retention zero"
    );
}

/// **`LatestCommit` answers with the newest *commit*, not with whatever record is on top**
/// ([ADR 0078](../../docs/adr/0078-a-marker-is-not-a-commit.md)).
///
/// A rollback marker lives at `commit_ts == start_ts`, so a transaction that takes a key and dies
/// leaves the newest record on it. This answer used to be computed by filtering that record out of
/// the `Option` afterwards, which turned "the newest commit is 20" into "nothing ever committed
/// this key" — and `changed_since_statement`, whose whole job is to notice the writer in front,
/// therefore saw nothing, did not re-run the statement, and left it to be refused at prewrite with
/// a `40001` nobody needed (`esker-sql`'s `store_locking` suite counts exactly those).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_newest_commit_survives_a_marker_written_above_it() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    commit_one(&transport, b"k", b"v", 10, 20).await;
    // A second transaction took the key and died. Its marker is now the newest record on `k`.
    let rolled = call(
        &transport,
        TxnKvReq::Rollback {
            start_ts: 30,
            keys: vec![key(b"k")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rolled,
        TxnKvResp::Rollback {
            status: TxnStatus::Ok
        }
    );

    let newest = call(&transport, TxnKvReq::LatestCommit { key: key(b"k") })
        .await
        .unwrap();
    assert_eq!(
        newest,
        TxnKvResp::LatestCommit { newest: Some(20) },
        "the commit at 20 is still the newest commit; the marker at 30 committed nothing and must \
         not erase it"
    );
}

/// **A marker inside a validated range is not a phantom** (ADR 0078).
///
/// The read-set range check (ADR 0067 §3) refuses a prewrite when anything committed inside the
/// range after the transaction's snapshot. A transaction that took a key in that range and then
/// rolled back committed nothing and changed nothing a reader could have seen, so refusing over
/// its marker is a `40001` for a phantom that does not exist. This is the range-shaped face of the
/// same rule the key-level check keeps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_marker_inside_a_checked_range_is_not_a_phantom() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    // Above the checking transaction's snapshot of 40, which is what makes it a candidate phantom.
    let rolled = call(
        &transport,
        TxnKvReq::Rollback {
            start_ts: 50,
            keys: vec![key(b"m")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rolled,
        TxnKvResp::Rollback {
            status: TxnStatus::Ok
        }
    );

    let checked = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::CheckRange {
                start: key(b"a"),
                end: key(b"z"),
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        checked,
        TxnKvResp::prewrite_ok(1),
        "nothing committed in the range: the only record above the snapshot is a rollback marker"
    );
}

/// **A lock inside a validated range refuses the check**
/// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §1).
///
/// The range check reads the `write` CF, and a transaction that has prewritten into the range but
/// not yet committed is in the `lock` CF. Two transactions that both scan a range and both insert
/// into it write *different* keys, so nothing collides at prewrite — and with only the write scan
/// neither sees the other and both commit, which is the write skew run 112a caught
/// (`transaction_test.rb`'s `raises SerializationFailure when a serialization failure occurs`
/// expected `40001` and nothing was raised).
///
/// `Locked` and not `Conflict`, because the holder may still roll back: what the client owes the
/// range is the same thing it owes a locked key — settle the holder, then ask again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_inside_a_checked_range_refuses_the_check() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    // The other transaction prewrites into the range and stops there: a lock, no commit.
    let locked = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 50,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"m"),
                value: Bytes::from_static(b"in flight"),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(locked, TxnKvResp::prewrite_ok(1));

    let checked = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"a"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::CheckRange {
                start: key(b"a"),
                end: key(b"z"),
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        checked,
        TxnKvResp::Prewrite {
            keys: vec![TxnStatus::Locked(LockInfo {
                key: key(b"m"),
                primary: key(b"m"),
                start_ts: 50,
                ttl_ms: 3_000,
            })]
        },
        "the lock is named by the key it sits on, which is not either bound of the range"
    );
}

/// **This transaction's own lock inside its own read range is not a phantom.**
///
/// The guard on the test above, and not a hypothetical: the client prewrites its keys **before**
/// it sends its range checks (`Transaction::commit` step 3), so a transaction that inserts into a
/// range it scanned always has a lock of its own sitting in that range by the time the check runs.
/// A scan that did not exclude it would refuse every such transaction — which is every transaction
/// that writes where it read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn our_own_lock_inside_a_checked_range_is_not_a_phantom() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    // Our own insert into the range we read, prewritten first, exactly as `commit` orders it.
    let ours = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"m"),
                value: Bytes::from_static(b"ours"),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(ours, TxnKvResp::prewrite_ok(1));

    let checked = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::CheckRange {
                start: key(b"a"),
                end: key(b"z"),
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        checked,
        TxnKvResp::prewrite_ok(1),
        "the only lock in the range is this transaction's own"
    );
}

/// **A release takes this transaction's lock and leaves nothing behind**
/// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2),
/// through the log and the apply loop like every other write.
///
/// The three assertions are the three ways it differs from a `Rollback`: the lock is gone, **no
/// write record was left**, and the same transaction can take the key again. A savepoint's
/// deadlock victim does exactly that — it gives its rows back at the `40P01` and writes one of
/// them the moment its `rescue` is over — and a marker would have made that impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_gives_the_key_back_without_a_marker() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let locked = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"m"),
                value: Bytes::from_static(b"inside the savepoint"),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(locked, TxnKvResp::prewrite_ok(1));

    let released = call(
        &transport,
        TxnKvReq::ReleaseLock {
            start_ts: 40,
            keys: vec![key(b"m")],
        },
    )
    .await
    .unwrap();
    assert_eq!(released, TxnKvResp::ReleaseLock { released: 1 });

    // Nothing was written down. A rollback would have left a marker at `commit_ts == start_ts`.
    let newest = call(&transport, TxnKvReq::LatestCommit { key: key(b"m") })
        .await
        .unwrap();
    assert_eq!(
        newest,
        TxnKvResp::LatestCommit { newest: None },
        "a release is not a rollback: it records nothing"
    );

    // And the key is free — to this transaction as much as to anybody.
    let again = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 40,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"m"),
                value: Bytes::from_static(b"after the rescue"),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        TxnKvResp::prewrite_ok(1),
        "the transaction that released it may take it again"
    );
}

/// A release names its owner: somebody else's lock is not ours to give away, and the count says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_leaves_another_transactions_lock_alone() {
    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    let theirs = call(
        &transport,
        TxnKvReq::Prewrite {
            start_ts: 50,
            primary: key(b"m"),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key(b"m"),
                value: Bytes::from_static(b"theirs"),
                read_ts: None,
            }],
        },
    )
    .await
    .unwrap();
    assert_eq!(theirs, TxnKvResp::prewrite_ok(1));

    let released = call(
        &transport,
        TxnKvReq::ReleaseLock {
            start_ts: 40,
            keys: vec![key(b"m")],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        released,
        TxnKvResp::ReleaseLock { released: 0 },
        "not ours, so not counted and not taken"
    );

    // Still held, by its owner.
    let met = call(
        &transport,
        TxnKvReq::Get {
            key: key(b"m"),
            ts: 60,
        },
    )
    .await;
    match met.expect_err("the owner still holds it") {
        ProtoError::Locked { lock_info } => {
            let lock = LockInfo::decode(&lock_info).expect("a lock the client can read");
            assert_eq!(lock.start_ts, 50, "still its owner's");
        }
        other => panic!("expected a Locked refusal, got {other:?}"),
    }
}
