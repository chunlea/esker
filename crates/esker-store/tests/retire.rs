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
//!
//! A region's data is not only its keys. The columnar copy is a tree of immutable run files
//! written *beside* the engine, one directory per region id, so nothing the engine reclaims can
//! reach it and it has to be reclaimed by name — which is why `FileSystem` grew a tree removal
//! and why both halves of the rule are asserted about it too.
//!
//! # And the same two halves across a crash
//!
//! Retiring a region is **two** durable steps — the batch that destroys its Raft state and its
//! `'m'` record, and the clear of its range — so a crash lands between them, and until
//! [ADR 0056](../../../docs/adr/0056-a-retirement-is-announced-before-the-record-that-names-it-goes.md)
//! that was permanent: the `'m'` record is the only thing on disk that says which *range* the
//! region was, and destroying it first left the keys with nothing that could name them again. The
//! last three tests here are the same rule at a restart — reclaim what is orphaned, and never
//! touch what is owned.

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
    dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = within("the server to shut down", self.handle.shutdown()).await;
    }

    /// Stops everything holding the database and hands back the directory, so the same store can
    /// be opened again — which is all a restart is.
    ///
    /// The server goes too, and is awaited: it holds an `Arc<Store>` of its own through
    /// `StoreService`, and an engine whose `Db` is still alive cannot be reopened.
    async fn crash(self) -> tempfile::TempDir {
        self.store.stop();
        let _ = within("the server to shut down", self.handle.shutdown()).await;
        self.dir
    }

    /// Where a region's columnar copy lives on this store: one directory per region id, written
    /// **beside** the engine rather than inside it (`esker_store::columnar::runs`).
    fn columnar_dir(&self, region_id: u64) -> std::path::PathBuf {
        self.dir.path().join("columnar").join(region_id.to_string())
    }
}

