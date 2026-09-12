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

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::ProtoError;
use esker_proto::fragment::result::{Body, Value as WireValue};
use esker_proto::fragment::{FragmentReq, FragmentResp, RefusalReason};
use esker_proto::txn::TxnStatus;
use esker_proto::{
    BlockingTransport, PeerRole, Request, RequestHeader, Response, Server, ServerHandle, Service,
    TransportConfig,
};
use esker_proto::{TxnKvReq, TxnKvResp, TxnMutation};
use esker_sql::Datum;
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
    address: SocketAddr,
    dir: tempfile::TempDir,
}

/// A free port, **held** until the server that wants it is about to bind.
///
/// Returning only the address and dropping the listener is a time-of-check race: between the
/// kernel picking the port and `Server::bind` asking for it, a sibling test in the same binary is
/// handed the same number. It failed roughly one run in three, on a different test each time,
/// with `Io { detail: "Address already in use (os error 98)" }` — and it reddens every lane's
/// gate, not only this file's.
///
/// `esker-store`'s eight cluster tests already do it this way: reserve every port up front, keep
/// the listeners, and drop each one as its own server binds. The window is then one call wide and
/// every *other* port stays occupied while it is open.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
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
    address: SocketAddr,
    store_id: u64,
    pd_address: SocketAddr,
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
    addresses: &[SocketAddr],
    region: &esker_proto::Region,
    pd_address: SocketAddr,
    oracle: Arc<dyn TimestampOracle>,
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

/// One decoded cell, on either side of the differential.
///
/// A shared shape rather than one per side: the comparison is only worth making if the two sides
/// cannot disagree about how to *say* a value, only about what it is. A wire `Value` and a
/// `Datum` are different types with the same three cases here, and flattening both into this is
/// the only place the two vocabularies meet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Cell {
    Int8(i64),
    Text(String),
    Null,
}

/// The gate's timestamp source: physical milliseconds in the high bits and a counter in the low
/// ones, which is the shape PD's TSO hands out.
///
/// **`CountingOracle` cannot be used here, and the reason is lock expiry.** Percolator resolves a
/// lock whose owner vanished by deciding it is dead, and `esker_client::is_expired` decides that
/// on the *physical half* of the timestamps — deliberately, so that no node judges another's
/// transaction by its own clock. Under a plain counter that half is zero and stays zero:
/// `physical_ms(1009)` is 0, `physical_ms(now)` is 0, and a lock left behind by a write whose
/// answer was lost can never expire. Anything that then touches those rows spins against it for
/// as long as it is willing to wait — seen exactly so, as `a lock from the transaction at 1009
/// could not be cleared`, for thirty seconds. (ADR 0104 §4 gave that sentence a suffix naming
/// which of the three waits gave up, so what a run prints today ends `… for a read`.)
///
/// The other tests in this crate never orphan a lock, which is why they can count and this cannot.
/// Nothing here reads a clock to *order* anything (`CLAUDE.md` invariant 6): this stands in for
/// PD's TSO, which is the one component whose job is to turn a clock into timestamps.
#[derive(Debug)]
struct WallClockOracle {
    /// The next timestamp to issue, never below the last one handed out.
    next: std::sync::Mutex<u64>,
}

impl WallClockOracle {
    fn new() -> Self {
        Self {
            next: std::sync::Mutex::new(0),
        }
    }
}

impl TimestampOracle for WallClockOracle {
    fn tso(&self, count: u32) -> Result<u64, ProtoError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Monotonic whatever the clock does, and never repeating inside one millisecond: the
        // logical bits are what a second caller in the same millisecond gets.
        let issued = (*next).max(now_ms << esker_client::TSO_LOGICAL_BITS);
        *next = issued.saturating_add(u64::from(count.max(1)));
        Ok(issued)
    }
}

/// The whole cluster: a placement driver, four stores, and one SQL node over them.
struct Gate {
    pd: Arc<Pd>,
    /// The SQL node's oracle, so a test can name the instant it reads at.
    oracle: Arc<dyn TimestampOracle>,
    pd_address: SocketAddr,
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
        // Every cluster in this file goes through here. See `tests/trace`: the subscriber is
        // installed at the harness rather than remembered per test, so `RUST_LOG` works on the
        // test somebody is already debugging.
        cluster::trace::on();

        let pd_listener = reserve();
        let pd_address = pd_listener.local_addr().unwrap();
        let listeners: Vec<std::net::TcpListener> = (0..STORES).map(|_| reserve()).collect();
        let addresses: Vec<SocketAddr> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        let mut listeners = listeners.into_iter();
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
        drop(pd_listener);
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
            drop(listeners.next());
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
        let oracle: Arc<dyn TimestampOracle> = Arc::new(WallClockOracle::new());
        let (backend, conn) = tokio::task::block_in_place(|| {
            sql_node(&addresses, &route.region, pd_address, Arc::clone(&oracle))
        });

