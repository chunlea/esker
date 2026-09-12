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
//! them with its own executor. No placement driver: what is under test is the store path, so the
//! oracle is local — one node is the one case a local counter would be correct for, and
//! `tests/two_nodes_one_clock.rs` is where that stops being true.
//!
//! **But it counts in milliseconds, not in tokens** ([`TickingOracle`]). A bare `CountingOracle`
//! is correct about ordering and silent about *leases*, and a file that kills a leader needs the
//! second half: a lock orphaned between a prewrite and its commit is cleared by its TTL running
//! out, and a TTL is compared in the timestamp's physical half.
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
use esker_client::{
    ClientOptions, LOCK_TTL_MS, Router, TcpStores, TimestampOracle, TxnClient, is_expired,
    physical_ms, ts_at_ms,
};
use esker_proto::{ProtoError, Server, ServerHandle, TransportConfig};
use esker_sql::backend::{Backend, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::session::Outcome;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};

const REGION: u64 = 1;
const TENANT: u64 = 1;
const STORES: usize = 3;

/// How long a test waits for the cluster's first election before giving up on its precondition.
/// An election here is 10–20 ticks at 5 ms, so this is three orders of magnitude of headroom —
/// the gate runs several clusters at once, and a fixture that fails for being on a busy machine
/// is testing the machine.
const SETTLE: Duration = Duration::from_secs(60);

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
            // **Twenty-five, because five churns leadership on scheduler noise.** The election
            // timeout is 10–20 ticks (`esker_raft::config`), so a five-millisecond tick puts it at
            // 50–100 ms — below the delay a busy machine can impose on a thread, and two live
            // replicas then keep taking the term off each other instead of electing one leader.
            // Every proposal in flight answers `40003 … region 1 stopped leading with this
            // proposal in its log`, which is what three gate runs and a loaded reproduction here
            // all show. `chaos_cluster/mod.rs` already wrote the rule down — "a tick that is too
            // short churns leadership on scheduler noise alone" — and every other fixture in this
            // repository that kills a node uses this number (`txn_cluster`, `retire`, `census`,
            // `balance`). Production is 100 ms; this is still four times faster than that.
            raft.tick = Duration::from_millis(25);
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

    /// Waits for an election, and answers whether one happened.
    ///
    /// **A precondition and not a measurement**: the tests below take a leader *away*, so one has
    /// to exist first. Its bound is a wall clock because there is nothing else here that scales
    /// with a busy machine — which is exactly why it is generous rather than tight, and why no
    /// assertion in this file about what the node *did* is written this way.
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
        let oracle: Arc<dyn TimestampOracle> = Arc::new(TickingOracle::new());
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

/// A timestamp oracle whose **physical half advances with real time**, because a lease depends on
/// it.
///
/// # What the counting oracle could not do here
///
/// `CountingOracle::starting_at(1_000)` hands out `1000, 1001, …`, and `esker_txn::physical_ms` is
/// `ts >> TSO_LOGICAL_BITS` with eighteen logical bits — so every timestamp this test could reach
/// reads as **physical millisecond zero**. `esker_txn::is_expired` compares in exactly that domain:
///
/// ```text
/// is_expired(1000, LOCK_TTL_MS, now) == physical_ms(now) > physical_ms(1000) + LOCK_TTL_MS
///                                    == 0 > 0 + 1000
///                                    == false, for every timestamp this fixture will ever mint
/// ```
///
/// So a lock minted here **never expires**, and `Transaction::classify` answers `Alive` about it
/// for ever. That is fine while every transaction finishes: nothing is ever left behind to expire.
/// It stops being fine the moment this file's kill lands between a prewrite and its commit — the
/// lock that prewrite wrote is then orphaned, and *every* later statement meets it, waits out the
/// client's resolution budget and answers
///
/// ```text
/// 40001 … a lock from the transaction at 1000 could not be cleared for a read
/// ```
///
/// which is what the gate saw 120 times in a row over 632 seconds. Retrying cannot help: each
/// retry is a new transaction and none of them can clear ts 1000. The design says so itself —
/// `Transaction::prewrite_or_roll_back`'s "what it cannot clean the TTL still will" — and here the
/// TTL never will.
///
/// # Why this shape
///
/// The same one `esker-client`'s `txn_cluster::SharedOracle` uses, for the sentence written beside
/// it there: *"the physical half has to advance with real time or no lease ever runs out."* That is
/// the other real-cluster fixture that kills leaders, and it reached this answer first.
///
/// `catalog_contention.rs` relies on the **opposite** property on purpose — "`CountingOracle` has
/// no physical clock, so this lock never expires" is how it makes an unresolvable lock
/// deterministic. Same fact, wanted one way there and the other way here.
#[derive(Debug)]
struct TickingOracle {
    state: std::sync::Mutex<esker_pd::tso::Oracle>,
    /// Wall-clock milliseconds at construction, and the monotonic instant they were read at.
    epoch_ms: u64,
    started: Instant,
}

impl TickingOracle {
    fn new() -> Self {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0));
        Self {
            state: std::sync::Mutex::new(esker_pd::tso::Oracle::load(None, epoch_ms, 3_000)),
            epoch_ms,
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch_ms
            .saturating_add(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }
}

