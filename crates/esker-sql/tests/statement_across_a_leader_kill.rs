//! **A statement survives losing the store that led its region.** ADR-less; this is run 124's
//! acceptance, at the layer the symptom appeared.
//!
//! # The symptom, and why the client-layer test is not this one
//!
//! run 124's leader-store kill cost a SQL node **184 × `08006` in 0.695 s**. Rails does not retry
//! `08006` in fixture setup, so ninety-two tests errored — on a cluster whose control sample ran
//! 484 assertions clean on the same binary.
//!
//! `esker-client`'s own `tests/leader_kill_retry.rs` proves the *mechanism*: a raw write now
//! outlives the store it was routed to. This proves the *symptom is gone*, which is a different
//! claim and the one that was asked for — a statement, through `esker_sql`, where
//! `SqlError::StoreUnavailable` is what becomes `08006` (`sqlstate::CONNECTION_FAILURE`). A fix
//! that repaired the client and left the statement refused would pass the first and fail this.
//!
//! # The arrangement
//!
//! Three real stores in one process, replicating one region over real sockets, and a SQL node over
//! them with its own executor. No placement driver: what is under test is the store path, and a
//! `CountingOracle` keeps the timestamp question out of it (this is one node, which is the one
//! case a local counter is a correct oracle for — `tests/two_nodes_one_clock.rs` is where that
//! stops being true).
//!
//! The kill is `Store::stop` plus a server shutdown — the in-process stand-in for a `SIGKILL`, as
//! `esker-client`'s `chaos_cluster` argues at length. What it proves is bounded and it is the
//! bound that matters here: the statement is asked after the store it was routed to has stopped
//! answering.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// **The session wrapper, not the cluster.** `cluster/mod.rs`'s `Cluster` is three *independent*
// stores of one region each, which is the wiring most of this crate's cluster tests need and the
// opposite of what this one does: killing a store there takes its region with it. Its `Session` is
// exactly right, though — it routes `BEGIN`/`COMMIT`/savepoints to the executor's own methods the
// way `pgwire::session` does, which a second copy here would have got wrong.
mod cluster;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::StaticRegion;
use esker_client::{ClientOptions, CountingOracle, Router, TcpStores, TimestampOracle, TxnClient};
use esker_proto::{Server, ServerHandle, TransportConfig};
use esker_sql::backend::{Backend, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::session::Outcome;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};

const REGION: u64 = 1;
const TENANT: u64 = 1;
const STORES: usize = 3;

/// What the acceptance asks: a statement that succeeds *within* this rather than being refused at
/// once. The cluster's own election is 10–20 ticks at 5 ms here, so this is generous against it
/// and still far from "the statement waited out a timeout".
const WITHIN: Duration = Duration::from_secs(3);

struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
}

struct Cluster {
    runtime: tokio::runtime::Runtime,
    nodes: Vec<std::sync::Mutex<Option<Node>>>,
    addresses: Vec<SocketAddr>,
    _dirs: Vec<tempfile::TempDir>,
}

impl Cluster {
    fn start() -> Self {
        // Every cluster in this file goes through here. See `tests/trace`: the subscriber is
        // installed at the harness rather than remembered per test, so `RUST_LOG` works on the
        // test somebody is already debugging.
        cluster::trace::on();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        // Bound first and handed to the servers, so no port is free between the address being
        // known and the server that uses it binding — the race `chaos_cluster` documents.
        let reserved: Vec<std::net::TcpListener> = (0..STORES)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addresses: Vec<SocketAddr> = reserved.iter().map(|l| l.local_addr().unwrap()).collect();
        let peers: Vec<PeerAddress> = (0..STORES)
            .map(|at| PeerAddress::new(at as u64 + 1, at as u64 + 1, addresses[at]))
            .collect();
        let dirs: Vec<tempfile::TempDir> =
            (0..STORES).map(|_| tempfile::tempdir().unwrap()).collect();

        let mut nodes = Vec::new();
        for (at, listener) in reserved.into_iter().enumerate() {
            let id = at as u64 + 1;
            let mut raft = RaftOptions::new(peers.clone(), 20_260_911);
            raft.tick = Duration::from_millis(5);
            let store = {
                let _guard = runtime.enter();
                Store::open(
                    dirs[at].path(),
                    StoreOptions {
                        store_id: id,
                        peer_id: id,
                        region_id: REGION,
                        raft: Some(raft),
                        ..StoreOptions::new()
                    },
                )
                .unwrap()
            };
            let service = StoreService::new(Arc::clone(&store));
            let handle = runtime.block_on(async {
                Server::from_listener(listener, service, TransportConfig::new())
                    .unwrap()
                    .spawn()
                    .unwrap()
            });
            nodes.push(std::sync::Mutex::new(Some(Node { store, handle })));
        }
        Self {
            runtime,
            nodes,
            addresses,
            _dirs: dirs,
        }
    }