        Gate {
            pd,
            oracle,
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
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                esker_sql::session::register(),
            )
            .reporting_columnar_to(Arc::clone(&self.conn) as Arc<dyn ColumnarReport>),
        }
    }

    /// The id of a table this session created.
    fn table_id(&self, name: &str) -> u64 {
        let txn = self.backend.begin().unwrap();
        let view = self.catalog.view(&*txn, TENANT).unwrap();
        let id = view.table(name).unwrap().unwrap().id;
        let _ = txn.rollback();
        id
    }

    /// The rows a fragment against the learner answers with, as `(id, name)`.
    fn fragment(
        &self,
        tenant: u64,
        table_id: u64,
        ts: u64,
        min_apply_index: u64,
        projection: Vec<u32>,
    ) -> Vec<Vec<Cell>> {
        // **A refusal is designed behaviour here, not a failure** (`#86`, `#88`). A columnar copy
        // cannot see an unresolved secondary lock, so rather than answer one row short in silence
        // it refuses the **whole** scan — which means a fragment asked while a committed
        // transaction still has a lock standing comes back refused, and a test that panicked on
        // that would be failing the design rather than a defect.
        //
        // What clears it is a **row** read: `row_scan` meets the standing lock and resolves it,
        // which is the same mechanism the row path uses in production. So that is what happens
        // between attempts rather than a sleep — it makes the thing the next ask needs happen,
        // instead of waiting for somebody else to do it. The bound is named and every refusal is
        // carried into the message, so a copy that refuses for a *different* reason still fails
        // with all of them in front of the reader.
        const ATTEMPTS: usize = 8;
        let learner = self.learner_node();
        let region_id = learner.store.regions().find(b"t").unwrap().id();
        let mut refusals: Vec<String> = Vec::new();
        let result = loop {
            let answer = Self::ask(
                learner,
                region_id,
                tenant,
                table_id,
                ts,
                min_apply_index,
                projection.clone(),
            );
            if let FragmentResp::Result { result, .. } = answer {
                break result;
            }
            refusals.push(format!("attempt {}: {answer:?}", refusals.len() + 1));
            assert!(
                refusals.len() < ATTEMPTS,
                "the learner refused the fragment {ATTEMPTS} times, with a row read between each \
                 to resolve any lock that was standing:\n  {}",
                refusals.join("\n  ")
            );
            let _ = self.row_scan(ts, table_id, &projection);
        };
        let Body::Rows { rows, .. } = esker_proto::fragment::result::decode(&result).unwrap()
        else {
            panic!("a scan fragment came back as groups");
        };
        let mut out: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        WireValue::Int8(n) => Cell::Int8(*n),
                        WireValue::Text(text) => Cell::Text(text.clone()),
                        WireValue::Null => Cell::Null,
                        other => panic!("unexpected column {other:?}"),
                    })
                    .collect()
            })
            .collect();
        out.sort();
        out
    }

    /// The same rows, read the other way: through the row store, at the same instant.
    ///
    /// **A second implementation, not a second call.** It goes to the voters over the wire,
    /// resolves MVCC in Percolator's `write` records, and decodes with the row codec — sharing the
    /// *rule* with the columnar path and none of its code, which is what makes the comparison
    /// worth making (`docs/plans/phase-8-learner.md`, RULED-2).
    fn row_scan(&self, ts: u64, table_id: u64, projection: &[u32]) -> Vec<Vec<Cell>> {
        let (start, end) = esker_keys::row::table_row_range(TENANT, table_id);
        // **A read here can be a write, which is why it retries.** The lock-TTL test's scan meets
        // a standing lock and *resolves* it — a `TxnRollback` or a roll-forward proposed from
        // inside the read — and a proposal whose leader steps down before it commits is answered
        // `OutcomeUnknown`. That is the honest answer and the reason `settle` exists for this
        // file's writes; the scan needed the same treatment and had a one-shot `unwrap`, which
        // made "leadership does not move" a silent precondition of a test *about* a resolver.
        //
        // Re-reading is what a client does and is safe whatever the ambiguous attempt did: the
        // resolution is idempotent, and a lock already resolved by the lost attempt simply is not
        // there on the retry. Seen twice under `cargo test --workspace` and never alone, which is
        // the shape this file has now met three times.
        let deadline = Instant::now() + Duration::from_secs(30);
        let (txn, pairs) = loop {
            let txn = self.backend.begin_at(ts).unwrap();
            match txn.scan(&start, &end, 1024) {
                Ok(pairs) => break (txn, pairs),
                Err(
                    error @ (esker_sql::SqlError::OutcomeUnknown(_)
                    | esker_sql::SqlError::StoreUnavailable(_)),
                ) => {
                    assert!(
                        Instant::now() < deadline,
                        "the row scan never settled: {error}"
                    );
                    let _ = txn.rollback();
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("the row scan failed: {error}"),
            }
        };
        let schema = {
            let view = self.catalog.view(&*txn, TENANT).unwrap();
            view.table("t").unwrap().unwrap().row_schema()
        };
        let mut out: Vec<Vec<Cell>> = pairs
            .iter()
            .map(|(_, value)| {
                // The **row codec's** padding, which is the half of this differential that knows
                // what a column added after a row was written should read as: `decode_row` fills
                // a short row from the schema's missing values, so a row stored two columns wide
                // comes back three wide with the `DEFAULT` in it.
                let row = esker_sql::row::decode_row(&schema, value, None).unwrap();
                projection
                    .iter()
                    .map(|at| match &row[*at as usize] {
                        Datum::Int8(n) => Cell::Int8(*n),
                        Datum::Text(text) => Cell::Text(text.clone()),
                        Datum::Null => Cell::Null,
                        other => panic!("unexpected column {other:?}"),
                    })
                    .collect()
            })
            .collect();
        let _ = txn.rollback();
        out.sort();
        out
    }

    /// One fragment request, over a real socket, to one store.
    ///
    /// An associated function rather than a method: what it needs is a node and a region, and a
    /// gate that lent it `self` would be lending nothing.
    fn ask(
        node: &Node,
        region_id: u64,
        tenant: u64,
        table_id: u64,
        ts: u64,
        min_apply_index: u64,
        projection: Vec<u32>,
    ) -> FragmentResp {
        let region = node
            .store
            .regions()
            .find(b"t")
            .expect("the store holds the region");
        let fragment = esker_columnar::Fragment::scan(
            esker_columnar::TableRef { tenant, table_id },
            projection,
        );
        // Retried on a leadership change and dumped on anything else — see
        // [`is_a_leadership_change`]. This is the call the third sighting of the watched flake
        // landed on, and the dump is what said it was an election gap rather than the deadline.
        let request = Request::Fragment {
            header: RequestHeader::new(region_id, region.region().epoch, 0),
            request: FragmentReq {
                fragment: esker_columnar::fragment::encode(&fragment).into(),
                ts,
                min_apply_index,
            },
        };
        let answer = call_through_an_election(
            std::slice::from_ref(node),
            region_id,
            "fragment call",
            // A fragment is a scan at a fixed `ts`: repeating it cannot change what the
            // cluster holds, so its deadline is retryable where the `TxnKv` call's is not.
            Idempotent::Yes,
            |_| Some((node.address, request.clone())),
        );
        match answer {
            Response::Fragment(response) => response,
            other => panic!("a fragment request answered {other:?}"),
        }
    }

    /// One `TxnKv` request, over a real socket, to whichever store **leads** the region.
    ///
    /// The wire and not the client, because what this drives is a half-finished transaction — a
    /// prewrite whose commit never comes for one of its keys — and the client's whole job is to
    /// not leave one of those behind.
    fn txn(&self, region_id: u64, request: &TxnKvReq) -> TxnKvResp {
        // A transport failure here is **not** a bare `expect`, and it is not a bare dump either:
        // this call is what a lock-expiry test hangs off, and it has been seen to fail twice for
        // two different reasons under a fully parallel `cargo test`
        // (`docs/plans/phase-9-rails.md` §8). The leader is looked up **per attempt**, because
        // the thing being retried is precisely that it changed.
        let answer = call_through_an_election(
            &self.nodes,
            region_id,
            "call",
            // A prewrite whose commit never comes is the point of the test around this call. A
            // timed-out write has an unknown outcome, so it dumps rather than repeating.
            Idempotent::No,
            |nodes| {
                let leader = nodes.iter().find(|node| {
                    node.store
                        .peer_of(region_id)
                        .is_some_and(|peer| peer.is_leader())
                })?;
                let epoch = leader
                    .store
                    .regions()
                    .get(region_id)
                    .expect("the leader hosts the region")
                    .region()
                    .epoch;
                Some((
                    leader.address,
                    Request::txn_kv(RequestHeader::new(region_id, epoch, 0), request.clone()),
                ))
            },
        );
        answer.into_txn_kv().expect("a TxnKv answer")
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
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(
            &mut session,
            "INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger')",
        );
        assert!(
            gate.columnar_learners().is_empty(),
            "nothing has asked for a columnar copy yet",
        );

        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
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
        settle(
            &mut gate.session(),
            "ALTER TABLE t SET (columnar_replicas = 0)",
        );
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

/// Writes the cluster down and fails, for a transport call that never answered.
///
/// The same discipline as [`compare`] and for the same reason — an artifact rather than a line in a
/// scrollback — but a different question, so a different dump: what failed here is the *wire*, so
/// what has to be on file is who led the region, what every store thought it was doing, and how
/// long the deadline was.
///
/// A free function over whatever nodes the caller can see, because the two calls that need it are
/// not both methods on the gate: the fragment one is an associated function holding a single node.
/// The retry a real client already has, at the two transport calls in this file that did not.
///
/// The watched flake (`docs/plans/phase-9-rails.md` §8) was chased at its third sighting and the
/// dump said what it is: **not** the 30-second deadline, but the gap between a leader stepping
/// down on a saturated machine and the next election — no store leading, and all four peers
/// agreed at the same applied index, with the 30 seconds untouched. `esker-client` treats that as
/// a redirect and tries again (`crates/esker-client/src/retry.rs`); these two calls went to the
/// wire directly and did not.
///
/// **Only a leadership change is retried.** Anything else still writes the cluster down and
/// fails, which is what the dump was added for and what would be lost by wrapping the call in a
/// blanket retry. Both tests assert what the two engines *answer*, not that the first attempt
/// lands on a leader that is still leading when it commits — so this is the assertion being
/// written correctly rather than a workaround for it.
///
/// The address and the request are built per attempt, by the caller, because on the `TxnKv` side
/// the thing being retried is precisely that the leader moved: a closure that returns `None` is
/// saying *nothing leads the region at this instant*, which is the gap itself and not a failure.
fn call_through_an_election(
    nodes: &[Node],
    region_id: u64,
    what: &str,
    idempotent: Idempotent,
    mut address_and_request: impl FnMut(&[Node]) -> Option<(SocketAddr, Request)>,
) -> Response {
    // Eight attempts a quarter-second apart. An election on an idle cluster takes one heartbeat;
    // two seconds is a saturated machine's worth of them, and still well inside the deadline a
    // single call already gets.
    const ATTEMPTS: usize = 8;
    const PAUSE: Duration = Duration::from_millis(250);

    let mut last = "no store led the region on any attempt".to_owned();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(PAUSE);
        }
        let Some((address, request)) = address_and_request(nodes) else {
            continue;
        };
        let outcome = BlockingTransport::connect(address).and_then(|transport| {
            transport.call(request, Instant::now() + Duration::from_secs(30))
        });
        let retryable = is_a_leadership_change(&error_of(&outcome))
            || (idempotent == Idempotent::Yes && is_a_deadline(&error_of(&outcome)));
        match outcome {
            Ok(answer) => return answer,
            Err(error) if retryable => last = format!("{error}"),
            Err(error) => transport_dump(nodes, region_id, what, &format!("{error}")),
        }
    }
    transport_dump(
        nodes,
        region_id,
        what,
        &format!("{ATTEMPTS} attempts each lost the leader; the last said: {last}"),
    )
}

