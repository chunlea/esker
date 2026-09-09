//! **A SQL table that spans several regions, on a real cluster, through the row path.**
//!
//! `docs/plans/split-region.md` §11 named this and left it: *"What no test here exercises is a SQL
//! query against a table that spans several regions — the client's region-cache refresh on
//! `EpochNotMatch`, `esker-sql`'s scan across two regions"*. Everything that existed was one of
//! three things and none of them is this: `fragment_route_repair.rs` splits a `MemoryBackend`,
//! `tests/cluster` is a real three-store cluster whose key space is carved at the **namespace**
//! bytes so every table still sits inside one region, and phase 8 proved the *columnar* arm.
//!
//! # How the table is made to span regions
//!
//! Nothing here places a boundary. The stores run with a real `esker-pd` and an 8 KiB split
//! threshold, rows are inserted through SQL until PD reports the region has split, and **the store
//! chooses where**. That is the point: a boundary this test picked would be a boundary this test
//! understands, and the thing under proof is a scan that meets one it does not.
//!
//! The client routes through `PdConn`, which is the production resolver — the same one the binary
//! builds — so the refresh after `EpochNotMatch` is the real path and not a harness that always
//! knows the answer.
//!
//! # How a wrong answer is recognised
//!
//! Every face is asked twice over the **same rows**: once while the table is in one region and once
//! after it spans several, and the two answers must be identical. A face that can only say "this
//! looks plausible" cannot see a row served twice or a row lost at a boundary; two answers that
//! must match can, and the single-region run is the control that says the query itself is right.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::RegionResolver;
use esker_client::router::{ClientOptions, Router};
use esker_client::{CountingOracle, TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{Server, ServerHandle, Service, TransportConfig};
use esker_sql::backend::{Backend, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pd::PdConn;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, RemotePd, SplitOptions, Store, StoreOptions, StoreService};

use cluster::{Session, TENANT};

/// One store is enough to hold a table that splits, and one keeps the test's cost to the thing it
/// is about. Replication is proved elsewhere; what is proved here is routing across a boundary.
const STORES: usize = 1;

/// Small enough that a few hundred SQL rows cross it. The same figure `esker-store`'s own split
/// tests use.
const SPLIT_SIZE: u64 = 8 * 1024;

/// How many rows the table is grown to. Chosen by measurement, not by taste: below this the region
/// does not reach the threshold, and the assertions below fail loudly rather than passing on a
/// table that never split.
const ROWS: i64 = 900;

fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A real placement driver, real stores that split, and a SQL node routed through `PdConn`.
struct Splitting {
    pd: Arc<Pd>,
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    _runtime: tokio::runtime::Runtime,
}

impl Splitting {
    fn start(split_size: u64) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let (pd, backend, catalog, handles, dirs) = runtime.block_on(Self::open(split_size));
        Splitting {
            pd,
            backend,
            catalog,
            _handles: handles,
            _dirs: dirs,
            _runtime: runtime,
        }
    }

    #[allow(clippy::type_complexity)]
    async fn open(
        split_size: u64,
    ) -> (
        Arc<Pd>,
        Arc<dyn Backend>,
        Arc<Catalog>,
        Vec<ServerHandle>,
        Vec<tempfile::TempDir>,
    ) {
        let pd_listener = reserve();
        let pd_address: SocketAddr = pd_listener.local_addr().unwrap();
        let listeners: Vec<std::net::TcpListener> = (0..STORES).map(|_| reserve()).collect();
        let addresses: Vec<SocketAddr> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
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
                target_replicas: 1,
                balance: false,
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

        let mut handles = vec![pd_handle];
        let mut dirs = vec![pd_dir];
        drop(listeners);
        for (at, address) in addresses.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let mut raft = RaftOptions::new(peers.clone(), 20_260_908);
            raft.tick = Duration::from_millis(5);
            let store = Store::open(
                dir.path(),
                StoreOptions {
                    store_id: at as u64 + 1,
                    peer_id: at as u64 + 1,
                    region_id: at as u64 + 1,
                    raft: Some(raft),
                    pd: Some(Arc::new(RemotePd::connect(pd_address).unwrap())),
                    address: address.to_string(),
                    heartbeat_tick: Duration::from_millis(5),
                    store_heartbeat: Duration::from_millis(20),
                    region_heartbeat: Duration::from_millis(20),
                    // **Splitting needs a placement driver**, whatever this says: PD is what hands
                    // out the child's cluster-unique id.
                    split: SplitOptions {
                        region_split_size: split_size,
                        max_sampled_keys: 1024,
                    },
                    ..StoreOptions::new()
                },
            )
            .unwrap();
            handles.push(
                Server::bind(
                    *address,
                    StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
                    TransportConfig::new(),
                )
                .await
                .unwrap()
                .spawn()
                .unwrap(),
            );
            dirs.push(dir);
        }

        let stores = tokio::task::spawn_blocking(move || {
            TcpStores::connect_all(&addresses, TransportConfig::new())
        })
        .await
        .unwrap()
        .unwrap();
        // **`PdConn`, not a fixed table.** A fixed route cannot describe a cluster that splits: the
        // child regions have ids this test never sees and an epoch that moves under it, and the
        // whole question is what the client does when the store refuses its stale route.
        let conn = Arc::new(PdConn::new(pd_address));
        let router = Router::with_options(
            Arc::new(stores),
            Arc::clone(&conn) as Arc<dyn RegionResolver>,
            ClientOptions {
                jitter_seed: Some(11),
                ..ClientOptions::default()
            },
        );
        let oracle: Arc<dyn TimestampOracle> = Arc::new(CountingOracle::starting_at(1_000));
        let client = Arc::new(TxnClient::on_router(Arc::new(router), Arc::clone(&oracle)));
        let backend: Arc<dyn Backend> = Arc::new(StoreBackend::new(client, oracle));
        (pd, backend, Arc::new(Catalog::new()), handles, dirs)
    }

    fn session(&self) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                esker_sql::session::register(),
            ),
        }
    }

    /// How many regions PD knows about — the number the client routes by.
    fn regions(&self) -> usize {
        self.pd.regions().map_or(0, |regions| regions.len())
    }
}

