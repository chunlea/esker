//! The joint gate: a DDL statement causes a columnar replica to be placed.
//!
//! Everything this exercises is tested in isolation somewhere else. What no unit test in this
//! phase can say is that the pieces are **connected**, and the connection is what was missing:
//! `ALTER TABLE t SET (columnar_replicas = 1)` wrote a durable catalog record that PD never heard
//! about, so no columnar learner was ever placed on a real cluster
//! (`docs/plans/phase-8-learner.md` §wire, "THE ONE GAP").
//!
//! So this is three real processes' worth of code in one test binary: a real `esker-pd`, four real
//! `esker-store`s over real sockets, and the real SQL node wiring — `PdConn`, `PdLease`, the
//! refresher, the executor. Nothing is a stand-in, because the seam *is* what is under test: the
//! phase-4 stall was a disagreement between PD's scheduler and the store's operator handling that
//! neither side's own fake could show (`esker-store/tests/promotion.rs` says the same thing).
//!
//! # What this gate does **not** prove, and the evidence for why
//!
//! The brief's gate ends with a differential — a fragment evaluated against the learner compared
//! with a row scan at the same `ts`. That cannot pass today, and not because of anything in this
//! lane: **`esker-store` has no columnar apply target on a live region and its fragment service
//! always refuses.** `Store::serve_fragment` answers `Refused { NotColumnar }` unconditionally,
//! saying of itself *"until the apply target is placed on regions (phase 8 unit 3), this is the
//! only answer this store has"*, and `ColumnarApply::open` is called from no path in
//! `esker-store/src`. So a columnar learner today is a learner that the region record says is
//! columnar and that applies rows like any other — which is exactly what §store units 1 and 2
//! built and what §store unit 3 has left to finish.
//!
//! [`the_fragment_service_still_refuses`] pins that as a fact rather than a claim, and is the test
//! to delete when unit 3 lands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{CountingOracle, TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{PeerRole, Server, ServerHandle, Service, TransportConfig};
use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pd::{ColumnarReport, LeaseRefresher, PdConn, PdLease};
use esker_store::server::RaftOptions;
use esker_store::{LogCompaction, PeerAddress, RemotePd, Store, StoreOptions, StoreService};

use cluster::{Session, TENANT};

/// Voters a region keeps. Three, as every acceptance scenario uses.
const VOTERS: usize = 3;
/// Stores in the cluster. One more than the voters, because a columnar learner is placed on the
/// healthiest store **without a peer** — a cluster with no spare store has nowhere to put one, and
/// a gate that could not tell "PD refused" from "PD had nowhere" would prove nothing.
const STORES: u64 = 4;

/// One store, and the socket it answers on.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    address: std::net::SocketAddr,
    dir: tempfile::TempDir,
}

fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn open_store(
    dir: tempfile::TempDir,
    address: std::net::SocketAddr,
    store_id: u64,
    pd_address: std::net::SocketAddr,
    peers: &[PeerAddress],
) -> Node {
    let mut raft = RaftOptions::new(peers.to_vec(), 20_260_831);
    raft.tick = Duration::from_millis(5);
    raft.compaction = LogCompaction {
        threshold: 64,
        keep: 16,
        ..LogCompaction::new()
    };
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: store_id,
            raft: Some(raft),
            pd: Some(Arc::new(RemotePd::connect(pd_address).unwrap())),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            // Also the latency of an operator: PD answers a region heartbeat and has no other way
            // to reach a store, so this is how long a placement decision takes to arrive.
            region_heartbeat: Duration::from_millis(20),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let handle = Server::bind(
        address,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap()
    .spawn()
    .unwrap();
    Node {
        store,
        handle,
        address,
        dir,
    }
}

/// The SQL node over this cluster, as the binary builds one: a client, a lease fetched before it
/// serves, a refresher thread, and the connection the executor reports through.
///
/// Routed from what PD actually says rather than from an assumption — PD allocates the region's
/// id, and a later conf change moves its epoch, which the client learns from the store's own
/// refusal, so this only has to be right at the start.
fn sql_node(
    addresses: &[std::net::SocketAddr],
    region: &esker_proto::Region,
    pd_address: std::net::SocketAddr,
) -> (Arc<dyn Backend>, Arc<PdConn>) {
    let stores = TcpStores::connect_all(addresses, TransportConfig::new()).unwrap();
    let resolver: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([Route {
        region: region.clone(),
        leader: None,
    }]));
    let router = Router::with_options(
        Arc::new(stores),
        resolver,
        ClientOptions {
            jitter_seed: Some(13),
            ..ClientOptions::default()
        },
    );
    let oracle: Arc<dyn TimestampOracle> = Arc::new(CountingOracle::starting_at(1_000));
    let client = Arc::new(TxnClient::on_router(Arc::new(router), Arc::clone(&oracle)));

    let lease = Arc::new(PdLease::new());
    let backend: Arc<dyn Backend> = Arc::new(
        esker_sql::backend::StoreBackend::new(client, oracle)
            .with_schema_lease(Arc::clone(&lease) as Arc<dyn SchemaLeaseSource>),
    );
    let conn = Arc::new(PdConn::new(pd_address));
    let refresher = LeaseRefresher::new(Arc::clone(&conn), lease)
        .asserting_columnar_for(Arc::clone(&backend), TENANT);
    refresher.refresh().expect("the node fetches its lease");
    std::thread::Builder::new()
        .name("schema-lease".to_owned())
        .spawn(move || refresher.run())
        .unwrap();
    (backend, conn)
}