/// Whether a call may be repeated after a timeout, which is a property of the *request* and not of
/// the error.
///
/// A timed-out call has an **unknown outcome**: the request may have been applied and the answer
/// lost. Repeating a write on that is how a test invents a second prewrite; repeating a read is
/// free. So the two call sites in this file answer differently, and neither answers for the other
/// — the `TxnKv` one is a half-finished transaction and stays a hard failure, the fragment one is
/// a scan at a fixed `ts` and is idempotent by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Idempotent {
    /// A read. Repeating it cannot change what the cluster holds.
    Yes,
    /// A write, or anything whose outcome a lost answer leaves unknown.
    No,
}

/// The error of an outcome, or a placeholder for the `Ok` case the caller has already handled.
fn error_of(outcome: &Result<Response, ProtoError>) -> ProtoError {
    match outcome {
        Err(error) => error.clone(),
        Ok(_) => ProtoError::internal("no error"),
    }
}

/// Whether an error is the 30-second deadline expiring with no answer at all.
///
/// The **first** mechanism `docs/plans/phase-9-rails.md` §8 recorded, and a different thing from
/// the election gap: there the peer answers immediately to say it stepped down, here nothing comes
/// back. Its cause is the same saturation — this machine runs a 1,153-test suite in parallel, and
/// another lane's build beside it — but a caller cannot tell a slow server from a lost one, which
/// is why only an idempotent call may retry it.
fn is_a_deadline(error: &ProtoError) -> bool {
    matches!(error, ProtoError::Timeout { .. })
}

/// Whether an error is the region changing leader while the call was in flight.
///
/// **Two shapes from one event**, and `esker_store::peer`'s `stopped_leading` sends both: an
/// orphaned *read* is answered `NotLeader`, and a *proposal* already in the peer's log is answered
/// `Closed` with a detail saying it may still commit. The dump caught the second; matching only
/// that one would leave the other half of the same instant unretried.
///
/// `Closed` is matched on its detail because that is the only structure it has — it is the
/// protocol's general "the connection went away", and a store that was killed must still dump
/// rather than be retried. The text is `esker-store`'s, one crate away and out of this lane; if it
/// ever changes, this stops retrying and the flake comes back as a dump, which is the safe
/// direction for it to fail in.
fn is_a_leadership_change(error: &ProtoError) -> bool {
    match error {
        ProtoError::NotLeader { .. } => true,
        ProtoError::Closed { detail } => detail.contains("stopped leading"),
        _ => false,
    }
}

