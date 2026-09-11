//! [`RemotePd`] against a real socket: the bridge between the store's synchronous `PdClient` and
//! the asynchronous wire.
//!
//! The placement driver here is a stand-in that answers the five methods from a `Mutex`, not
//! `esker-pd`. That is deliberate rather than a shortcut: `esker-store` does not depend on
//! `esker-pd` and must not — they are peers that meet on the wire — so a test that imported it
//! would be testing a dependency this crate does not have. What is under test is the *bridge*:
//! that a synchronous call reaches the socket, that the cluster id is stamped after a bootstrap,
//! and that a store that opens against a real address gets a real answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use esker_proto::{
    BoxFuture, Epoch, PdReq, PdResp, Peer, ProtoError, Region, Reply, Request, Response, Server,
    ServerHandle, Service, StoreInfo as WireStoreInfo,
};
use esker_store::pd::{PdClient, StoreInfo};
use esker_store::{RemotePd, Store, StoreOptions};

/// A placement driver in a `Mutex`, behind the real framing.
#[derive(Debug, Default)]
struct StandIn {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    cluster_id: u64,
    next_id: u64,
    /// The cluster id each request carried, so a test can assert that it is stamped.
    seen_cluster_ids: Vec<u64>,
    store_beats: Vec<u64>,
    region_beats: Vec<u64>,
}

impl StandIn {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn answer(&self, cluster_id: u64, request: PdReq) -> PdResp {
        fn lone_membership() -> esker_proto::PdMembership {
            esker_proto::PdMembership {
                group_id: 1,
                this_id: 1,
                leader_id: 1,
                term: 1,
                members: vec![esker_proto::PdMemberInfo {
                    id: 1,
                    address: "127.0.0.1:2379".to_owned(),
                    role: esker_proto::PdRole::Voter,
                }],
            }
        }

        let mut state = self.lock();
        state.seen_cluster_ids.push(cluster_id);
        match request {
            // The safepoint round (ADR 0110). This stand-in publishes nothing, which is the
            // safe answer and the one a store must survive: zero collects nothing.
            PdReq::Safepoint { .. } => PdResp::Safepoint { safepoint: 0 },
            PdReq::Bootstrap { store } => {
                let first = state.cluster_id == 0;
                if first {
                    state.cluster_id = 77;
                    state.next_id = 2;
                }
                PdResp::Bootstrap {
                    cluster_id: state.cluster_id,
                    region: first.then(|| Region::bootstrap(1, store.store_id, 1)),
                }
            }
            // A SQL node telling PD which tables want columnar replicas. **A store never sends
            // one** — it is reported from above, because PD carries where a replica lives and not
            // what a table looks like — so this stand-in records nothing and answers the empty
            // acknowledgement, which is what a store's PD client would see if it ever asked.
            PdReq::ReportColumnar { .. } => PdResp::ReportColumnar,
            PdReq::StoreHeartbeat { store_id, .. } => {
                state.store_beats.push(store_id);
                PdResp::StoreHeartbeat
            }
            PdReq::RegionHeartbeat { region, .. } => {
                state.region_beats.push(region.id);
                // 4c's operator rides here; this stand-in issues none.
                PdResp::RegionHeartbeat { operator: None }
            }
            PdReq::GetRegion { key } => {
                let region = Region {
                    id: 1,
                    start_key: Bytes::new(),
                    end_key: Bytes::new(),
                    peers: vec![Peer::voter(1, 1)],
                    epoch: Epoch::INITIAL,
                };
                PdResp::GetRegion {
                    region: region.contains(&key).then_some(region),
                    leader_peer_id: 1,
                    stores: vec![WireStoreInfo::new(1, "127.0.0.1:20160")],
                }
            }
            PdReq::AllocId { count } => {
                let start = state.next_id.max(1);
                state.next_id = start + count;
                PdResp::AllocId { start, count }
            }
            PdReq::Tso { count } => PdResp::Tso { start_ts: 1, count },
            // ADR 0020's lease; this stand-in answers a fixed one and removes nothing.
            PdReq::SchemaLease => PdResp::SchemaLease {
                lease_ms: 1_000,
                step_interval_ms: 1_500,
                removal_extra_ms: 0,
            },
            // An operator's question, not a store's: `esker pd status` asks it. Answered rather
            // than refused so the match stays exhaustive — which is what made this file the first
            // place to notice the new method.
            PdReq::Status => PdResp::Status {
                now_ms: 0,
                operators: Vec::new(),
            },
            // Also an operator's: `esker region ls` pages the routing table with it. A store
            // asks `GetRegion` about the key it has, never for a list.
            PdReq::ScanRegions { .. } => PdResp::ScanRegions {
                regions: Vec::new(),
                stores: Vec::new(),
            },
            // Between two placement drivers, never between a store and one. A stand-in that
            // answered it would be pretending to be a member of a group it is not in.
            PdReq::Raft(_) => PdResp::Raft,
            // An operator's, not a store's. A stand-in that is a group of one has nothing to
            // change and says so.
            PdReq::MemberChange { .. } => PdResp::MemberChange {
                membership: lone_membership(),
                done: true,
            },
            // An operator's question, not a store's, and this stand-in is a group of one.
            PdReq::Members => PdResp::Members(lone_membership()),
        }
    }
}