    /// The index of the node a **live** peer believes leads.
    ///
    /// Asked of the published value rather than of the core's role, so this crate needs no
    /// `esker-raft` dependency to run it: every peer publishes who it believes leads, and a store
    /// that believes itself is the leader.
    fn leader(&self) -> Option<usize> {
        for at in 0..self.nodes.len() {
            let guard = self.nodes[at].lock().unwrap();
            let Some(node) = guard.as_ref() else { continue };
            let Some(peer) = node.store.peer() else {
                continue;
            };
            if let Some(id) = peer.leader() {
                return usize::try_from(id).ok().and_then(|id| id.checked_sub(1));
            }
        }
        None
    }

    fn settle(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.leader().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn kill(&self, at: usize) {
        let node = self.nodes[at].lock().unwrap().take();
        if let Some(node) = node {
            node.store.stop();
            self.runtime.block_on(async {
                let _ = node.handle.shutdown().await;
            });
        }
    }

    fn shutdown(&self) {
        for at in 0..self.nodes.len() {
            self.kill(at);
        }
    }

    /// A SQL node over every store: the real `StoreBackend`, the real executor, one session.
    fn sql_node(&self) -> (cluster::Session, Arc<Catalog>) {
        let addresses = self.addresses.clone();
        let stores = TcpStores::connect_all(&addresses, TransportConfig::new()).unwrap();
        let ids = stores.store_ids();
        let router = Router::with_options(
            Arc::new(stores),
            Arc::new(StaticRegion::replicated(REGION, &ids)),
            ClientOptions {
                jitter_seed: Some(11),
                ..ClientOptions::default()
            },
        );
        let oracle: Arc<dyn TimestampOracle> = Arc::new(CountingOracle::starting_at(1_000));
        let client = Arc::new(TxnClient::on_router(Arc::new(router), Arc::clone(&oracle)));
        let backend: Arc<dyn Backend> = Arc::new(StoreBackend::new(client, oracle));
        let catalog = Arc::new(Catalog::new());
        let executor = Executor::new(
            backend,
            Arc::clone(&catalog),
            TENANT,
            esker_sql::session::register(),
        );
        (cluster::Session { executor }, catalog)
    }
}

/// **The acceptance.** A statement after the leading store is taken away must answer, not `08006`.
#[test]
fn a_statement_answers_after_the_store_leading_its_region_is_killed() {
    let cluster = Cluster::start();
    assert!(
        cluster.settle(Duration::from_secs(20)),
        "the cluster never elected a leader to take away"
    );
    let (mut sql, _catalog) = cluster.sql_node();

    setup(&mut sql);

    let leader = cluster.leader().expect("somebody leads");
    cluster.kill(leader);

    // **One statement, and the clock.** Not a loop: a client gets one answer per statement, and
    // Rails' fixture setup does not ask twice — which is why the node has to.
    let began = Instant::now();
    let answered = sql.run("INSERT INTO t VALUES (2, 'after')");
    let took = began.elapsed();

    if let Err(error) = answered {
        let state = error.sqlstate();
        panic!(
            "the statement was refused {} ms after the store leading its region was killed, \
             SQLSTATE {state}: {error}. Two replicas were live and an election was already under \
             way. This is run 124's 184 × 08006 in 0.695 s — a node that surfaces a store it \
             could not reach spends no budget at all, and Rails does not retry 08006 in fixture \
             setup.",
            took.as_millis()
        );
    }
    assert!(
        took < WITHIN,
        "the statement answered but took {took:?}, past the {WITHIN:?} a statement may spend on a \
         leader kill"
    );

    // The kill really took the leader, so this cannot pass on a follower kill — the arithmetic
    // run 123 got wrong and run 124 fixed.
    assert!(
        cluster.leader() != Some(leader),
        "the killed store still leads, so nothing was taken away"
    );

    // And the row is there: a statement that "succeeded" without writing would pass every
    // assertion above.
    let rows = run(&mut sql, "SELECT v FROM t ORDER BY id");
    assert_eq!(
        rows, 2,
        "the write that survived the kill is not in the table"
    );

    cluster.shutdown();
}

/// Runs one statement that must succeed, and answers with how many rows came back.
/// **#68's acceptance.** The fixture is safe to repeat, and does not need a settled cluster.
///
/// The setup used to be two bare statements through `run`, which panics on any error, and a
/// natural election during startup made its `CREATE TABLE` answer `40003` — four red gates, none
/// of them about the thing this file tests:
///
/// ```text
/// 40003: the transaction's outcome is unknown: the `TxnPrewrite` may or may not have been
/// applied … region 1 stopped leading with this proposal in its log; it may still commit
/// ```
///
/// **Two halves, and the second is the one that can be made deterministic.** A `40003` says the
/// write *may already have landed*, so a retry is only safe if the statement is safe to repeat —
/// and that half is testable without an election at all: call the fixture twice. With the bare
/// statements the second call answers `42P07` on the table and `23505` on the row, which is
/// exactly what a retry after a `40003` would have met.
///
/// The first half — that it tolerates a region which is electing — is exercised by taking the
/// leader away first. That is **not** where the discrimination is, and this test says so rather
/// than implying it: a kill before the first statement lets the cluster elect while the node is
/// still connecting, so it passes with the bare form too. It is here because it costs nothing and
/// the shape is the one the gate kept finding.
#[test]
fn the_setup_is_safe_to_repeat_and_does_not_need_a_settled_cluster() {
    let cluster = Cluster::start();
    assert!(
        cluster.settle(Duration::from_secs(20)),
        "the cluster never elected a leader to take away"
    );
    let leader = cluster.leader().expect("somebody leads");
    cluster.kill(leader);

    // No `settle` after the kill, deliberately: the point is that the fixture does not need one.
    let (mut sql, _catalog) = cluster.sql_node();
    setup(&mut sql);
    // **The discriminating half.** This is what a retry after an outcome-unknown does.
    setup(&mut sql);

    assert_eq!(
        run(&mut sql, "SELECT v FROM t WHERE id = 1"),
        1,
        "the fixture ran twice and left either no row or two"
    );
}

/// The fixture this file's tests need, written so that repeating it is safe.
///
/// # Why idempotent **and** retried, rather than either
///
/// A bare retry is wrong on its own: `40003` means the write may already have landed, so resending
/// `CREATE TABLE` meets `42P07` and resending the `INSERT` meets `23505`. And idempotence is not
/// enough on its own either — `IF NOT EXISTS` still has to be *sent again* after the connection
/// that carried the first one was closed. So each statement is written to be safe to repeat, and
/// then repeated while the error says the cluster is still changing its mind.
///
/// Waiting for a settled cluster is what `Cluster::settle` already does and it is **not** a
/// substitute: a leader can be lost at any moment, not only during startup, and a fixture that
/// only works between elections is one that fails on a loaded gate.
fn setup(sql: &mut cluster::Session) {
    for statement in [
        "CREATE TABLE IF NOT EXISTS t (id int PRIMARY KEY, v text)",
        "INSERT INTO t VALUES (1, 'before') ON CONFLICT (id) DO UPDATE SET v = 'before'",
    ] {
        retry_while_the_cluster_settles(sql, statement);
    }
}

/// Sends `statement` until it takes, or until the deadline says the cluster is not settling.
///
/// **Only the states that mean "ask again".** An error outside them is a real failure and is raised
/// as one: a fixture that swallowed a syntax error would make every test in this file pass by not
/// running.
fn retry_while_the_cluster_settles(sql: &mut cluster::Session, statement: &str) {
    /// Long enough for an election and a retry or two, short enough that a cluster which is never
    /// going to settle fails rather than hangs.
    const WITHIN: Duration = Duration::from_secs(30);
    let deadline = Instant::now() + WITHIN;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let Err(error) = sql.run(statement) else {
            return;
        };
        let state = error.sqlstate();
        assert!(
            matches!(state, "40003" | "40001" | "08006" | "08000"),
            "`{statement}` failed with SQLSTATE {state}, which is not the cluster changing its \
             mind: {error}"
        );
        assert!(
            Instant::now() < deadline,
            "`{statement}` was still answering SQLSTATE {state} after {WITHIN:?} and {attempts} \
             attempts, so the cluster is not settling: {error}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run(sql: &mut cluster::Session, statement: &str) -> usize {
    match sql.run(statement) {
        Ok(Outcome::Rows { rows, .. }) => rows.len(),
        Ok(_) => 0,
        Err(error) => panic!(
            "`{statement}` failed with SQLSTATE {}: {error}",
            error.sqlstate()
        ),
    }
}