fn transport_dump(nodes: &[Node], region_id: u64, what: &str, error: &str) -> ! {
    let mut dump = String::new();
    let _ = writeln!(dump, "a {what} to the leader of region {region_id} failed");
    let _ = writeln!(dump, "error            {error}");
    let _ = writeln!(dump, "deadline         30s");
    dump.push_str("\n-- every store this caller can see --------------------------------\n");
    for node in nodes {
        let peer = node.store.peer_of(region_id);
        let _ = writeln!(
            dump,
            "store {}  address={}  leader={:?}  applied={:?}",
            node.store.store_id(),
            node.address,
            peer.as_ref().map(|peer| peer.is_leader()),
            peer.as_ref().map(|peer| peer.applied_index()),
        );
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("joint-gate-transport-{stamp}.txt"));
    let written = std::fs::write(&path, &dump);
    panic!(
        "a {what} to the leader of region {region_id} failed: {error}; dump {} at {}\n{dump}",
        if written.is_ok() {
            "written"
        } else {
            "NOT written"
        },
        path.display(),
    )
}

/// Compares the two engines and, on a disagreement, **writes everything down before failing**.
///
/// A silent disagreement between the row store and the columnar copy is what ADR 0022 calls the
/// worst failure this feature can have. When one appears it is very likely to be rare, and a rare
/// failure whose evidence went to a terminal that was grepped is worth almost nothing — that has
/// happened once already on this test (`docs/plans/phase-8-learner.md` §close). So the dump goes
/// to a **file**: `target/joint-gate-disagreement-<ts>.txt`, named in the panic, holding both
/// sides, the instant, the fragment's shape, what each store holds for the table, and the lock
/// column family — because the difference between "a version is missing" and "a version is
/// hidden" is the difference between a catch-up bug and an MVCC one.
fn compare(gate: &Gate, what: &Comparison) {
    if what.columns == what.rows {
        return;
    }
    let mut dump = String::new();
    dump.push_str("the columnar copy and the row store disagree\n\n");
    let _ = writeln!(dump, "ts               {}", what.ts);
    let _ = writeln!(dump, "min_apply_index  {}", what.min_apply_index);
    let _ = writeln!(dump, "table_id         {}", what.table_id);
    let _ = writeln!(dump, "projection       {:?}", what.projection);
    let _ = writeln!(dump, "region           {}", what.region_id);
    dump.push_str("\n-- the fragment answered ------------------------------------------\n");
    for row in &what.columns {
        let _ = writeln!(dump, "{row:?}");
    }
    dump.push_str("\n-- the row scan answered ------------------------------------------\n");
    for row in &what.rows {
        let _ = writeln!(dump, "{row:?}");
    }
    dump.push_str("\n-- only the fragment has ------------------------------------------\n");
    for row in what.columns.iter().filter(|row| !what.rows.contains(row)) {
        let _ = writeln!(dump, "{row:?}");
    }
    dump.push_str("\n-- only the row scan has -----------------------------------------\n");
    for row in what.rows.iter().filter(|row| !what.columns.contains(row)) {
        let _ = writeln!(dump, "{row:?}");
    }
    dump.push_str("\n-- every store ---------------------------------------------------\n");
    for node in &gate.nodes {
        let store = &node.store;
        let peer = store.peer_of(what.region_id);
        let _ = writeln!(
            dump,
            "store {}  leader={:?}  applied={:?}  columnar={}",
            store.store_id(),
            peer.as_ref().map(|peer| peer.is_leader()),
            peer.as_ref().map(|peer| peer.applied_index()),
            store
                .regions()
                .get(what.region_id)
                .is_some_and(|state| state.region().peers.iter().any(|peer| {
                    peer.store_id == store.store_id() && peer.role == PeerRole::ColumnarLearner
                })),
        );
        let _ = writeln!(dump, "  what it holds for this table:");
        for line in table_state(store, what.region_id, what.table_id, &what.ids) {
            let _ = writeln!(dump, "    {line}");
        }
    }

    let landed = write_dump(&dump, what.ts);
    panic!(
        "the columnar copy and the row store disagree at ts {}; dump {}\n{dump}",
        what.ts, landed,
    );
}

/// Writes the dump somewhere that exists, and says where.
///
/// **The first place this tried did not exist in the container the gate runs in.** It was
/// `CARGO_MANIFEST_DIR/../../target`, which is `/work/target` there — while cargo's target
/// directory is a volume mounted at `/target`, named by `CARGO_TARGET_DIR`. So the one time this
/// fired on the gate it reported `dump NOT written`, and the only reason the failure was
/// diagnosable at all is that the panic message carries the dump inline as well.
///
/// Three places, first that works: `CARGO_TARGET_DIR` when the build sets one, then the
/// manifest-relative `target` a plain `cargo test` on a host has, then the temporary directory,
/// which exists everywhere. Returning where it landed rather than whether it landed, because
/// "written" without a path is what sent somebody looking in the wrong container.
fn write_dump(dump: &str, ts: u64) -> String {
    let name = format!("joint-gate-disagreement-{ts}.txt");
    let candidates = [
        std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from),
        Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")),
        Some(std::env::temp_dir()),
    ];
    for directory in candidates.into_iter().flatten() {
        let path = directory.join(&name);
        if std::fs::write(&path, dump).is_ok() {
            return format!("written at {}", path.display());
        }
    }
    "NOT written anywhere: the dump is inline above".to_owned()
}

/// Everything one comparison is made of, so the dump can say all of it.
struct Comparison {
    ts: u64,
    min_apply_index: u64,
    table_id: u64,
    region_id: u64,
    projection: Vec<u32>,
    columns: Vec<Vec<Cell>>,
    rows: Vec<Vec<Cell>>,
    /// The primary keys the workload touched, so the dump can ask every store about each one.
    ids: Vec<i64>,
}