/// The whole cluster: a placement driver, four stores, and one SQL node over them.
struct Gate {
    pd: Arc<Pd>,
    pd_address: std::net::SocketAddr,
    pd_handle: Option<ServerHandle>,
    nodes: Vec<Node>,
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    conn: Arc<PdConn>,
    region_id: u64,
}

impl Gate {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// The same cluster, with PD's balancer running.
    async fn start_balancing() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(balance: bool) -> Self {
        let pd_address = reserve();
        let addresses: Vec<std::net::SocketAddr> = (0..STORES).map(|_| reserve()).collect();
        let peers: Vec<PeerAddress> = addresses
            .iter()
            .enumerate()
            .map(|(at, address)| {
                let id = at as u64 + 1;
                PeerAddress::new(id, id, *address)
            })
            .collect();

        let pd_dir = tempfile::tempdir().unwrap();
        let pd = Pd::open(
            pd_dir.path(),
            PdOptions {
                target_replicas: VOTERS,
                operator_timeout_ms: 5_000,
                max_store_down_time_ms: 5_000,
                // **Off by default here**, as PD's own columnar tests have it: this gate is about
                // a placement a DDL statement caused, and balance moving a voter onto the spare
                // store would decide where the learner can go for reasons that have nothing to do
                // with the `ALTER`. What that hides is not nothing —
                // [`a_columnar_learner_does_not_cost_the_region_a_voter`] is the case it hides.
                balance,
                ..PdOptions::new()
            },
        )
        .unwrap();
        let pd_handle = Server::bind(
            pd_address,
            PdService::new(Arc::clone(&pd)) as Arc<dyn Service>,
            TransportConfig::new(),
        )
        .await
        .unwrap()
        .spawn()
        .unwrap();

        // The first store bootstraps the cluster; PD's repair grows the region to `VOTERS`.
        let mut nodes = Vec::new();
        for (at, address) in addresses.iter().enumerate() {
            nodes.push(
                open_store(
                    tempfile::tempdir().unwrap(),
                    *address,
                    at as u64 + 1,
                    pd_address,
                    &peers,
                )
                .await,
            );
        }
        wait_for("the region to reach three voters", 60, || {
            pd.regions().is_ok_and(|regions| {
                regions.iter().any(|record| {
                    record
                        .region
                        .peers
                        .iter()
                        .filter(|peer| peer.role == PeerRole::Voter)
                        .count()
                        == VOTERS
                })
            })
        })
        .await;

        // PD allocates the region's id, so the client is told what PD actually says rather than
        // what a fresh cluster usually comes out as.
        let route = pd
            .get_region(b"")
            .unwrap()
            .expect("a region covers the key space");
        let region_id = route.region.id;
        let (backend, conn) =
            tokio::task::block_in_place(|| sql_node(&addresses, &route.region, pd_address));

        Gate {
            pd,
            pd_address,
            pd_handle: Some(pd_handle),
            nodes,
            backend,
            catalog: Arc::new(Catalog::new()),
            conn,
            region_id,
        }
    }

