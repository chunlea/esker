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

/// `prompts/05-txn.md`'s GC acceptance: many versions of one key, a safepoint, a compaction —
/// and the visible read is unchanged while the versions are gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_safepoint_collects_every_version_but_the_visible_one() {
    // Enough that the collector's work is visible in a count, small enough that the test stays
    // a second rather than a minute.
    const VERSIONS: u64 = 200;

    let running = start().await;
    let transport = TcpTransport::connect(running.handle.local_addr())
        .await
        .unwrap();

    for round in 1..=VERSIONS {
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

    let newest = Bytes::from(format!("v{VERSIONS}"));
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
            safepoint: VERSIONS * 10 + 5,
        },
    )
    .await
    .unwrap();
    running.store.flush().unwrap();
    running.store.compact_write_cf().unwrap();

    // The visible read is unchanged — which is the half that matters, and the half a collector
    // that dropped the newest version below the safepoint would break.
    assert_eq!(get(&transport, b"hot", u64::MAX).await, Some(newest));

    // And the versions really went: one survivor rather than two hundred.
    let left = running.store.write_records(b"hot").unwrap();
    assert_eq!(
        left, 1,
        "the newest version below the safepoint survives and the rest are collected"
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