/// What each store holds for the table's rows, read from that store and no other.
///
/// Two facts per row and they answer different questions: the number of `write` records says
/// whether the version **is there**, and a direct read at "whatever is committed" says whether it
/// is **visible** and whether a lock is standing over it. A version missing on the learner is a
/// catch-up or a tee; a version present and not visible is MVCC; a lock is the resolution path.
///
/// Read through the store's own direct path rather than the engine's column families, because
/// this test crate does not link `esker-engine` and a dump is not worth a dependency.
fn table_state(store: &Arc<Store>, region_id: u64, table_id: u64, ids: &[i64]) -> Vec<String> {
    let Some(state) = store.regions().get(region_id) else {
        return vec!["does not host the region".to_owned()];
    };
    ids.iter()
        .map(|id| {
            let key = bytes::Bytes::from(
                esker_keys::row::row_key(TENANT, table_id, &[Datum::Int8(*id)]).expect("a row key"),
            );
            let versions = store
                .write_records(&key)
                .map_or_else(|error| format!("unreadable: {error}"), |n| n.to_string());
            let visible = match store.handle_txn(
                &state,
                TxnKvReq::Get {
                    key: key.clone(),
                    ts: u64::MAX,
                },
            ) {
                Ok(TxnKvResp::Get { value: Some(value) }) => {
                    format!("{} bytes", value.len())
                }
                Ok(TxnKvResp::Get { value: None }) => "absent".to_owned(),
                Ok(other) => format!("{other:?}"),
                Err(error) => format!("refused: {error}"),
            };
            format!("id {id}: {versions} write records, reads as {visible}")
        })
        .collect()
}

/// One of `t`'s row keys, as the SQL layer writes them.
fn row_key(table_id: u64, id: i64) -> bytes::Bytes {
    bytes::Bytes::from(
        esker_keys::row::row_key(TENANT, table_id, &[Datum::Int8(id)]).expect("a row key"),
    )
}

/// One of `t`'s row values, `(id int8, name text)`, as the SQL layer encodes them.
fn row_value(id: i64, name: &str) -> bytes::Bytes {
    bytes::Bytes::from(
        esker_keys::row::encode_row(
            &[
                esker_keys::value::ColumnType::Int8,
                esker_keys::value::ColumnType::Text,
            ],
            &[Datum::Int8(id), Datum::Text(name.to_owned())],
        )
        .expect("a row value"),
    )
}

/// Leaves `secondary` locked by a transaction whose primary **committed**: prewrite both, commit
/// the primary alone.
///
/// The state a client that died between its two steps leaves behind, and the only one in which a
/// resolver has to roll a lock *forward*. Driven over the wire because the client exists to not
/// produce it.
fn strand_a_secondary_lock(
    gate: &Gate,
    region_id: u64,
    primary: &bytes::Bytes,
    secondary: &bytes::Bytes,
    ttl_ms: u64,
) {
    let start_ts = gate.oracle.timestamp().unwrap();
    let answer = gate.txn(
        region_id,
        &TxnKvReq::Prewrite {
            start_ts,
            primary: primary.clone(),
            ttl_ms,
            mutations: vec![
                TxnMutation::Put {
                    key: primary.clone(),
                    value: row_value(4, "katherine"),
                    read_ts: None,
                },
                TxnMutation::Put {
                    key: secondary.clone(),
                    value: row_value(5, "barbara"),
                    read_ts: None,
                },
            ],
        },
    );
    assert_eq!(
        answer,
        TxnKvResp::prewrite_ok(2),
        "the prewrite this test is built on was refused",
    );

    let commit_ts = gate.oracle.timestamp().unwrap();
    let answer = gate.txn(
        region_id,
        &TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![primary.clone()],
        },
    );
    assert_eq!(
        answer,
        TxnKvResp::Commit {
            status: TxnStatus::Ok
        },
        "the primary did not commit, so there is no roll-forward to test",
    );
}

