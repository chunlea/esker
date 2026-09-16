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

pub mod profile;

/// The subscriber, shared with every test binary that includes this harness.
///
/// **Declared once per binary, here.** Clippy's `duplicate_mod` refuses the same file loaded as
/// two modules, and four of this crate's cluster tests include both this harness and their own,
/// so the declaration lives with the harness they all already have and they call through it.
#[path = "../trace/mod.rs"]
pub(crate) mod trace;

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
    /// The node's reserved sequence blocks, shared by every session it serves (ADR 0072).
    pub sequences: Arc<esker_sql::sequence::Blocks>,
    /// The client the default backend is built over, so a test can build a second backend of its
    /// own — one holding a schema lease, say — against the same three stores.
    pub client: Arc<TxnClient>,
    /// The oracle that client allocates from. Shared, because two backends over one cluster that
    /// numbered their transactions independently would not be one cluster.
    pub oracle: Arc<dyn TimestampOracle>,
    /// Where the three stores listen, so a test can build a second client of its own.
    pub addresses: Vec<std::net::SocketAddr>,
    /// The stores themselves, kept so a probe can read the engine's own counters — a
    /// `ServerHandle` alone cannot answer `esker.entries-stepped` (#58 round 3).
    stores: Vec<Arc<Store>>,
    _handles: Vec<ServerHandle>,
    _dirs: Vec<tempfile::TempDir>,
    /// The runtime the stores were started on, when this cluster owns one. `None` when the caller
    /// was already inside a runtime and lent us theirs — a runtime built inside a runtime panics,
    /// which is why [`Cluster::start_on_this_runtime`] exists at all.
    runtime: Option<tokio::runtime::Runtime>,
}

/// What a cluster is started with.
///
/// **Every field is off by default**, so that the harness the other tests use is the one it has
/// always been: three stores at the engine's own 64 MiB write buffer, neither of which collects by
/// itself. Two tests need something else and each says so at its own `Cluster::start_with`, which
/// is a good deal easier to find than an environment variable.
#[derive(Clone, Copy, Default)]
pub struct Settings {
    /// Memtable bytes before a flush, or `None` for the engine's own.
    ///
    /// `natural_compaction` turns it down because its question is about the level scores, and at
    /// the default this workload writes ~36 KiB per store per round — a hundred rounds short of a
    /// single flush, which is run 127e's finding and not an answer to anything.
    pub write_buffer_size: Option<usize>,
    /// Collect when the safepoint rises, with no debounce at all.
    ///
    /// The real default is five minutes, which is the right cadence for a store that runs for days
    /// and the wrong one for a test that runs for thirty seconds.
    pub collecting: bool,
}