    fn session(&self) -> Session {
        Session {
            executor: Executor::new(Arc::clone(&self.backend), Arc::clone(&self.catalog), TENANT)
                .reporting_columnar_to(Arc::clone(&self.conn) as Arc<dyn ColumnarReport>),
        }
    }

    /// The store ids holding a columnar learner of any region, as **PD** records them.
    fn columnar_learners(&self) -> Vec<u64> {
        let mut found: Vec<u64> = self
            .pd
            .regions()
            .unwrap()
            .iter()
            .flat_map(|record| record.region.peers.clone())
            .filter(|peer| peer.role == PeerRole::ColumnarLearner)
            .map(|peer| peer.store_id)
            .collect();
        found.sort_unstable();
        found
    }

    /// The node PD placed the columnar learner on, and the region it now holds.
    fn learner_node(&self) -> &Node {
        let placed = *self
            .columnar_learners()
            .first()
            .expect("a columnar learner has been placed");
        self.nodes
            .iter()
            .find(|node| node.store.store_id() == placed)
            .expect("the store PD placed it on is one of ours")
    }

    async fn stop(mut self) {
        for node in &self.nodes {
            node.store.stop();
        }
        for node in self.nodes.drain(..) {
            let _ = node.handle.shutdown().await;
            drop(node.dir);
        }
        if let Some(handle) = self.pd_handle.take() {
            let _ = handle.shutdown().await;
        }
    }
}

/// **The gate.** A DDL statement on a real cluster places a columnar replica, and clearing the
/// flag retires it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_alter_places_a_columnar_replica_and_clearing_it_takes_it_back() {
    let gate = Gate::start().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .unwrap();
        session
            .run("INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger')")
            .unwrap();
        assert!(
            gate.columnar_learners().is_empty(),
            "nothing has asked for a columnar copy yet",
        );

        session
            .run("ALTER TABLE t SET (columnar_replicas = 1)")
            .unwrap();
    });

    // PD heard it, as a key range and not as a table id: a range is PD's own vocabulary, and it
    // acts on this without ever learning that a table exists (`CLAUDE.md` invariant 7).
    let wishes = gate.pd.columnar_wishes();
    assert_eq!(wishes.len(), 1, "the ALTER reported one range: {wishes:?}");
    assert_eq!(wishes[0].replicas, 1);
    assert!(
        wishes[0].start_key.starts_with(b"t"),
        "the wish names the table's row range: {:?}",
        wishes[0],
    );

    // PD scheduled it, and a store built it.
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    let placed = gate.columnar_learners()[0];
    assert!(
        !gate.pd.regions().unwrap().iter().any(|record| record
            .region
            .peers
            .iter()
            .filter(|peer| peer.role == PeerRole::Voter)
            .count()
            != VOTERS),
        "a columnar learner is never a voter and never counted as one",
    );
    let learner = gate
        .nodes
        .iter()
        .find(|node| node.store.store_id() == placed)
        .expect("the store PD placed it on is one of ours");
    wait_for("the store to build the learner", 60, || {
        learner.store.regions().find(b"t").is_some()
    })
    .await;
    assert!(
        !learner
            .store
            .peer_of(gate.region_id)
            .is_some_and(|peer| peer.is_leader()),
        "a columnar learner never leads",
    );

    // It stays a learner. A columnar replica is a learner that is **never** promoted (ADR 0022
    // Decision 1), so this is the assertion that would fail if it were treated as a repair.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        gate.columnar_learners(),
        vec![placed],
        "the columnar learner was promoted or replaced",
    );

    // The flag back to zero: the next assertion simply omits the table, and removal falls out of
    // the full-assertion shape rather than needing a message of its own.
    tokio::task::block_in_place(|| {
        gate.session()
            .run("ALTER TABLE t SET (columnar_replicas = 0)")
            .unwrap();
    });
    assert!(
        gate.pd.columnar_wishes().is_empty(),
        "a table set to zero is absent from the report",
    );
    wait_for("PD to retire the columnar learner", 60, || {
        gate.columnar_learners().is_empty()
    })
    .await;

    gate.stop().await;
}