/// **The interleaving the physical oracle made possible**: a lock the TTL kills, resolved by the
/// row read, and the columnar copy asked about the same instant afterwards.
///
/// Constructed rather than waited for. `docs/plans/phase-8-learner.md` §close records the
/// differential disagreeing once, at the moment this gate's oracle stopped being a counter, and
/// names the candidate the change introduced: with physical timestamps a lock **can** now be
/// judged dead, and a reader that finds one resolves it — rolling it forward if its primary
/// committed, back if not — which under a counter could never happen at all.
///
/// The interleaving in full, driven over the wire so that no layer smooths it over:
///
/// 1. a transaction prewrites two of the table's rows and commits **only its primary**, which is
///    the state a client that died between the two steps leaves behind;
/// 2. the secondary's lock is therefore standing, over a transaction that *did* commit;
/// 3. time passes — real time, which is what the TTL is measured in — until any reader will judge
///    that lock dead;
/// 4. the row scan resolves it, which for a committed primary means **rolling it forward**: a
///    `write` record appears for a key that had none, written by a reader rather than by a writer;
/// 5. the columnar copy is asked about the same instant, after catching up to the leader.
///
/// Step 4 is the one that could go wrong silently. That record is created by `ResolveLock`, not by
/// `Commit`, and a tee that only watched commits would never see it — the learner would hold every
/// version except the ones a resolver produced, for ever, and only for transactions whose client
/// died at exactly the wrong moment. `esker_store::peer`'s `commits_of` covers it, and this is
/// what says so from outside.
/// **#86: a lock nobody has resolved yet is a row the columnar copy cannot see.**
///
/// The test below is this one with the two reads in the other order, and the order is the whole
/// difference. It resolves the lock **first**, through the row scan, and only then asks the copy —
/// so it proves that a resolver's `write` record reaches the copy. This asks the copy **while the
/// lock is still standing**, which is the state the cluster is in whenever a client's secondary
/// commit did not land: `Transaction::commit` discards that result on purpose (*"a secondary that
/// fails here is not a failed transaction … a reader that meets one of these locks will roll it
/// forward"*), so *primary committed, secondary still locked* is a normal intermediate state and
/// not a fault.
///
/// The row path is built for it. `txnkv::user_keys_in` collects **every key that is only locked**
/// beside every key with a version, and says why in the sentence this test is named after:
///
/// > The `lock` column family is the half that is easy to leave out, and leaving it out is a silent
/// > wrong answer. A key prewritten by a transaction that has since **committed its primary** has a
/// > lock and no `write` record … A scan built from versions alone answers without the row and
/// > reports no lock, so the caller has nothing to resolve and no way to notice.
///
/// **The columnar path is a scan built from versions alone.** `columnar::region::convert` walks
/// `cf::WRITE`; the tee fires on commits; nothing on either path reads `cf::LOCK`. So the copy
/// answers without the row, the row store answers with it, and nobody reports anything — which
/// `e8d31d68`'s gate saw once as `only the row scan has [Int8(4), Text("barbara"), …]`.
///
/// A learner cannot resolve a lock: resolution is a write and it is not a voter. So the answer is
/// to **refuse** and let the planner fall back to the row path, which resolves it and answers — and
/// a later fragment then succeeds, which is exactly what `RefusalReason::TooFarBehind` promises
/// ("the same node may succeed later"). Answering short is the one thing it must not do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fragment_refuses_while_a_committed_transaction_is_still_locked() {
    let gate = Gate::start().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(
            &mut session,
            "INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger')",
        );
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;

    let table_id = tokio::task::block_in_place(|| gate.table_id("t"));
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let primary = row_key(table_id, 4);
    let secondary = row_key(table_id, 5);

    // Primary committed, secondary left locked — and past its TTL, so the lock is *resolvable*.
    // That matters: a lock still inside its lease is one the row path waits on rather than rolls
    // forward, and then neither engine answers and there is nothing to disagree about.
    let ttl_ms = 300;
    tokio::task::block_in_place(|| {
        strand_a_secondary_lock(&gate, region_id, &primary, &secondary, ttl_ms);
    });
    tokio::time::sleep(Duration::from_millis(ttl_ms * 4)).await;

    let ts = gate.oracle.timestamp().unwrap();
    let min_apply_index = gate
        .nodes
        .iter()
        .filter_map(|node| node.store.peer_of(region_id))
        .filter(|peer| peer.is_leader())
        .map(|peer| peer.applied_index())
        .max()
        .expect("some store leads the region");

    // **The copy first, while the lock still stands.** Asked through `Gate::ask` and not
    // `Gate::fragment`, because a refusal is the answer this is about and `fragment` panics on one.
    let answer = tokio::task::block_in_place(|| {
        Gate::ask(
            gate.learner_node(),
            region_id,
            TENANT,
            table_id,
            ts,
            min_apply_index,
            vec![0, 1],
        )
    });

    // And the row path at the same instant, so the row the copy could not see is on the record.
    let rows = tokio::task::block_in_place(|| gate.row_scan(ts, table_id, &[0, 1]));
    let rolled_forward = rows.contains(&vec![Cell::Int8(5), Cell::Text("barbara".to_owned())]);
    assert!(
        rolled_forward,
        "the row scan did not roll the stranded lock forward, so the state this test is about was          never reached: {rows:?}"
    );

    match answer {
        FragmentResp::Refused { reason, detail } => {
            assert_eq!(
                reason,
                RefusalReason::TooFarBehind,
                "a lock this learner cannot resolve is something it may answer later, not                  something it can never answer: {detail}"
            );
        }
        FragmentResp::Result { result, .. } => {
            let Body::Rows { rows: answered, .. } =
                esker_proto::fragment::result::decode(&result).unwrap()
            else {
                panic!("a scan fragment came back as groups");
            };
            panic!(
                "the copy answered with {} rows while a committed transaction's secondary was                  still locked, so it answered without a row the row store has — the silent                  disagreement ADR 0022 calls the worst failure this feature can have. The row                  scan, at the same instant, answered {rows:?}",
                answered.len()
            );
        }
    }

    gate.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines() {
    let gate = Gate::start().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(
            &mut session,
            "INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger')",
        );
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;

    let table_id = tokio::task::block_in_place(|| gate.table_id("t"));
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let primary = row_key(table_id, 4);
    let secondary = row_key(table_id, 5);

    // 1 and 2. A short TTL because the wait below is real seconds and this test has no reason to
    // spend three of them.
    let ttl_ms = 300;
    tokio::task::block_in_place(|| {
        strand_a_secondary_lock(&gate, region_id, &primary, &secondary, ttl_ms);
    });

    // 3. Past the TTL, in the only units a TTL has. `is_expired` compares the *physical* halves of
    // two oracle timestamps, so this is the wait that a counting oracle could not express — which
    // is the whole reason this test exists.
    tokio::time::sleep(Duration::from_millis(ttl_ms * 4)).await;

    // 4. The row scan, at an instant after the commit. It meets the standing lock, resolves it
    // against a primary that committed, and rolls it forward.
    let ts = gate.oracle.timestamp().unwrap();
    let projection = vec![0, 1];
    let rows = tokio::task::block_in_place(|| gate.row_scan(ts, table_id, &projection));
    assert!(
        rows.contains(&vec![Cell::Int8(5), Cell::Text("barbara".to_owned())]),
        "the row store did not roll the secondary forward, so the case this test is about did \
         not happen: {rows:?}",
    );

    // 5. And the columnar copy, once it has applied at least as much as the leader — which now
    // includes whatever the resolution proposed.
    let min_apply_index = gate
        .nodes
        .iter()
        .filter_map(|node| node.store.peer_of(region_id))
        .filter(|peer| peer.is_leader())
        .map(|peer| peer.applied_index())
        .max()
        .expect("some store leads the region");
    let columns = tokio::task::block_in_place(|| {
        gate.fragment(TENANT, table_id, ts, min_apply_index, projection.clone())
    });

    compare(
        &gate,
        &Comparison {
            ts,
            min_apply_index,
            table_id,
            region_id,
            projection,
            columns,
            rows,
            ids: vec![1, 2, 3, 4, 5],
        },
    );

    gate.stop().await;
}