impl TimestampOracle for TickingOracle {
    fn tso(&self, count: u32) -> Result<u64, ProtoError> {
        let now = self.now_ms();
        let mut oracle = self
            .state
            .lock()
            .map_err(|_| ProtoError::invalid("the oracle's lock was poisoned"))?;
        // Nothing here restarts, so there is nothing for the high-water mark to be durable
        // against; a real PD persists it before handing out a timestamp above it.
        oracle
            .allocate(count.max(1), now, |_mark| Ok(()))
            .map_err(|error| ProtoError::invalid(error.to_string()))
    }
}

/// **What the two tests below silently depend on**, asserted so that it cannot be taken away
/// again: a lock minted by this file's oracle can die of old age.
///
/// This is the whole of the defect [`TickingOracle`] documents, written as the arithmetic it is —
/// no cluster, no kill, no load, and no way to flake. It goes red the moment this fixture is given
/// an oracle without a physical half, which is the state it spent a night failing the gate in:
/// 120 attempts, 632 seconds, `40001 … a lock from the transaction at 1000 could not be cleared
/// for a read`, every one of them.
///
/// The negative half is about a counter and not about a type, deliberately: if `CountingOracle`
/// ever grows a clock this assertion is the one that says so, and the right answer then is to
/// delete this half rather than to work around it.
#[test]
fn a_lock_this_fixture_mints_can_run_out_of_lease() {
    let oracle = TickingOracle::new();
    let minted = oracle.timestamp().expect("the oracle answers");

    assert!(
        physical_ms(minted) > 1_600_000_000_000,
        "this fixture's oracle stamps physical millisecond {}, which is not a wall clock — a lease \
         is compared in that half and one that never moves never runs out",
        physical_ms(minted)
    );
    assert!(
        is_expired(
            minted,
            LOCK_TTL_MS,
            ts_at_ms(physical_ms(minted) + LOCK_TTL_MS + 1)
        ),
        "a lock minted at {minted} is still alive {} ms later, so a lock this file's kill orphans \
         could never be cleared and every later statement would answer 40001",
        LOCK_TTL_MS + 1
    );

    // The counting oracle this file used to hold, as arithmetic: it starts at 1,000 and hands out
    // consecutive integers, and `physical_ms` is a shift of eighteen bits — so a *million*
    // timestamps later it has aged three milliseconds against a three-second lease.
    assert_eq!(
        physical_ms(1_000),
        0,
        "a counter's timestamps read as physical millisecond zero"
    );
    assert!(
        !is_expired(1_000, LOCK_TTL_MS, 1_000 + 1_000_000),
        "a lock at 1,000 is still alive a million timestamps later, which is why a counting \
         oracle cannot be used by a fixture that can orphan a lock"
    );
}

/// **The acceptance.** A statement after the leading store is taken away must answer, not `08006`.
#[test]
fn a_statement_answers_after_the_store_leading_its_region_is_killed() {
    let cluster = Cluster::start();
    assert!(
        cluster.settle(SETTLE),
        "the cluster never elected a leader to take away"
    );
    let (mut sql, _catalog) = cluster.sql_node();

    setup(&mut sql);

    let leader = cluster.leader().expect("somebody leads");
    cluster.kill(leader);

    // **One statement.** Not a loop: a client gets one answer per statement, and Rails' fixture
    // setup does not ask twice — which is why the node has to. The clock below is read for the
    // message and for the record, and decides nothing.
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
    // **Measured, not asserted** (LANE-RULES). The acceptance is that the statement *answers* —
    // run 124's failure was 184 refusals in 0.695 s, fast and wrong, and the panic above is what
    // catches it. How long the answer took is a number about the machine the gate is sharing, so
    // it is printed for the next reader of a slow run and left out of the verdict.
    println!(
        "the statement answered {} ms after the leader was killed",
        took.as_millis()
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
        cluster.settle(SETTLE),
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
    /// How many times the fixture asks before it calls the cluster stuck.
    ///
    /// **Attempts and not a clock** (LANE-RULES: a test inside the gate carries no wall-clock
    /// assertion). This used to give up 30 seconds in, and a gate running several clusters at once
    /// took longer than that to finish an election — so the fixture failed for being on a busy
    /// machine, which is the one thing it is not testing. A count is load-tolerant in the way a
    /// deadline is not: each attempt already carries the client's own timeout, so the wall time
    /// this spans grows with the load rather than running out under it, while a cluster that is
    /// never going to settle still fails rather than hangs.
    ///
    /// **Forty and not a hundred and twenty.** The first number was picked to be generous, and a
    /// loaded round spent 546 seconds exhausting it and failed anyway — a budget is not what
    /// saves this fixture, the tick above is, so the budget's job is only to keep a genuine
    /// failure short. Forty attempts is about thirty seconds of asking per statement, ten times
    /// an election at the tick this file now uses.
    const ATTEMPTS: usize = 40;
    let mut attempts = 0usize;
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
            attempts < ATTEMPTS,
            "`{statement}` was still answering SQLSTATE {state} after {ATTEMPTS} attempts, so the \
             cluster is not settling: {error}"
        );
        // Backing off rather than a fixed pause, so a cluster that needs a second election is
        // waited for with the same number of attempts as one that needs none.
        std::thread::sleep(Duration::from_millis(50 * attempts.min(20) as u64));
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