/// Puts a file where a region's columnar runs would be, so a reclamation has something to reclaim.
///
/// Written by hand rather than by driving a columnar learner, because what is under test is that
/// the **directory** goes with the region: a manifest and a run file are what a real copy leaves,
/// and this asserts about the tree rather than about their contents.
fn plant_columnar_copy(node: &Node, region_id: u64) -> std::path::PathBuf {
    let dir = node.columnar_dir(region_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("RUNS"), b"a manifest").unwrap();
    std::fs::write(dir.join("000001.run"), b"a run").unwrap();
    dir
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

/// A port, **held** until the server that will serve on it adopts the socket.
///
/// Returning the address and dropping the listener leaves the port belonging to nobody until the
/// rebind, and under a parallel suite run something else takes it — `Address already in use`.
/// `Server::from_listener` takes the socket itself, so there is no window.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
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

#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn open(
    address_listener: std::net::TcpListener,
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
    bootstrap_region: u64,
) -> Node {
    let address = address_listener.local_addr().unwrap();
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

    let server = Server::from_listener(
        address_listener,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .unwrap();
    let handle = server.spawn().unwrap();
    Node { store, handle, dir }
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

/// Four committed rows, one uncommitted prewrite and one `RawKV` put, so `default`, `write` and
/// `lock` all hold keys in the region's range.
///
/// The prewrite is what makes the third family non-empty: a committed key leaves `write` and
/// `default` and no lock, so a reclamation that missed `lock` entirely would pass a test built only
/// from commits — which is the shape of the version-1 snapshot miss this file's header names.
async fn seed_all_three_families(store: &Arc<Store>, region: &Region) {
    for n in 0..4 {
        commit_one(
            store,
            region,
            key(n),
            10 + u64::from(n) * 2,
            11 + u64::from(n) * 2,
        )
        .await;
    }
    prewrite_only(store, region, key(50), 100).await;
    put(store, region, key(200), b"raw").await;
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
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address_listener,
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
    seed_all_three_families(&first.store, &region).await;

    let second = open(
        second_address_listener,
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

    // A columnar copy of the region being shed, and one of a region that is not — the second is
    // what says the removal is per region id rather than a sweep of `<data_dir>/columnar`.
    let shed_copy = plant_columnar_copy(&second, 1);
    let other_copy = plant_columnar_copy(&second, 7);

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

    // The columnar copy goes with the region, on the same terms and after the same gates: it is a
    // tree of immutable run files under no manifest but its own, so nothing the engine reclaims
    // would ever touch it.
    wait_for("the shed region's columnar copy to be removed", || {
        !shed_copy.exists()
    })
    .await;
    assert!(
        other_copy.exists(),
        "reclaiming one region's columnar copy took another region's with it"
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

/// Opens a store on a directory that already holds a database, as a restart does.
///
/// No server: what is under test happens inside `Store::open`, before anything is served, and a
/// socket would only add a way for the test to fail for another reason.
fn reopen(
    dir: &tempfile::TempDir,
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
) -> Arc<Store> {
    Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: store_id,
            raft: Some(raft),
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: format!("127.0.0.1:{}", 40_000 + store_id),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                region_split_size: u64::MAX,
                max_sampled_keys: 1024,
            },
            ..StoreOptions::new()
        },
    )
    .unwrap()
}

/// Whether a region's retirement is still announced on disk.
fn retirement_announced(store: &Arc<Store>, region_id: u64) -> bool {
    esker_store::meta::load_retiring(store.db())
        .unwrap()
        .iter()
        .any(|region| region.id == region_id)
}

/// The first of a retirement's two durable steps, and then nothing — which is what a crash between
/// them leaves.
///
/// It is the call `Store::retire_region` makes, with the same arguments: the region's own record
/// for the range, and `Some` because the membership no longer names a peer on this store. Driving
/// it directly rather than racing the real path is the only way to *stop* between the two steps;
/// what the real path does with the pair is the subject of the test above this one.
///
/// The peers are stopped first, and that is not tidiness. `retire_region` stops the region's peer
/// before it destroys anything, and a peer left running writes its state record back underneath
/// this — which is what the first version of this test observed, as a restarted store hosting the
/// region it had just been removed from.
fn destroy_but_do_not_reclaim(store: &Arc<Store>, region: &Region) {
    store.stop();
    let entries = esker_store::raft_log::destroy(store.db(), region.id, Some(region)).unwrap();
    assert!(
        entries > 0,
        "the region had no log entries, so this store never really held it"
    );
}

/// A crash between a retirement's two durable steps costs a restart, not the range.
///
/// # What used to happen, and why nothing could find it afterwards
///
/// The batch that ends a region on this store deletes its `'m'` record, and that record is the
/// only thing on disk that says which **keys** the region was. Delete it first and crash, and the
/// range's keys are in all three column families under no region, with nothing that can name them
/// again: not the store, which hosts no region covering them; not the placement driver, which has
/// no idea what this disk holds; and not a later retirement, which needs the record that is gone.
/// One rebalance interrupted by a `kill -9` leaked a whole region, for ever.
///
/// The three assertions in the middle are what make the last three mean anything: they are the
/// crash *observed* — the record gone, the announcement present, and the keys still there.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_between_the_record_and_the_range_is_finished_at_the_next_open() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address_listener,
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
    seed_all_three_families(&first.store, &region).await;

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), Some(vec![2])),
        2,
    )
    .await;
    let arrived = place_on(&pd, &first, &second).await;
    let held = counts(&second.store, &arrived);
    assert!(
        held.iter().all(|(_, keys)| *keys > 0),
        "the second store does not hold all three column families: {held:?}"
    );
    let shed_copy = plant_columnar_copy(&second, 1);
    let other_copy = plant_columnar_copy(&second, 7);

    // The crash. Step one runs, step two never does.
    destroy_but_do_not_reclaim(&second.store, &arrived);
    assert!(
        esker_store::meta::load_regions(second.store.db())
            .unwrap()
            .is_empty(),
        "the record that names the range survived the step that destroys it"
    );
    assert!(
        retirement_announced(&second.store, 1),
        "the range was orphaned: its record is gone and nothing announces what it was"
    );
    assert_eq!(
        counts(&second.store, &arrived),
        held,
        "the range was reclaimed by the step that only destroys the record"
    );
    let dir = second.crash().await;

    // The restart.
    let restarted = reopen(&dir, 2, &pd, raft_options(peers, None));
    assert!(
        restarted.regions().is_empty(),
        "the restarted store hosts a region it was removed from"
    );
    assert_eq!(
        counts(&restarted, &arrived),
        vec![
            (esker_engine::cf::DEFAULT, 0),
            (esker_engine::cf::LOCK, 0),
            (esker_engine::cf::WRITE, 0)
        ],
        "a retirement interrupted by a crash left its range on disk for ever"
    );
    assert!(
        !retirement_announced(&restarted, 1),
        "the retirement finished but is still announced, so every later open sweeps it again"
    );
    assert!(
        !shed_copy.exists(),
        "the columnar copy of a region whose retirement crashed was left behind"
    );
    assert!(
        other_copy.exists(),
        "finishing one region's retirement took another region's columnar copy with it"
    );

    // And the store that still owns the range lost nothing to a neighbour's restart.
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
        "the owner's own key stopped reading after a neighbour finished a retirement"
    );

    restarted.stop();
    first.stop().await;
}

