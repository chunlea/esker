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
use esker_client::{CountingOracle, TcpStores, TxnClient};
use esker_proto::{Epoch, Peer, Region, ServerHandle, TransportConfig};
use esker_sql::backend::{Backend, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_store::{Store, StoreOptions, StoreService};

/// The tenant every session here is served as.
pub const TENANT: u64 = 1;

/// Three stores, the routing that divides the key space between them, and a SQL node over it.
pub struct Cluster {
    pub backend: Arc<dyn Backend>,
    pub catalog: Arc<Catalog>,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    _runtime: tokio::runtime::Runtime,
}

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
    /// Starts three stores and points a SQL node at them.
    pub fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime");

        let started: Vec<_> = (1..=3).map(|id| start_store(&runtime, id)).collect();
        let addresses: Vec<_> = started
            .iter()
            .map(|(handle, _)| handle.local_addr())
            .collect();
        let stores = TcpStores::connect_all(&addresses, TransportConfig::new())
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
        let client = TxnClient::on_router(
            Arc::new(router),
            // Starting well above zero so that a timestamp is never mistaken for an absent one.
            Arc::new(CountingOracle::starting_at(1_000)),
        );

        let (handles, dirs) = started.into_iter().unzip();
        Cluster {
            backend: Arc::new(StoreBackend::new(Arc::new(client))),
            catalog: Arc::new(Catalog::new()),
            _handles: handles,
            _dirs: dirs,
            _runtime: runtime,
        }
    }

    /// A session on this node. Sessions share the store and the catalog cache, as they do in the
    /// real binary.
    pub fn session(&self) -> Session {
        Session {
            executor: Executor::new(Arc::clone(&self.backend), Arc::clone(&self.catalog), TENANT),
        }
    }
}

/// One connection's worth of executor.
pub struct Session {
    pub executor: Executor,
}

impl Session {
    /// Runs every statement in the string, stopping at the first failure, as a session would.
    pub fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in esker_sql::parse::parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
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
