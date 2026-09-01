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
use esker_client::{TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::fragment::result::{Body, Value as WireValue};
use esker_proto::fragment::{FragmentReq, FragmentResp, RefusalReason};
use esker_proto::{
    BlockingTransport, PeerRole, Request, RequestHeader, Response, Server, ServerHandle, Service,
    TransportConfig,
};
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
/// could not be cleared`, for thirty seconds.
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
    fn tso(&self, count: u32) -> Result<u64, esker_proto::ProtoError> {
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
            executor: Executor::new(Arc::clone(&self.backend), Arc::clone(&self.catalog), TENANT)
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
        let learner = self.learner_node();
        let region_id = learner.store.regions().find(b"t").unwrap().id();
        let answer = Self::ask(
            learner,
            region_id,
            tenant,
            table_id,
            ts,
            min_apply_index,
            projection,
        );
        let FragmentResp::Result { result, .. } = answer else {
            panic!("the learner refused the fragment: {answer:?}");
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
        let txn = self.backend.begin_at(ts).unwrap();
        let (start, end) = esker_keys::row::table_row_range(TENANT, table_id);
        let pairs = txn.scan(&start, &end, 1024).unwrap();
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
                let row = esker_sql::row::decode_row(&schema, value).unwrap();
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
        let transport = BlockingTransport::connect(node.address).unwrap();
        let answer = transport
            .call(
                Request::Fragment {
                    header: RequestHeader::new(region_id, region.region().epoch, 0),
                    request: FragmentReq {
                        fragment: esker_columnar::fragment::encode(&fragment).into(),
                        ts,
                        min_apply_index,
                    },
                },
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap();
        match answer {
            Response::Fragment(response) => response,
            other => panic!("a fragment request answered {other:?}"),
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
    // The leader's apply index, so the learner has to **catch up** before it may answer — the
    // half of Decision 4 that a fragment at `min_apply_index = 0` would never exercise.
    let region_id = gate.learner_node().store.regions().find(b"t").unwrap().id();
    let min_apply_index = gate
        .nodes
        .iter()
        .filter_map(|node| node.store.peer_of(region_id))
        .filter(|peer| peer.is_leader())
        .map(|peer| peer.applied_index())
        .max()
        .expect("some store leads the region");

    let projection = vec![0, 1, 2];
    let columns = tokio::task::block_in_place(|| {
        gate.fragment(TENANT, table_id, ts, min_apply_index, projection.clone())
    });
    let rows = tokio::task::block_in_place(|| gate.row_scan(ts, table_id, &projection));

    assert_eq!(
        columns, rows,
        "the columnar copy and the row store disagree at ts {ts}",
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