/// Announces a retirement of `region` by hand, as the batch that destroys a record does.
fn announce_retirement(store: &Arc<Store>, region: &Region) {
    let cf_id = store.db().cf_id(esker_engine::cf::RAFT).unwrap();
    let mut batch = esker_engine::WriteBatch::new();
    esker_store::meta::stage_retiring(&mut batch, cf_id, region);
    store
        .db()
        .write(batch, &esker_engine::WriteOptions::synced())
        .unwrap();
}

/// A region this store still hosts is not emptied by a stale announcement of the range it covers.
///
/// **This is the half that is data loss when it is wrong**, and it is not hypothetical: a parent
/// whose split narrowed it, retired against the range it had *before*, announces a range the child
/// is now serving. The gate that catches it lives in the region map, which is why the sweep runs
/// after the peers are hosted and not before — run first it would find an empty map, conclude that
/// nothing overlaps, and delete the child's keys under it.
///
/// The announcement is dropped rather than kept, because the range is not orphaned: it belongs to
/// a region that is being served, and an announcement kept for it would ask the same refused
/// question at every open for the life of the store.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_announcement_never_empties_a_range_this_store_still_serves() {
    trace();
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let peers = vec![PeerAddress::new(1, 1, address)];

    let node = open(
        address_listener,
        1,
        &pd,
        raft_options(peers.clone(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = node.store.regions().regions()[0].clone();
    seed_all_three_families(&node.store, &region).await;
    let held = counts(&node.store, &region);
    assert!(
        held.iter().all(|(_, keys)| *keys > 0),
        "the store does not hold all three column families: {held:?}"
    );

    // The stale announcement: a region that is gone, over a range this store is still serving as
    // region 1. A split parent's record is exactly this shape.
    let stale = Region {
        id: 99,
        ..region.clone()
    };
    announce_retirement(&node.store, &stale);
    let dir = node.crash().await;

    let restarted = reopen(&dir, 1, &pd, raft_options(peers, Some(vec![1])));
    assert_eq!(
        counts(&restarted, &region),
        held,
        "a stale retirement announcement emptied a range this store still serves"
    );
    assert!(
        !retirement_announced(&restarted, 99),
        "a refused announcement was kept, so every later open asks the same refused question"
    );
    restarted.stop();
}

/// Finishing a retirement twice is finishing it once.
///
/// A crash may land *after* the range is empty and before the announcement is dropped, so the
/// ordinary case at open is a sweep over a range that has already been swept. It has to be cheap
/// and it has to be silent: `clear_range` returns early on an empty range, and a columnar tree
/// that is not there is a removal that is already done.
///
/// It runs on a store that hosts **nothing**, and that is the whole setup rather than a detail: on
/// a store that hosts a region, every range is inside one, so gate 2 would refuse the sweep and
/// this would pass without ever reaching the code it is about — the same shape as a green test
/// over a feature that is switched off.
#[tokio::test(flavor = "multi_thread")]
async fn an_announcement_whose_range_is_already_empty_is_simply_dropped() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    // The first store exists only to bootstrap the cluster, so that the second one hosts nothing.
    let first = open(
        first_address_listener,
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

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), Some(vec![2])),
        2,
    )
    .await;
    assert!(
        second.store.regions().is_empty(),
        "the second store hosts a region, so no range on it is unowned"
    );

    // A retirement that got as far as emptying its range and no further.
    let finished = Region {
        id: 42,
        ..region.clone()
    };
    announce_retirement(&second.store, &finished);
    let dir = second.crash().await;

    let restarted = reopen(&dir, 2, &pd, raft_options(peers, None));
    assert!(
        !retirement_announced(&restarted, 42),
        "an announcement over an already-empty range was kept rather than dropped, so every \
         later open sweeps it again"
    );
    restarted.stop();
    first.stop().await;
}