/// **The differential, on a live cluster.** Fragments and rows agree at one timestamp.
///
/// ADR 0022 names the two engines disagreeing as the worst failure this feature can have,
/// *"because it is silent"*. `esker-store`'s own differential defends the apply target against a
/// reference written longhand; this defends the whole path — placement, the apply tee, the
/// conversion, the fragment service, the wire — against **the row store**, which is the only
/// reference a user can tell the difference from.
///
/// Two implementations of one rule, which is what makes it a differential rather than a
/// tautology: the fragment resolves visibility in `esker-columnar`'s evaluator over sorted runs on
/// a learner, and the reference reads the same instant through Percolator's `write` records on a
/// voter and decodes with the row codec. The workload has an update and a delete in it precisely
/// so that "every version" and "the visible version" are different answers.
///
/// # The corpus, and the one case that hides
///
/// An update and a delete separate "every version" from "the visible version", and that is not
/// the hardest thing here. The case `docs/plans/phase-8-learner.md` §store names is
/// **`ADD COLUMN` with a non-`NULL` default over rows written before it**: the rows on disk are
/// two columns wide and the table is three, so the answer for the third column of an old row is
/// the column's *missing value* — `attmissingval`, PostgreSQL 11's trick, which is why the
/// `ALTER` is instant on both sides. The row codec pads from the schema; the columnar decoder has
/// to be built from `Published`'s `(type, missing)` pairs and not from the types alone, or it
/// pads with `NULL` and the two engines disagree about a row nobody has touched since.
///
/// So the workload ends with an `ADD COLUMN ... NOT NULL DEFAULT`, one row inserted after it at
/// the new width, and one older row rewritten — three widths of row alive at the timestamp this
/// reads at.
/// **The half that hides: a copy opened over rows nothing touches again.**
///
/// #86. `the_learner_answers_fragments_that_agree_with_a_row_scan` below writes four rows *before*
/// the flag and then rewrites or deletes three of them after it, so three of the four reach the copy
/// through the live tee whatever the conversion at open does. On `e8d31d68`'s gate the fragment came
/// back without **`id 4`** — the one row of the six that nothing touched after the flag, so the only
/// row whose sole possible source was the conversion. Every row the answer *did* hold was one the
/// stream had supplied. Read that way the dump says the conversion produced nothing and one row
/// happened to notice; the same run passed three times over when re-run, because whether the
/// conversion is asked before or after the stream is a race.
///
/// So this is that test with the stream taken away: **every row is written before the flag and not a
/// byte after it**, which leaves the conversion as the only thing that can put a row in the copy. A
/// fragment that agrees here is a conversion that ran and covered the region; one that comes back
/// short — or refused, which `Gate::fragment` panics on rather than reading as agreement — is the
/// conversion, with nothing else to blame.
///
/// It is deliberately *not* a construction of the race. If this is green the conversion is right on
/// its own and the defect is in the interleaving, which needs a different instrument: holding a
/// fragment between "the entry is committed" and "its batch is written", and again between "the
/// batch is written" and "the tee has run".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_opened_over_rows_nothing_rewrites_holds_all_of_them() {
    let gate = Gate::start().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(
            &mut session,
            "INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger'), (4, 'barbara')",
        );
        // **The flag last, and nothing after it.** The copy therefore has history to convert and no
        // stream to hide behind: a row in the answer can only have come from the walk at open.
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
    });

    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;

    let table_id = tokio::task::block_in_place(|| gate.table_id("t"));
    let ts = gate.oracle.timestamp().unwrap();
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let min_apply_index = gate
        .nodes
        .iter()
        .filter_map(|node| node.store.peer_of(region_id))
        .filter(|peer| peer.is_leader())
        .map(|peer| peer.applied_index())
        .max()
        .expect("some store leads the region");

    let projection = vec![0, 1];
    let columns = tokio::task::block_in_place(|| {
        gate.fragment(TENANT, table_id, ts, min_apply_index, projection.clone())
    });
    let rows = tokio::task::block_in_place(|| gate.row_scan(ts, table_id, &projection));

    compare(
        &gate,
        &Comparison {
            ts,
            min_apply_index,
            table_id,
            region_id,
            projection,
            columns: columns.clone(),
            rows: rows.clone(),
            ids: vec![1, 2, 3, 4],
        },
    );
    // **And the count, said separately.** `compare` reports a disagreement between the two engines;
    // this says the workload happened at all, so a run in which both sides answered nothing cannot
    // pass as agreement.
    assert_eq!(
        columns.len(),
        4,
        "the conversion at open put {} of four rows in the copy: {columns:?}",
        columns.len()
    );

    gate.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_learner_answers_fragments_that_agree_with_a_row_scan() {
    let gate = Gate::start().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(
            &mut session,
            "INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger'), (4, 'barbara')",
        );
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
        // After the flag, so the copy has history to convert *and* a stream to follow.
        settle(
            &mut session,
            "UPDATE t SET name = 'ada lovelace' WHERE id = 1",
        );
        settle(&mut session, "DELETE FROM t WHERE id = 3");
        settle(&mut session, "INSERT INTO t VALUES (5, 'grace hopper')");
        // The column that was not there when rows 1, 2, 4 and 5 were written. Nothing is
        // rewritten by it — that is the feature — so what those rows read for it comes from the
        // catalog rather than from their bytes.
        settle(
            &mut session,
            "ALTER TABLE t ADD COLUMN region text NOT NULL DEFAULT 'unknown'",
        );
        // One row born at the new width, and one older row rewritten to it, so the copy holds all
        // three shapes at once.
        settle(
            &mut session,
            "INSERT INTO t VALUES (6, 'katherine', 'west')",
        );
        settle(&mut session, "UPDATE t SET region = 'east' WHERE id = 2");
    });

    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;

    let table_id = tokio::task::block_in_place(|| gate.table_id("t"));
    // One instant, read two ways. Taken after the writes, so both sides see all of them.
    let ts = gate.oracle.timestamp().unwrap();
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let projection = vec![0, 1, 2];

    // **The row scan first, and the order is load-bearing** (#86). A read on the row path
    // *resolves* what it meets: a committed transaction whose secondary is still locked is rolled
    // forward by it, and a fragment asked before that has no version for such a key — it refuses
    // now and answered without the row before, which is how `e8d31d68`'s gate lost `id 4`. Asking
    // the copy *while* a lock stands is
    // `a_fragment_refuses_while_a_committed_transaction_is_still_locked`'s question, not this
    // one's; the sibling test below takes the same care and for the same reason.
    let rows = tokio::task::block_in_place(|| gate.row_scan(ts, table_id, &projection));

    // The leader's apply index, so the learner has to **catch up** before it may answer — the half
    // of Decision 4 that a fragment at `min_apply_index = 0` would never exercise. Sampled after
    // the scan, so it includes whatever that scan's resolutions proposed.
    let min_apply_index = gate
        .nodes
        .iter()
        .filter_map(|node| node.store.peer_of(region_id))
        .filter(|peer| peer.is_leader())
        .map(|peer| peer.applied_index())
        .max()
        .expect("some store leads the region");
    let columns = tokio::task::block_in_place(|| {
        gate.fragment(TENANT, table_id, ts, min_apply_index, projection.clone())
    });

    compare(
        &gate,
        &Comparison {
            ts,
            min_apply_index,
            table_id,
            region_id,
            projection,
            columns,
            rows: rows.clone(),
            ids: vec![1, 2, 3, 4, 5, 6],
        },
    );
    let text = |value: &str| Cell::Text(value.to_owned());
    assert_eq!(
        rows,
        vec![
            // Written before `ADD COLUMN`, never touched since: the default, not NULL.
            vec![Cell::Int8(1), text("ada lovelace"), text("unknown")],
            // Rewritten after it.
            vec![Cell::Int8(2), text("grace"), text("east")],
            vec![Cell::Int8(4), text("barbara"), text("unknown")],
            vec![Cell::Int8(5), text("grace hopper"), text("unknown")],
            // Born at the new width.
            vec![Cell::Int8(6), text("katherine"), text("west")],
        ],
        "the reference itself is wrong, so the agreement above means nothing",
    );

    gate.stop().await;
}