/// **The same rows, once in one region and once spread over several, must answer identically.**
///
/// Two clusters rather than one grown in place, and the reason is a measurement: with an 8 KiB
/// threshold the region has already split **five** ways by the hundredth row, so there is no window
/// in which a real cluster holds a useful table in one region. The control is therefore a cluster
/// whose threshold is `u64::MAX` — one that cannot split — carrying the same DDL and the same rows.
/// That is a better control anyway: it differs from the other in exactly one setting.
#[test]
fn a_table_that_spans_regions_answers_what_one_region_answered() {
    let one = Splitting::start(u64::MAX);
    let many = Splitting::start(SPLIT_SIZE);

    let mut control = one.session();
    let mut spread = many.session();
    for session in [&mut control, &mut spread] {
        load(session);
    }

    assert_eq!(one.regions(), 1, "the control cluster split after all");
    wait_for(
        "the table's region to split at least three ways",
        60,
        || many.regions() >= 3,
    );
    let regions = many.regions();
    assert!(regions >= 3, "the table spans only {regions} region(s)");

    let expected = faces(&mut control);
    let actual = faces(&mut spread);
    assert_eq!(
        expected.len(),
        actual.len(),
        "the two runs asked different questions"
    );
    for (want, got) in expected.iter().zip(&actual) {
        assert_eq!(
            want.0, got.0,
            "the faces are out of order, which makes every comparison below meaningless"
        );
        assert_eq!(
            want.1, got.1,
            "`{}` answered differently over {regions} regions than over one",
            want.0
        );
    }
}

/// The same table and the same rows on either side.
fn load(session: &mut Session) {
    session
        .run("CREATE TABLE ledger (id int8 PRIMARY KEY, who text, amount int8)")
        .unwrap();
    session
        .run("CREATE INDEX ledger_who ON ledger (who)")
        .unwrap();
    for id in 1..=ROWS {
        session
            .run(&format!(
                "INSERT INTO ledger VALUES ({id}, 'who-{}', {})",
                id % 7,
                id * 3
            ))
            .unwrap();
    }
}

/// Every face, as text, so that a difference is a diff and not a guess.
fn faces(session: &mut Session) -> Vec<(String, Vec<Vec<Option<String>>>)> {
    let upto = ROWS;
    let queries = [
        (
            "full scan",
            format!("SELECT id, who, amount FROM ledger WHERE id <= {upto} ORDER BY id"),
        ),
        (
            "count",
            format!("SELECT count(*) FROM ledger WHERE id <= {upto}"),
        ),
        (
            "point read, low",
            "SELECT id, amount FROM ledger WHERE id = 1".to_owned(),
        ),
        (
            "point read, high",
            format!("SELECT id, amount FROM ledger WHERE id = {upto}"),
        ),
        (
            "range, order, limit",
            format!("SELECT id FROM ledger WHERE id BETWEEN 40 AND {upto} ORDER BY id LIMIT 25"),
        ),
        (
            "secondary index",
            "SELECT id FROM ledger WHERE who = 'who-3' AND id <= 100 ORDER BY id".to_owned(),
        ),
        (
            "aggregate by group",
            format!(
                "SELECT who, count(*), sum(amount) FROM ledger WHERE id <= {upto} GROUP BY who ORDER BY who"
            ),
        ),
    ];
    queries
        .into_iter()
        .map(|(name, sql)| (name.to_owned(), session.rows(&sql)))
        .collect()
}