/// The half of the gate that cannot pass yet, pinned as a fact.
///
/// A learner placed by the test above is a learner the *record* says is columnar. Ask it for a
/// fragment and it refuses with `NotColumnar`, because `esker-store` has no columnar apply target
/// on a live region and `serve_fragment` is a documented placeholder. That refusal is the correct
/// answer for what is built — a refusal means "fall back to a row scan" and is never an error —
/// and it is also the evidence that the wave's differential is blocked on §store unit 3 rather
/// than on this lane.
///
/// **Delete this test when unit 3 lands**, and put the differential here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fragment_service_still_refuses() {
    use esker_proto::fragment::{FragmentReq, FragmentResp, RefusalReason};
    use esker_proto::{BlockingTransport, Request, RequestHeader, Response};

    let gate = Gate::start().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .unwrap();
        session.run("INSERT INTO t VALUES (1, 'ada')").unwrap();
        session
            .run("ALTER TABLE t SET (columnar_replicas = 1)")
            .unwrap();
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    let placed = gate.columnar_learners()[0];
    let learner = gate
        .nodes
        .iter()
        .find(|node| node.store.store_id() == placed)
        .unwrap();
    wait_for("the store to build the learner", 60, || {
        learner.store.regions().find(b"t").is_some()
    })
    .await;

    let region = learner.store.regions().find(b"t").unwrap();
    let address = learner.address;
    let answer = tokio::task::block_in_place(|| {
        let transport = BlockingTransport::connect(address).unwrap();
        transport
            .call(
                Request::Fragment {
                    header: RequestHeader::new(region.id(), region.region().epoch, 0),
                    request: FragmentReq {
                        fragment: bytes::Bytes::new(),
                        ts: 1,
                        min_apply_index: 0,
                    },
                },
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap()
    });
    match answer {
        Response::Fragment(FragmentResp::Refused { reason, detail }) => {
            assert_eq!(
                reason,
                RefusalReason::NotColumnar,
                "the store refused for a different reason: {detail}",
            );
        }
        other => panic!(
            "the fragment service answered {other:?}; if it evaluated the fragment then \
             §store unit 3 has landed and this test should be replaced by the differential",
        ),
    }

    gate.stop().await;
}

/// The learner is killed while it is catching up, and comes back to the same job.
///
/// Not a literal `kill -9`: a store here is an object in this test's process, so what it takes is
/// the abrupt stop an in-process store can be given — the socket closed and the tasks aborted,
/// with nothing flushed on the way out — and then a reopen from the same directory. A real
/// `SIGKILL` between processes is `esker-cli`'s `cluster_chaos` battery, which owns that question
/// for every store in the cluster and is where it should stay.
///
/// What is being asked here is narrower and is this lane's: **placement survives the replica.** A
/// columnar learner is a peer PD placed and a peer the store recorded, so a store that comes back
/// must still hold the region as a learner and must catch up on what it missed — without PD having
/// to be told again, and without another `ALTER`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_learner_that_dies_comes_back_to_the_same_job() {
    let mut gate = Gate::start().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .unwrap();
        session.run("INSERT INTO t VALUES (1, 'ada')").unwrap();
        session
            .run("ALTER TABLE t SET (columnar_replicas = 1)")
            .unwrap();
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    let placed = gate.columnar_learners()[0];
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let before = gate
        .learner_node()
        .store
        .peer_of(region_id)
        .expect("the learner has a peer")
        .applied_index();

    // Down it goes, holding nothing: the socket first, so nothing can reach it, then the tasks.
    let at = gate
        .nodes
        .iter()
        .position(|node| node.store.store_id() == placed)
        .unwrap();
    let dead = gate.nodes.remove(at);
    let (address, dir) = (dead.address, dead.dir);
    dead.handle.shutdown().await.unwrap();
    dead.store.stop();
    drop(dead.store);

    // The cluster carries on: a columnar learner is not a voter, so losing one costs the region
    // no quorum and the writes below are unaffected by its absence.
    tokio::task::block_in_place(|| {
        gate.session()
            .run("INSERT INTO t VALUES (2, 'grace'), (3, 'edsger')")
            .unwrap();
    });

    // And back, from its own directory, on its own address.
    let peers: Vec<PeerAddress> = gate
        .nodes
        .iter()
        .map(|node| PeerAddress::new(node.store.store_id(), node.store.store_id(), node.address))
        .chain(std::iter::once(PeerAddress::new(placed, placed, address)))
        .collect();
    let reopened = open_store(dir, address, placed, gate.pd_address, &peers).await;
    gate.nodes.push(reopened);

    // It knows what it is from its own records, without PD saying so again...
    wait_for("the reopened store to hold its region", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;
    assert_eq!(
        gate.columnar_learners(),
        vec![placed],
        "it came back as the same thing: a learner that is never promoted",
    );
    // ...and it catches up on what it missed.
    wait_for(
        "the learner to catch up on the writes it missed",
        60,
        || {
            gate.learner_node()
                .store
                .peer_of(region_id)
                .is_some_and(|peer| peer.applied_index() > before)
        },
    )
    .await;

    gate.stop().await;
}

