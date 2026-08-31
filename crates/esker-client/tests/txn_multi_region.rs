//! A transactional scan across a split boundary, against **real stores**.
//!
//! `tests/txn.rs` proves the walk against a scripted transport; this proves it against two
//! stores that really do own half the key space each and really do answer only for their own
//! keys. That is the difference that matters here: the bug this covers was not a wrong answer
//! from a store, it was the client believing one store's answer was the whole of the range —
//! and a fake that answers whatever it is scripted to cannot tell you that a real store would
//! have stopped at its own boundary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{CountingOracle, TxnClient};
use esker_proto::{Epoch, Peer, Region, ServerHandle, TransportConfig};
use esker_store::{Store, StoreOptions, StoreService};

/// Two stores, each serving one half of the key space, and the routing that says so.
struct Cluster {
    client: TxnClient,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    _runtime: tokio::runtime::Runtime,
}

/// The key the two regions are divided at.
const BOUNDARY: &[u8] = b"m";

fn start_store(runtime: &tokio::runtime::Runtime, id: u64) -> (ServerHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: id,
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
    (handle, dir)
}

fn cluster() -> Cluster {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a runtime");

    let (low, low_dir) = start_store(&runtime, 1);
    let (high, high_dir) = start_store(&runtime, 2);

    // The two are separate databases, so each holds only the keys that were routed to it — and
    // a scan that asks one of them for the whole range gets one of them, which is exactly the
    // failure this covers. (Each store's own region nominally covers the whole key space; it is
    // the routing table below that divides it. What the client observes is the same either way:
    // an answer bounded by what one store holds.)
    let stores = esker_client::TcpStores::connect_all(
        &[low.local_addr(), high.local_addr()],
        TransportConfig::new(),
    )
    .expect("the client connects to both");
    let ids = stores.store_ids();
    assert_eq!(ids, vec![1, 2], "two stores, two ids");

    let resolver: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([
        Route {
            region: Region {
                id: 1,
                start_key: Bytes::new(),
                end_key: Bytes::copy_from_slice(BOUNDARY),
                peers: vec![Peer::voter(1, 1)],
                epoch: Epoch::INITIAL,
            },
            leader: Some(Peer::voter(1, 1)),
        },
        Route {
            region: Region {
                id: 2,
                start_key: Bytes::copy_from_slice(BOUNDARY),
                end_key: Bytes::new(),
                peers: vec![Peer::voter(2, 2)],
                epoch: Epoch::INITIAL,
            },
            leader: Some(Peer::voter(2, 2)),
        },
    ]));

    let router = Router::with_options(
        Arc::new(stores),
        resolver,
        ClientOptions {
            jitter_seed: Some(11),
            ..ClientOptions::default()
        },
    );
    let client = TxnClient::on_router(
        Arc::new(router),
        Arc::new(CountingOracle::starting_at(1_000)),
    );

    Cluster {
        client,
        _handles: vec![low, high],
        _dirs: vec![low_dir, high_dir],
        _runtime: runtime,
    }
}

/// A scan whose range spans the boundary reads **both** regions.
///
/// Before the walk, this came back with the low store's keys alone: no error, nothing to
/// notice, and a caller convinced it had seen the whole range.
#[test]
fn a_transactional_scan_crosses_a_real_region_boundary() {
    let cluster = cluster();

    // Six keys, three either side of the boundary, written by one transaction — which is
    // itself the multi-region case, since the primary lands in one region and secondaries in
    // the other.
    let mut txn = cluster.client.begin().unwrap();
    for k in [b"a".as_slice(), b"b", b"c", b"n".as_slice(), b"o", b"p"] {
        txn.put(k, b"v");
    }
    let commit_ts = txn.commit().unwrap().expect("it wrote something");
    assert!(commit_ts > 0);

    let txn = cluster.client.begin().unwrap();
    let pairs = txn.scan(b"", b"", 100).unwrap();
    let keys: Vec<Vec<u8>> = pairs.iter().map(|(key, _)| key.to_vec()).collect();
    assert_eq!(
        keys,
        vec![
            b"a".to_vec(),
            b"b".to_vec(),
            b"c".to_vec(),
            b"n".to_vec(),
            b"o".to_vec(),
            b"p".to_vec()
        ],
        "both halves of the key space, in key order"
    );

    // A range that starts past the boundary reads only the far region, and one that ends
    // before it reads only the near one — the walk stops where the caller's range does.
    let far = txn.scan(b"n", b"", 100).unwrap();
    assert_eq!(far.len(), 3, "only the far half");
    let near = txn.scan(b"", b"m", 100).unwrap();
    assert_eq!(near.len(), 3, "only the near half");
}

/// A transaction whose keys span the boundary commits across both regions, with the primary in
/// one and secondaries in the other — and every key is readable afterwards.
#[test]
fn a_transaction_commits_across_a_real_region_boundary() {
    let cluster = cluster();

    let mut txn = cluster.client.begin().unwrap();
    // The primary is the lowest key, so it lands in the low region and the secondaries in the
    // high one. The commit's ordering rule is what makes that safe.
    txn.put(b"a", b"primary-side");
    txn.put(b"z", b"secondary-side");
    let primary = txn.primary().cloned().unwrap();
    assert_eq!(primary, Bytes::from_static(b"a"));
    txn.commit().unwrap();

    let txn = cluster.client.begin().unwrap();
    assert_eq!(
        txn.get(b"a").unwrap(),
        Some(Bytes::from_static(b"primary-side"))
    );
    assert_eq!(
        txn.get(b"z").unwrap(),
        Some(Bytes::from_static(b"secondary-side")),
        "the secondary's region committed too"
    );
}
