//! What a store does with a region's **data** once it has been removed from the region.
//!
//! `RemovePeer` is not rare: every rebalance is one, and until this file existed the keys of every
//! region a store had ever shed stayed on its disk for the life of the process — in all three
//! column families, under no region, served by nothing. `retire_region` said so in as many words
//! and blamed the engine for having no range tombstones, which stopped being true in phase 5
//! ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)).
//!
//! The two assertions here are the two halves of the same rule, and only one of them is about
//! disk space. A store must reclaim the range it no longer owns, **and** a store that still owns
//! it must be untouched: a reclamation that ran on the wrong store, or against a stale record of
//! a range that has since narrowed, deletes acknowledged writes (invariant 5).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::{
    Operator, PeerRole, RawKvReq, Region, RequestHeader, Server, ServerHandle, Service,
    TransportConfig, TxnKvReq, TxnMutation,
};
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

/// A store on a socket, with everything needed to keep it alive.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = within("the server to shut down", self.handle.shutdown()).await;
    }
}

/// The deadline every store-reaching await gets, so a wedge becomes a failure rather than a hang.
const AWAIT_DEADLINE: Duration = Duration::from_secs(60);

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(AWAIT_DEADLINE, future)
        .await
        .unwrap_or_else(|_: tokio::time::error::Elapsed| {
            panic!(
                "timed out after {AWAIT_DEADLINE:?} waiting for {what}. This is a wait with no \
                 end rather than a slow one: most likely a proposal whose index never applied, \
                 which the store has no timeout of its own for."
            )
        })
}

fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// 25 ms and not 5: `esker-raft` counts ticks, so the election timeout is 10-20 of these, and a
/// 50 ms election timeout is a bet that a saturated box will schedule this thread within 50 ms
/// (`docs/plans/debt-c1.md` section 3).
fn raft_options(peers: Vec<PeerAddress>, bootstrap_voters: Option<Vec<u64>>) -> RaftOptions {
    let mut raft = RaftOptions::new(peers, 20_260_901);
    raft.tick = Duration::from_millis(25);
    raft.compaction = LogCompaction::new();
    raft.bootstrap_voters = bootstrap_voters;
    raft
}

async fn open(
    address: std::net::SocketAddr,
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
    bootstrap_region: u64,
) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: bootstrap_region,
            raft: Some(raft),
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                // Nothing here is about splitting, and a region that split under the test would
                // move the very range it is asserting about.
                region_split_size: u64::MAX,
                max_sampled_keys: 1024,
            },
            ..StoreOptions::new()
        },
    )
    .unwrap();

    let server = Server::bind(
        address,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap();
    let handle = server.spawn().unwrap();
    Node {
        store,
        handle,
        _dir: dir,
    }
}

async fn wait_for<F: FnMut() -> bool>(what: &str, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:05}"))
}