/// A fragment a replica cannot honour is **refused**, and refused for the right reason.
///
/// Both are normal answers meaning "fall back to a row scan" (ADR 0022 Decision 4), and telling
/// them apart is what lets a planner know whether asking another replica would help.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fragment_is_refused_by_a_voter_and_by_a_learner_that_is_behind() {
    let gate = Gate::start().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(&mut session, "INSERT INTO t VALUES (1, 'ada')");
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
    });
    wait_for("PD to place a columnar learner", 60, || {
        gate.columnar_learners().len() == 1
    })
    .await;
    wait_for("the store to build the learner", 60, || {
        gate.learner_node().store.regions().find(b"t").is_some()
    })
    .await;
    let table_id = tokio::task::block_in_place(|| gate.table_id("t"));
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();

    // A voter holds rows, and says so rather than answering from something it does not have.
    let voter = gate
        .nodes
        .iter()
        .find(|node| {
            node.store.store_id() != gate.learner_node().store.store_id()
                && node.store.regions().find(b"t").is_some()
        })
        .expect("three voters hold the region");
    let refusal = tokio::task::block_in_place(|| {
        Gate::ask(voter, region_id, TENANT, table_id, 1, 0, vec![0, 1])
    });
    assert!(
        matches!(
            refusal,
            FragmentResp::Refused {
                reason: RefusalReason::NotColumnar,
                ..
            }
        ),
        "a voter answered {refusal:?}",
    );

    // And a bound this replica cannot reach is `TooFarBehind` — a different replica may be closer,
    // which is what the planner does with it.
    let unreachable = u64::from(u32::MAX);
    let behind = tokio::task::block_in_place(|| {
        Gate::ask(
            gate.learner_node(),
            region_id,
            TENANT,
            table_id,
            1,
            unreachable,
            vec![0, 1],
        )
    });
    assert!(
        matches!(
            behind,
            FragmentResp::Refused {
                reason: RefusalReason::TooFarBehind,
                ..
            }
        ),
        "a learner asked for an index it cannot have answered {behind:?}",
    );

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
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(&mut session, "INSERT INTO t VALUES (1, 'ada')");
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
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
        settle(
            &mut gate.session(),
            "INSERT INTO t VALUES (2, 'grace'), (3, 'edsger')",
        );
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

/// Runs one of this file's statements, retrying an answer that says the outcome is genuinely
/// unknown.
///
/// **Every write in this file goes through here**, because leadership moves under all five tests
/// and for three different reasons: `Balance::TransferLeader` moves it by design in the balancing
/// one, another kills a store outright, and a saturated box elects on its own. A write proposed
/// into a log whose leader then steps down is answered `OutcomeUnknown` — the honest answer, and
/// one the **client** may not turn into a retry on the caller's behalf, because a write that
/// *may* have applied is not a write that may be repeated (`esker_store`'s pending-proposal rule).
///
/// A single writer of these particular statements may, which is the whole argument for this
/// function and the reason it is not a general helper. Every statement here is idempotent under a
/// second apply: `CREATE TABLE` answers `DuplicateTable`, a single-row `INSERT` answers
/// `UniqueViolation`, `ADD COLUMN` answers `DuplicateColumn`, and the `UPDATE`s, the `DELETE` and
/// the `ALTER ... SET` write the same value again. Each of those means the ambiguous attempt had
/// in fact applied; anything else fails the test where it stands.
///
/// Found by `cargo test --workspace` rather than by a run of this file: alone it passes, and under
/// the whole suite the box is saturated enough for a transfer and a statement to land together.
/// Which is the shape worth remembering — a one-shot `unwrap` on a write makes "leadership does
/// not move" a silent precondition, and the tests it is most false for are the ones about
/// leadership moving.
fn settle(session: &mut Session, sql: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match session.run(sql) {
            // Applied — or applied by an attempt whose answer was lost, which is what a duplicate
            // says to a retry of an idempotent statement. One arm because they are one outcome:
            // the statement's effect is in the database either way.
            Ok(_)
            | Err(
                esker_sql::SqlError::DuplicateTable(_)
                | esker_sql::SqlError::DuplicateColumn(_)
                | esker_sql::SqlError::DuplicateColumnInRelation { .. }
                | esker_sql::SqlError::UniqueViolation { .. },
            ) => return,
            // A retry can collide with **its own** first attempt: an ambiguous write left a
            // Percolator lock behind, and until the transaction that owns it resolves — by
            // committing, or by its TTL expiring — a second attempt at the same rows cannot
            // clear it and says so. Waiting is the answer, and it is the same answer a client
            // gives a write conflict.
            Err(
                error @ (esker_sql::SqlError::OutcomeUnknown(_)
                | esker_sql::SqlError::StoreUnavailable(_)
                | esker_sql::SqlError::SerializationFailure { .. }),
            ) => {
                assert!(Instant::now() < deadline, "`{sql}` never settled: {error}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("`{sql}`: {error}"),
        }
    }
}

/// A columnar learner must not cost the region a voter.
///
/// Handed across a lane boundary as a **red test** rather than a report — un-`ignore` it and it
/// either passes or says exactly what is still wrong — and green since `571b2b0`. The only test
/// here that runs with PD's balancer on, which is what it is for.
///
/// `balance::region_balance` asked `region.peers.len() > cluster.target_replicas`, over **every**
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
async fn a_columnar_learner_does_not_cost_the_region_a_voter() {
    let gate = Gate::start_balancing().await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, name text)",
        );
        settle(&mut session, "INSERT INTO t VALUES (1, 'ada')");
        settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
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