impl Service for StandIn {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        Box::pin(async move {
            match request {
                Request::Pd {
                    cluster_id,
                    request,
                } => Ok(Reply::Unary(Response::Pd(self.answer(cluster_id, request)))),
                other => Err(ProtoError::invalid(format!(
                    "this stand-in only answers Pd requests, not {}",
                    other.method().name()
                ))),
            }
        })
    }

    fn store_id(&self) -> u64 {
        0
    }
}

async fn serve() -> (Arc<StandIn>, ServerHandle, std::net::SocketAddr) {
    let pd = Arc::new(StandIn::default());
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::clone(&pd) as Arc<dyn Service>,
        esker_proto::TransportConfig::new(),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.spawn().unwrap();
    (pd, handle, address)
}

fn info(store_id: u64) -> StoreInfo {
    StoreInfo {
        store_id,
        address: format!("127.0.0.1:{}", 20_160 + store_id),
    }
}

/// The whole bridge in one call: a synchronous method on a blocking thread reaches a real socket
/// and comes back with the answer, and the cluster id the bootstrap minted is stamped on every
/// request after it without the caller carrying it.
#[tokio::test(flavor = "multi_thread")]
async fn a_bootstrap_latches_the_cluster_id_for_every_later_call() {
    let (pd, handle, address) = serve().await;
    let client = tokio::task::spawn_blocking(move || {
        let client = RemotePd::connect(address).unwrap();
        let answer = client.bootstrap(&info(1)).unwrap();
        assert_eq!(answer.cluster_id, 77);
        let region = answer.region.expect("the first store creates region 1");
        assert_eq!(region.id, 1);
        assert_eq!(region.end_key, Bytes::new(), "it covers everything");

        // Everything after the bootstrap carries the id, without this caller holding a copy.
        assert_eq!(client.alloc_id(4).unwrap(), 2);
        assert_eq!(client.alloc_id(1).unwrap(), 6, "blocks are not reused");
        let route = client.get_region(b"anything").unwrap().expect("covered");
        assert_eq!(route.region.id, 1);
        assert_eq!(route.leader_peer_id, 1);
        assert_eq!(route.stores, vec![(1, "127.0.0.1:20160".to_owned())]);
        client
    })
    .await
    .unwrap();

    let seen = pd.lock().seen_cluster_ids.clone();
    assert_eq!(
        seen,
        vec![0, 77, 77, 77],
        "the bootstrap says `not known yet`; everything after it says 77"
    );

    drop(client);
    handle.shutdown().await.unwrap();
}

/// A second store is told the cluster already exists, and hosts nothing rather than claiming the
/// key space a second time. This is the same rule as `multi_region.rs` asserts against the fake,
/// checked once against the wire so that the fake is known to be answering the real question.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_first_store_is_told_to_bootstrap() {
    let (_pd, handle, address) = serve().await;
    tokio::task::spawn_blocking(move || {
        let client = RemotePd::connect(address).unwrap();
        assert!(client.bootstrap(&info(1)).unwrap().region.is_some());
        for store in [2, 3, 1] {
            let answer = client.bootstrap(&info(store)).unwrap();
            assert_eq!(answer.cluster_id, 77);
            assert!(
                answer.region.is_none(),
                "store {store} was told to bootstrap"
            );
        }
    })
    .await
    .unwrap();
    handle.shutdown().await.unwrap();
}

/// A store opened against a real placement driver bootstraps through it and then reports to it —
/// the two ends of 4a's PD story, over a socket, in one process.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_bootstraps_and_reports_over_the_wire() {
    let (pd, handle, address) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let client: Arc<dyn PdClient> = Arc::new(RemotePd::connect(address).unwrap());

    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            pd: Some(Arc::clone(&client)),
            address: "127.0.0.1:20161".to_owned(),
            heartbeat_tick: std::time::Duration::from_millis(2),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    assert_eq!(store.regions().len(), 1, "PD told it to create region 1");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        {
            let state = pd.lock();
            if !state.store_beats.is_empty() && !state.region_beats.is_empty() {
                assert_eq!(state.store_beats[0], 1);
                assert_eq!(state.region_beats[0], 1);
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no heartbeat reached the placement driver"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    store.stop();
    drop(store);
    handle.shutdown().await.unwrap();
}

/// A store started before its placement driver fails to open. The alternative — bootstrapping a
/// region of its own — would be a second claim to every key in the cluster, and the two would not
/// find out until a client asked one of them.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_started_before_its_placement_driver_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 on the loopback: privileged, and nothing of ours listens there.
    let client: Arc<dyn PdClient> =
        Arc::new(RemotePd::connect("127.0.0.1:1".parse().unwrap()).unwrap());
    let error = Store::open(
        dir.path(),
        StoreOptions {
            pd: Some(client),
            address: "127.0.0.1:20160".to_owned(),
            ..StoreOptions::new()
        },
    )
    .unwrap_err();
    assert!(!error.to_string().is_empty(), "{error}");

    // And the database is left with no region, so a later open against a live PD is still a
    // bootstrap rather than a store that half-exists.
    assert!(
        esker_store::meta::load_regions(
            &esker_engine::Db::open_with(
                dir.path(),
                esker_engine::Options {
                    create_if_missing: true,
                    ..esker_engine::Options::default()
                },
                Arc::new(esker_engine::LocalFileSystem::new()),
                &esker_engine::cf::BUILTIN,
            )
            .unwrap()
        )
        .unwrap()
        .is_empty()
    );
}