/// Writes one key, retrying while the answer is one the caller is told to retry. Every write in
/// this file is an idempotent put of one fixed value from one writer, so repeating an ambiguous
/// answer cannot be observed — the argument `tests/snapshot.rs` makes, and no wider.
async fn put(store: &Arc<Store>, region: &Region, key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        let request = RawKvReq::put(key.clone(), Bytes::copy_from_slice(value));
        match within("a put to be applied", store.serve(header, request)).await {
            Ok(_) => return,
            Err(error) => {
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "writing {key:?}: {error}");
                assert!(
                    Instant::now() < deadline,
                    "writing {key:?} never succeeded; last answer {error}"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// One transactional request through the replicated path, retried on the same terms as [`put`].
async fn txn(store: &Arc<Store>, region: &Region, request: TxnKvReq) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        match within(
            "a transactional write to be applied",
            store.serve_txn(header, request.clone()),
        )
        .await
        {
            Ok(_) => return,
            Err(error) => {
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "{request:?}: {error}");
                assert!(
                    Instant::now() < deadline,
                    "{request:?} never succeeded; last answer {error}"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// Commits one key through Percolator, as a client would: prewrite, then commit. Leaves a `write`
/// record and a `default` value behind, and no lock.
async fn commit_one(store: &Arc<Store>, region: &Region, k: Bytes, start_ts: u64, commit_ts: u64) {
    txn(
        store,
        region,
        TxnKvReq::Prewrite {
            start_ts,
            primary: k.clone(),
            ttl_ms: 60_000,
            mutations: vec![TxnMutation::Put {
                key: k.clone(),
                value: Bytes::from_static(b"committed"),
            }],
        },
    )
    .await;
    txn(
        store,
        region,
        TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![k],
        },
    )
    .await;
}

/// Prewrites without committing, so the `lock` column family holds a record at retirement.
///
/// **The third family has to be non-empty or the test cannot see the bug it is about.** A
/// committed key leaves `write` and `default` and no lock, so a reclamation that missed `lock`
/// entirely would pass a test built only from commits — which is the shape of the version-1
/// snapshot miss this file's header names.
async fn prewrite_only(store: &Arc<Store>, region: &Region, k: Bytes, start_ts: u64) {
    txn(
        store,
        region,
        TxnKvReq::Prewrite {
            start_ts,
            primary: k.clone(),
            ttl_ms: 600_000,
            mutations: vec![TxnMutation::Put {
                key: k,
                value: Bytes::from_static(b"uncommitted"),
            }],
        },
    )
    .await;
}

/// Every shipped column family's key count inside a region's range, on one store.
fn counts(store: &Arc<Store>, region: &Region) -> Vec<(&'static str, usize)> {
    esker_store::snapshot::key_counts(store.db(), region)
        .unwrap()
        .to_vec()
}

/// Puts a voting replica of region 1 on `second`, and waits until every column family of it has
/// landed there. Answers the region as the second store holds it.
///
/// **The waits are the point.** Without them "the range is empty afterwards" is satisfied by a
/// transfer that never happened, which is how a test of a deletion passes while deleting nothing.
async fn place_on(pd: &Arc<FakePd>, first: &Node, second: &Node) -> Region {
    assert!(
        second.store.regions().is_empty(),
        "the second store bootstrapped a region of its own"
    );
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch: first.store.regions().regions()[0].epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;
    wait_for("the second store's peer to become a voter", || {
        first.store.regions().get(1).is_some_and(|state| {
            state
                .region()
                .peers
                .iter()
                .any(|peer| peer.peer_id == 2 && peer.role == PeerRole::Voter)
        })
    })
    .await;

    let arrived = second.store.regions().get(1).unwrap().region().clone();
    wait_for("every column family to arrive on the second store", || {
        counts(&second.store, &arrived)
            .iter()
            .all(|(_, held)| *held > 0)
    })
    .await;
    arrived
}

/// A rebalance sheds a replica, and the shed store's copy of the range **goes away** — every
/// column family of it — while the store that still hosts the region keeps every key.
///
/// # Why the write side is built the way it is
///
/// Four committed rows, one uncommitted prewrite and one `RawKV` put, so `default`, `write` and
/// `lock` all hold keys in the range at the moment the peer is removed. Counting them before the
/// removal is not decoration: it is what makes the "empty afterwards" assertion mean *emptied*
/// rather than *never filled*, which a two-store test gets wrong for free if the transfer to the
/// second store has not finished.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_peer_reclaims_the_range_in_every_column_family() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address = reserve();
    let second_address = reserve();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address,
        1,
        &pd,
        raft_options(peers.clone(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let region = first.store.regions().regions()[0].clone();
    for n in 0..4 {
        commit_one(
            &first.store,
            &region,
            key(n),
            10 + u64::from(n) * 2,
            11 + u64::from(n) * 2,
        )
        .await;
    }
    prewrite_only(&first.store, &region, key(50), 100).await;
    put(&first.store, &region, key(200), b"raw").await;

    let second = open(
        second_address,
        2,
        &pd,
        raft_options(peers.clone(), Some(vec![2])),
        2,
    )
    .await;
    let arrived = place_on(&pd, &first, &second).await;
    let before_on_leader = counts(&first.store, &region);
    assert!(
        before_on_leader.iter().all(|(_, held)| *held > 0),
        "the leader does not hold all three column families: {before_on_leader:?}"
    );

    pd.issue(Operator::RemovePeer {
        region_id: 1,
        epoch: first.store.regions().regions()[0].epoch,
        peer_id: 2,
    });
    wait_for("the second store to stop hosting the region", || {
        second.store.regions().get(1).is_none()
    })
    .await;

    // The reclamation itself. Spawned behind the conf change, so it is waited for rather than
    // asserted the instant the region leaves the map.
    wait_for("the shed range to be reclaimed", || {
        counts(&second.store, &arrived)
            .iter()
            .all(|(_, held)| *held == 0)
    })
    .await;
    assert_eq!(
        counts(&second.store, &arrived),
        vec![
            (esker_engine::cf::DEFAULT, 0),
            (esker_engine::cf::LOCK, 0),
            (esker_engine::cf::WRITE, 0)
        ],
        "a store removed from a region kept keys of it"
    );

    // And the store that still hosts it lost nothing. This is the half that would turn the fix
    // into data loss if it were wrong, so it is asserted per family and by value.
    assert_eq!(
        counts(&first.store, &region),
        before_on_leader,
        "reclaiming a shed replica's range changed the range on a store that still owns it"
    );
    assert_eq!(
        first
            .store
            .handle(
                RequestHeader::new(region.id, first.store.regions().regions()[0].epoch, 0),
                RawKvReq::get(key(200))
            )
            .unwrap(),
        esker_proto::RawKvResp::Get {
            value: Some(Bytes::from_static(b"raw"))
        },
        "the owner's own key stopped reading after a neighbour's retirement"
    );

    first.stop().await;
    second.stop().await;
}
