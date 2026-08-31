//! An operator decided by a placement driver, over a real socket, reaching a Raft proposal.
//!
//! Every other test of operator consumption hands the store a `FakePd` directly, so the operator
//! never touches the wire. That leaves one seam untested and it is the seam that matters: PD
//! decides in one process and the store acts in another, and everything between them —
//! `PdResp::RegionHeartbeat`'s encoding, `PdChannel`, the blocking bridge in `pd_remote.rs`, the
//! heartbeat schedule that collects the answer — is code an in-process fake skips entirely. An
//! operator dropped anywhere along it is a placement driver whose decisions silently never happen.
//!
//! So the placement driver here is a real server on a real port. It is deliberately not
//! `esker-pd`: what is under test is the store's side of the contract, and a fake that answers
//! from a script fails for one reason rather than two.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esker_proto::{
    BoxFuture, Epoch, Operator, PdReq, PdResp, ProtoError, Region, Reply, Request, Response,
    Server, Service, TransportConfig,
};
use esker_store::pd_remote::RemotePd;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};

/// The store this test grows a replica onto. It does not exist, and does not need to: what is
/// asserted is that the *proposal* happened, which a single-voter group commits alone.
const TARGET_STORE: u64 = 2;
/// The peer id a real placement driver would have allocated for it.
const NEW_PEER: u64 = 1_001;

/// A placement driver that answers from a script, and records what it was asked.
///
/// It issues its `AddPeer` on **every** region heartbeat rather than once, which is what a real
/// one does until it sees the region's epoch move — and which means the test cannot pass by
/// catching a single lucky round.
#[derive(Debug)]
struct ScriptedPd {
    cluster_id: u64,
    bootstrap_region: Mutex<Option<Region>>,
    heartbeats: AtomicU64,
    /// Every region as the store reported it, in order. The last one is what the store believes
    /// after the operator applied.
    reported: Mutex<Vec<Region>>,
}

impl ScriptedPd {
    fn new(bootstrap_region: Region) -> Arc<Self> {
        Arc::new(Self {
            cluster_id: 42,
            bootstrap_region: Mutex::new(Some(bootstrap_region)),
            heartbeats: AtomicU64::new(0),
            reported: Mutex::new(Vec::new()),
        })
    }

    fn answer(&self, request: PdReq) -> Result<PdResp, ProtoError> {
        Ok(match request {
            PdReq::Bootstrap { .. } => PdResp::Bootstrap {
                cluster_id: self.cluster_id,
                // Exactly once in the life of a cluster, as `Store::open` requires.
                region: self.bootstrap_region.lock().unwrap().take(),
            },
            PdReq::StoreHeartbeat { .. } => PdResp::StoreHeartbeat,
            PdReq::RegionHeartbeat { region, .. } => {
                self.heartbeats.fetch_add(1, Ordering::Relaxed);
                let epoch = region.epoch;
                let region_id = region.id;
                let grown = region.peers.iter().any(|peer| peer.peer_id == NEW_PEER);
                self.reported.lock().unwrap().push(region);
                PdResp::RegionHeartbeat {
                    // Once the region has the peer, the work is done and a real driver stops
                    // issuing. Keeping on would also be harmless — the store's own idempotence
                    // makes a repeat a no-op — but stopping is what lets the test tell the two
                    // apart.
                    operator: (!grown).then_some(Operator::AddPeer {
                        region_id,
                        epoch,
                        store_id: TARGET_STORE,
                        peer_id: NEW_PEER,
                    }),
                }
            }
            PdReq::AllocId { count } => PdResp::AllocId {
                start: 10_000,
                count,
            },
            other => {
                return Err(ProtoError::invalid(format!(
                    "this placement driver does not answer {other:?}"
                )));
            }
        })
    }
}

impl Service for ScriptedPd {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        Box::pin(async move {
            match request {
                Request::Pd { request, .. } => self
                    .answer(request)
                    .map(|answer| Response::Pd(answer).into()),
                other => Err(ProtoError::invalid(format!(
                    "a placement driver was sent {:?}",
                    other.method()
                ))),
            }
        })
    }

    fn store_id(&self) -> u64 {
        0
    }
}

fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A placement driver decides, over a socket, and the store proposes it.
///
/// The assertion is the region's own peer list: it moves only when the conf-change entry has been
/// proposed, committed and applied, so nothing short of the whole path satisfies it.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_decided_over_the_wire_reaches_a_raft_proposal() {
    let pd_address = reserve();
    let store_address = reserve();

    let region = Region::bootstrap(1, 1, 1);
    let pd = ScriptedPd::new(region);
    let pd_server = Server::bind(
        pd_address,
        Arc::clone(&pd) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap();
    let pd_handle = pd_server.spawn().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(vec![PeerAddress::new(1, 1, store_address)], 20_260_830);
    raft.tick = Duration::from_millis(5);
    raft.bootstrap_voters = Some(vec![1]);

    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            raft: Some(raft),
            pd: Some(Arc::new(RemotePd::connect(pd_address).unwrap())),
            address: store_address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let store_server = Server::bind(
        store_address,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap();
    let store_handle = store_server.spawn().unwrap();

    wait_for("a leader", 10, || {
        store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    // Only a leader heartbeats a region, so this also pins that the schedule is running at all.
    wait_for(
        "a region heartbeat to reach the placement driver",
        10,
        || pd.heartbeats.load(Ordering::Relaxed) > 0,
    )
    .await;

    wait_for("the operator to become a peer of the region", 20, || {
        store
            .regions()
            .get(1)
            .is_some_and(|state| state.region().peers.iter().any(|p| p.peer_id == NEW_PEER))
    })
    .await;

    // The peer arrived as a **learner on the store PD named**, which is the whole content of the
    // operator: a conf change that added a voter, or added it somewhere else, would mean the
    // decision reached the store with a field missing.
    let grown = store.regions().get(1).unwrap().region().clone();
    let added = grown
        .peers
        .iter()
        .find(|peer| peer.peer_id == NEW_PEER)
        .expect("the new peer");
    assert_eq!(added.store_id, TARGET_STORE, "{grown:#?}");
    assert_eq!(added.role, esker_proto::PeerRole::Learner, "{grown:#?}");
    assert!(
        grown.epoch.conf_ver > Epoch::INITIAL.conf_ver,
        "a membership change that did not bump conf_ver leaves every cached epoch wrong"
    );

    // And PD saw the result reported back, which is how a real one knows to stop issuing.
    wait_for("the grown region to be reported back", 10, || {
        pd.reported
            .lock()
            .unwrap()
            .last()
            .is_some_and(|region| region.peers.iter().any(|p| p.peer_id == NEW_PEER))
    })
    .await;

    store.stop();
    let _ = store_handle.shutdown().await;
    let _ = pd_handle.shutdown().await;
}
