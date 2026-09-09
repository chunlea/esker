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
    /// Where the stores and PD listen, so a test can build a **second** SQL node over the same
    /// cluster — one with a region cache of its own, which is what makes a stale route reachable.
    addresses: Vec<SocketAddr>,
    pd_address: SocketAddr,
    /// **Shared with every node here, because two clocks are two clusters.** A second node with an
    /// oracle of its own starts numbering at the same instant the first did and reads at a snapshot
    /// from before the first node's `CREATE TABLE` — `42P01` for a table that is right there. The
    /// harness in `tests/cluster` says the same thing in its own words.
    oracle: Arc<dyn TimestampOracle>,
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
        let (pd, addresses, pd_address, oracle, backend, catalog, handles, dirs) =
            runtime.block_on(Self::open(split_size));
        Splitting {
            pd,
            addresses,
            pd_address,
            oracle,
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
        Vec<SocketAddr>,
        SocketAddr,
        Arc<dyn TimestampOracle>,
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

        let (backend, oracle) = Self::node_over(addresses.clone(), pd_address).await;
        (
            pd,
            addresses,
            pd_address,
            oracle,
            backend,
            Arc::new(Catalog::new()),
            handles,
            dirs,
        )
    }

    /// The client half: connections, a `PdConn` resolver, a clock and a backend over them.
    async fn node_over(
        addresses: Vec<SocketAddr>,
        pd_address: SocketAddr,
    ) -> (Arc<dyn Backend>, Arc<dyn TimestampOracle>) {
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
        let backend: Arc<dyn Backend> = Arc::new(StoreBackend::new(client, Arc::clone(&oracle)));
        (backend, oracle)
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

    /// **A second SQL node over the same stores, with a region cache of its own.**
    ///
    /// Its own `PdConn`, its own `Router`, its own connections — which is what makes it a second
    /// node rather than a second session. Two sessions of one node share a cache and could never
    /// hold different ideas about where a region is, and holding different ideas is the whole
    /// subject of `a_stale_region_cache_still_reads_every_row_once`.
    ///
    /// The **catalog is shared**, because a catalog is the node's view of the schema and both nodes
    /// are looking at one cluster; what is not shared is the routing.
    fn another_node(&self) -> Session {
        let addresses = self.addresses.clone();
        let stores = std::thread::spawn(move || {
            TcpStores::connect_all(&addresses, TransportConfig::new()).unwrap()
        })
        .join()
        .unwrap();
        let conn = Arc::new(PdConn::new(self.pd_address));
        let router = Router::with_options(
            Arc::new(stores),
            conn as Arc<dyn RegionResolver>,
            ClientOptions {
                jitter_seed: Some(29),
                ..ClientOptions::default()
            },
        );
        let client = Arc::new(TxnClient::on_router(
            Arc::new(router),
            Arc::clone(&self.oracle),
        ));
        let backend: Arc<dyn Backend> =
            Arc::new(StoreBackend::new(client, Arc::clone(&self.oracle)));
        Session {
            executor: Executor::new(
                backend,
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

/// **A transaction that writes across the boundaries commits as one, and a rollback leaves no
/// half.**
///
/// Percolator picks its primary from one region and its secondaries live in the others, so a
/// statement touching rows on both sides of a boundary is a two-phase commit *across regions* —
/// the thing §11 said nothing exercised. The rollback is the half worth asserting: a transaction
/// that wrote to several regions and then abandoned them must leave every one as it was, and a
/// partial one is visible as some rows moved and some not.
///
/// This is the face that found `esker-client`'s re-cut defect: before it, every one of these
/// commits died with `gave up after 9 attempts: region epoch does not match`.
#[test]
fn a_transaction_across_the_boundaries_commits_whole_or_not_at_all() {
    let many = Splitting::start(SPLIT_SIZE);
    let mut session = many.session();
    load(&mut session);
    wait_for(
        "the table's region to split at least three ways",
        60,
        || many.regions() >= 3,
    );
    let regions = many.regions();

    // **Spread rather than large.** `id % 97 = 0` picks a handful of rows scattered over the whole
    // key space, so every region is written and the transaction stays small enough that what is
    // being measured is the crossing rather than the size.
    let spread = "WHERE id % 97 = 0";
    let before = session.rows(&format!(
        "SELECT id, amount FROM ledger {spread} ORDER BY id"
    ));
    assert!(
        before.len() >= 3,
        "the write set has to reach several regions: {} rows",
        before.len()
    );

    session.run("BEGIN").unwrap();
    session
        .run(&format!("UPDATE ledger SET amount = amount + 1 {spread}"))
        .unwrap();
    session.run("COMMIT").unwrap();

    let committed = session.rows(&format!(
        "SELECT id, amount FROM ledger {spread} ORDER BY id"
    ));
    assert_eq!(committed.len(), before.len(), "a row went missing");
    for (now, was) in committed.iter().zip(&before) {
        assert_eq!(now[0], was[0], "the rows came back in a different order");
        let moved: i64 = now[1].as_deref().unwrap().parse().unwrap();
        let started: i64 = was[1].as_deref().unwrap().parse().unwrap();
        assert_eq!(
            moved,
            started + 1,
            "row {:?} did not move with the others across {regions} regions",
            now[0]
        );
    }

    // And the other half: a transaction that writes across every boundary and rolls back.
    let whole = session.rows("SELECT id, amount FROM ledger ORDER BY id");
    session.run("BEGIN").unwrap();
    session
        .run(&format!(
            "UPDATE ledger SET amount = amount + 1000 {spread}"
        ))
        .unwrap();
    session
        .run(&format!(
            "DELETE FROM ledger {spread} AND id > {}",
            ROWS / 2
        ))
        .unwrap();
    session.run("ROLLBACK").unwrap();

    assert_eq!(
        session.rows("SELECT id, amount FROM ledger ORDER BY id"),
        whole,
        "the rollback left something behind in at least one of {regions} regions"
    );
}

/// **A node whose region cache is stale reads the whole table anyway** — no row lost, none twice.
///
/// This is §11's first clause, `EpochNotMatch` and the refresh, and the only way to reach it
/// deliberately is two nodes: the first caches routes for the regions as they are, the second
/// grows the table until the store splits them, and then the first is asked for every row. Its
/// cached routes name regions that no longer exist at the epoch it holds, so every one of them is
/// refused and re-fetched from PD before the scan can answer.
///
/// The assertion is the whole table, in order, against what the writer sees: a scan that dropped a
/// region's worth of rows or served one twice cannot pass it.
#[test]
fn a_stale_region_cache_still_reads_every_row_once() {
    let many = Splitting::start(SPLIT_SIZE);
    let mut reader = many.session();
    load(&mut reader);
    wait_for("the first splits", 60, || many.regions() >= 3);
    let first = many.regions();

    // The reader caches a route per region by reading every one of them.
    let seen = reader.rows("SELECT count(*) FROM ledger");
    assert_eq!(seen[0][0].as_deref(), Some(ROWS.to_string().as_str()));

    // A **second node**, with a region cache of its own, grows the table until the store splits
    // again. Nothing tells the first node.
    let mut writer = many.another_node();
    // **Until it splits, not a fixed number of rows.** How many inserts cross the next threshold
    // depends on where the last split left the halves, so a fixed count is a test that passes on
    // some runs — it did, and then it did not, which is how this loop came to be here.
    let mut last = ROWS;
    let deadline = Instant::now() + Duration::from_secs(90);
    while many.regions() <= first {
        assert!(
            Instant::now() < deadline,
            "the table never split again: still {first} regions after {last} rows"
        );
        for id in (last + 1)..=(last + 200) {
            writer
                .run(&format!(
                    "INSERT INTO ledger VALUES ({id}, 'who-{}', {})",
                    id % 7,
                    id * 3
                ))
                .unwrap();
        }
        last += 200;
    }
    let written = last;
    let now = many.regions();
    assert!(
        now > first,
        "the table did not split again: {first} regions before and {now} after"
    );

    // The reader's cache is stale for every region that split. This is the scan that has to
    // notice, refresh, and finish.
    let rows = reader.rows("SELECT id FROM ledger ORDER BY id");
    assert_eq!(
        i64::try_from(rows.len()).unwrap(),
        written,
        "a scan over {now} regions from a cache built for {first} lost or repeated rows"
    );
    let ids: Vec<i64> = rows
        .iter()
        .map(|row| row[0].as_deref().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        ids,
        (1..=written).collect::<Vec<_>>(),
        "the ids are not exactly 1..={written} once each"
    );
    // And the writer, whose cache is current, agrees.
    assert_eq!(
        writer.rows("SELECT count(*) FROM ledger")[0][0].as_deref(),
        Some(written.to_string().as_str())
    );
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

/// **A cursor left open across a split still reads its snapshot, once each.**
///
/// The faces above meet a boundary that moved *between* statements. This one moves it **while the
/// scan is open**: the reader declares a cursor over the whole table, fetches part of it, and only
/// then does a second node grow the table until the store splits again. Every remaining `FETCH`
/// crosses regions that did not exist when the cursor was declared.
///
/// Two assertions, and they are about different things:
///
/// * the ids are exactly `1..=ROWS`, **once each** — the scan did not lose a region's worth of rows
///   at a boundary that moved under it, and did not serve one twice by restarting a range;
/// * the rows the writer added are **not** among them, because a cursor reads the snapshot it was
///   declared at. A cursor that picked them up would be a scan that re-read the table rather than
///   resuming it, which is the failure this shape is most likely to have.
#[test]
fn a_cursor_open_across_a_split_reads_its_snapshot_once() {
    let many = Splitting::start(SPLIT_SIZE);
    let mut reader = many.session();
    load(&mut reader);
    wait_for(
        "the table's region to split at least three ways",
        60,
        || many.regions() >= 3,
    );
    let first = many.regions();

    reader.run("BEGIN").unwrap();
    // **No `ORDER BY`, and that is the whole difference between this test and a test of nothing.**
    // `Node::Sort` drains its input by definition — its input's last row can be its output's first
    // — so a cursor over an ordered query has read the entire table before it answers the first
    // `FETCH`, and a split afterwards touches nothing it will ever look at. The first version of
    // this test was ordered and passed for that reason. A bare scan is chunked (`SCAN_CHUNK` keys
    // at a time), so the rows after the split really are fetched from regions that did not exist
    // when the cursor was declared.
    reader
        .run("DECLARE c CURSOR FOR SELECT id FROM ledger")
        .unwrap();
    let mut seen: Vec<i64> = Vec::new();
    for _ in 0..50 {
        let row = reader.rows("FETCH c");
        assert_eq!(row.len(), 1, "the cursor ran out before the split");
        seen.push(row[0][0].as_deref().unwrap().parse().unwrap());
    }

    // **Now**, with the cursor open and its snapshot taken, a second node splits the table under
    // it. Nothing tells the reader.
    let mut writer = many.another_node();
    let mut last = ROWS;
    let deadline = Instant::now() + Duration::from_secs(90);
    while many.regions() <= first {
        assert!(
            Instant::now() < deadline,
            "the table never split again: still {first} regions after {last} rows"
        );
        for id in (last + 1)..=(last + 200) {
            writer
                .run(&format!(
                    "INSERT INTO ledger VALUES ({id}, 'who-{}', {})",
                    id % 7,
                    id * 3
                ))
                .unwrap();
        }
        last += 200;
    }
    let now = many.regions();

    // The rest of the cursor, over regions that did not exist when it was declared.
    loop {
        let row = reader.rows("FETCH c");
        if row.is_empty() {
            break;
        }
        seen.push(row[0][0].as_deref().unwrap().parse().unwrap());
    }
    reader.run("COMMIT").unwrap();

    // Sorted, because a bare scan promises no order — what is asserted is the **set** and the
    // count, which is what "every row once" means. A duplicate or a loss changes one or the other.
    assert_eq!(
        i64::try_from(seen.len()).unwrap(),
        ROWS,
        "a cursor declared over {first} regions and finished over {now} returned {} rows",
        seen.len()
    );
    seen.sort_unstable();
    assert_eq!(
        seen,
        (1..=ROWS).collect::<Vec<_>>(),
        "a cursor declared over {first} regions and finished over {now} did not read 1..={ROWS} \
         once each"
    );
}

/// **A row lock survives the split that moves its row**, which is the whole argument for reusing
/// the Percolator lock ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md) (a')).
///
/// The option this ADR refused put a lock of its own in the store, and the objection to it was that
/// a lock has to travel: a region that splits hands its keys to a child, and a lock left behind — or
/// dropped — is a promise broken in silence. The Percolator lock is a record in the `lock` column
/// family under the row's own key, so it travels the way the row does, by being the same bytes in
/// the same range. This is the test that says so rather than the paragraph that assumes it.
///
/// **The control is the half that makes it evidence.** A second node refused a locked row after a
/// split could be a second node refused *anything* after a split — a stale route, a region cache
/// that has not caught up, any of the failures this file was written for. So the same node, in the
/// same transaction, immediately takes a **different** row of the same table: if the split had
/// broken its routing it would fail there too, and it does not.
#[test]
fn a_row_lock_survives_the_split_that_moves_its_row() {
    let many = Splitting::start(SPLIT_SIZE);
    let mut writer = many.session();
    load(&mut writer);
    wait_for("the table to split", 60, || many.regions() >= 3);
    let before = many.regions();

    // The lock, taken while the row lives in whatever region holds it now.
    let mut holder = many.session();
    holder.run("BEGIN").unwrap();
    holder
        .run("SELECT amount FROM ledger WHERE id = 450 FOR UPDATE")
        .unwrap();

    // **Before the split, so that a refusal after it means the split.** Without this the test
    // cannot tell "the lock travelled" from "the lock was never taken", and those fail the same
    // way.
    let mut early = many.another_node();
    early.run("BEGIN").unwrap();
    early.run("SET lock_timeout = '2s'").unwrap();
    let held = early
        .run("SELECT amount FROM ledger WHERE id = 450 FOR UPDATE")
        .expect_err("the lock was never taken, so the split has nothing to lose");
    assert_eq!(held.sqlstate(), "55P03", "before the split: {held}");
    early.run("ROLLBACK").unwrap();

    // And now the ground moves under it: more rows, more splits, until the table is cut at least
    // twice more than it was when the lock was taken.
    for id in ROWS + 1..=ROWS * 2 {
        writer
            .run(&format!(
                "INSERT INTO ledger VALUES ({id}, 'who-{}', {})",
                id % 7,
                id * 3
            ))
            .unwrap();
    }
    wait_for("two further splits", 60, || many.regions() >= before + 2);

    // A second node, with a region cache of its own, asks for the row the first node is holding.
    let mut other = many.another_node();
    other.run("BEGIN").unwrap();
    other.run("SET lock_timeout = '2s'").unwrap();
    let refused = other
        .run("SELECT amount FROM ledger WHERE id = 450 FOR UPDATE")
        .expect_err("the lock did not survive the split: a second node took the row");
    assert_eq!(
        refused.sqlstate(),
        "55P03",
        "the wait ended some other way than the timeout: {refused}"
    );

    // **The control**: the same node, the same transaction, a row nobody holds. If the splits had
    // broken this node's routing rather than the lock holding, this would fail too.
    let free = other.rows("SELECT amount FROM ledger WHERE id = 451 FOR UPDATE");
    assert_eq!(free.len(), 1, "an unlocked row of the same table");
    other.run("ROLLBACK").unwrap();

    // And the holder still owns what it took, across every split that happened under it.
    holder
        .run("UPDATE ledger SET amount = 7 WHERE id = 450")
        .unwrap();
    holder.run("COMMIT").unwrap();
    let mut after = many.session();
    assert_eq!(
        after.rows("SELECT amount FROM ledger WHERE id = 450"),
        vec![vec![Some("7".to_owned())]]
    );
}
