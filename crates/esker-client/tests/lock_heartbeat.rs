//! **A transaction that holds a lock keeps it, however long it holds it**
//! ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
//!
//! `TxnKv::Heartbeat` has been on the wire since phase 5, with a handler, a replicated
//! `TxnCommand::Heartbeat` and a store-side `txnkv::heartbeat` behind it — and **nobody sending
//! one**. `docs/DESIGN.md` §8 says so in its own words: *"The TTL is not extended in practice:
//! `TxnKv::Heartbeat` is an RPC with a handler and no sender, so a lock outlives a dead holder by
//! at most one TTL."*
//!
//! That was a cost measured in cleanup latency while the only locks were a commit's, which live
//! for the length of a two-phase commit and no longer. ADR 0088 changed what it costs: a
//! `SELECT … FOR UPDATE` lock is taken when the statement runs and released when the transaction
//! ends, and a client sitting between two statements for four seconds is an ordinary client. Under
//! a three-second lease its lock is resolved out from under it and it cannot commit — the row it
//! was promised goes to somebody else, and it finds out at the end.
//!
//! **The lease is not the thing to lengthen.** A long lease is how long a *dead* holder blocks
//! everybody, and the whole point of a short one is that a crashed client is cleaned up quickly.
//! What a live holder needs is to say so, which is what the heartbeat is for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{Acquired, TcpStores, TimestampOracle, TxnClient};
use esker_proto::{Epoch, Peer, ProtoError, Region, ServerHandle, TransportConfig};
use esker_store::{Store, StoreOptions, StoreService};

/// Short enough that a test can outlive it, long enough that a scheduling hiccup is not a lease.
const TTL_MS: u64 = 400;

/// The row the whole test is about.
const KEY: &[u8] = b"held";

/// **A timestamp with a physical part, which `CountingOracle` has not.**
///
/// Every other test in this crate counts from a thousand, and that is right for them: what they
/// need from a timestamp is an order. This one needs an *age* — `is_expired` reads the physical
/// milliseconds out of `start_ts` and compares them to now — and a counter's physical part is zero
/// for the first quarter of a million ticks, so under it no lock ever expires and a lease can
/// neither run out nor be renewed. It is the same reason `esker-pd` composes the two halves.
#[derive(Debug)]
struct WallClock {
    last: AtomicU64,
}

impl TimestampOracle for WallClock {
    fn tso(&self, _count: u32) -> Result<u64, ProtoError> {
        let ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_millis(),
        )
        .expect("milliseconds since 1970 fit in 64 bits until the year 584 million");
        // Strictly increasing, because two transactions sharing a `start_ts` are one transaction
        // as far as every lock record is concerned.
        let composed = esker_pd::compose_ts(ms, 0);
        let mut previous = self.last.load(Ordering::SeqCst);
        loop {
            let next = composed.max(previous + 1);
            match self
                .last
                .compare_exchange(previous, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(next),
                Err(now) => previous = now,
            }
        }
    }
}

struct One {
    client: Arc<TxnClient>,
    _handle: ServerHandle,
    _dir: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
}

fn one_store() -> One {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            ..StoreOptions::new()
        },
    )
    .expect("the store opens");
    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            StoreService::new(store),
            TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });
    let routes: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([Route {
        region: Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::INITIAL,
        },
        leader: Some(Peer::voter(1, 1)),
    }]));
    let stores = TcpStores::connect_all(&[handle.local_addr()], TransportConfig::new())
        .expect("a client connects");
    let oracle: Arc<dyn TimestampOracle> = Arc::new(WallClock {
        last: AtomicU64::new(0),
    });
    let client = TxnClient::on_router(
        Arc::new(Router::with_options(
            Arc::new(stores),
            routes,
            ClientOptions {
                jitter_seed: Some(7),
                ..ClientOptions::default()
            },
        )),
        oracle,
    )
    .with_lock_ttl_ms(TTL_MS);
    One {
        client: Arc::new(client),
        _handle: handle,
        _dir: dir,
        _runtime: runtime,
    }
}

/// **The red one: a transaction holding a lock across three leases must still own it.**
///
/// Nothing here is asleep by accident — the sleep *is* the test. What it asserts is not that the
/// second transaction is refused (it is, and that is the wound-wait rule doing its own job) but
/// that the **holder can still commit**: without a heartbeat its lock is expired, the second
/// transaction settles its primary to take the key, and the commit that follows is refused for a
/// transaction that never did anything wrong.
#[test]
fn a_lock_held_past_its_lease_is_still_its_holders() {
    let cluster = one_store();
    let mut holder = cluster.client.begin().unwrap();
    assert_eq!(holder.lock(KEY).unwrap(), Acquired::Taken);

    // Three leases. One would do; three says it is not a boundary case.
    std::thread::sleep(Duration::from_millis(TTL_MS * 3));

    // Somebody else wants the row, and asks the way every waiter asks.
    let mut other = cluster.client.begin().unwrap();
    let met = other.lock(KEY).unwrap();
    assert!(
        matches!(met, Acquired::Held { by, .. } if by == holder.start_ts()),
        "the holder's lock was gone after {} ms, so the second transaction took the row: {met:?}",
        TTL_MS * 3
    );
    other.rollback().unwrap();

    // And the holder goes on to do what it took the lock for.
    holder.put(KEY, b"mine");
    holder
        .commit()
        .expect("a live transaction lost its own lock to the lease");

    let reader = cluster.client.begin().unwrap();
    assert_eq!(
        reader.get(KEY).unwrap().as_deref(),
        Some(&b"mine"[..]),
        "the holder's write is not what the row says"
    );
}

/// **A transaction that ends stops being heartbeated**, which is the half that keeps the first one
/// honest: a renewal that outlived its transaction would make every lock immortal and every
/// crashed client a permanent one, which is worse than the gap it closes.
#[test]
fn a_lock_stops_being_renewed_when_its_transaction_ends() {
    let cluster = one_store();
    let mut gone = cluster.client.begin().unwrap();
    let start_ts = gone.start_ts();
    gone.lock(KEY).unwrap();
    gone.rollback().unwrap();

    // Past every lease it could have been given, with nobody to renew it.
    std::thread::sleep(Duration::from_millis(TTL_MS * 3));

    let mut next = cluster.client.begin().unwrap();
    assert_eq!(
        next.lock(KEY).unwrap(),
        Acquired::Taken,
        "the lock of the transaction at {start_ts} is still being renewed after it ended"
    );
    next.rollback().unwrap();
}