/// A columnar learner must not cost the region a voter — and today it does.
///
/// **This test is `ignore`d because it fails, and it fails on a defect in `esker-pd` that this
/// lane is not allowed to fix.** It is here rather than in a report because a red test is the
/// fastest thing to hand across a lane boundary: un-`ignore` it, and it either passes or it says
/// exactly what is still wrong.
///
/// `balance::region_balance` asks `region.peers.len() > cluster.target_replicas`, over **every**
/// peer. A healthy three-voter region that gains a columnar learner is four peers against a
/// target of three, so balance sheds "the replica on the busiest store" — a voter — and repair
/// then has to put one back. That is the third instance of the family `wy-c2` named at the end of
/// wave A: *"a count taken over `peers` rather than over voters"*, after `urgency_for` and
/// `repair_for`, both of which were fixed and both of which read healthy on a cluster that was
/// not.
///
/// Seen first on a real cluster, not here (`docs/bench/columnar-learner.md`). PD's own history,
/// out of `esker pd inspect` after the run:
///
/// ```text
///   1788233921876 ms  region 1  AddLearner  issued     store 4  peer 5
///   1788233921975 ms  region 1  AddLearner  done       store 4  peer 5
///   1788233921975 ms  region 1  RemovePeer  issued     store 0  peer 3
///   1788233922074 ms  region 1  RemovePeer  done       store 0  peer 3
///   1788233922074 ms  region 1  AddPeer     issued     store 1  peer 6
///   1788234222175 ms  region 1  AddPeer     timed out  store 1  peer 6
/// ```
///
/// The voter went in the same millisecond the columnar learner landed, and no store was down —
/// all four were heart-beating throughout, so `repair_for`, which only removes a *dead* peer, is
/// not what did it. The cost is not cosmetic: the region sat at **two** voters for the five
/// minutes the replacement's `AddPeer` took to time out, one failure from losing quorum, and
/// every other operator for that region — including the removal the next `ALTER` asked for —
/// waited behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "fails on a defect in esker-pd's balance rule: see this test's doc comment"]
async fn a_columnar_learner_does_not_cost_the_region_a_voter() {
    let gate = Gate::start_balancing().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .unwrap();
        session.run("INSERT INTO t VALUES (1, 'ada')").unwrap();
        session
            .run("ALTER TABLE t SET (columnar_replicas = 1)")
            .unwrap();
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;

    // Long enough for the next few heartbeats to have been answered: the removal that this test
    // is about arrived in the millisecond after the learner landed.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let voters: Vec<u64> = gate
        .pd
        .regions()
        .unwrap()
        .iter()
        .flat_map(|record| record.region.peers.clone())
        .filter(|peer| peer.role == PeerRole::Voter)
        .map(|peer| peer.peer_id)
        .collect();
    assert_eq!(
        voters.len(),
        VOTERS,
        "the region lost a voter when it gained a columnar learner: {:?}",
        gate.pd.regions().unwrap(),
    );

    gate.stop().await;
}
