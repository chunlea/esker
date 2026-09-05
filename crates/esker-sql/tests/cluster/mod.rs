//! A real cluster for the SQL node to run against: three stores, three regions, real sockets.
//!
//! Every other test in this crate runs the executor over `MemoryBackend`, which is a real little
//! MVCC store and is still one process. What that cannot show is the wiring: whether a range this
//! crate walks actually spans the stores that hold it, whether a transaction whose keys land in
//! three different regions commits through Percolator's two phases, and whether a conflict comes
//! back as one.
//!
//! # Where the boundaries are, and why there
//!
//! The key space is divided so that a single ordinary statement has to cross it. Esker's SQL keys
//! are `'m' ++ …` for the catalog and `'t' ++ tenant ++ table_id ++ …` for rows and index entries
//! (`esker_keys::prefix`), so:
//!
//! ```text
//! region 1   [ , 'm')    the relation-id and row-id counters live under 'm' too, but the
//!                        interesting thing about this region is that it is *not* where the rows
//!                        are: every statement reads the catalog here and its rows elsewhere
//! region 2   ['m', 't')  the catalog: table records, names, the version counter
//! region 3   ['t', )     every row and every index entry
//! ```
//!
//! So **every statement is already a cross-region transaction**: `CREATE TABLE` writes the catalog
//! in region 2 and bumps a counter there; an `INSERT` reads the catalog from region 2 and writes
//! its row to region 3, and Percolator has to pick a primary in one of them and commit the
//! secondaries in the other. That is the shape this file exists to exercise, and it is why the
//! split is at the namespace bytes rather than somewhere arbitrary.

#![allow(
    dead_code,
    reason = "shared by several test binaries; each uses a subset"
)]
#![allow(
    unreachable_pub,
    reason = "a test-only module: `pub` is what makes it reachable from the binaries that include it"
)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::{CountingOracle, TcpStores, TimestampOracle, TxnClient};
use esker_proto::{Epoch, Peer, Region, ServerHandle, TransportConfig};
use esker_sql::backend::{Backend, SchemaLease, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::StatementClass;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_store::{Store, StoreOptions, StoreService};

/// The tenant every session here is served as.
pub const TENANT: u64 = 1;

/// Three stores, the routing that divides the key space between them, and a SQL node over it.
pub struct Cluster {
    pub backend: Arc<dyn Backend>,
    pub catalog: Arc<Catalog>,
    /// The client the default backend is built over, so a test can build a second backend of its
    /// own — one holding a schema lease, say — against the same three stores.
    pub client: Arc<TxnClient>,
    /// The oracle that client allocates from. Shared, because two backends over one cluster that
    /// numbered their transactions independently would not be one cluster.
    pub oracle: Arc<dyn TimestampOracle>,
    /// Where the three stores listen, so a test can build a second client of its own.
    pub addresses: Vec<std::net::SocketAddr>,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    /// The runtime the stores were started on, when this cluster owns one. `None` when the caller
    /// was already inside a runtime and lent us theirs — a runtime built inside a runtime panics,
    /// which is why [`Cluster::start_on_this_runtime`] exists at all.
    runtime: Option<tokio::runtime::Runtime>,
}

async fn start_store(id: u64) -> (ServerHandle, tempfile::TempDir) {
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
    let handle = esker_proto::transport::Server::bind(
        "127.0.0.1:0",
        StoreService::new(store),
        TransportConfig::new(),
    )
    .await
    .expect("the server binds")
    .spawn()
    .expect("the server starts");
    (handle, dir)
}

fn route(id: u64, start: &[u8], end: &[u8]) -> Route {
    Route {
        region: Region {
            id,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(id, id)],
            epoch: Epoch::INITIAL,
        },
        leader: Some(Peer::voter(id, id)),
    }
}