async fn start_store(id: u64, settings: Settings) -> (ServerHandle, tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut options = StoreOptions::new();
    options.store_id = id;
    options.peer_id = id;
    options.region_id = id;
    options.collect_debounce = settings.collecting.then_some(std::time::Duration::ZERO);
    if let Some(size) = settings.write_buffer_size {
        options.engine.cf_options.write_buffer_size = size;
    }
    let store = Store::open(dir.path(), options).expect("the store opens");
    let handle = esker_proto::transport::Server::bind(
        "127.0.0.1:0",
        StoreService::new(Arc::clone(&store)),
        TransportConfig::new(),
    )
    .await
    .expect("the server binds")
    .spawn()
    .expect("the server starts");
    (handle, dir, store)
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
        Self::start_with(Settings::default())
    }

    /// The same, with something other than the defaults. See [`Settings`].
    pub fn start_with(settings: Settings) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut cluster = runtime.block_on(Cluster::start_on_this_runtime_with(settings));
        cluster.runtime = Some(runtime);
        cluster
    }

    /// The same, on the caller's runtime — for a test that is already inside one, where building
    /// a second would panic.
    pub async fn start_on_this_runtime() -> Self {
        Self::start_on_this_runtime_with(Settings::default()).await
    }

    /// The same, with something other than the defaults. See [`Settings`].
    pub async fn start_on_this_runtime_with(settings: Settings) -> Self {
        // **Every cluster this crate's tests build goes through here**, which is why the
        // subscriber is installed here rather than remembered at each test: `RUST_LOG` should
        // work without anyone having added a line to the test being debugged.
        trace::on();
        let mut started = Vec::new();
        for id in 1..=3 {
            started.push(start_store(id, settings).await);
        }
        let addresses: Vec<_> = started
            .iter()
            .map(|(handle, _, _)| handle.local_addr())
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

        let mut handles = Vec::new();
        let mut dirs = Vec::new();
        let mut kept = Vec::new();
        for (handle, dir, store) in started {
            handles.push(handle);
            dirs.push(dir);
            kept.push(store);
        }
        let addresses = handles.iter().map(ServerHandle::local_addr).collect();
        Cluster {
            backend: Arc::new(StoreBackend::new(Arc::clone(&client), Arc::clone(&oracle))),
            catalog: Arc::new(Catalog::new()),
            sequences: Arc::new(esker_sql::sequence::Blocks::default()),
            client,
            oracle,
            addresses,
            stores: kept,
            _handles: handles,
            _dirs: dirs,
            runtime: None,
        }
    }

    /// One engine counter, summed over the three stores.
    ///
    /// The cluster is three processes' worth of state in one, so a workload's cost is the total —
    /// and `esker.entries-stepped` is per database. Exists for #58's round-3 probe: the statement
    /// tap counts a read as one read however many stored entries it walked, and this is the half it
    /// cannot see.
    pub fn engine_counter(&self, name: &str) -> u64 {
        self.stores
            .iter()
            .filter_map(|store| store.property(name))
            .filter_map(|value| value.parse::<u64>().ok())
            .sum()
    }

    /// What every store is holding: entries, and the SSTs they are spread over.
    ///
    /// **Two numbers, because they fail differently.** Entries are what #58's space half is
    /// about; the file count is what a read pays directly — run 127h measured
    /// `corr(seconds, SSTs standing) = +0.58` over twenty identical repeats, against `+0.65` for
    /// the entries inside them. A store that held its entry count still while its file count
    /// climbed would be one that had stopped growing and kept getting slower, and one number
    /// alone would call that a pass.
    pub fn standing(&self) -> (u64, u64) {
        self.stores
            .iter()
            .filter_map(|store| store.cf_entries().ok())
            .flatten()
            .fold((0, 0), |(entries, ssts), family| {
                (entries + family.entries, ssts + family.ssts)
            })
    }

    /// Files per level, summed over every store and every column family.
    ///
    /// The shape and not just the count: one file in L0 and one in L6 are the difference between
    /// versions that are all still there and versions that were merged away, and a total hides
    /// exactly that (ADR 0109 is why the RPC carries the level at all).
    pub fn levels(&self) -> std::collections::BTreeMap<u32, usize> {
        let mut per_level = std::collections::BTreeMap::new();
        for store in &self.stores {
            for family in store.sst_files().into_iter().flatten() {
                for (level, _) in family.files {
                    *per_level.entry(level).or_default() += 1;
                }
            }
        }
        per_level
    }

    /// Flushes every store's memtables, so what they hold is on the books and countable.
    ///
    /// **A measurement's, not a workload's.** [`Cluster::standing`] and [`Cluster::entries_in`]
    /// read SST properties, so an arm that never flushes reports zero however much it is holding —
    /// and an arm that sweeps flushes on its way. Comparing the two without this compares "flushed"
    /// against "never flushed" and calls the difference a collection.
    pub fn flush(&self) {
        for store in &self.stores {
            store.flush().expect("a flush runs");
        }
    }

    /// Entries one column family's SSTs hold, summed over every store.
    ///
    /// `standing` sums every family, which is the right number for "what does a read walk" and the
    /// wrong one for a question about **bytes**: a value too long to inline lives in `default`
    /// alone, and #60's whole point is that the two families were collected differently.
    pub fn entries_in(&self, family: &str) -> u64 {
        self.stores
            .iter()
            .filter_map(|store| store.cf_entries().ok())
            .flatten()
            .filter(|entries| entries.cf == family)
            .map(|entries| entries.entries)
            .sum()
    }

    /// The highest collection safepoint any store is working to.
    ///
    /// A **denominator**: "the store stopped growing" is satisfied perfectly by a cluster in which
    /// no version was ever collectable, and this is how a test says the measurement took place.
    pub fn safepoint(&self) -> u64 {
        self.stores
            .iter()
            .map(|store| store.safepoint())
            .max()
            .unwrap_or(0)
    }

    /// Publishes a collection safepoint to every store — **and does nothing else**.
    ///
    /// This is the whole of what a store's heartbeat delivers
    /// ([ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md)):
    /// PD answers with a number, the store raises its own, and that is the end of the exchange.
    /// [`Cluster::collect_everything`] is the other one — it flushes and compacts too — and a
    /// test asking "does raising the safepoint make the store collect?" must use **this** one,
    /// or it does the work it is trying to observe.
    ///
    /// Publishing at `now` is the small-retention case: the safepoint is
    /// `min(now - retention, oldest active read)`, and with no transaction held open and a
    /// retention an operator has turned down, that is `now`. Reads taken after this are above
    /// it, so decision 5's floor refuses nothing.
    pub fn publish_safepoint(&self) -> u64 {
        let now = self
            .oracle
            .timestamp()
            .expect("a timestamp to publish as the safepoint");
        self.stores
            .iter()
            .map(|store| store.raise_safepoint(now))
            .max()
            .unwrap_or(0)
    }

    /// Publishes an **arbitrary** safepoint, for a test that needs one other than `now`.
    ///
    /// **The usual reason is a control arm, not a bigger bite.** This doc used to say that
    /// [`Cluster::publish_safepoint`] could not make the collector decide anything, because a
    /// retention window in milliseconds underflows against a `CountingOracle`'s timestamps — and
    /// that is true of *PD*, which is where the window is subtracted (`esker-pd`'s
    /// `a_counting_oracle_collects_nothing` is the trap, and it is PD's).
    /// `MvccCollector::effective_safepoint` does not subtract it: it adjusts a published safepoint
    /// by a table's override against the cluster default and returns it unchanged when there is
    /// none. So a store told `now` collects everything at or below `now`, and
    /// `publish_safepoint` bites — `name_without_its_table` measures 3,001 entries standing
    /// against 817 over the same two hundred rounds.
    ///
    /// What this is for is the other arm: a safepoint that **rises** every round, so the sweeper
    /// flushes and compacts exactly as it does in the other arm, and that is **below** every
    /// version's `commit_ts`, so the collector is reached, asked, and unable to drop anything. That
    /// is what makes a fall in the entry count the collector's rule rather than the engine's.
    pub fn publish_safepoint_at(&self, safepoint: u64) -> u64 {
        self.stores
            .iter()
            .map(|store| store.raise_safepoint(safepoint))
            .max()
            .unwrap_or(0)
    }

    /// Bytes the memtables are holding, across every store and every column family.
    ///
    /// The counterpart to [`Cluster::standing`], which sees only what has been flushed: at the
    /// default 64 MiB write buffer a short workload never flushes at all, so a probe that asked
    /// only about SSTs would report a store that holds nothing while it holds everything.
    pub fn memtable_bytes(&self) -> u64 {
        self.stores
            .iter()
            .flat_map(|store| {
                store
                    .cf_names()
                    .into_iter()
                    .filter_map(|cf| store.property(&format!("esker.mem-table-size.{cf}")))
            })
            .filter_map(|value| value.parse::<u64>().ok())
            .sum()
    }

    /// Raises every store's collection safepoint and compacts, so a probe can ask what the cost
    /// would be if the history were reclaimed (ADR 0110 step 1, by hand).
    pub fn collect_everything(&self) {
        for store in &self.stores {
            // **Not `u64::MAX`.** A safepoint above every timestamp says "all history is
            // collectable", and since ADR 0110 a read below the safepoint is refused — so
            // `u64::MAX` collects everything and then refuses every read that follows, which is
            // the floor working rather than a bug. Collect up to *now* instead: that is what an
            // operator asking for "everything collectable" means, and it leaves the present
            // readable.
            let now = self
                .oracle
                .timestamp()
                .expect("a timestamp to collect up to");
            store.raise_safepoint(now);
            // **Flush first, or this measures a no-op.** `compact_range` compacts SSTs; at the
            // default 64 MiB write buffer a test's whole workload is still in the memtable, so a
            // compaction without this has nothing to compact and reports that nothing changed —
            // which reads exactly like "collecting does not help".
            store.flush().expect("a flush runs");
            let before = store.cf_entries().expect("the families");
            for cf in store.cf_names() {
                store.compact_cf(&cf).expect("a compaction runs");
            }
            let after = store.cf_entries().expect("the families");
            for (was, now) in before.iter().zip(after.iter()) {
                println!(
                    "    COLLECT {:>8}: {} entries in {} sst  ->  {} entries in {} sst",
                    was.cf, was.entries, was.ssts, now.entries, now.ssts
                );
            }
        }
    }

    /// A session on this node. Sessions share the store, the catalog cache **and the sequence
    /// allocator**, as they do in the real binary — a block belongs to the node and not to the
    /// connection (ADR 0072), and a harness that gave each session its own would not be able to
    /// see the thing that cost `range_test.rb` a test.
    pub fn session(&self) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                esker_sql::session::register(),
            )
            .sharing_sequence_blocks(Arc::clone(&self.sequences)),
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

    /// A router of its own, for a test that has to speak the wire directly.
    ///
    /// The one thing a `TxnClient` cannot do is leave a transaction **prewritten and
    /// uncommitted** — its `commit` does both halves — and that is exactly the state a DDL is in
    /// while it commits, which is the window ADR 0105 is about. Driving it by hand is the only way
    /// to hold it still.
    pub fn router(&self) -> Router {
        let stores = TcpStores::connect_all(&self.addresses, TransportConfig::new())
            .expect("a raw router connects to all three");
        let resolver: Arc<dyn RegionResolver> = Arc::new(RegionTable::from_routes([
            route(1, b"", b"m"),
            route(2, b"m", b"t"),
            route(3, b"t", b""),
        ]));
        Router::with_options(
            Arc::new(stores),
            resolver,
            ClientOptions {
                jitter_seed: Some(13),
                ..ClientOptions::default()
            },
        )
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