impl Cluster {
    /// Starts three stores on a runtime of its own, for a synchronous test.
    pub fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut cluster = runtime.block_on(Cluster::start_on_this_runtime());
        cluster.runtime = Some(runtime);
        cluster
    }

    /// The same, on the caller's runtime — for a test that is already inside one, where building
    /// a second would panic.
    pub async fn start_on_this_runtime() -> Self {
        let mut started = Vec::new();
        for id in 1..=3 {
            started.push(start_store(id).await);
        }
        let addresses: Vec<_> = started
            .iter()
            .map(|(handle, _)| handle.local_addr())
            .collect();
        // `connect_all` is synchronous and blocks, so it cannot run on a runtime thread -- which
        // is where this function is. `spawn_blocking` is the seam for exactly that, and it is the
        // same one the SQL node uses to run the executor (`CLAUDE.md`: async only at the network
        // edge).
        let stores = tokio::task::spawn_blocking(move || {
            TcpStores::connect_all(&addresses, TransportConfig::new())
        })
        .await
        .expect("the connect task runs")
        .expect("the client connects to all three");
        assert_eq!(stores.store_ids(), vec![1, 2, 3]);

        let resolver: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([
            route(1, b"", b"m"),
            route(2, b"m", b"t"),
            route(3, b"t", b""),
        ]));
        let router = Router::with_options(
            Arc::new(stores),
            resolver,
            ClientOptions {
                jitter_seed: Some(7),
                ..ClientOptions::default()
            },
        );
        // Starting well above zero so that a timestamp is never mistaken for an absent one.
        let oracle: Arc<dyn TimestampOracle> = Arc::new(CountingOracle::starting_at(1_000));
        let client = Arc::new(TxnClient::on_router(Arc::new(router), Arc::clone(&oracle)));

        let (handles, dirs): (Vec<_>, Vec<_>) = started.into_iter().unzip();
        let addresses = handles.iter().map(ServerHandle::local_addr).collect();
        Cluster {
            backend: Arc::new(StoreBackend::new(Arc::clone(&client), Arc::clone(&oracle))),
            catalog: Arc::new(Catalog::new()),
            client,
            oracle,
            addresses,
            _handles: handles,
            _dirs: dirs,
            runtime: None,
        }
    }

    /// A session on this node. Sessions share the store and the catalog cache, as they do in the
    /// real binary.
    pub fn session(&self) -> Session {
        Session {
            executor: Executor::new(Arc::clone(&self.backend), Arc::clone(&self.catalog), TENANT),
        }
    }

    /// A second backend over the same three stores, holding `lease`.
    ///
    /// What the real binary builds when it is given `--pd`: the same client and the same oracle,
    /// with a lease source attached — so a test can take the lease away from a node without
    /// taking the cluster away from it.
    pub fn backend_holding(&self, lease: Arc<dyn SchemaLease>) -> Arc<dyn Backend> {
        self.backend_for(Arc::clone(&self.client), lease)
    }

    /// The same, over a client the caller built.
    pub fn backend_for(
        &self,
        client: Arc<TxnClient>,
        lease: Arc<dyn SchemaLease>,
    ) -> Arc<dyn Backend> {
        Arc::new(StoreBackend::new(client, Arc::clone(&self.oracle)).with_schema_lease(lease))
    }

    /// A second client over the same three stores: what a **second SQL node** holds.
    ///
    /// Its own connections and its own region cache, which is what makes it another node — over
    /// the same oracle, which is what two nodes against one TSO have. Two independent counters
    /// would hand the same timestamp to two different transactions, which is not a second node
    /// but a broken cluster (`CLAUDE.md` invariant 6).
    ///
    /// Blocks, so it belongs off the reactor like every other synchronous client here.
    pub fn another_client(&self) -> Arc<TxnClient> {
        let stores = TcpStores::connect_all(&self.addresses, TransportConfig::new())
            .expect("a second client connects to all three");
        let resolver: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([
            route(1, b"", b"m"),
            route(2, b"m", b"t"),
            route(3, b"t", b""),
        ]));
        let router = Router::with_options(
            Arc::new(stores),
            resolver,
            ClientOptions {
                jitter_seed: Some(11),
                ..ClientOptions::default()
            },
        );
        Arc::new(TxnClient::on_router(
            Arc::new(router),
            Arc::clone(&self.oracle),
        ))
    }
}

/// One connection's worth of executor.
pub struct Session {
    pub executor: Executor,
}

impl Session {
    /// Runs every statement in the string, stopping at the first failure, as a session would.
    ///
    /// Transaction control goes to the executor's own `begin`/`commit`/`rollback` rather than to
    /// `execute`, because that is where `esker_sql::pgwire::session` sends it: `BEGIN` moves the
    /// status a client sees in every `ReadyForQuery`, so it belongs to the session and reaches the
    /// executor as a call and not as a statement.
    pub fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in esker_sql::parse::parse_statements(sql)? {
            last = match parsed.class() {
                StatementClass::Begin => {
                    self.executor.begin(parsed.begins_read_only())?;
                    Outcome::done("BEGIN")
                }
                StatementClass::Commit => {
                    self.executor.commit()?;
                    Outcome::done("COMMIT")
                }
                StatementClass::Rollback => {
                    self.executor.rollback()?;
                    Outcome::done("ROLLBACK")
                }
                // Savepoints go the same way and for the same reason: `pgwire::session` routes
                // them to the executor's own methods, so a harness that sent them to `execute`
                // got `FeatureNotSupported("SAVEPOINT")` — a gap in the harness that reads exactly
                // like the server refusing a statement it in fact serves.
                StatementClass::Savepoint(name) => {
                    self.executor.savepoint(name)?;
                    Outcome::done("SAVEPOINT")
                }
                StatementClass::RollbackTo(name) => {
                    self.executor.rollback_to(name)?;
                    Outcome::done("ROLLBACK")
                }
                StatementClass::Release(name) => {
                    self.executor.release(name)?;
                    Outcome::done("RELEASE")
                }
                _ => self.executor.execute(&parsed, &Params::NONE)?,
            };
        }
        Ok(last)
    }

    /// The rows of a query, each column rendered as the text a client would receive.
    pub fn rows(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| {
                            value.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        })
                        .collect()
                })
                .collect(),
            other @ Outcome::Done { .. } => {
                panic!("{sql} did not return rows: {other:?}")
            }
        }
    }
}
