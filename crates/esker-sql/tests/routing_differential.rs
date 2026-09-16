//! **The test that has to exist before routing is default-on**, on a real cluster.
//!
//! `docs/plans/phase-10-routing.md` U5, and ADR 0022 asks for it by name and says why:
//!
//! > the worst failure this feature can have \[is\] the two engines disagreeing — a query answered
//! > one way from the row store and another way from the columnar one. That is silent, and the
//! > only defence is a differential test that runs every query both ways and compares, which has
//! > to be built **before** the routing rule and not after.
//!
//! Every query here is run twice **at the same instant**: once as the planner routes it, and once
//! with `SET esker.engine = 'row'`. The second is not a second call to the same code — it is the
//! row executor over Percolator's `write` records, which shares the *rule* with the columnar path
//! and none of its code. A disagreement dumps both sides and fails.
//!
//! # What is real here and what is not
//!
//! Everything: a real placement driver, four real stores over real sockets, a real columnar
//! learner placed by an `ALTER`, the real fragment service, and a SQL node routed by PD's own
//! `GetRegion` rather than by a table a test wrote down. This is what closes phase 8's
//! *"nothing on a real cluster can ask a fragment"*.
//!
//! The one thing that is not a separate process is the process boundary itself — PD, the stores
//! and the SQL node share this one — which is the same honest boundary the joint gate draws.
//!
//! # The helpers below are copied from `tests/joint_gate.rs`, deliberately
//!
//! That file belongs to another lane and this one does not edit it. What is copied is the *cluster
//! construction*; what is new is a SQL node that can ask a fragment, which is the whole subject.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use esker_client::region_cache::RegionResolver;
use esker_client::router::{ClientOptions, Router};
use esker_client::{TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{PeerRole, Server, ServerHandle, Service, TransportConfig};
use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{Answer, ClientFragments, FragmentSource, RefusalReason, Shard};
use esker_sql::pd::{ColumnarReport, LeaseRefresher, PdConn, PdLease};
use esker_store::server::RaftOptions;
use esker_store::{LogCompaction, PeerAddress, RemotePd, Store, StoreOptions, StoreService};

use cluster::{Session, TENANT};

/// Voters a region keeps.
const VOTERS: usize = 3;
/// Stores in the cluster: one more than the voters, because a columnar learner is placed on the
/// healthiest store **without a peer** and a cluster with no spare has nowhere to put one.
const STORES: u64 = 4;

/// The queries compared. Every one is an aggregate, because that is the shape this milestone
/// routes, and between them they cover what a fold can get wrong.
const QUERIES: &[&str] = &[
    "SELECT count(*) FROM t",
    "SELECT count(amount) FROM t",
    "SELECT sum(amount) FROM t",
    "SELECT min(amount), max(amount) FROM t",
    "SELECT avg(rate) FROM t",
    "SELECT region, count(*) FROM t GROUP BY region ORDER BY region",
    "SELECT region, sum(amount), min(amount), max(amount) FROM t GROUP BY region ORDER BY region",
    "SELECT count(*) FROM t WHERE amount > 20",
    "SELECT region, count(*) FROM t WHERE amount > 20 GROUP BY region ORDER BY region",
    "SELECT count(*), sum(amount) FROM t WHERE region = 'north'",
];

/// Joins that **must** reach the columnar path once the join fragment lands.
///
/// Each is an aggregate over a fact table joined to a dimension on the dimension's **primary
/// key**, which is what makes the semi-join rewrite exact: `Probe::PrimaryKey` means at most one
/// inner row per outer row, so replacing the join with a membership test cannot change a count
/// (`docs/plans/phase-16-mpp.md` §J3).
const JOIN_QUERIES_THAT_MUST_ROUTE: &[&str] = &[
    "SELECT count(*) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 1",
    "SELECT count(*), sum(amount) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 1 AND f.amount > 20",
    "SELECT f.region, count(*) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 1 \
     GROUP BY f.region ORDER BY f.region",
    "SELECT count(*), min(amount), max(amount) FROM f JOIN d ON f.dk = d.k",
    // **The same condition where the user wrote it, not where the planner moved it.** Every query
    // above puts `d.bucket = 1` in the `WHERE`, and the fragment reads it out of the `Filter`
    // standing above the loop. Written in the `ON` there is no such `Filter`: the probe answers
    // `f.dk = d.k` and the rest becomes the join's **residual**, which is the only path by which
    // this condition can reach the key set. A build that dropped it answers **8** where these
    // answer 4 — a wrong count, not a slow one — and the three tests above do not notice,
    // because their copy in the `WHERE` narrows the key set whatever the residual does.
    "SELECT count(*) FROM f JOIN d ON f.dk = d.k AND d.bucket = 1",
    "SELECT count(*), sum(amount) FROM f JOIN d ON f.dk = d.k AND d.bucket = 1 WHERE f.amount > 20",
];

/// Joins that must **not** reach it, each failing a different one of §J3's conditions.
///
/// They are here for the same reason the routed ones are: over-routing is the failure this whole
/// milestone has to avoid, and a rewrite that quietly answered one of these would return a wrong
/// number rather than a slow one.
const JOIN_QUERIES_THAT_MUST_REFUSE: &[(&str, &str)] = &[
    (
        "SELECT d.label, count(*) FROM f JOIN d ON f.dk = d.k GROUP BY d.label ORDER BY d.label",
        "an inner column is grouped by, and a filter yields no dimension values",
    ),
    (
        "SELECT count(*) FROM f LEFT JOIN d ON f.dk = d.k",
        "a LEFT JOIN keeps unmatched outer rows, and a filter removes them",
    ),
    (
        "SELECT count(*) FROM f JOIN d ON f.dk = d.bucket",
        "the inner side is joined on a non-unique column, so one outer row may match many",
    ),
];

/// **The join differential, red until the join fragment lands.**
///
/// `docs/plans/phase-16-mpp.md` §J8.1. Two assertions, and the second is the one that is red:
/// every query agrees with the row engine (true today, trivially, because every one of them *is*
/// the row engine), and every query in [`JOIN_QUERIES_THAT_MUST_ROUTE`] was actually answered by
/// the columns (false today, because `exec::mod`'s guard excludes any select with joins).
///
/// The first assertion without the second is worth nothing: a query that fell back agrees with the
/// row engine for free. That is ADR 0040 Decision 5's lesson — its bug surfaced as *"this query
/// was not answered by the columns"* rather than as a wrong number — and it is why this test
/// asserts its own denominator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_over_columnar_tables_answers_what_the_row_engine_answers() {
    let gate = Gate::start().await;
    gate.fill_join().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        for query in JOIN_QUERIES_THAT_MUST_ROUTE {
            assert!(
                compare(&gate, &mut session, query, " (join)"),
                "`{query}` was not answered by the columns, so its agreement is free"
            );
        }
    });

    gate.stop().await;
}

/// **The third silence, closed.** `EXPLAIN` names an engine for a join.
///
/// `crates/esker-sql/src/exec/mod.rs` used to call the router only for a select with no joins, so
/// a joined plan carried no decision and printed no `Engine:` line at all — while `SELECT *` over
/// the same table printed `Engine: rows` with a reason. ADR 0040 Decision 3 lists two deliberate
/// silences and this was a third, undocumented one: a reader debugging *"why is my join not on
/// the columns"* got nothing back.
///
/// Every join now says which rule refused it, and two of them are checked by name — because a
/// refusal for the wrong reason is a bug that a test asserting only "it fell back" would pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explain_names_an_engine_for_a_join() {
    let gate = Gate::start().await;
    gate.fill_join().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        for query in JOIN_QUERIES_THAT_MUST_ROUTE
            .iter()
            .chain(JOIN_QUERIES_THAT_MUST_REFUSE.iter().map(|(query, _)| query))
        {
            let plan = explain(&mut session, &format!("EXPLAIN {query}"));
            assert!(
                plan.contains("Engine:"),
                "a joined plan printed no engine line, which is the silence this closes:\n{plan}"
            );
        }

        // The two conditions that are checked today name themselves. The rest refuse with "not
        // yet", which is the honest sentence for a half that is not built.
        let left = explain(
            &mut session,
            "EXPLAIN SELECT count(*) FROM f LEFT JOIN d ON f.dk = d.k",
        );
        assert!(
            left.contains("LEFT JOIN"),
            "a LEFT JOIN must say so:\n{left}"
        );
        let materialised = explain(
            &mut session,
            "EXPLAIN SELECT count(*) FROM f JOIN d ON f.dk = d.bucket",
        );
        assert!(
            materialised.contains("non-unique"),
            "a non-unique inner column must say so:\n{materialised}"
        );
    });

    gate.stop().await;
}

/// **The join is visibly a semi-join, not just fast.** `EXPLAIN ANALYZE` names the table the keys
/// came from and how many there were.
///
/// A plan that absorbed a join has no `Nested Loop` in it any more, so without this line a reader
/// sees an aggregate over one table and no account of where the join went
/// (`docs/plans/phase-16-mpp.md` §J6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explain_shows_the_join_it_absorbed() {
    let gate = Gate::start().await;
    gate.fill_join().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(&mut session, "SET esker.engine = 'auto'");
        let plan = explain(
            &mut session,
            "EXPLAIN ANALYZE SELECT count(*) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 1",
        );
        assert!(
            plan.contains("Engine: columnar"),
            "the join did not run on the columns:\n{plan}"
        );
        assert!(
            plan.contains("Semi Join Filter: dk in d"),
            "the absorbed join is not named:\n{plan}"
        );
        // `d` holds k = 1..=4 and `bucket = k % 2`, so `bucket = 1` selects k = 1 and 3.
        assert!(
            plan.contains("(2 keys)"),
            "the key count is wrong; d.bucket = 1 selects two of four:\n{plan}"
        );
    });

    gate.stop().await;
}

/// An inner side that matches nothing must answer nothing — and must not be expressed as an empty
/// `IN` list, which the fragment format refuses (ADR 0074).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_whose_inner_side_is_empty_answers_zero_on_both_engines() {
    let gate = Gate::start().await;
    gate.fill_join().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        assert!(
            compare(
                &gate,
                &mut session,
                "SELECT count(*) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 99",
                " (empty inner side)"
            ),
            "an empty inner side was not answered by the columns"
        );
    });

    gate.stop().await;
}

/// The other half: a join the rewrite must not take, and does not.
///
/// Green today and green afterwards, which is the point — it is the guard that says the join
/// fragment did not become a wrong answer while it was becoming a fast one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_the_rewrite_cannot_express_stays_on_the_rows() {
    let gate = Gate::start().await;
    gate.fill_join().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        for (query, why) in JOIN_QUERIES_THAT_MUST_REFUSE {
            let answered_by_columns = compare(&gate, &mut session, query, " (refused join)");
            assert!(
                !answered_by_columns,
                "`{query}` ran on the columns and must not have: {why}"
            );
        }
    });

    gate.stop().await;
}

/// **The differential.** Every query, both engines, one snapshot, and they agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_routed_query_answers_what_the_row_engine_answers() {
    let gate = Gate::start().await;
    gate.fill().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        for query in QUERIES {
            assert!(
                compare(&gate, &mut session, query, ""),
                "`{query}` was not answered by the columns, so its agreement is free"
            );
        }
    });

    gate.stop().await;
}

/// The same, under a writer that never stops.
///
/// **This is the arm that exercises the fallback rather than assuming it.** A learner behind a
/// stream of commits refuses `TooFarBehind`, and a refusal must be answered by the rows *in the
/// same snapshot* — so the client's answer is the one the snapshot always had, whichever engine
/// produced it. Under concurrency that is not a claim a reader can check by inspection; it is what
/// this test is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_engines_agree_while_a_writer_keeps_committing() {
    let gate = Gate::start().await;
    gate.fill().await;

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let backend = Arc::clone(&gate.backend);
        let catalog = Arc::clone(&gate.catalog);
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("differential-writer".to_owned())
            .spawn(move || {
                let mut session = Session {
                    executor: Executor::new(
                        backend,
                        catalog,
                        TENANT,
                        esker_sql::session::register(),
                    ),
                };
                let mut id = 1_000_i64;
                while !stop.load(Ordering::Relaxed) {
                    // A failure here is not the test's subject — a writer racing a reader loses
                    // sometimes — so it is counted by not stopping rather than by panicking on a
                    // thread whose panic would be reported somewhere else.
                    let _ = session.run(&format!(
                        "INSERT INTO t VALUES ({id}, 'north', {}, 1.0)",
                        id % 50
                    ));
                    id += 1;
                }
                id - 1_000
            })
            .unwrap()
    };

    let rounds = tokio::task::block_in_place(|| {
        let mut session = gate.session();
        let mut rounds = 0;
        let mut columnar = 0;
        for _ in 0..3 {
            for query in QUERIES {
                columnar += usize::from(compare(&gate, &mut session, query, " (under a writer)"));
                rounds += 1;
            }
        }
        (rounds, columnar)
    });
    let (rounds, columnar) = rounds;

    stop.store(true, Ordering::Relaxed);
    let written = writer.join().unwrap();
    // **The denominator, printed rather than assumed.** A round that fell back agrees for free, so
    // what this test proves depends on how many did not — and how many *did* fall back is what it
    // is for. Neither number is asserted to a value, because both depend on a race; what is
    // asserted is that the comparisons ran and that the writer really wrote.
    println!(
        "{columnar} of {rounds} comparisons were answered by the columns; {written} rows written"
    );
    assert!(rounds >= 30, "only {rounds} comparisons ran");
    assert!(
        written > 0,
        "the writer committed nothing, so nothing was raced"
    );
    assert!(
        columnar > 0,
        "nothing was routed under the writer, so every agreement here is free"
    );

    gate.stop().await;
}

/// A fragment really is what answered, and `EXPLAIN ANALYZE` says so.
///
/// **Without this the differential can pass by proving nothing**: a query that fell back to rows
/// is compared with the row engine and agrees trivially. This is the assertion that says the
/// columnar path was exercised on a real cluster at all — the sentence phase 8 closed without.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_cluster_answers_a_select_from_its_columnar_learner() {
    let gate = Gate::start().await;
    gate.fill().await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        let plan = explain(&mut session, "EXPLAIN SELECT count(*) FROM t");
        assert!(plan.contains("Columnar Aggregate on t"), "{plan}");
        assert!(plan.contains("Engine: columnar"), "{plan}");

        let ran = explain(&mut session, "EXPLAIN ANALYZE SELECT count(*) FROM t");
        assert!(
            ran.contains("Fragments: 1 asked, 1 answered"),
            "the fragment did not answer:\n{ran}"
        );
        // The learner really read the file: stripes and rows, not zeroes.
        assert!(
            ran.contains("Rows: 12 scanned"),
            "the scan reported no work:\n{ran}"
        );
        assert_eq!(
            rows(&mut session, "SELECT count(*) FROM t"),
            vec![vec![Some("12".to_owned())]]
        );
    });

    gate.stop().await;
}

/// **The fallback, forced rather than waited for.** With the learner's store stopped, the query
/// still answers what it always did — and `EXPLAIN ANALYZE` says the columns were tried.
///
/// A refusal is silent to the client by design, because the answer is the same either way. That
/// makes it exactly the thing a test must produce on purpose: waiting for one to happen under load
/// is how a fallback path stays untested for a release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_answers_the_same_when_the_learner_is_gone() {
    let gate = Gate::start().await;
    gate.fill().await;

    let before = tokio::task::block_in_place(|| {
        let mut session = gate.session();
        rows(
            &mut session,
            "SELECT region, count(*), sum(amount) FROM t GROUP BY region ORDER BY region",
        )
    });

    gate.stop_the_learner();

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        let after = rows(
            &mut session,
            "SELECT region, count(*), sum(amount) FROM t GROUP BY region ORDER BY region",
        );
        assert_eq!(
            after, before,
            "the answer changed when the learner went away"
        );

        let plan = explain(
            &mut session,
            "EXPLAIN ANALYZE SELECT region, count(*), sum(amount) FROM t GROUP BY region",
        );
        assert!(plan.contains("Engine: rows"), "{plan}");
        assert!(
            plan.contains("columnar refused") || plan.contains("could not be reached"),
            "the plan does not say the columns were tried:\n{plan}"
        );
        // The row plan that answered instead is printed beneath it.
        assert!(plan.contains("Seq Scan on t"), "{plan}");
    });

    gate.stop().await;
}

/// One query, two engines, one instant.
///
/// The snapshot is pinned with `esker.read_as_of` so that both runs read the *same* history even
/// while a writer is committing — otherwise a disagreement could be two different moments rather
/// than two different answers, which is the one way this test could cry wolf.
fn compare(gate: &Gate, session: &mut Session, query: &str, note: &str) -> bool {
    let at = gate.now();
    let (routed, plan) = at_snapshot(session, "auto", at, query);
    let (by_rows, _) = at_snapshot(session, "row", at, query);

    if routed != by_rows {
        // **Ask again, at the same instant, before saying anything.**
        //
        // A snapshot is a function: the same `ts` must answer the same rows for ever, whichever
        // engine reads it. So a second disagreeing pair is a *wrong answer* and a second agreeing
        // pair is something that moved while the first pair was being taken — and those two want
        // opposite investigations. The gate log for `76001434` had one of these and could not say
        // which, so the next one says it itself.
        //
        // The order is reversed on purpose: rows first, then routed. If the disagreement follows
        // the *order* rather than the engine, that is the tell for a read that is not pinned at
        // all.
        let (rows_again, rows_plan) = at_snapshot(session, "row", at, query);
        let (routed_again, plan_again) = at_snapshot(session, "auto", at, query);
        panic!(
            "the two engines disagree{note} on `{query}` at {at}\n\
             routed:       {routed:?}\n\
             rows:         {by_rows:?}\n\
             rows again:   {rows_again:?}   (same instant, asked second)\n\
             routed again: {routed_again:?}   (same instant, asked second)\n\
             stable?       routed {}, rows {} — a snapshot that answers differently twice is not \
             pinned; one that answers the same twice is a wrong answer rather than a moving one\n\
             applied indexes, taken now:\n{}\
             plan:\n{plan}\n\
             plan again:\n{plan_again}\n\
             row plan:\n{rows_plan}",
            if routed == routed_again {
                "stable"
            } else {
                "MOVED"
            },
            if by_rows == rows_again {
                "stable"
            } else {
                "MOVED"
            },
            gate.witness(),
        );
    }
    // **Whether the columns actually answered**, from the plan that ran rather than from the plan
    // that was made: a query that fell back is compared with the row engine and agrees trivially,
    // so a caller that did not look at this would be counting agreements it got for free.
    plan.contains("Engine: columnar")
}

/// One query on one engine at one instant, with the plan that answered it.
///
/// **Reached by token, not by instant.** `SET esker.read_as_of` takes a time a user can write, and
/// a TSO timestamp is not one — its physical half is milliseconds and its logical half is what
/// separates two commits inside one. `SET TRANSACTION SNAPSHOT` takes an exported token that
/// carries the `start_ts` whole (`esker_sql::time_machine`'s `esker-<16 hex>`), which is the only
/// spelling that names *this* instant rather than the millisecond it fell in.
fn at_snapshot(
    session: &mut Session,
    engine: &str,
    at: u64,
    query: &str,
) -> (Vec<Vec<Option<String>>>, String) {
    // Outside the block: a `SET` is not part of the transaction it precedes, and `SET TRANSACTION
    // SNAPSHOT` may only be called before any query in the block it is in.
    session
        .run(&format!("SET esker.engine = '{engine}'"))
        .expect("the override sets");
    session.run("BEGIN").expect("a block opens");
    session
        .run(&format!("SET TRANSACTION SNAPSHOT 'esker-{at:016x}'"))
        .expect("the snapshot imports");
    let answer = rows(session, query);
    let plan = explain(session, &format!("EXPLAIN ANALYZE {query}"));
    session.run("COMMIT").expect("the block closes");
    session
        .run("RESET esker.engine")
        .expect("the override clears");
    (answer, plan)
}

// ---------------------------------------------------------------------------------------------
// The cluster
// ---------------------------------------------------------------------------------------------

/// A placement driver, four stores, and one SQL node that can ask a fragment.
struct Gate {
    pd: Arc<Pd>,
    oracle: Arc<dyn TimestampOracle>,
    pd_handle: Option<ServerHandle>,
    nodes: Vec<Node>,
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    conn: Arc<PdConn>,
    fragments: Arc<dyn FragmentSource>,
    /// What every statement on this cluster spent at the `Backend` seam — debt #109.
    ///
    /// Always present, never behind a flag: see the note where it is installed.
    profile: Arc<cluster::profile::Profile>,
    /// The transaction client the node's backend is built on — 0117's stranded-lock fixture drives
    /// it directly, because a SQL session cannot leave a lock behind.
    client: Arc<TxnClient>,
}

struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Gate {
    async fn start() -> Self {
        // Never splits, which is every existing test in this file: they are about which engine
        // answers and what it answers, and one region is enough to ask that.
        Self::start_splitting(u64::MAX).await
    }

    /// A cluster whose regions **split at `split_size` bytes**, for the §10 re-measure: the
    /// fragment-count axis §9 could not produce needs a table that occupies more than one region.
    async fn start_splitting(split_size: u64) -> Self {
        Self::start_with(split_size, Duration::from_millis(5), 4).await
    }

    /// [`Gate::start_splitting`], with the two numbers that decide how much Raft one process is
    /// driving: the tick every group counts in, and how many threads the pool spreads them over.
    async fn start_with(split_size: u64, tick: Duration, workers: usize) -> Self {
        // Every cluster in this file goes through here. See `tests/trace`: the subscriber is
        // installed at the harness rather than remembered per test, so `RUST_LOG` works on the
        // test somebody is already debugging.
        cluster::trace::on();

        println!("{}", the_box_right_now("starting a cluster"));
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
                // Off, as PD's own columnar tests have it: this is about a placement an `ALTER`
                // caused, and balance moving a voter onto the spare store would decide where the
                // learner can go for reasons that have nothing to do with the statement.
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

        let mut nodes = Vec::new();
        for (at, address) in addresses.iter().enumerate() {
            drop(listeners.next());
            nodes.push(
                open_store(
                    *address,
                    at as u64 + 1,
                    pd_address,
                    &peers,
                    split_size,
                    tick,
                    workers,
                )
                .await,
            );
        }
        wait_for("the region to reach three voters", PLACEMENT_WITHIN, || {
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

        let oracle: Arc<dyn TimestampOracle> = Arc::new(esker_client::WallClockOracle::new());
        let (inner, conn, fragments, client) =
            tokio::task::block_in_place(|| sql_node(&addresses, pd_address, Arc::clone(&oracle)));

        // **Always wrapped, never behind a flag** (debt #109). A harness with a measured build and
        // an unmeasured one is two harnesses, and the numbers would describe the one nobody else
        // runs. What it costs is two relaxed atomic adds per call at a seam whose cheapest
        // operation is a buffered write.
        //
        // The wrapper goes on **after** `sql_node`, on purpose: the schema-lease refresher inside
        // it keeps the bare backend, because that thread is not a statement and its round trips
        // are nobody's `INSERT`.
        let profile = Arc::new(cluster::profile::Profile::default());
        let backend: Arc<dyn Backend> = Arc::new(cluster::profile::ProfiledBackend::new(
            inner,
            Arc::clone(&profile),
        ));

        Gate {
            pd,
            oracle,
            pd_handle: Some(pd_handle),
            nodes,
            backend,
            catalog: Arc::new(Catalog::new()),
            conn,
            fragments,
            profile,
            client,
        }
    }

    /// A session on this node, reporting columnar placement to PD and able to ask a fragment.
    fn session(&self) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                esker_sql::session::register(),
            )
            .reporting_columnar_to(Arc::clone(&self.conn) as Arc<dyn ColumnarReport>)
            .asking_fragments_of(Arc::clone(&self.fragments)),
        }
    }

    /// The table, its rows, its columnar copy — and a wait on the **observable** that the copy can
    /// answer, never on a sleep.
    /// A fact table with a columnar copy and a dimension keyed by its primary key.
    ///
    /// Six columns on the fact table so that a query projecting two passes the ratio
    /// (`esker_sql::plan::routing::RATIO`), and a dimension whose `bucket` repeats — which is the
    /// case a careless uniqueness argument gets wrong. The probe is on `d.k`, the primary key, so
    /// each fact row matches at most one dimension row however many share a bucket.
    ///
    /// Some `dk` are NULL and some match no dimension row, because an inner join drops both and a
    /// membership test must drop exactly the same ones.
    async fn fill_join(&self) {
        tokio::task::block_in_place(|| {
            let mut session = self.session();
            settle(
                &mut session,
                "CREATE TABLE f (id int8 PRIMARY KEY, dk int8, region text, amount int8, \
                 rate double precision, note text)",
            );
            settle(
                &mut session,
                "CREATE TABLE d (k int8 PRIMARY KEY, bucket int8, label text)",
            );
            settle(&mut session, "ALTER TABLE f SET (columnar_replicas = 1)");
            for k in 1..=4_i64 {
                settle(
                    &mut session,
                    &format!("INSERT INTO d VALUES ({k}, {}, 'label-{k}')", k % 2),
                );
            }
            for id in 1..=12_i64 {
                let region = if id % 3 == 0 { "north" } else { "south" };
                // `dk` cycles 1..=4 over the dimension, then NULL, then 99 which matches nothing:
                // an inner join drops the last two and so must the filter.
                let dk = match id % 6 {
                    4 => "NULL".to_owned(),
                    5 => "99".to_owned(),
                    other => (other + 1).to_string(),
                };
                let amount = if id % 4 == 0 {
                    "NULL".to_owned()
                } else {
                    (id * 7).to_string()
                };
                settle(
                    &mut session,
                    &format!(
                        "INSERT INTO f VALUES ({id}, {dk}, '{region}', {amount}, {id}.0, 'n{id}')"
                    ),
                );
            }
        });

        self.wait_for_a_learner_that_answers("f").await;
    }

    /// A join fixture with a **cost curve** in it, which `fill_join`'s twelve rows cannot have.
    ///
    /// `d` holds `inner` keys and `f` holds `outer` rows whose `dk` cycles over them, so a
    /// `WHERE d.k <= N` selects exactly N keys and every one of them matches. Rows go in five
    /// hundred at a time: one statement per row is one transaction per row, and twenty thousand of
    /// those is the measurement's own cost rather than the thing being measured.
    async fn fill_join_at_scale(&self, inner: i64, outer: i64) {
        tokio::task::block_in_place(|| {
            let mut session = self.session();
            settle(
                &mut session,
                "CREATE TABLE f (id int8 PRIMARY KEY, dk int8, amount int8)",
            );
            settle(
                &mut session,
                "CREATE TABLE d (k int8 PRIMARY KEY, label text)",
            );
            settle(&mut session, "ALTER TABLE f SET (columnar_replicas = 1)");
            for chunk in (1..=inner).collect::<Vec<i64>>().chunks(500) {
                let values: Vec<String> = chunk.iter().map(|k| format!("({k}, 'l{k}')")).collect();
                settle(
                    &mut session,
                    &format!("INSERT INTO d VALUES {}", values.join(", ")),
                );
            }
            for chunk in (1..=outer).collect::<Vec<i64>>().chunks(500) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, {}, {id})", (id - 1) % inner + 1))
                    .collect();
                settle(
                    &mut session,
                    &format!("INSERT INTO f VALUES {}", values.join(", ")),
                );
            }
        });
        self.wait_for_a_learner_that_answers("f").await;
    }

    /// How many regions the cluster has, as PD sees them.
    fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    fn regions(&self) -> usize {
        self.pd.regions().map_or(0, |regions| regions.len())
    }

    /// **The distinct stores holding a columnar learner**, which is the number §10 item 2 says a
    /// verdict must read instead of the region count: an exchange's parallelism is the number of
    /// *nodes* holding fragments, and PD places a learner on the healthiest store without a peer of
    /// that region — so with three voters and four stores, learners cluster. Five regions on two
    /// stores is what the only multi-region cluster anyone had run turned out to be.
    fn learner_stores(&self) -> usize {
        let Ok(regions) = self.pd.regions() else {
            return 0;
        };
        let mut stores: BTreeSet<u64> = BTreeSet::new();
        for record in regions {
            for peer in &record.region.peers {
                if peer.role == PeerRole::ColumnarLearner {
                    stores.insert(peer.store_id);
                }
            }
        }
        stores.len()
    }

    /// A table with three grouping columns of very different cardinality, so the **finish** can be
    /// varied without touching the row count or the region count.
    async fn fill_grouped(&self, rows: i64) {
        tokio::task::block_in_place(|| {
            let mut session = self.session();
            settle(
                &mut session,
                "CREATE TABLE t (id int8 PRIMARY KEY, g1 int8, g100 int8, gmax int8, amount int8)",
            );
            settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
            for chunk in (1..=rows).collect::<Vec<i64>>().chunks(500) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, 1, {}, {id}, {id})", id % 100))
                    .collect();
                settle(
                    &mut session,
                    &format!("INSERT INTO t VALUES {}", values.join(", ")),
                );
            }
        });
        self.wait_until_the_columns_answer("t").await;
    }

    /// Waits until a routed plan over `table` says the columns answered.
    ///
    /// **Not "1 asked, 1 answered"**, which is what `wait_for_a_learner_that_answers` checks and
    /// which is only true on a cluster of one region. Here the count is the region count and the
    /// point is to be indifferent to it.
    async fn wait_until_the_columns_answer(&self, table: &str) {
        let began = Instant::now();
        let deadline = began + Duration::from_secs(ROUTED_WITHIN);
        loop {
            let answered = tokio::task::block_in_place(|| {
                let mut session = self.session();
                explain(
                    &mut session,
                    &format!("EXPLAIN ANALYZE SELECT count(*) FROM {table}"),
                )
                .contains("Engine: columnar")
            });
            if answered {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no routed plan over {table} was answered by the columns after {:?}, \
                 and the bound is {ROUTED_WITHIN} s",
                began.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn fill(&self) {
        tokio::task::block_in_place(|| {
            let mut session = self.session();
            settle(
                &mut session,
                "CREATE TABLE t (id int8 PRIMARY KEY, region text, amount int8, \
                 rate double precision)",
            );
            settle(&mut session, "ALTER TABLE t SET (columnar_replicas = 1)");
            for id in 1..=12_i64 {
                let region = if id % 3 == 0 { "north" } else { "south" };
                // A NULL every fourth row, so `count(*)` and `count(col)` differ and `sum` meets
                // one.
                let amount = if id % 4 == 0 {
                    "NULL".to_owned()
                } else {
                    (id * 7).to_string()
                };
                settle(
                    &mut session,
                    &format!("INSERT INTO t VALUES ({id}, '{region}', {amount}, {id}.0)"),
                );
            }
        });

        self.wait_for_a_learner_that_answers("t").await;
    }

    /// Waits until a columnar learner is placed **and has answered a fragment over `table`**.
    ///
    /// Factored out of [`Gate::fill`] unchanged so a second fixture can use it. The observable is
    /// an answer, not a placement: a learner that exists and has not caught up refuses, and a test
    /// that started comparing then would be comparing the row engine with itself.
    async fn wait_for_a_learner_that_answers(&self, table: &str) {
        wait_for("PD to place a columnar learner", PLACEMENT_WITHIN, || {
            self.pd.regions().is_ok_and(|regions| {
                regions.iter().any(|record| {
                    record
                        .region
                        .peers
                        .iter()
                        .any(|peer| peer.role == PeerRole::ColumnarLearner)
                })
            })
        })
        .await;

        let began = Instant::now();
        let deadline = began + Duration::from_secs(PLACEMENT_WITHIN);
        loop {
            let answered = tokio::task::block_in_place(|| {
                let mut session = self.session();
                explain(
                    &mut session,
                    &format!("EXPLAIN ANALYZE SELECT count(*) FROM {table}"),
                )
                .contains("Fragments: 1 asked, 1 answered")
            });
            if answered {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the columnar learner never answered a fragment over {table} after {:?}, \
                 and the bound is {PLACEMENT_WITHIN} s",
                began.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stops the store PD placed the columnar learner on.
    ///
    /// By **role**, from PD's own records, because which store it is is a placement decision and
    /// not something a test may assume.
    fn stop_the_learner(&self) {
        let placed = self
            .pd
            .regions()
            .unwrap()
            .iter()
            .flat_map(|record| record.region.peers.clone())
            .find(|peer| peer.role == PeerRole::ColumnarLearner)
            .expect("a columnar learner has been placed")
            .store_id;
        for node in &self.nodes {
            if node.store.store_id() == placed {
                node.store.stop();
                return;
            }
        }
        panic!("the store PD placed the learner on is not one of ours");
    }

    /// An instant both engines can be asked about.
    fn now(&self) -> u64 {
        self.oracle.timestamp().unwrap()
    }

    /// **What every store had applied, at the moment a disagreement was found.**
    ///
    /// The third number the investigation asked for, and the only one of the three that exists. A
    /// fragment's runs have no `[lo, hi]` timestamp window to print: they hold **every version**
    /// and visibility is resolved at read time — *"the newest version of each key with
    /// `commit_ts <= ts`"* (`esker_columnar::scan::visible`) — so a run is bounded by an **apply
    /// index**, not by a time. What stands in for that window is how far each replica has applied,
    /// which is exactly what "the learner is behind" means.
    ///
    /// Taken in process, from the stores this harness owns. Nothing is added to the wire for it:
    /// `FragmentResp` carries no apply index, and putting one there would be a format change for a
    /// diagnostic.
    fn witness(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (at, node) in self.nodes.iter().enumerate() {
            for status in node.store.region_statuses() {
                let _ = writeln!(
                    out,
                    "  store {} region {} applied {} leader {} (self: {})",
                    at + 1,
                    status.region.id,
                    status.applied_index,
                    status.leader_peer_id,
                    status.is_leader
                );
            }
        }
        out
    }

    async fn stop(mut self) {
        println!("{}", the_box_right_now("stopping a cluster"));
        for node in self.nodes.drain(..) {
            node.store.stop();
            let _ = node.handle.shutdown().await;
        }
        if let Some(handle) = self.pd_handle.take() {
            let _ = handle.shutdown().await;
        }
    }
}

/// The SQL node, as the binary builds one with `--pd`: routed by PD's own `GetRegion`, holding a
/// schema lease, reporting columnar wishes, and able to ask a fragment.
///
/// **`PdConn` is the resolver**, which is the piece milestone 4 added: a static routing table
/// cannot answer "which peer is the columnar learner", because a learner joins through a conf
/// change after the table was written down.
/// What [`sql_node`] hands back: the backend a session runs on, the placement driver's connection,
/// the fragment source, and the transaction client — the last for fixtures that have to speak the
/// wire directly, which a SQL session cannot do.
type SqlNode = (
    Arc<dyn Backend>,
    Arc<PdConn>,
    Arc<dyn FragmentSource>,
    Arc<TxnClient>,
);

fn sql_node(
    addresses: &[SocketAddr],
    pd_address: SocketAddr,
    oracle: Arc<dyn TimestampOracle>,
) -> SqlNode {
    let stores = TcpStores::connect_all(addresses, TransportConfig::new()).unwrap();
    let conn = Arc::new(PdConn::new(pd_address));
    let router = Arc::new(Router::with_options(
        Arc::new(stores),
        Arc::clone(&conn) as Arc<dyn RegionResolver>,
        ClientOptions {
            jitter_seed: Some(29),
            ..ClientOptions::default()
        },
    ));
    let client = Arc::new(TxnClient::on_router(
        Arc::clone(&router),
        Arc::clone(&oracle),
    ));

    let lease = Arc::new(PdLease::new());
    let backend: Arc<dyn Backend> = Arc::new(
        esker_sql::backend::StoreBackend::new(Arc::clone(&client), oracle)
            .with_schema_lease(Arc::clone(&lease) as Arc<dyn SchemaLeaseSource>),
    );
    let refresher = LeaseRefresher::new(Arc::clone(&conn), lease)
        .asserting_columnar_for(Arc::clone(&backend), TENANT);
    refresher.refresh().expect("the node fetches its lease");
    std::thread::Builder::new()
        .name("schema-lease".to_owned())
        .spawn(move || refresher.run())
        .unwrap();

    let fragments: Arc<dyn FragmentSource> = Arc::new(ClientFragments::new(router));
    // **The client comes back too**, for 0117's stranded-lock fixture: a transaction that takes a
    // Percolator lock and is then abandoned has to be driven through `TxnClient`, because a SQL
    // session cannot leave one behind — `COMMIT` commits everything and `ROLLBACK` releases
    // everything. It is the same `Arc` the backend holds, so the fixture's locks and the node's
    // statements meet in one cluster rather than in two.
    (backend, conn, fragments, client)
}

async fn open_store(
    address: SocketAddr,
    store_id: u64,
    pd_address: SocketAddr,
    peers: &[PeerAddress],
    split_size: u64,
    tick: Duration,
    workers: usize,
) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(peers.to_vec(), 20_260_901);
    raft.tick = tick;
    raft.driver_workers = workers;
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
            split: esker_store::SplitOptions {
                region_split_size: split_size,
                ..esker_store::SplitOptions::default()
            },
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
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
        _dir: dir,
    }
}

/// A free port, **held** until the server that wants it is about to bind — see
/// `tests/joint_gate.rs`'s copy for the race this closes and the numbers behind it.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

/// How long a wait on a **placement or a catch-up** is given before it is called a failure.
///
/// **A precondition and not a measurement.** Nothing here asserts that the placement driver is
/// fast; these waits exist because the comparison below cannot start until a learner exists and has
/// caught up, and a bound is only here so that a cluster which is never going to get there fails
/// rather than hangs. So it is sized against the worst machine this runs on and not against the
/// good one: at sixty seconds it went red twice in one night on a gate running four thousand tests
/// beside it — `the columnar learner never answered a fragment over t` — with nothing wrong but the
/// box. Three minutes costs nothing on a run that succeeds, because a wait that is satisfied stops.
const PLACEMENT_WITHIN: u64 = 180;

/// The same shape for "a plan was routed to the columns at all": a readiness wait, so the bound is
/// sized for the worst machine rather than for this one, and it is named rather than written as a
/// literal at the site (#89, the four shapes of a wall-clock assertion).
const ROUTED_WITHIN: u64 = 120;

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let began = Instant::now();
    let deadline = began + Duration::from_secs(seconds);
    while !ready() {
        // **A readiness wait keeps its bound and says how long it waited** (#89). Removing the bound
        // would turn a false red into a hang; a message without the elapsed leaves the reader unable
        // to tell "the box was slow" from "this never happens".
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} after {:?}, and the bound is {seconds} s",
            began.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// The oracle this test needs lives in `esker-client` now: `WallClockOracle`, promoted out
// of this file and `routing_differential.rs`, which had the same copy for the same reason
// (debt #84, ADR 0116). Its doc carries the argument that used to be here.

/// A statement that has to succeed, retried through the leadership gap a saturated machine
/// produces (`docs/plans/phase-9-rails.md` §8).
fn settle(session: &mut Session, sql: &str) {
    // **Attempts, not seconds** (`#89`). This was a thirty-second wall clock, and thirty seconds is
    // not a property of the statement — it is a property of the box. Measured on one run of this
    // binary at load 3.6–4.3, its tests ranged from **4.2 s to 252 s**, so a statement that needs
    // one retry on a quiet box and thirty on a busy one is the same statement, and the bound has to
    // count the asking rather than the clock. The same change `#68` made to
    // `statement_across_a_leader_kill` for the same reason, and the lane rule that a gate test must
    // not assert on wall clock says it in general.
    //
    // What this does **not** do is excuse a cluster with no leader: forty attempts with a rising
    // backoff is a long time to ask, the elapsed is in the message, and the failure names the
    // attempt count and the last answer — so a real liveness hole is still a failure with a
    // diagnosis rather than a timeout.
    const ATTEMPTS: usize = 40;
    let began = Instant::now();
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        match session.run(sql) {
            // Applied — or applied by an attempt whose answer was lost, which is what a duplicate
            // says to a retry of an idempotent statement. One arm because they are one outcome:
            // the effect is in the database either way. **A blanket retry gets this exactly
            // backwards**: it re-sends a statement that already succeeded and then spends its
            // whole deadline on the permanent error that says so, which is how this helper failed
            // under a fully parallel run before it was written the way `joint_gate.rs` writes it.
            Ok(_)
            | Err(
                esker_sql::SqlError::DuplicateTable(_)
                | esker_sql::SqlError::DuplicateColumn(_)
                | esker_sql::SqlError::DuplicateColumnInRelation { .. }
                | esker_sql::SqlError::UniqueViolation { .. },
            ) => return,
            // A retry can collide with **its own** first attempt: an ambiguous write left a
            // Percolator lock behind, and until the transaction that owns it resolves a second
            // attempt at the same rows cannot clear it and says so. Waiting is the answer.
            Err(
                error @ (esker_sql::SqlError::OutcomeUnknown(_)
                | esker_sql::SqlError::StoreUnavailable(_)
                | esker_sql::SqlError::SerializationFailure { .. }),
            ) => {
                assert!(
                    attempts < ATTEMPTS,
                    "`{sql}` never settled in {ATTEMPTS} attempts over {:?}; last answer {error}",
                    began.elapsed()
                );
                // Rising and capped, so a statement that needs one wait is not made to wait as
                // long as one that needs twenty.
                std::thread::sleep(Duration::from_millis(50 * attempts.min(20) as u64));
            }
            Err(error) => panic!("`{sql}`: {error}"),
        }
    }
}

/// A read that has to answer, retried through the three transient failures a saturated machine
/// produces and **nothing else**.
///
/// A read is idempotent, so a retry cannot change what an earlier attempt did — which is why this
/// list is shorter than [`settle`]'s and needs no duplicate arm. Anything outside it is the test's
/// answer, not a condition to wait out.
fn rows(session: &mut Session, sql: &str) -> Vec<Vec<Option<String>>> {
    // **Attempts, not seconds** (`#89`). This was a thirty-second wall clock, and thirty seconds is
    // not a property of the statement — it is a property of the box. Measured on one run of this
    // binary at load 3.6–4.3, its tests ranged from **4.2 s to 252 s**, so a statement that needs
    // one retry on a quiet box and thirty on a busy one is the same statement, and the bound has to
    // count the asking rather than the clock. The same change `#68` made to
    // `statement_across_a_leader_kill` for the same reason, and the lane rule that a gate test must
    // not assert on wall clock says it in general.
    //
    // What this does **not** do is excuse a cluster with no leader: forty attempts with a rising
    // backoff is a long time to ask, the elapsed is in the message, and the failure names the
    // attempt count and the last answer — so a real liveness hole is still a failure with a
    // diagnosis rather than a timeout.
    const ATTEMPTS: usize = 40;
    let began = Instant::now();
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        match session.run(sql) {
            Ok(esker_sql::pgwire::session::Outcome::Rows { rows, .. }) => {
                return rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| {
                                cell.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                            })
                            .collect()
                    })
                    .collect();
            }
            Ok(other) => panic!("`{sql}` answered {other:?}"),
            Err(
                error @ (esker_sql::SqlError::OutcomeUnknown(_)
                | esker_sql::SqlError::StoreUnavailable(_)
                | esker_sql::SqlError::SerializationFailure { .. }),
            ) => {
                assert!(
                    attempts < ATTEMPTS,
                    "`{sql}` never settled in {ATTEMPTS} attempts over {:?}; last answer {error}",
                    began.elapsed()
                );
                // Rising and capped, so a statement that needs one wait is not made to wait as
                // long as one that needs twenty.
                std::thread::sleep(Duration::from_millis(50 * attempts.min(20) as u64));
            }
            Err(error) => panic!("`{sql}`: {error}"),
        }
    }
}

fn explain(session: &mut Session, sql: &str) -> String {
    let mut out = String::new();
    for row in rows(session, sql) {
        for cell in row.into_iter().flatten() {
            let _ = writeln!(out, "{cell}");
        }
    }
    out
}

/// **A pinned snapshot is a function**, on either engine, with the commits made in between rather
/// than concurrently.
///
/// `the_two_engines_agree_while_a_writer_keeps_committing` asks the same question under a racing
/// writer, which is what makes a disagreement rare and its record hard to read: the gate for
/// `76001434` caught one at 03:18 and ten rounds since have not (`docs/plans/phase-16-mpp.md`
/// §J14). This asks the deterministic half of it — take an instant, answer it, commit a hundred
/// rows, answer the same instant again — so that a snapshot that is not honoured fails **every**
/// time rather than under load.
///
/// The asymmetry the concurrent test cannot control is the point. There the routed run goes first
/// and the row run second, so whichever engine fails to pin sees *more* commits by the time it
/// runs, and the two failures are indistinguishable from one number. Here both engines answer the
/// same instant twice, before and after a hundred commits, so each is compared with **itself**.
///
/// **What would make this pass for nothing**: the columns refusing and the rows answering both
/// times. So it asserts the columnar engine actually answered, which is §10's rule — *agreement is
/// not correctness* — applied to a test that would otherwise be comparing the row engine with
/// itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pinned_snapshot_does_not_move_when_later_rows_commit() {
    const QUERY: &str = "SELECT region, count(*) FROM t GROUP BY region ORDER BY region";

    let gate = Gate::start().await;
    gate.fill().await;

    let (before, after, columnar) = tokio::task::block_in_place(|| {
        let mut session = gate.session();
        let at = gate.now();
        let (routed_before, plan) = at_snapshot(&mut session, "auto", at, QUERY);
        let (rows_before, _) = at_snapshot(&mut session, "row", at, QUERY);
        assert_eq!(
            routed_before, rows_before,
            "the two engines disagree before anything else happened, at {at}"
        );
        let answered = plan.contains("Engine: columnar");

        // A hundred rows, committed and settled, all of them after the instant above.
        for id in 1_000..1_100_i64 {
            settle(
                &mut session,
                &format!("INSERT INTO t VALUES ({id}, 'north', {}, 1.0)", id % 50),
            );
        }

        // The same instant, asked again. Row first this time, so that the order is not what
        // decides.
        let (rows_after, _) = at_snapshot(&mut session, "row", at, QUERY);
        let (routed_after, plan_after) = at_snapshot(&mut session, "auto", at, QUERY);
        (
            (routed_before, rows_before),
            (routed_after, rows_after),
            answered || plan_after.contains("Engine: columnar"),
        )
    });

    assert_eq!(
        after.1, before.1,
        "the ROW engine's answer at a pinned instant moved when a hundred later rows committed"
    );
    assert_eq!(
        after.0, before.0,
        "the COLUMNAR answer at a pinned instant moved when a hundred later rows committed"
    );
    assert!(
        columnar,
        "the columns refused both times, so this compared the row engine with itself — which is \
         exactly the free agreement §10 says not to count"
    );

    gate.stop().await;
}

/// **Where an `In` of N keys stops beating a nested loop** — the number `docs/plans/phase-16-mpp.md`
/// §J11 says is all that is left of the join rewrite.
///
/// `MAX_IN_VALUES` is 4,096 and it is a **format** limit: the most keys a fragment can carry. That
/// is not the same question as the most keys it is *worth* carrying, and nothing had measured the
/// second. A planner-side threshold below the format's ceiling is what this buys — or the evidence
/// that the ceiling is the right place to stop, which is also an answer.
///
/// Both paths, same data, same node, medians of five: `esker.engine = 'auto'` pushes the key set
/// down as an `Expr::In`, `'row'` runs the nested loop the rewrite replaced.
///
/// **It records whether the columns actually answered at each N.** A pushdown that refused and fell
/// back is the row path timed twice, and a curve made of that would say the two are identical
/// everywhere — the free agreement §10 warns about, wearing a stopwatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not an assertion; builds a 20k-row fixture and prints a curve"]
async fn what_an_in_list_costs_against_the_nested_loop() {
    const INNER: i64 = 4_096;
    // **Eight thousand, and it was twenty.** Building the larger fixture through a three-store
    // in-process cluster took the box from 12 to 29 on its own and the cluster lost its leader
    // before the first query ran — the measurement's own cost becoming the thing measured. Eight
    // thousand outer rows over four thousand keys still gives every key about two rows and leaves
    // the per-row membership test plenty to be seen in.
    const OUTER: i64 = 8_000;
    const ROUNDS: usize = 5;

    let gate = Gate::start().await;
    gate.fill_join_at_scale(INNER, OUTER).await;

    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        println!("\n  {OUTER} outer rows over {INNER} inner keys, median of {ROUNDS}\n");
        println!("     N    pushdown      rows      pushed?   answer");
        for n in [1_i64, 8, 64, 512, 4_096] {
            let query = format!("SELECT count(*) FROM f JOIN d ON f.dk = d.k WHERE d.k <= {n}");
            let (routed, routed_at, plan) = timed_engine(&mut session, "auto", &query, ROUNDS);
            let (by_rows, rows_at, _) = timed_engine(&mut session, "row", &query, ROUNDS);
            assert_eq!(
                routed, by_rows,
                "the two engines disagree at N = {n}, which is a correctness failure and not a cost"
            );
            println!(
                "  {n:>4}  {:>8.2} ms  {:>8.2} ms   {:>7}   {:?}",
                routed_at.as_secs_f64() * 1000.0,
                rows_at.as_secs_f64() * 1000.0,
                if plan.contains("Semi Join Filter") {
                    "yes"
                } else {
                    "NO — fell back"
                },
                routed
                    .first()
                    .and_then(|row| row.first().cloned())
                    .flatten(),
            );
        }
    });

    gate.stop().await;
}

/// One query on one engine, timed, with the plan that answered it.
fn timed_engine(
    session: &mut Session,
    engine: &str,
    query: &str,
    rounds: usize,
) -> (Vec<Vec<Option<String>>>, Duration, String) {
    session
        .run(&format!("SET esker.engine = '{engine}'"))
        .expect("the override sets");
    // One run before the clock starts: the first of anything pays for a cold region cache and a
    // learner's first fragment, which is not what the curve is about.
    let answer = rows(session, query);
    let mut samples: Vec<Duration> = (0..rounds)
        .map(|_| {
            let at = Instant::now();
            let _ = rows(session, query);
            at.elapsed()
        })
        .collect();
    samples.sort_unstable();
    let plan = explain(session, &format!("EXPLAIN ANALYZE {query}"));
    session
        .run("RESET esker.engine")
        .expect("the override clears");
    (answer, samples[samples.len() / 2], plan)
}

/// **A bulk load into a splitting table fails on a count, not on its deadline.**
///
/// The 160-region tier, which is where `where_a_bulk_load_into_a_splitting_table_breaks` measured
/// it: three rounds on 2026-09-09 broke at 162, 16 and 120 regions, and two of the three broke with
/// `08006 … gave up after 9 or 10 attempts: peer is not the leader`
/// ([ADR 0100](../../../docs/adr/0100-a-region-between-leaders-waits-on-the-callers-deadline.md)).
///
/// **The assertion is the shape, not a duration**: a refusal whose own sentence says it ran out of
/// *attempts* is the defect, whatever the wall clock says, because the caller's deadline had time
/// left when it was raised. A load that fails because it ran out of *time* is a different answer
/// and this test lets it through — that one is a machine being slow, which is not what this is
/// about.
///
/// Kept beside the measurement rather than in the gate, and `#[ignore]`d for the same reason it is:
/// a hundred and sixty in-process Raft groups over four stores is minutes of a quiet box.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the 160-region tier: minutes, and it wants a quiet box"]
async fn a_splitting_bulk_load_never_fails_for_want_of_attempts() {
    let gate = Gate::start_splitting(8 * 1024).await;
    let mut counted = Vec::new();
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, filler text)",
        );
        let filler = "x".repeat(256);
        for at in (1..=10_000_i64).step_by(250) {
            let values: Vec<String> = (at..at + 250)
                .map(|id| format!("({id}, '{filler}')"))
                .collect();
            let statement = format!("INSERT INTO t VALUES {}", values.join(", "));
            if let Err(error) = session.run(&statement) {
                let text = error.to_string();
                // **The sentence names what ended the call.** `gave up after N attempts` is the
                // count; `deadline` is the caller's own limit and is allowed.
                if text.contains("gave up after") {
                    counted.push(format!(
                        "at row {at} and {} regions: {text}",
                        gate.regions()
                    ));
                }
                break;
            }
        }
    });
    gate.stop().await;
    assert!(
        counted.is_empty(),
        "a load into a splitting table was refused for want of attempts while its deadline had \
         time left:\n  {}",
        counted.join("\n  ")
    );
}

/// **What the box was doing, printed rather than asserted.**
///
/// A duration measured here is worth what the machine was doing while it was taken, and this lane
/// has already read one wrong: two runs of one test took 214 s and then over 400 s, and the
/// obvious culprit — a gate — turned out not to have been running at all. The chain log said so;
/// a twelve-second sample extrapolated backwards said otherwise, and it was the sample that was
/// wrong. So every cluster this file starts prints the environment at both ends, and a reader
/// comparing two numbers can see whether they were taken in the same world.
///
/// **Printed and never asserted.** A test that fails because another lane was busy is a worse
/// test than one that is slow, and the number this is here to explain is the *duration*, not the
/// verdict.
///
/// From `/proc`, which is the **Linux VM's** — so it moves when another lane's *container* runs
/// and is **blind to the host**, where a `cargo build` competes for the same physical cores. That
/// is why the sampler in `esker-coord/h1/env-sampler.sh` counts host compilers beside this: a
/// number taken here with no sampler next to it can be slow for a reason this line cannot show.
///
/// **And a line like this is worth more than the theories it replaces.** The two runs that
/// prompted it were attributed to a gate, then to another lane, then to a compile inside the
/// deadline — three times, each refused by evidence somebody had to go and find: the chain log,
/// the transcript re-read *with the dates on*, and the runner's own first line
/// (*Finished `test` profile … in 0.09s*). None of it needed a theory, and all of it would
/// have been in this line.
fn the_box_right_now(what: &str) -> String {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
                .into()
        })
        .unwrap_or_else(|| "unavailable".to_owned());
    let free = std::fs::read_to_string("/proc/meminfo").ok().map_or_else(
        || "unavailable".to_owned(),
        |text| {
            let field = |name: &str| {
                text.lines()
                    .find(|line| line.starts_with(name))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map_or_else(|| "?".to_owned(), |kb| format!("{} MB", kb / 1024))
            };
            format!(
                "free {}, available {}",
                field("MemFree:"),
                field("MemAvailable:")
            )
        },
    );
    format!("  [box] {what}: load {load}, {free}")
}

/// **Does the leaderless window move with what the process is driving?** — `docs/plans/debts-v1.1.md`
/// #34's first question, which is not the fix.
///
/// Four stores in one process driving three hundred Raft groups at a five-millisecond tick is its
/// own explanation for a region that cannot hold an election, and nothing has ruled it out. The two
/// numbers that decide how much Raft a process is driving are the **tick** every group counts in
/// and the **driver threads** the pool spreads them over, so this sweeps both and measures the same
/// thing each time.
///
/// If the window shrinks when the tick lengthens or the pool widens, the stall is the harness's
/// capacity and not the system's. If it does not move at all, the harness is not what is holding
/// the election up, and the next arm — the same load on four real store processes — is what
/// separates the rest.
///
/// # **This sweep has never fired, and until that is understood its zero is not evidence**
///
/// Two rounds on 2026-09-09: **0 sightings** here at 332 and 300 regions, while
/// [`how_long_a_writer_waits_for_a_region_between_leaders`] — running in the next process, minutes
/// later — stalled **3 of 3** both times, at 321 and 235 regions. Everything that should matter is
/// the same, and it was checked rather than assumed:
///
/// | | this sweep | its sibling |
/// |---|---|---|
/// | harness | `Gate::start_with(8 KiB, 5 ms, 4)` | `Gate::start_splitting(8 KiB)` = the same call |
/// | tick and driver threads | **reach the stores**: `open_store` sets `raft.tick` and `raft.driver_workers` from them | the same |
/// | the load | 4,000 rows, 250 a batch, 256-byte filler, one table | identical |
/// | the writer | `session.run`, the SQL path | identical |
/// | what counts as a sighting | a refusal whose text says `not the leader` | identical |
/// | the retry | the same statement every 20 ms until it lands | identical |
/// | give-up | 20 s | 30 s |
/// | the loop's exit | after 4 sightings | after 25 *cleared* waits, so never |
/// | position in the run | **first in its process, on an idle box** | third overall, after two clusters have been built and torn down |
///
/// The last row is the only difference big enough to matter, and it cuts against the obvious
/// reading: **inside this sweep the second cluster runs on the warmer box and fires even less**.
/// So "later fires more" is not it either, and the honest state is that two tests which differ in
/// nothing that should matter answer differently, twice.
///
/// **The experiment that decides it** is to run the sibling first and this sweep second — one
/// flip, and whichever way the answer moves names the variable. Until then neither number is
/// evidence about the tick or the thread count, which is what this sweep was built to measure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "debts #34, arm (b): two clusters, minutes, and it wants a quiet box"]
async fn how_the_leaderless_window_moves_with_the_drivers() {
    const GIVE_UP_AFTER: Duration = Duration::from_secs(20);
    for (tick_ms, workers) in [(5_u64, 4_usize), (20, 8)] {
        let gate = Gate::start_with(8 * 1024, Duration::from_millis(tick_ms), workers).await;
        let mut met = 0_usize;
        let mut cleared: Vec<Duration> = Vec::new();
        println!("\n  tick {tick_ms} ms, {workers} driver threads");
        tokio::task::block_in_place(|| {
            let mut session = gate.session();
            settle(
                &mut session,
                "CREATE TABLE t (id int8 PRIMARY KEY, filler text)",
            );
            let filler = "x".repeat(256);
            for at in (1..=4_000_i64).step_by(250) {
                if met >= 4 {
                    break;
                }
                let values: Vec<String> = (at..at + 250)
                    .map(|id| format!("({id}, '{filler}')"))
                    .collect();
                let statement = format!("INSERT INTO t VALUES {}", values.join(", "));
                let Err(first) = session.run(&statement) else {
                    continue;
                };
                if !first.to_string().contains("not the leader") {
                    continue;
                }
                met += 1;
                let regions = gate.regions();
                let began = Instant::now();
                loop {
                    match session.run(&statement) {
                        Ok(_) => {
                            println!(
                                "    row {at:<6} {regions:>4} regions   cleared in {:>8.1} ms",
                                began.elapsed().as_secs_f64() * 1000.0
                            );
                            cleared.push(began.elapsed());
                            break;
                        }
                        Err(_) if began.elapsed() >= GIVE_UP_AFTER => {
                            println!(
                                "    row {at:<6} {regions:>4} regions   still refused after {:?}",
                                began.elapsed()
                            );
                            break;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            }
        });
        println!(
            "  tick {tick_ms} ms, {workers} threads: {met} met, {} cleared inside \
             {GIVE_UP_AFTER:?}, load finished at {} regions",
            cleared.len(),
            gate.regions()
        );
        gate.stop().await;
    }
}

/// **What the catalog read costs on a topology that has stores in it** —
/// [ADR 0102](../../../docs/adr/0102-the-catalogs-read-path.md)'s numbers, taken where run 111
/// could not take them.
///
/// Run 111 counted 4,790,406 catalog views in one `ActiveRecord` pass at a mean of under half a
/// microsecond, and the mean is the tell: the harness starts `esker-sql 127.0.0.1:PORT` with no
/// store addresses, so its backend is `MemoryBackend` — an in-process `BTreeMap` with no store, no
/// placement driver, no Raft and no socket. The count is real and topology-independent; the cost
/// is a table lookup and says nothing about the read this ADR is about.
///
/// This `Gate` is the other thing: real `Store`s behind real listeners, a real placement driver, a
/// `StoreBackend` over a router with a lease. A catalog view here crosses a socket and Raft, which
/// is what the criteria in §④ are written against.
///
/// **Prints and asserts nothing.** The numbers go in `docs/bench/v1.1.md` and the ADR; a
/// measurement that fails a build is a gate, and this is not one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "an ADR 0102 measurement: a cluster, and it wants ESKER_CATALOG_STATS=1"]
async fn what_a_catalog_read_costs_with_stores_under_it() {
    const ROWS: i64 = 2_000;
    let gate = Gate::start_splitting(u64::MAX).await;
    println!("{}", the_box_right_now("catalog measurement"));
    if std::env::var_os("ESKER_CATALOG_STATS").is_none() {
        println!(
            "  ESKER_CATALOG_STATS is not set: the counters stay at zero and this run says nothing"
        );
    }
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(&mut session, "CREATE TABLE t (id int8 PRIMARY KEY, n int8)");
        let before = esker_sql::catalog::stats::summary();
        // One statement per round trip on purpose: each takes its own catalog view, which is the
        // thing being counted. A batched insert would count one view for five hundred rows.
        for n in 1..=ROWS {
            // **The transients a splitting, electing cluster answers with are waited out, not
            // failed on.** `40003` is an outcome nobody can know, `08006` is a client that spent
            // its attempts, and both mean *not now*.
            //
            // **A `23505` after any earlier attempt is that attempt's own row, and is a success.**
            // Every insert names its own primary key, the table is created empty by this test, and
            // nothing else writes it — so the only value that can collide with `(n, n)` is the one
            // an earlier turn of this loop wrote. The rule used to accept a `23505` only after an
            // answer that said in so many words that it *may or may not have been applied*, and
            // that is one of **three** shapes the client has for "the write's fate is unknown":
            // `esker_client::Error` also has `RetriesExhausted` — `gave up after N attempts` —
            // and `DeadlineExceeded`, and neither of those proves the write did not land either.
            // A request that went out and lost its answer surfaces as `08006 could not reach the
            // store`, which this loop was already retrying *as though the write had not
            // happened*: the first attempt landed, the second met its own row, `unknown` was
            // still false, and the assertion below fired —
            // `INSERT INTO t VALUES (249, 249) never landed: duplicate key value violates unique
            // constraint`, on r1's real-topology run of the Gate directory measurement.
            //
            // So the condition is the attempt number and not the wording. Wording is a list that
            // has to be kept in step with a taxonomy in another crate; the attempt number is the
            // fact the acceptance actually rests on. A `23505` on the **first** attempt is still a
            // failure, because then nothing of this loop's has run and the row came from somewhere
            // this measurement is not about.
            let statement = format!("INSERT INTO t VALUES ({n}, {n})");
            let mut unknown = false;
            for attempt in 0..40 {
                match session.run(&statement) {
                    Ok(_) => break,
                    Err(error) => {
                        let text = error.to_string();
                        if attempt > 0 && text.contains("23505") {
                            break;
                        }
                        unknown |= text.contains("may or may not have been applied");
                        assert!(
                            unknown
                                || text.contains("not the leader")
                                || text.contains("could not reach the store"),
                            "`{statement}` was refused by something this measurement is not \
                             about: {error}"
                        );
                        assert!(attempt < 39, "`{statement}` never landed: {error}");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        let after_writes = esker_sql::catalog::stats::summary();
        for n in 1..=ROWS {
            let _ = rows(&mut session, &format!("SELECT n FROM t WHERE id = {n}"));
        }
        println!("\n  {ROWS} point writes and {ROWS} point reads through a store-backed node");
        println!("  before      {before}");
        println!("  after writes {after_writes}");
        println!("  after reads  {}", esker_sql::catalog::stats::summary());
    });
    gate.stop().await;
}

/// **Whether the region nobody leads is one whose handle lost its core** — ADR 0099's state,
/// asked while the stall is happening rather than after it.
///
/// Two facts per store, printed for the region that is refusing: what its handle publishes and
/// what the core answering for it says. They are the same peer or the store is in the state
/// [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md) closes — and asking during
/// the window is the only time the answer means anything, because a displaced core never
/// un-displaces and a real election ends.
fn refused_region(error: &impl std::fmt::Display) -> u64 {
    let text = error.to_string();
    text.rsplit_once("region ")
        .and_then(|(_, tail)| {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(1)
}

async fn who_answers_for(nodes: &[Node], region_id: u64) -> String {
    let mut said = Vec::new();
    for node in nodes {
        let store = &node.store;
        let Some(peer) = store.peer_of(region_id) else {
            said.push(format!("store {}: does not host it", store.store_id()));
            continue;
        };
        let status = peer.status().await.ok();
        said.push(format!(
            "store {}: handle is peer {} · published leader {:?} term {} · core says {}",
            store.store_id(),
            peer.peer_id(),
            peer.leader(),
            peer.term(),
            status.map_or_else(
                || "unavailable".to_owned(),
                |status| format!(
                    "peer {} {:?} term {} leader {:?}",
                    status.id, status.role, status.term, status.leader
                )
            )
        ));
    }
    said.join("\n      ")
}

/// **How long a writer has to wait when it meets a region between leaders** — the measurement
/// [ADR 0100](../../../docs/adr/0100-a-region-between-leaders-waits-on-the-callers-deadline.md)
/// defers its decision on.
///
/// The red test beside this one says the client gives up on a **count** while the caller's deadline
/// still has time. Whether that count should simply go depends on a number nobody has: **how long a
/// region really has no leader during a burst of splits**, now that
/// [ADR 0094](../../../docs/adr/0094-a-split-childs-leader-is-the-parents-leader.md) makes a split
/// child campaign at once. If the distribution sits well inside the default ten seconds, removing
/// the count turns a hard failure into a slower success. If it has a long tail, removing it turns a
/// two-second failure into a ten-second one for every writer that meets it.
///
/// So this measures it the way the caller experiences it: when a statement is refused for want of
/// attempts, it is asked again — and again — until it lands, and what is recorded is **how long the
/// deadline would have had to be**. Nothing else here is timed, so a slow box moves every number
/// the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement for ADR 0100: the 160-region tier, minutes, and it wants a quiet box"]
async fn how_long_a_writer_waits_for_a_region_between_leaders() {
    const GIVE_UP_AFTER: Duration = Duration::from_secs(30);
    let gate = Gate::start_splitting(8 * 1024).await;
    let mut waits: Vec<(i64, usize, Duration)> = Vec::new();
    let mut never = Vec::new();
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, filler text)",
        );
        let filler = "x".repeat(256);
        // Four thousand rows and twenty-five waits: enough to cross three hundred regions and
        // meet the window repeatedly, and small enough to finish inside the window it is measured
        // in. The first version asked for ten thousand and was killed at ten minutes.
        for at in (1..=4_000_i64).step_by(250) {
            if waits.len() >= 25 {
                break;
            }
            let values: Vec<String> = (at..at + 250)
                .map(|id| format!("({id}, '{filler}')"))
                .collect();
            let statement = format!("INSERT INTO t VALUES {}", values.join(", "));
            let Err(first) = session.run(&statement) else {
                continue;
            };
            // Only the refusal this ADR is about. Anything else is somebody else's row.
            if !first.to_string().contains("not the leader") {
                continue;
            }
            let regions = gate.regions();
            let began = Instant::now();
            loop {
                match session.run(&statement) {
                    Ok(_) => {
                        // **Printed here, not at the end.** The first run of this was killed by
                        // its own timeout at 602 s and printed nothing at all, because every
                        // number was in a `Vec` waiting for a summary that never ran. A
                        // measurement under a budget streams.
                        println!(
                            "    row {at:<6} {regions:>4} regions   {:>8.1} ms",
                            began.elapsed().as_secs_f64() * 1000.0
                        );
                        waits.push((at, regions, began.elapsed()));
                        break;
                    }
                    Err(error) if began.elapsed() >= GIVE_UP_AFTER => {
                        // **Asked during the stall, which is the only time it means anything.**
                        let who = tokio::runtime::Handle::current()
                            .block_on(who_answers_for(gate.nodes(), refused_region(&error)));
                        never.push(format!(
                            "row {at} at {regions} regions: still refused after {:?}: {error}\n      {who}",
                            began.elapsed()
                        ));
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    });
    gate.stop().await;

    let mut millis: Vec<u128> = waits.iter().map(|(_, _, at)| at.as_millis()).collect();
    millis.sort_unstable();
    println!(
        "\n  {} statements met a region between leaders",
        millis.len()
    );
    if let (Some(min), Some(max)) = (millis.first(), millis.last()) {
        let median = millis[millis.len() / 2];
        let p90 = millis[millis.len() * 9 / 10];
        println!("  min {min} ms   median {median} ms   p90 {p90} ms   max {max} ms");
        println!("  every wait, in order met:");
        for (row, regions, waited) in &waits {
            println!(
                "    row {row:<6} {regions:>4} regions   {:>8.1} ms",
                waited.as_secs_f64() * 1000.0
            );
        }
    }
    for line in &never {
        println!("  {line}");
    }
    assert!(
        never.is_empty(),
        "a region stayed leaderless past {GIVE_UP_AFTER:?}, which is not the window this measures"
    );
}

/// **Which part of the per-region cost grows with the table.**
///
/// `docs/plans/phase-16-mpp.md` §10 has the region axis at three thousand rows and the group axis
/// at ten thousand, and between them sits a number nothing explains: the dispatch slope is
/// **1.21 ms a region** on the first and **8.07 ms a region** on the second. A round trip does not
/// know how many rows a table has, so something a fragment does is growing — and the two runs
/// differ in *rows per region* (61 against 238) as well as in region count.
///
/// So this holds the split threshold fixed and moves only the row count, and prints what the store
/// says it did: `EXPLAIN ANALYZE`'s `Fragments`, `Stripes`, `Chunks` and `Rows` lines are per-query
/// totals summed over every fragment, so they separate "each fragment read past its region" from
/// "each fragment read more because its region holds more".
///
/// Deliberately a measurement and not an assertion: what it produces is a paragraph in §10.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a §10 measurement: two clusters, minutes, and it wants a quiet box"]
async fn what_grows_in_a_fragment_when_the_table_grows() {
    const ROUNDS: usize = 5;
    println!("\n  split 16 KB, medians of {ROUNDS}, count(*) — the dispatch-only query\n");
    println!("  rows    regions  rows/region  routed        per region   the store's own account");
    for rows_in_table in [3_000_i64, 10_000] {
        let gate = Gate::start_splitting(16 * 1024).await;
        gate.fill_grouped(rows_in_table).await;
        let regions = gate.regions();
        tokio::task::block_in_place(|| {
            let mut session = gate.session();
            let (_, at, plan) =
                timed_engine(&mut session, "auto", "SELECT count(*) FROM t", ROUNDS);
            let ms = at.as_secs_f64() * 1000.0;
            // One line of the plan carries the counts, and the fragment line carries the fan-out.
            let account: Vec<&str> = plan
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("Fragments:") || line.starts_with("Stripes:"))
                .collect();
            println!(
                "  {rows_in_table:<6}  {regions:>7}  {:>11.1}  {ms:>8.2} ms  {:>8.2} ms   {}",
                f64::from(u32::try_from(rows_in_table).unwrap_or(u32::MAX))
                    / f64::from(u32::try_from(regions).unwrap_or(1).max(1)),
                ms / f64::from(u32::try_from(regions).unwrap_or(1).max(1)),
                account.join("  |  ")
            );
        });
        gate.stop().await;
    }
}

/// **The §10 re-measure: what a fragment costs per region, and what a finish costs per group.**
///
/// `docs/plans/phase-16-mpp.md` §10's table, arms 1 to 4. The verdict there is single-region and
/// says so, and the one fact that shapes this is §10 item 4: **dispatch is still serial**, so R
/// regions cost R sequential round trips before any merging happens. A single number at R regions
/// would measure that and be read as the exchange's verdict.
///
/// So the two costs are varied independently, which needs none of §9a's four missing instruments:
///
/// * **R varies at a fixed row count**, by giving each cluster a different split threshold over the
///   same data — not by growing the table, which would move the scan cost with it;
/// * **the finish varies at a fixed R**, by grouping on one of three columns whose cardinalities
///   are 1, 100 and one-per-row.
///
/// It also prints the **distinct stores holding learners** beside the region count, because §10
/// item 2 says an exchange's parallelism is nodes and not regions, and the only multi-region
/// cluster anyone had run put five regions on two stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the §10 re-measure: four clusters, minutes, and it wants a quiet box"]
async fn what_a_fragment_costs_per_region_and_per_group() {
    // **Ten thousand, for the group axis.** The three-thousand-row run is in §10: it walked the
    // region axis to 95 and the finish never came near the dispatch. What it could not reach is
    // `regions × groups`, which §10 item 3 names as the shape that would flip the verdict — so this
    // raises the group count with the rows and lets the region count follow the split threshold.
    const ROWS: i64 = 10_000;
    const ROUNDS: usize = 5;
    let queries = [
        ("count(*)          ", "SELECT count(*) FROM t"),
        (
            "group by g1    (1)",
            "SELECT g1, count(*) FROM t GROUP BY g1 ORDER BY g1",
        ),
        (
            "group by g100(100)",
            "SELECT g100, count(*) FROM t GROUP BY g100 ORDER BY g100",
        ),
        (
            "group by gmax(=rows)",
            "SELECT gmax, count(*) FROM t GROUP BY gmax ORDER BY gmax",
        ),
    ];

    println!("\n  {ROWS} rows, medians of {ROUNDS}, one row per (split threshold, query)\n");
    println!("  split      regions  stores   query                 routed        rows");
    // 8 KB is left out at this row count: three thousand rows put 95 regions on it, so ten
    // thousand would be past three hundred in-process Raft groups over four stores, and the
    // measurement would be of the harness.
    for split in [u64::MAX, 64 * 1024, 16 * 1024] {
        let gate = Gate::start_splitting(split).await;
        gate.fill_grouped(ROWS).await;
        let regions = gate.regions();
        let stores = gate.learner_stores();
        tokio::task::block_in_place(|| {
            let mut session = gate.session();
            for (label, query) in queries {
                let (routed, routed_at, plan) = timed_engine(&mut session, "auto", query, ROUNDS);
                let (by_rows, rows_at, _) = timed_engine(&mut session, "row", query, ROUNDS);
                assert_eq!(
                    routed, by_rows,
                    "the two engines disagree on `{query}` at {regions} regions"
                );
                let name = if split == u64::MAX {
                    "none  ".to_owned()
                } else {
                    format!("{:>4} KB", split / 1024)
                };
                println!(
                    "  {name}   {regions:>7}  {stores:>6}   {label}  {:>8.2} ms  {:>8.2} ms{}",
                    routed_at.as_secs_f64() * 1000.0,
                    rows_at.as_secs_f64() * 1000.0,
                    if plan.contains("Engine: columnar") {
                        ""
                    } else {
                        "   <- FELL BACK"
                    },
                );
            }
        });
        gate.stop().await;
    }
}

/// **Where a bulk load into a table splitting under itself starts failing** — the first of the two
/// things `docs/plans/phase-16-mpp.md` §10's re-measure left unsmoothed.
///
/// Loading ten thousand rows across the ~160 regions a 16 KB threshold produces failed with
/// `a lock from the transaction at … could not be cleared`, which is `Error::LockNotCleared`
/// mapped to `40001`: a client that met somebody's lock, spent its resolution budget and gave up.
///
/// **The loader is sequential and its transactions share no keys**, so the lock it meets is not a
/// concurrent writer's. The candidate this exists to confirm or kill is the one the commit path
/// names itself: `Transaction::commit` finishes its secondaries with `let _ = self.commit_grouped(…)`
/// — *"a secondary that fails here is not a failed transaction"*, which is right — and a region that
/// splits between the prewrite and that call is exactly how it fails. The lock left behind belongs
/// to a **committed** transaction, and the next writer of that key has to roll it forward through a
/// region that has moved under both of them.
///
/// This prints the region count at each step and stops at the first failure, so the answer is a
/// number of splits rather than an anecdote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "an investigation: loads until it breaks and prints where"]
async fn where_a_bulk_load_into_a_splitting_table_breaks() {
    const BATCH: i64 = 250;
    const UP_TO: i64 = 6_000;

    let gate = Gate::start_splitting(8 * 1024).await;
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, pad text)",
        );
        println!("\n  rows    regions   outcome");
        let mut at = 1_i64;
        while at <= UP_TO {
            let values: Vec<String> = (at..at + BATCH)
                .map(|id| format!("({id}, 'pad-{id}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"))
                .collect();
            // **`run`, not `settle`.** The helper retries for thirty seconds and would turn the
            // thing being measured into a pause; what this wants is the first refusal, with its
            // own words.
            let outcome = session.run(&format!("INSERT INTO t VALUES {}", values.join(", ")));
            let regions = gate.regions();
            match outcome {
                Ok(_) => println!("  {:<6}  {regions:>7}   ok", at + BATCH - 1),
                Err(error) => {
                    println!(
                        "  {:<6}  {regions:>7}   {error} [{}]",
                        at + BATCH - 1,
                        error.sqlstate()
                    );
                    println!(
                        "\n  first refusal at {} rows and {regions} regions\n",
                        at + BATCH - 1
                    );

                    // **Does it clear, or is it stuck?** `settle`'s own comment says a retry
                    // collides with its first attempt's lock and that waiting is the answer; the
                    // §10 load waited thirty seconds and gave up. So keep asking, and report the
                    // time and every *distinct* thing it says on the way — a lock that clears in
                    // forty seconds is a slow cluster, and one that never clears is a defect.
                    let began = Instant::now();
                    let mut seen: Vec<String> = Vec::new();
                    loop {
                        match session.run(&format!("INSERT INTO t VALUES {}", values.join(", "))) {
                            Ok(_) => {
                                println!(
                                    "  the retry settled after {:.1} s",
                                    began.elapsed().as_secs_f64()
                                );
                                break;
                            }
                            Err(esker_sql::SqlError::UniqueViolation { .. }) => {
                                println!(
                                    "  the first attempt had committed after all, seen after \
                                     {:.1} s",
                                    began.elapsed().as_secs_f64()
                                );
                                break;
                            }
                            Err(error) => {
                                let said = format!("[{}] {error}", error.sqlstate());
                                if !seen.contains(&said) {
                                    println!("  +{:>5.1} s  {said}", began.elapsed().as_secs_f64());
                                    seen.push(said);
                                }
                                if began.elapsed() > Duration::from_secs(120) {
                                    println!("  STILL REFUSING after 120 s");
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(200));
                            }
                        }
                    }

                    // **And the harness hypothesis**: a 250-row INSERT across 37 regions is one
                    // prewrite over 37 regions. One row at a time touches one.
                    println!("\n  now one row per statement, from {}", at + BATCH);
                    let mut singles = 0;
                    for id in at + BATCH..at + BATCH + 250 {
                        match session.run(&format!("INSERT INTO t VALUES ({id}, 'pad-{id}')")) {
                            Ok(_) => singles += 1,
                            Err(error) => {
                                println!(
                                    "  single-row insert refused after {singles} of 250: \
                                     {error} [{}]",
                                    error.sqlstate()
                                );
                                break;
                            }
                        }
                    }
                    if singles == 250 {
                        println!(
                            "  250 single-row inserts all committed, at {} regions",
                            gate.regions()
                        );
                    }
                    return;
                }
            }
            at += BATCH;
        }
        println!(
            "\n  no refusal up to {UP_TO} rows and {} regions",
            gate.regions()
        );
    });
    gate.stop().await;
}

/// The writer that makes the table split under itself.
fn spawn_loader(
    gate: &Gate,
    stop: &Arc<AtomicBool>,
    refusals: &Arc<std::sync::Mutex<Vec<String>>>,
) -> std::thread::JoinHandle<i64> {
    let backend = Arc::clone(&gate.backend);
    let catalog = Arc::clone(&gate.catalog);
    let stop = Arc::clone(stop);
    let refusals = Arc::clone(refusals);
    std::thread::Builder::new()
        .name("splitting-loader".to_owned())
        .spawn(move || {
            let mut session = Session {
                executor: Executor::new(backend, catalog, TENANT, esker_sql::session::register()),
            };
            let mut id = 1_i64;
            while !stop.load(Ordering::Relaxed) && id < 4_000 {
                let values: Vec<String> = (id..id + 50)
                    .map(|n| format!("({n}, 'pad-{n}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"))
                    .collect();
                if let Err(error) =
                    session.run(&format!("INSERT INTO t VALUES {}", values.join(", ")))
                    && let Ok(mut seen) = refusals.lock()
                {
                    seen.push(format!("[{}] {error}", error.sqlstate()));
                }
                id += 50;
            }
            id
        })
        .unwrap()
}

/// What one run of a splitting load saw.
struct Sightings {
    /// Milliseconds from a child first being seen to its first leader.
    led_after: Vec<f64>,
    /// Children whose first leader was the peer that led their parent.
    inherited: usize,
    /// Children whose first leader was somebody else.
    elsewhere: usize,
    /// What the loader was refused, as `[sqlstate] message`.
    refusals: Vec<String>,
    rows: i64,
    regions: usize,
}

/// Loads a table that splits under itself and watches every child's first leader.
///
/// **The stores, not PD.** A child appears in its store's own map the instant `adopt_split` runs;
/// PD learns of it at the next region heartbeat — 20 ms here — by which time the election is over.
/// Sampling PD reported every one of 130 children as led at zero milliseconds, which is not the
/// window being small, it is the window being invisible.
///
/// **A child's parent is the region whose `end_key` its `start_key` equals.** A split turns a parent
/// `[a, c)` into `[a, b)` and a child `[b, c)`, so that match is exact rather than a guess, and it
/// is what lets this say *whose* leader the child's first leader was.
fn watch_a_splitting_load(gate: &Gate) -> Sightings {
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, pad text)",
        );
    });

    let stop = Arc::new(AtomicBool::new(false));
    let refusals = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let loader = spawn_loader(gate, &stop, &refusals);

    let mut first_seen: BTreeMap<u64, Instant> = BTreeMap::new();
    let mut ends_at: BTreeMap<bytes::Bytes, u64> = BTreeMap::new();
    let mut leader_of: BTreeMap<u64, usize> = BTreeMap::new();
    let mut parent_led_by: BTreeMap<u64, usize> = BTreeMap::new();
    let mut done: BTreeSet<u64> = BTreeSet::new();
    let mut led_after: Vec<f64> = Vec::new();
    let (mut inherited, mut elsewhere) = (0usize, 0usize);

    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline && !loader.is_finished() {
        let now = Instant::now();
        // **Which *store* leads it, not which peer id.** A peer id is numbered per region, so the
        // parent's `leader_peer_id` and the child's are ids in two different spaces and comparing
        // them answers no question — measured: it matched once in a hundred and thirty while a
        // trace of `adopt_split` showed the parent's leader campaigning its child every single
        // time. `is_leader` is each store's statement about itself, so the store index is the
        // thing both halves of this comparison can be in.
        let records: Vec<(usize, esker_proto::RegionStatus)> = gate
            .nodes
            .iter()
            .enumerate()
            .flat_map(|(at, node)| {
                node.store
                    .region_statuses()
                    .into_iter()
                    .map(move |status| (at, status))
            })
            .collect();
        for (at, record) in &records {
            if record.is_leader {
                leader_of.insert(record.region.id, *at);
            }
            ends_at.insert(record.region.end_key.clone(), record.region.id);
        }
        for (_, record) in records {
            let id = record.region.id;
            if done.contains(&id) {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(slot) = first_seen.entry(id) {
                slot.insert(now);
                // Its parent is the region its start key used to be the end of.
                if let Some(parent) = ends_at.get(&record.region.start_key)
                    && let Some(leader) = leader_of.get(parent)
                {
                    parent_led_by.insert(id, *leader);
                }
            }
            if let Some(&leader) = leader_of.get(&id)
                && let Some(seen) = first_seen.remove(&id)
            {
                led_after.push(now.duration_since(seen).as_secs_f64() * 1000.0);
                if let Some(parent_leader) = parent_led_by.get(&id) {
                    if *parent_leader == leader {
                        inherited += 1;
                    } else {
                        elsewhere += 1;
                    }
                }
                done.insert(id);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    stop.store(true, Ordering::Relaxed);
    let rows = loader.join().unwrap();
    let refusals = refusals.lock().map(|seen| seen.clone()).unwrap_or_default();
    Sightings {
        led_after,
        inherited,
        elsewhere,
        refusals,
        rows,
        regions: gate.regions(),
    }
}

/// **A split child's first leader is the store that led its parent.**
///
/// [ADR 0094](../../../docs/adr/0094-a-split-childs-leader-is-the-parents-leader.md)'s gate test,
/// and it asserts a **structure** rather than a duration. The first version asserted a median
/// time-to-leader under thirty milliseconds, which is a *performance* property: it passed on a
/// quiet box and failed at 84 ms on a gate that was running two chains at once, which is the
/// load-sensitive assertion this repository has been bitten by before and had just written down
/// again for the mpp differential. The wall clock stays, in the `#[ignore]`d measurement below,
/// where a number is information rather than a verdict.
///
/// What the change actually does is make the **parent's leader** the one that stands, so that is
/// what this asks. Before it, the child elects from scratch and the winner is whichever of three
/// voters times out first — about one in three. After it, the parent's leader campaigns before any
/// timeout can fire and the others answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_child_is_led_by_the_store_that_led_its_parent() {
    let gate = Gate::start_splitting(8 * 1024).await;
    let seen = tokio::task::block_in_place(|| watch_a_splitting_load(&gate));
    gate.stop().await;

    let decided = seen.inherited + seen.elsewhere;
    assert!(
        decided >= 20,
        "only {decided} children could be matched to a parent, so this asserts nothing \
         ({} rows, {} regions)",
        seen.rows,
        seen.regions
    );
    // Four in five, against about one in three when the child elects from scratch: the bar is far
    // from both, so it is a statement about which mechanism ran and not about how fast the box is.
    assert!(
        seen.inherited * 5 >= decided * 4,
        "only {} of {decided} split children were led by the store that led their parent, which \
         is what an election from scratch looks like rather than a campaign the parent started",
        seen.inherited
    );
}

/// **The window itself, printed rather than asserted.**
///
/// The number ADR 0094 is about — a child's time to its first leader — measured across every split
/// of a real load. It is `#[ignore]`d because a duration on a shared box is information and not a
/// verdict; `a_split_child_is_led_by_the_store_that_led_its_parent` is what the gate runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints the leaderless-window distribution; a duration is not a gate assertion"]
async fn how_long_a_split_child_has_no_leader() {
    let gate = Gate::start_splitting(8 * 1024).await;
    let mut seen = tokio::task::block_in_place(|| watch_a_splitting_load(&gate));
    gate.stop().await;
    report(seen.rows, seen.regions, &mut seen.led_after, &seen.refusals);
    println!(
        "  {} of {} children were led by their parent's leader",
        seen.inherited,
        seen.inherited + seen.elsewhere
    );
}

/// A refusal's message with its numbers folded away, so two refusals that differ only in a region
/// id or a byte count group together.
///
/// Digits rather than words: the ids in these messages (`region 1`, `after 9 attempts`) are what
/// makes every refusal look unique, and folding them is what turns a list into a census.
fn refusal_shape(said: &str) -> String {
    let mut shape = String::with_capacity(said.len());
    let mut in_digits = false;
    for ch in said.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                shape.push_str("<n>");
                in_digits = true;
            }
        } else {
            in_digits = false;
            shape.push(ch);
        }
    }
    shape
}

/// The distribution and the refusals, lifted out of the test that takes them.
fn report(rows: i64, regions: usize, led_after: &mut [f64], refusals: &[String]) -> f64 {
    led_after.sort_by(f64::total_cmp);
    println!("\n  {rows} rows loaded, {regions} regions");
    println!(
        "  {} children measured from first sighting to first leader",
        led_after.len()
    );
    if !led_after.is_empty() {
        let at = |num: usize, den: usize| led_after[(led_after.len() - 1) * num / den];
        println!(
            "  min {:.0} ms   median {:.0} ms   p90 {:.0} ms   max {:.0} ms",
            led_after[0],
            at(1, 2),
            at(9, 10),
            led_after[led_after.len() - 1]
        );
    }
    println!("  {} refusals while loading", refusals.len());
    // **Grouped by the shape of the message, not by its first seven bytes** — #107. Seven bytes is
    // the sqlstate bracket, and the two mechanisms that row is about arrive with the *same* one: a
    // region that genuinely had no leader, and a client that kept asking a peer that was not the
    // leader while one existed. What separates them is the text, which this collected and then
    // dropped. One sample is printed verbatim per shape, because the shape is what groups and the
    // sample is what a reader recognises.
    let mut kinds: BTreeMap<String, (usize, &str)> = BTreeMap::new();
    for said in refusals {
        let seen = kinds
            .entry(refusal_shape(said))
            .or_insert((0, said.as_str()));
        seen.0 += 1;
    }
    for (shape, (count, sample)) in &kinds {
        println!("    x{count}  {shape}");
        println!("          e.g. {sample}");
    }
    if led_after.is_empty() {
        0.0
    } else {
        led_after[(led_after.len() - 1) / 2]
    }
}

/// **A bulk load into a table that splits fifty times is not refused because it split.**
///
/// [ADR 0094](../../../docs/adr/0094-a-split-childs-leader-is-the-parents-leader.md)'s red test, in
/// the shape that produces the failure: two hundred and fifty rows a statement, which met `40003`
/// at 37 and at 76 regions where a fifty-row loader met none.
///
/// **What it asserts is the code and not the count.** A refusal that is `08006` — a client whose
/// region cache is behind a split — is the routing repair working and is not this test's subject.
/// A `40003` is *the outcome of this statement is unknown*, which a client cannot retry and a user
/// cannot ignore, and a table splitting under its own load is not a reason to hand one out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "counts ambiguous outcomes across fifty splits; one run in three sees one"]
async fn a_bulk_load_into_a_splitting_table_is_not_told_it_does_not_know() {
    let gate = Gate::start_splitting(8 * 1024).await;
    let ambiguous = tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE t (id int8 PRIMARY KEY, pad text)",
        );
        let mut ambiguous: Vec<String> = Vec::new();
        let mut id = 1_i64;
        while id < 6_000 && gate.regions() < 60 {
            let values: Vec<String> = (id..id + 250)
                .map(|n| format!("({n}, 'pad-{n}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"))
                .collect();
            // `run`, not `settle`: the retry is what hides the answer this test is about.
            if let Err(error) = session.run(&format!("INSERT INTO t VALUES {}", values.join(", ")))
                && error.sqlstate() == esker_sql::sqlstate::STATEMENT_COMPLETION_UNKNOWN
            {
                ambiguous.push(format!(
                    "at {} rows, {} regions: {error}",
                    id,
                    gate.regions()
                ));
            }
            id += 250;
        }
        ambiguous
    });
    let regions = gate.regions();
    gate.stop().await;

    assert!(
        regions >= 50,
        "the table split only {regions} times, so this measured nothing"
    );
    // **Counted, not asserted, and the three rounds that decided it are the reason.** One of them
    // was refused twice and two were not, so an assertion here would be a gate test that fails one
    // run in three — and the message on those refusals is `region N *stopped leading* with this
    // proposal in its log`, which is a peer that had leadership and lost it, not a child that never
    // had one. ADR 0094 removes the second and says so: *"the `40003` is not removed, it is made
    // rare"*. What it changes is measured by `how_long_a_split_child_has_no_leader`, which asserts.
    println!(
        "  {} ambiguous outcome(s) across {regions} splits{}",
        ambiguous.len(),
        if ambiguous.is_empty() {
            String::new()
        } else {
            format!(":\n  {}", ambiguous.join("\n  "))
        }
    );
}

/// **What one catalog-introspection statement costs below the SQL** — `debts-v1.1.md` #49's first
/// number, and the reason its instrument was built.
///
/// Run 113 timed three of these on the real topology at **0.5–0.8 s each** — `pg_index ⋈
/// pg_attribute` 790 ms, `obj_description` 546 ms, `pg_inherits` 793 ms — and
/// [ADR 0102](../../../docs/adr/0102-the-catalogs-read-path.md)'s instrument, which was **on for
/// the same run**, accounts for 2.2 ms of a 506 ms statement. So the half-second is not a slower
/// version of a known cost, and the three numbers that could name it did not exist.
///
/// This is the statement `ActiveRecord` sends to find a table's primary key, taken verbatim from
/// `tests/corpus/activerecord_8_1_statements.txt` rather than invented, run against a `Gate` — real
/// stores, a real placement driver, a `StoreBackend` over a router with a lease.
///
/// **Prints and asserts nothing about the numbers.** A measurement that fails a build is a gate,
/// and this is not one: what it asserts is that the instrument was on and that the statement
/// answered, because a zero from a switched-off counter reads exactly like a zero from a statement
/// that made no reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a debts-v1.1.md #49 measurement: a cluster, and it wants ESKER_STMT_STATS=1"]
async fn what_an_introspection_statement_costs_below_the_sql() {
    let gate = Gate::start_splitting(u64::MAX).await;
    println!("{}", the_box_right_now("introspection measurement"));
    assert!(
        esker_sql::stmt_stats::enabled(),
        "ESKER_STMT_STATS is not set: every counter would stay at zero and this run would say \
         nothing — which reads exactly like a statement that made no reads"
    );
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        settle(
            &mut session,
            "CREATE TABLE widgets (id int8 PRIMARY KEY, n int8)",
        );
        // `ActiveRecord`'s primary-key reflection, verbatim.
        let introspection = "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON \
                             a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) WHERE \
                             i.indrelid = 'widgets'::regclass AND i.indisprimary \
                             ORDER BY array_position(i.indkey, a.attnum)";
        let before = esker_sql::stmt_stats::counts();
        let began = Instant::now();
        let answered = session.run(introspection).is_ok();
        let took = began.elapsed();
        let after = esker_sql::stmt_stats::counts();
        assert!(answered, "the introspection statement did not answer");
        println!("\n  {introspection}\n");
        println!("  took          {:>8.1} ms", took.as_secs_f64() * 1000.0);
        println!("  point reads   {:>8}", after.1 - before.1);
        println!("  range scans   {:>8}", after.2 - before.2);
        println!("  round trips   {:>8}", after.3 - before.3);
        println!("  regions       {:>8}", after.4 - before.4);
        println!("\n  {}\n", esker_sql::stmt_stats::summary());
    });
    gate.stop().await;
}

// -- #88: what a fragment meeting a lock costs ---------------------------------------------------

/// A [`FragmentSource`] that counts what it was asked and what came back.
///
/// #88's row says the cost half of that debt "has no measurement at all": nobody has counted how
/// often a fragment meets a lock under a real write workload, and that number decides whether
/// carrying the locked keys in the refusal is a day's work or a note. Counting here rather than in
/// the product keeps the measurement out of the thing measured — the same reason `CountingOracle`
/// and the fake transport exist.
#[derive(Debug)]
struct CountingFragments {
    inner: Arc<dyn FragmentSource>,
    asked: AtomicU64,
    too_far_behind: AtomicU64,
    not_columnar: AtomicU64,
    unsupported: AtomicU64,
    /// **One entry per encounter with a lock** — 0117's third question, and the reason this
    /// wrapper records rather than classifies.
    ///
    /// `TooFarBehind` is **not** the unresolved-lock refusal's own reason: #86 reused it on purpose
    /// (*"`TooFarBehind` and not a reason of its own"*), so a learner that is genuinely behind
    /// answers the same tag. The only thing that separates them is the detail, which #86 writes as
    /// *"a transaction at `{start_ts}` still holds a lock on … in region …"*. The detail is kept
    /// verbatim here and read by the caller; parsing the key back out of it is **not** possible,
    /// because `printable` is not reversible.
    ///
    /// Recording rather than classifying keeps the forensics where the table is known: this
    /// wrapper sees a shard, and deriving `(tenant, table_id)` from a shard's start key is a
    /// guess when the region begins before the table does.
    encounters: std::sync::Mutex<Vec<(u64, String)>>,
}

impl CountingFragments {
    fn new(inner: Arc<dyn FragmentSource>) -> Self {
        Self {
            inner,
            asked: AtomicU64::new(0),
            too_far_behind: AtomicU64::new(0),
            not_columnar: AtomicU64::new(0),
            unsupported: AtomicU64::new(0),
            encounters: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Asked, and refused for each reason, since the last [`Self::reset`].
    fn taken(&self) -> (u64, u64, u64, u64) {
        (
            self.asked.load(Ordering::Relaxed),
            self.too_far_behind.load(Ordering::Relaxed),
            self.not_columnar.load(Ordering::Relaxed),
            self.unsupported.load(Ordering::Relaxed),
        )
    }

    fn reset(&self) {
        self.asked.store(0, Ordering::Relaxed);
        self.too_far_behind.store(0, Ordering::Relaxed);
        self.not_columnar.store(0, Ordering::Relaxed);
        self.unsupported.store(0, Ordering::Relaxed);
        // **Cleared with the counters, not left to accumulate.** A round's encounters belong to
        // that round: a list that outlived `reset` would let one round's locks be counted in the
        // next one's ratio, which is the arithmetic this measurement exists to get right.
        if let Ok(mut met) = self.encounters.lock() {
            met.clear();
        }
    }
}

impl FragmentSource for CountingFragments {
    /// Forwarded, never answered here: the trait says a defaulted method is a silent opt-out, and
    /// a decorator that answered this itself would be declaring something about a store it wraps.
    fn runs_are_region_scoped(&self) -> bool {
        self.inner.runs_are_region_scoped()
    }

    fn shards(&self, start: &[u8], end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        self.inner.shards(start, end)
    }

    fn evaluate(
        &self,
        shard: &Shard,
        fragment: &[u8],
        ts: u64,
        min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        let answer = self.inner.evaluate(shard, fragment, ts, min_apply_index);
        // **`detail` is bound rather than discarded**, which is the whole of this hook: the counter
        // above says a fragment was refused, and only the detail says whether a *lock* was what
        // refused it.
        if let Ok(Answer::Refused { reason, detail }) = &answer {
            match reason {
                RefusalReason::TooFarBehind => &self.too_far_behind,
                RefusalReason::NotColumnar => &self.not_columnar,
                RefusalReason::Unsupported => &self.unsupported,
            }
            .fetch_add(1, Ordering::Relaxed);
            // #86's own words, and the one string that tells its refusal from a learner that is
            // simply behind. Matched on the stable head of the sentence, not on the whole of it:
            // the tail carries a printed key and a region id that change every time.
            if *reason == RefusalReason::TooFarBehind
                && detail.contains("still holds a lock on")
                && let Ok(mut met) = self.encounters.lock()
            {
                met.push((ts, detail.clone()));
            }
        }
        answer
    }
}

impl Gate {
    /// [`Gate::session`], asking a fragment source of the caller's choosing.
    fn session_asking(&self, source: Arc<dyn FragmentSource>) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                TENANT,
                esker_sql::session::register(),
            )
            .reporting_columnar_to(Arc::clone(&self.conn) as Arc<dyn ColumnarReport>)
            .asking_fragments_of(source),
        }
    }

    /// One table of `rows` rows with a columnar copy, filled five hundred rows to a statement.
    ///
    /// The batching is `fill_join_at_scale`'s and for its reason: one statement per row is one
    /// *transaction* per row, and a million of those is the measurement's own cost rather than the
    /// thing being measured. The fill's own duration is printed because it decides how much of a
    /// window is left for measuring.
    /// The id of a table this harness created, or `None` before it exists.
    ///
    /// Copied from `tests/joint_gate.rs`, as the header says these helpers deliberately are. The
    /// `Option` is not decoration: the first statement the fill settles is the `CREATE TABLE`, so
    /// the forensics below run at an instant when this table has no id yet, and a diagnostic that
    /// panics is the worst kind of failure to add.
    fn table_id(&self, name: &str) -> Option<u64> {
        let txn = self.backend.begin().ok()?;
        let id = self
            .catalog
            .view(&*txn, TENANT)
            .ok()
            .and_then(|view| view.table(name).ok().flatten())
            .map(|def| def.id);
        let _ = txn.rollback();
        id
    }

    /// **#108**: every lock every store holds over this table's rows, at the instant of a refusal.
    ///
    /// The two incidents this is written for ended with a fill's own lock still unclearable minutes
    /// later, and both left only a `start_ts` because the in-process cluster dies with the test.
    /// What separates *the transaction is still alive* from *the lock is stranded and resolution
    /// never reaches it* is in the record — its `primary` and its lease — and in whether that
    /// primary has a `write` record at all. Both are read here, and no format is touched.
    fn lock_forensics(&self, table: &str) {
        let Some(table_id) = self.table_id(table) else {
            println!("  #108: {table} has no id yet, so it holds no row locks");
            return;
        };
        let first = self.sample_locks(table, table_id, "first");
        if first.is_empty() {
            println!("  #108: no lock over {table} on any store — nothing to attribute");
            return;
        }
        // **A second look, one lease later, and it is the whole discriminator.** `is_expired`
        // compares `physical_ms(now)` against `physical_ms(start_ts) + ttl_ms`, and `start_ts`
        // cannot move — so a heartbeat can only show as a **larger `ttl_ms`**. One sample says a
        // lock is past its lease; two say whether anybody is still pushing that lease out.
        std::thread::sleep(Duration::from_millis(esker_client::LOCK_TTL_MS));
        let second = self.sample_locks(table, table_id, "second");
        compare_samples(&first, &second);
    }

    /// One pass over every store, printed as it is taken.
    fn sample_locks(&self, table: &str, table_id: u64, which: &str) -> Vec<StrandedLock> {
        let Ok(now_ts) = self.oracle.timestamp() else {
            println!("  #108: the oracle would not answer, so no lease can be judged");
            return Vec::new();
        };
        let mut all = Vec::new();
        for (at, node) in self.nodes.iter().enumerate() {
            let locks = locks_over_table(at, &node.store, TENANT, table_id, now_ts);
            report_stranded(at, &node.store, &locks);
            all.extend(locks);
        }
        println!(
            "  #108 {which} sample: {} lock(s) over {table} across {} stores, at ts {now_ts}",
            all.len(),
            self.nodes.len()
        );
        all
    }

    /// `settle`, and if it gives up, the lock census before the panic is let through.
    ///
    /// The panic is resumed rather than swallowed: this adds a diagnosis to the failure, it does
    /// not turn a failure into a pass.
    fn settle_or_report(&self, session: &mut Session, sql: &str, table: &str) {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settle(session, sql)));
        if let Err(panic) = outcome {
            println!("#108 forensics: `{sql}` gave up; the locks over {table} at this instant:");
            self.lock_forensics(table);
            std::panic::resume_unwind(panic);
        }
    }

    /// [`Gate::fill_one_at_scale`], with a reading of the profile taken around every statement —
    /// debt #109.
    ///
    /// **The same fill, not a second one.** It goes through `settle_or_report`, so #108's lock
    /// forensics still fire if a statement gives up, and the rows land exactly as the #88
    /// measurement's do. One window pays for one fill and three questions are answered from it.
    ///
    /// What comes back is the fill's own duration and **one reading per `INSERT`**, each the
    /// difference between the profile before and after that statement. A median over those is a
    /// median over *statements*, which is what #109 asks for: a total over the whole fill cannot
    /// separate a phase that grows from a phase that is merely run more often.
    /// `index` gives the table a **secondary** index before a single row is inserted, which is the
    /// only way the index phases can be anything but structurally zero: the shape this fill shares
    /// with #88's has a primary key and nothing else, so `dml.rs`'s index-entry loops never run.
    /// A tier built this way is **not comparable** with the others and the report says so.
    async fn fill_profiled(
        &self,
        table: &str,
        rows: i64,
        index: bool,
    ) -> (Duration, Vec<cluster::profile::Counts>) {
        let filled = tokio::task::block_in_place(|| {
            let mut session = self.session();
            self.settle_or_report(
                &mut session,
                &format!("CREATE TABLE {table} (id int8 PRIMARY KEY, n int8, note text)"),
                table,
            );
            self.settle_or_report(
                &mut session,
                &format!("ALTER TABLE {table} SET (columnar_replicas = 1)"),
                table,
            );
            if index {
                // **Before the first row**, so every `INSERT` pays for it. An index built after the
                // fill would price a different statement than the one this measures.
                self.settle_or_report(
                    &mut session,
                    &format!("CREATE INDEX {table}_n ON {table} (n)"),
                    table,
                );
            }
            // **The two statements above are outside the readings on purpose.** They run once, and
            // a per-`INSERT` median carrying a share of them would report a cost no row ever pays
            // again — the same confound as pricing a fill by its total.
            let mut per_statement: Vec<cluster::profile::Counts> = Vec::new();
            let began = Instant::now();
            for chunk in (1..=rows).collect::<Vec<i64>>().chunks(500) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, {id}, 'n{id}')"))
                    .collect();
                // A statement that `settle` had to retry spends the retry inside this window, which
                // is right: what is being priced is the statement, not the attempt.
                let before = self.profile.read();
                self.settle_or_report(
                    &mut session,
                    &format!("INSERT INTO {table} VALUES {}", values.join(", ")),
                    table,
                );
                per_statement.push(self.profile.read().since(before));
            }
            let filled_in = began.elapsed();
            println!(
                "{table}: {rows} rows filled in {filled_in:?}, {} statements measured",
                per_statement.len()
            );
            (filled_in, per_statement)
        });
        self.wait_for_a_learner_that_answers(table).await;
        filled
    }

    async fn fill_one_at_scale(&self, table: &str, rows: i64) -> Duration {
        let filled_in = tokio::task::block_in_place(|| {
            let mut session = self.session();
            self.settle_or_report(
                &mut session,
                &format!("CREATE TABLE {table} (id int8 PRIMARY KEY, n int8, note text)"),
                table,
            );
            self.settle_or_report(
                &mut session,
                &format!("ALTER TABLE {table} SET (columnar_replicas = 1)"),
                table,
            );
            let began = Instant::now();
            for chunk in (1..=rows).collect::<Vec<i64>>().chunks(500) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, {id}, 'n{id}')"))
                    .collect();
                self.settle_or_report(
                    &mut session,
                    &format!("INSERT INTO {table} VALUES {}", values.join(", ")),
                    table,
                );
            }
            let filled_in = began.elapsed();
            println!("{table}: {rows} rows filled in {filled_in:?}");
            filled_in
        });
        self.wait_for_a_learner_that_answers(table).await;
        filled_in
    }
}

/// Rows each table starts with. Enough that a scan is worth routing and small enough that the
/// writer can touch a meaningful share of them.
const MEASURE_ROWS: i64 = 400;

/// Scans per table. The number the measurement divides by.
const MEASURE_SCANS: usize = 120;

/// How long the writer keeps a transaction open between its statements and its `COMMIT`.
///
/// A few milliseconds is what makes the lock *findable*: long enough that a scan arriving at random
/// can meet it, short enough that the writer still commits hundreds of times inside a window.
const WRITER_HOLDS_MS: u64 = 3;

/// **How often a columnar scan meets a lock, and what the fallback costs when it does** — #88.
///
/// Two tables with columnar copies: a **hot** one a writer commits to continuously, and a **cold**
/// one nobody writes. The same aggregate is scanned on each, and the source is wrapped so that the
/// refusals are counted where they happen rather than guessed at from `EXPLAIN`.
///
/// It prints and asserts only that it measured something: the ratio is the number #88 owes, and a
/// threshold on it would be a claim about this box. The fallback's cost is printed as the median of
/// the scans that met a lock against the median of those that did not — a print, not a bound, for
/// the reason every clock in this file is a print.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#88's cost: a real cluster, a writer thread, and minutes of a quiet box"]
async fn what_a_fragment_meeting_a_lock_costs() {
    let gate = Gate::start().await;
    fill_measure_tables(&gate);
    gate.wait_for_a_learner_that_answers("hot").await;
    gate.wait_for_a_learner_that_answers("cold").await;

    let counted = Arc::new(CountingFragments::new(Arc::clone(&gate.fragments)));
    let mut report: Vec<Round> = Vec::new();

    for table in ["hot", "cold"] {
        let writing = Arc::new(AtomicBool::new(table == "hot"));
        let held = Arc::new(AtomicU64::new(0));
        let writer = spawn_writer_over(&gate, "hot", MEASURE_ROWS, &writing, &held);

        let scanning = Instant::now();
        let (met_times, clean_times, failed) =
            tokio::task::block_in_place(|| scan_a_table_n(&gate, &counted, table, MEASURE_SCANS));
        let scanned_for = scanning.elapsed();

        writing.store(false, Ordering::Relaxed);
        writer.join().expect("the writer thread ends");
        // The denominator the first run had no way to state: how much of the scanning window a
        // transaction was open for at all. A ratio of encounters to scans means nothing without it.
        println!(
            "{table}: scanned for {scanned_for:?}, a transaction was open for {:?} of it",
            Duration::from_micros(held.load(Ordering::Relaxed))
        );
        report.push(Round {
            table: table.to_owned(),
            met: u64::try_from(met_times.len()).unwrap_or(u64::MAX),
            clean: u64::try_from(clean_times.len()).unwrap_or(u64::MAX),
            failed,
            met_times,
            clean_times,
        });
    }

    print_report(&report);

    let scans: u64 = report.iter().map(|round| round.met + round.clean).sum();
    assert_eq!(
        scans,
        2 * MEASURE_SCANS as u64,
        "the measurement did not run every scan"
    );
    assert!(
        report.iter().any(|round| round.met > 0),
        "no scan met a lock at all: the writer's shape, not the system's, is what this measured"
    );
}

/// One table's worth of the measurement. A struct rather than a tuple, because six fields of two
/// types is exactly the shape nobody can read at the call site.
struct Round {
    table: String,
    met: u64,
    clean: u64,
    failed: u64,
    met_times: Vec<Duration>,
    clean_times: Vec<Duration>,
}

/// The hot and cold tables, each with a columnar copy and the same rows.
fn fill_measure_tables(gate: &Gate) {
    tokio::task::block_in_place(|| {
        let mut session = gate.session();
        for table in ["hot", "cold"] {
            settle(
                &mut session,
                &format!("CREATE TABLE {table} (id int8 PRIMARY KEY, n int8, note text)"),
            );
            settle(
                &mut session,
                &format!("ALTER TABLE {table} SET (columnar_replicas = 1)"),
            );
            for chunk in (1..=MEASURE_ROWS).collect::<Vec<i64>>().chunks(100) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, {id}, 'n{id}')"))
                    .collect();
                settle(
                    &mut session,
                    &format!("INSERT INTO {table} VALUES {}", values.join(", ")),
                );
            }
        }
    });
}

/// The writer beside the hot table: **an open transaction, not an autocommit statement**.
///
/// A statement that commits at once leaves a lock for microseconds, and a scan meets it only by
/// coincidence — the first run of this measurement did exactly that and saw one encounter in sixty
/// scans, which is a fact about the writer's shape and not about the system. #88 is about a scan
/// meeting a lock whose transaction is *still open*, which is what Rails does between its `BEGIN`
/// and its `COMMIT`.
/// Leaves **#86's shape** behind the caller: a prewrite over two keys, and then — if asked — a
/// commit of the **primary alone**, so the secondary's lock stays with nobody coming back for it.
///
/// **Why the wire and not the client.** `esker-client`'s `Transaction` does not expose prewrite on
/// its own; both phases happen inside `commit`, so a transaction driven through it either commits
/// everything or leaves nothing committed. The store's request API is what separates them, which is
/// how `joint_gate.rs::strand_a_secondary_lock` builds the same state. This one sends through
/// [`esker_client::router::Router::call`] rather than copying that file's election-retry helper:
/// the router already looks up the leader, carries the epoch, retries and re-routes.
///
/// **Two arms, one helper.** The lock's life is `ttl_ms`, a field of the prewrite rather than the
/// client's `LOCK_TTL_MS`, so the caller picks it:
///
/// * **stranded** — a short `ttl_ms`, `commit_primary = true`, and the caller waits past the TTL
///   before reading. Past its lease the lock is *resolvable*, which is what makes the row path roll
///   it forward and the fragment refuse.
/// * **alive (the control)** — a long `ttl_ms`, `commit_primary = false`, and the caller reads well
///   inside the lease. `joint_gate.rs`'s own comment is why the distinction matters: *"a lock still
///   inside its lease is one the row path waits on rather than rolls forward, and then neither
///   engine answers and there is nothing to disagree about."*
///
/// Answers the `start_ts`, which is how a caller recognises **its own** lock in the forensics
/// rather than one the fill happened to leave.
fn strand_a_secondary(
    router: &Arc<Router>,
    oracle: &Arc<dyn TimestampOracle>,
    table_id: u64,
    primary_id: i64,
    secondary_id: i64,
    ttl_ms: u64,
    commit_primary: bool,
) -> u64 {
    let row = |id: i64, note: &str| {
        esker_keys::row::encode_row(
            &[
                esker_keys::value::ColumnType::Int8,
                esker_keys::value::ColumnType::Int8,
                esker_keys::value::ColumnType::Text,
            ],
            &[
                esker_keys::value::Datum::Int8(id),
                esker_keys::value::Datum::Int8(id),
                esker_keys::value::Datum::Text(note.to_owned()),
            ],
        )
        .expect("a row value")
    };
    let key = |id: i64| {
        bytes::Bytes::from(
            esker_keys::row::row_key(TENANT, table_id, &[esker_keys::value::Datum::Int8(id)])
                .expect("a row key"),
        )
    };
    // **The router and the oracle rather than the gate**, so a planting thread can hold them: a
    // thread needs `'static`, and `&Gate` is not. Both are `Arc` on the gate already, so a planter
    // clones them out and plants into the same cluster the scans are reading.
    let (primary, secondary) = (key(primary_id), key(secondary_id));
    let send = |request: esker_proto::txn::TxnKvReq| match router
        .call(&request.into())
        .expect("the router answered")
    {
        esker_client::wire::Response::TxnKv(answer) => answer,
        other => panic!("a TxnKv request came back as {other:?}"),
    };

    let start_ts = oracle.timestamp().expect("a timestamp");
    let prewritten = send(esker_proto::txn::TxnKvReq::Prewrite {
        start_ts,
        primary: primary.clone(),
        ttl_ms,
        mutations: vec![
            esker_proto::txn::TxnMutation::Put {
                key: primary.clone(),
                value: bytes::Bytes::from(row(primary_id, "primary")),
                read_ts: None,
            },
            esker_proto::txn::TxnMutation::Put {
                key: secondary.clone(),
                value: bytes::Bytes::from(row(secondary_id, "secondary")),
                read_ts: None,
            },
        ],
    });
    assert_eq!(
        prewritten,
        esker_proto::txn::TxnKvResp::prewrite_ok(2),
        "the prewrite this fixture is built on was refused, so no lock was left to find"
    );

    if commit_primary {
        let commit_ts = oracle.timestamp().expect("a timestamp");
        let committed = send(esker_proto::txn::TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![primary],
        });
        assert!(
            matches!(
                committed,
                esker_proto::txn::TxnKvResp::Commit {
                    status: esker_proto::txn::TxnStatus::Ok
                }
            ),
            "the primary did not commit, so the secondary is not stranded but merely locked: \
             {committed:?}"
        );
    }
    start_ts
}

/// Keeps planting stranded pairs until told to stop, and remembers every `start_ts` it planted.
///
/// **This is what the 12:09 window was missing, and the reason it is missing is worth keeping.**
/// That run planted *one* pair per arm and scanned sixty times, and met a lock three times in a
/// hundred and twenty scans — far under the floor of twenty written down before the run. The
/// classifier was not the problem and neither was the workload: a single pair is met a handful of
/// times and then the row path rolls it forward, so **the encounter rate was capped by the fixture
/// itself**. A planter lifts that cap without touching what is being measured.
///
/// **Not [`spawn_writer_over`]**, which commits after three milliseconds and therefore strands
/// nothing: what it makes is contention, and what this makes is #86's shape.
///
/// Every planted pair gets fresh row ids from `next_id`, so one round's locks are never mistaken
/// for the previous round's, and the `start_ts` list is how [`classify_planted`] tells a lock this
/// fixture placed from one the fill happened to leave.
fn spawn_planter(
    gate: &Gate,
    table_id: u64,
    first_id: i64,
    ttl_ms: u64,
    commit_primary: bool,
    planting: &Arc<AtomicBool>,
    planted: &Arc<std::sync::Mutex<Vec<u64>>>,
) -> std::thread::JoinHandle<()> {
    // Cloned out of the gate rather than borrowed from it: a thread outlives this frame, and both
    // of these are `Arc` precisely so that it can.
    let router = Arc::clone(gate.client.router());
    let oracle = Arc::clone(&gate.oracle);
    let planting = Arc::clone(planting);
    let planted = Arc::clone(planted);
    std::thread::spawn(move || {
        let mut next_id = first_id;
        while planting.load(Ordering::Relaxed) {
            let start_ts = strand_a_secondary(
                &router,
                &oracle,
                table_id,
                next_id,
                next_id + 1,
                ttl_ms,
                commit_primary,
            );
            if let Ok(mut seen) = planted.lock() {
                seen.push(start_ts);
            }
            next_id += 2;
            // **Half a lease between plantings**, derived rather than passed: the interval is only
            // ever meaningful against the lease it is planting, and a caller free to choose both
            // could set an interval longer than the lease and quietly plant one lock at a time.
            std::thread::sleep(Duration::from_millis(ttl_ms / 2));
        }
    })
}

fn spawn_writer_over(
    gate: &Gate,
    table: &str,
    rows: i64,
    writing: &Arc<AtomicBool>,
    held: &Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    let backend = Arc::clone(&gate.backend);
    let catalog = Arc::clone(&gate.catalog);
    let writing = Arc::clone(writing);
    let held = Arc::clone(held);
    // The thread outlives this frame, so it owns its table's name rather than borrowing it.
    let table = table.to_owned();
    std::thread::spawn(move || {
        let mut session = Session {
            executor: Executor::new(backend, catalog, TENANT, esker_sql::session::register()),
        };
        let mut at = rows;
        while writing.load(Ordering::Relaxed) {
            at += 1;
            let opened = Instant::now();
            let _ = session.run("BEGIN");
            let _ = session.run(&format!("INSERT INTO {table} VALUES ({at}, {at}, 'w')"));
            let _ = session.run(&format!(
                "UPDATE {table} SET n = n + 1 WHERE id = {}",
                at % rows + 1
            ));
            std::thread::sleep(Duration::from_millis(WRITER_HOLDS_MS));
            let _ = session.run("COMMIT");
            held.fetch_add(
                u64::try_from(opened.elapsed().as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    })
}

/// One table's scans: the times of those that met a lock, of those that did not, and how many
/// statements the contention refused outright.
fn scan_a_table_n(
    gate: &Gate,
    counted: &Arc<CountingFragments>,
    table: &str,
    scans: usize,
) -> (Vec<Duration>, Vec<Duration>, u64) {
    let mut session = gate.session_asking(Arc::clone(counted) as Arc<dyn FragmentSource>);
    let mut met = Vec::new();
    let mut missed = Vec::new();
    let mut refused_statements = 0_u64;
    for _ in 0..scans {
        counted.reset();
        let at = Instant::now();
        // **Tolerant on purpose.** `rows` unwraps, and the writer beside this is making contention:
        // one `40001` would end a measurement rather than be part of it. A statement that fails is
        // counted and the run carries on, because "how often does this happen" is the question.
        let answered = session
            .run(&format!("SELECT count(*) FROM {table}"))
            .is_ok();
        let took = at.elapsed();
        if !answered {
            refused_statements += 1;
        }
        let (_, behind, not_columnar, unsupported) = counted.taken();
        if behind + not_columnar + unsupported > 0 {
            met.push(took);
        } else {
            missed.push(took);
        }
    }
    (met, missed, refused_statements)
}

/// The table the measurement prints. **Scans per lock is integer tenths**, not a float: the ratio is
/// a count divided by a count, and casting two `u64`s to `f64` to print one decimal is a precision
/// loss with nothing to gain.
fn print_report(report: &[Round]) {
    println!(
        "table  scans  met a lock  clean  statements that failed  scans per lock  median met  median clean"
    );
    for round in report {
        let scans = round.met + round.clean;
        // `checked_div` rather than a zero check standing beside a division: clippy's
        // `manual_checked_ops` is right that writing the guard and the operation apart is what lets
        // the two drift. Still integers — a count over a count needs no float to print one decimal.
        let per_lock = (scans * 10).checked_div(round.met).map_or_else(
            || "never".to_owned(),
            |tenths| format!("{}.{}", tenths / 10, tenths % 10),
        );
        let met_column = if round.met_times.len() <= 3 {
            format!("{:?}", round.met_times)
        } else {
            format!("{:?}", median_of(&round.met_times))
        };
        let (table, met, clean, failed) =
            (round.table.as_str(), round.met, round.clean, round.failed);
        println!(
            "{table:<7}{scans:<7}{met:<12}{clean:<7}{failed:<24}{per_lock:<16}{met_column:<28}{:?}",
            median_of(&round.clean_times),
        );
    }
}

/// The median of `samples`, or zero when there are none — a measurement that read nothing says so
/// rather than dividing by it. **Three samples or fewer are printed raw by the caller**: a "median"
/// of one is a single reading wearing a statistic's name.
fn median_of(samples: &[Duration]) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// Rows per tier. The first tier is run whole before the next is filled, so a window that runs out
/// leaves finished tiers rather than half a table.
///
/// **Two tiers, not three.** The first window measured the fill at 356.5 s for 100k rows and found
/// the rate falling as the store grows (657 rows a second into an empty store, 280 into one holding
/// 100k), which puts a 500k fill near half an hour and a 1M fill past the whole window — the third
/// tier was never reachable, and reaching for it is what cost the first window its data. Two points
/// give a slope, and a slope is what the ADR needs.
const SCALE_TIERS: [i64; 2] = [100_000, 200_000];

/// Rounds per tier. One round is not the number: the same measurement run twice on the small table
/// met a lock 22 times in 120 scans and then 13, and put the fallback's cost at +27% and then
/// +105% — a 1.7x spread. A median over rounds with its own spread beside it is the number.
const SCALE_ROUNDS: usize = 5;

/// One stranded lock as the forensics print it — #108.
///
/// A struct rather than a tuple: six fields of four types is what nobody can read at the call site.
struct StrandedLock {
    /// Which store this copy was read from. **Not decoration**: every replica of the region
    /// holds the same lock row, so a census across four stores lists one logical lock four
    /// times, and pairing the two samples by key alone would match store 0's second look
    /// against store 2's first. The pairing is `(at, key)` for that reason.
    at: usize,
    key: Vec<u8>,
    kind: String,
    start_ts: u64,
    ttl_ms: u64,
    primary: Vec<u8>,
    /// How far past its lease it is, in physical milliseconds; `None` while it is still inside one.
    expired_by_ms: Option<u64>,
}

/// Every lock one store holds over a table's rows, decoded — **#108's forensics**.
///
/// Two incidents have ended with a fill's own lock still unclearable minutes later, and both left
/// only a `start_ts` because the in-process cluster dies with the test. What separates *the
/// transaction is still alive* from *the lock is stranded and resolution never reaches it* is the
/// record itself: its `primary` and its lease. Both are already in `LockRecord`, so nothing here
/// changes a format — this walks `cf::LOCK` the way `columnar::region::unresolved_lock` does,
/// because that is the reader whose refusal is being diagnosed.
fn locks_over_table(
    at: usize,
    store: &Arc<Store>,
    tenant: u64,
    table_id: u64,
    now_ts: u64,
) -> Vec<StrandedLock> {
    let (table_start, table_end) = esker_keys::row::table_row_range(tenant, table_id);
    let low = esker_txn::key::prefix(&table_start);
    let high = esker_txn::key::prefix(&table_end);
    let mut found = Vec::new();
    let Ok(mut iter) = store.db().iter(
        esker_engine::cf::LOCK,
        &esker_engine::ReadOptions::default(),
    ) else {
        return found;
    };
    iter.seek(&low);
    while iter.valid() && iter.key() < high.as_slice() {
        if let Ok(user_key) = esker_txn::key::split_lock(iter.key())
            && let Ok(lock) = esker_txn::LockRecord::decode(iter.value())
        {
            // Integer milliseconds throughout: `is_expired` compares physical halves, and a
            // saturating subtraction says "not yet" as zero rather than wrapping.
            let dead_at = esker_client::physical_ms(lock.start_ts).saturating_add(lock.ttl_ms);
            let past = esker_client::physical_ms(now_ts).saturating_sub(dead_at);
            found.push(StrandedLock {
                at,
                key: user_key,
                kind: format!("{:?}", lock.kind),
                start_ts: lock.start_ts,
                ttl_ms: lock.ttl_ms,
                primary: lock.primary.to_vec(),
                expired_by_ms: (past > 0).then_some(past),
            });
        }
        iter.next();
    }
    found
}

/// Prints one store's stranded locks, and what the `write` column family says about each primary.
///
/// The primary's write record is the other half of the discriminator: a lock whose primary has
/// **no** write record belongs to a transaction that neither committed nor rolled back, which is
/// the shape #108 is about.
fn report_stranded(at: usize, store: &Arc<Store>, locks: &[StrandedLock]) {
    for lock in locks {
        let primary_versions = store.write_records(&lock.primary).unwrap_or(0);
        let lease = match lock.expired_by_ms {
            Some(ms) => format!("expired {ms} ms ago"),
            None => "still inside its lease".to_owned(),
        };
        let is_primary = lock.key == lock.primary;
        println!(
            "  store {at}: key={} kind={} start_ts={} ttl_ms={} {lease} \
             is_primary={is_primary} primary={} primary_write_records={primary_versions}",
            printable(&lock.key),
            lock.kind,
            lock.start_ts,
            lock.ttl_ms,
            printable(&lock.primary),
        );
    }
}

/// What two samples a lease apart say about the same locks — **#108's discriminator**.
///
/// A heartbeat extends a transaction's lease, and the only field it can move is `ttl_ms`: the
/// comparison in `is_expired` is against `physical_ms(start_ts) + ttl_ms`, and `start_ts` is fixed
/// for the life of the transaction. So a lock whose `ttl_ms` grew between the two looks has an
/// owner that is alive and being kept alive; one whose `ttl_ms` did not move, and whose lease is
/// long past, is stranded with nothing resolving it. That is the difference two incidents left no
/// evidence for.
fn compare_samples(first: &[StrandedLock], second: &[StrandedLock]) {
    for before in first {
        let same = |later: &&StrandedLock| later.at == before.at && later.key == before.key;
        let verdict = match second.iter().find(same) {
            None => "gone — something resolved it between the two looks".to_owned(),
            Some(later) if later.start_ts != before.start_ts => format!(
                "replaced — a different transaction holds it now ({} then {})",
                before.start_ts, later.start_ts
            ),
            Some(later) if later.ttl_ms > before.ttl_ms => format!(
                "ALIVE — its lease grew from {} ms to {} ms, so its owner is being heartbeated",
                before.ttl_ms, later.ttl_ms
            ),
            Some(later) => match later.expired_by_ms {
                Some(ms) => format!(
                    "STRANDED — same lease of {} ms, now {ms} ms past it, and still here",
                    later.ttl_ms
                ),
                None => format!(
                    "inside its lease still ({} ms), too early to judge",
                    later.ttl_ms
                ),
            },
        };
        println!(
            "  #108 verdict for store {} key {}: {verdict}",
            before.at,
            printable(&before.key)
        );
    }
}

/// A key as a reader can recognise it, without pulling in a hex crate for a diagnostic.
fn printable(key: &[u8]) -> String {
    key.iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                (*byte as char).to_string()
            } else {
                format!("\\x{byte:02x}")
            }
        })
        .collect()
}

/// Scans per round, lowered as the table grows because one `count(*)` over a million rows is not
/// one over four hundred. The count is reported, so a reader never has to infer the sample size.
fn scans_for(rows: i64) -> usize {
    match rows {
        r if r <= 100_000 => 60,
        r if r <= 500_000 => 40,
        _ => 30,
    }
}

/// One tier's worth of the measurement: the table's size, the scans each round made, and a `Round`
/// per round. A struct rather than a tuple for `Round`'s own reason.
struct Tier {
    rows: i64,
    scans: usize,
    /// How long this tier's table took to fill. **First-class data, not a side note**: the first
    /// window filled 20k rows into an empty store at 657 rows a second and 100k at 280, so what an
    /// insert costs as a table grows is something this measurement reports rather than merely pays.
    fill: Duration,
    rounds: Vec<Round>,
}

/// **What the fallback costs as the table grows** — #88's second measurement.
///
/// The first one priced the fallback on a four-hundred-row table (86.6 ms against 68.2 ms), and that
/// is a constructed bargain: a table that small sits whole in the block cache, so falling back to
/// the row path costs almost nothing. Whether [ADR 0117] is worth writing depends on the *curve*,
/// which is why this runs three tiers rather than one big table.
///
/// The shape is the small measurement's — the same counting decorator, the same writer holding a
/// transaction open, the same `SELECT count(*)` — and only the row count moves. A cold table is
/// filled at the smallest tier only: the decisive comparison is met-a-lock against clean **on the
/// same table**, so a second table of a million rows would spend half the window on rows that take
/// part in no conclusion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#88's curve: a real cluster, a writer thread, and tens of minutes of a quiet box"]
async fn what_the_fallback_costs_as_the_table_grows() {
    let gate = Gate::start().await;
    let counted = Arc::new(CountingFragments::new(Arc::clone(&gate.fragments)));
    let mut tiers: Vec<Tier> = Vec::new();

    for rows in SCALE_TIERS {
        let hot = format!("hot{rows}");
        let fill = gate.fill_one_at_scale(&hot, rows).await;
        let scans = scans_for(rows);
        let mut rounds = Vec::new();
        for _ in 0..SCALE_ROUNDS {
            let round = measure_one_round(&gate, &counted, &hot, rows, scans);
            // **Printed as it finishes, not when the tier does.** The first large-table run lost
            // five completed rounds exactly here: the tier's table is printed after the tier, the
            // step after those rounds panicked, and everything measured was still in a `Vec`. A
            // round that has been printed cannot be taken away by a later failure.
            print_round(&round, scans);
            rounds.push(round);
        }
        tiers.push(Tier {
            rows,
            scans,
            fill,
            rounds,
        });
        print_scale_report(&tiers);
    }

    // **The control runs last, after every tier**, and nothing depends on it. It is the least
    // load-bearing step here, and in the first run its own fill and readiness check were what
    // failed — taking five measured rounds down with them.
    let cold_rows = SCALE_TIERS[0];
    let cold_scans = scans_for(cold_rows);
    let cold = format!("cold{cold_rows}");
    let _ = gate.fill_one_at_scale(&cold, cold_rows).await;
    let cold_round = measure_one_round(&gate, &counted, &cold, cold_rows, cold_scans);
    print_round(&cold_round, cold_scans);

    // As with the small measurement, the only assertion is that it measured something: a threshold
    // here would be a claim about this box rather than about the system.
    let met: u64 = tiers
        .iter()
        .flat_map(|tier| tier.rounds.iter())
        .chain(std::iter::once(&cold_round))
        .map(|round| round.met)
        .sum();
    assert!(
        met > 0,
        "no scan in any tier met a lock: the writer is not holding one"
    );
}

/// One round over one table: start the writer, scan, stop the writer.
///
/// A table whose name begins with `cold` is the control and gets no writer, which is how the same
/// helper serves both arms.
fn measure_one_round(
    gate: &Gate,
    counted: &Arc<CountingFragments>,
    table: &str,
    rows: i64,
    scans: usize,
) -> Round {
    let writing = Arc::new(AtomicBool::new(!table.starts_with("cold")));
    let held = Arc::new(AtomicU64::new(0));
    let writer = spawn_writer_over(gate, table, rows, &writing, &held);

    let (met_times, clean_times, failed) =
        tokio::task::block_in_place(|| scan_a_table_n(gate, counted, table, scans));

    writing.store(false, Ordering::Relaxed);
    writer.join().expect("the writer thread ends");
    // The denominator: a ratio of encounters to scans means nothing without how much of the window
    // a transaction was open at all.
    let open_for = Duration::from_micros(held.load(Ordering::Relaxed));
    println!("{table}: a transaction was open for {open_for:?} of the scanning window");
    Round {
        table: table.to_owned(),
        met: u64::try_from(met_times.len()).unwrap_or(u64::MAX),
        clean: u64::try_from(clean_times.len()).unwrap_or(u64::MAX),
        failed,
        met_times,
        clean_times,
    }
}

/// One round, printed the moment it is measured.
///
/// The tier table below is the report; this is the receipt. A measurement that prints only when a
/// tier completes can be erased by anything that fails after the rounds and before the print, which
/// is how the first large-table run lost five of them.
fn print_round(round: &Round, scans: usize) {
    let per_lock = (u64::try_from(scans).unwrap_or(u64::MAX) * 10)
        .checked_div(round.met)
        .map_or_else(
            || "never".to_owned(),
            |tenths| format!("{}.{}", tenths / 10, tenths % 10),
        );
    let met_median = median_of(&round.met_times);
    let clean_median = median_of(&round.clean_times);
    let (table, met, clean, failed) = (round.table.as_str(), round.met, round.clean, round.failed);
    println!(
        "round {table}: {met} met, {clean} clean, {failed} failed, one lock every {per_lock} scans, \
         median met {met_median:?}, median clean {clean_median:?}"
    );
}

/// The tier table: the median **across rounds**, with the spread that median is hiding beside it.
///
/// Scans per lock stays integer tenths for `print_report`'s reason — a count over a count needs no
/// float to print one decimal.
fn print_scale_report(tiers: &[Tier]) {
    println!(
        "rows      scans/round  rounds  fill          rows/s      met: median (min-max)  scans per lock  median met  median clean"
    );
    for tier in tiers {
        let met: Vec<u64> = tier.rounds.iter().map(|round| round.met).collect();
        let met_median = median_u64(&met);
        let per_lock = (u64::try_from(tier.scans).unwrap_or(u64::MAX) * 10)
            .checked_div(met_median)
            .map_or_else(
                || "never".to_owned(),
                |tenths| format!("{}.{}", tenths / 10, tenths % 10),
            );
        // A round that met no lock has no median to contribute; it still counts in the spread.
        let met_times: Vec<Duration> = tier
            .rounds
            .iter()
            .filter(|round| !round.met_times.is_empty())
            .map(|round| median_of(&round.met_times))
            .collect();
        let clean_times: Vec<Duration> = tier
            .rounds
            .iter()
            .map(|round| median_of(&round.clean_times))
            .collect();
        let low = met.iter().min().copied().unwrap_or(0);
        let high = met.iter().max().copied().unwrap_or(0);
        let spread = format!("{met_median} ({low}-{high})");
        let (rows, scans, rounds) = (tier.rows, tier.scans, tier.rounds.len());
        let met_time = median_of(&met_times);
        let clean_time = median_of(&clean_times);
        let fill = format!("{:?}", tier.fill);
        // Rows a second in integer tenths, for the same reason the ratio above is: a count over a
        // count (or a time) prints one decimal without ever becoming a float.
        let rate_tenths = (tier.rows * 10_000)
            .checked_div(i64::try_from(tier.fill.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let rate = format!("{}.{}", rate_tenths / 10, rate_tenths % 10);
        println!(
            "{rows:<10}{scans:<13}{rounds:<8}{fill:<14}{rate:<12}{spread:<23}{per_lock:<16}{met_time:?}  {clean_time:?}"
        );
    }
}

/// [`median_of`] for counts. Empty says zero rather than being divided by.
/// Rows each tier of #109's profile fills. **A fresh cluster per tier**, because the question is
/// what an `INSERT` costs as *this table* grows, and a second tier filled into the store that
/// already holds the first would move table size and store size together — which is the confound
/// that made the first batch-size probe worthless (§ the `PROBE_ORDER` note above).
const PROFILE_TIERS: [i64; 3] = [20_000, 100_000, 200_000];

/// How long the fills may take before the stop rule bites, written down **before the run**.
///
/// At the rates the last window measured — 657 rows a second into an empty store, 280 into one
/// holding 100k — the three tiers cost roughly half a minute, six minutes and a quarter of an
/// hour. The budget leaves the rest of the window for the readiness waits and the lock census, and
/// a tier that cannot start inside it is **reported as not run**, never guessed at.
const PROFILE_BUDGET: Duration = Duration::from_secs(32 * 60);

/// One tier of #109's profile.
struct TierProfile {
    rows: i64,
    /// Whether this tier's table carried a secondary index. The three comparable tiers do not;
    /// the fourth exists only so the index phases have one real number beside them.
    indexed: bool,
    fill: Duration,
    /// One reading per `INSERT`. The median over these is the number this measurement reports.
    statements: Vec<cluster::profile::Counts>,
}

/// The median of one phase across a tier's statements.
///
/// Sorted here rather than trusted to be sorted: a median of an unsorted vector is a number that
/// looks like a measurement and is not one.
fn median_phase(
    statements: &[cluster::profile::Counts],
    of: fn(&cluster::profile::Counts) -> u64,
) -> u64 {
    let mut samples: Vec<u64> = statements.iter().map(of).collect();
    samples.sort_unstable();
    median_u64(&samples)
}

/// What one `INSERT` of five hundred rows spent, per phase, per tier.
///
/// **Two columns are `n/a` and not zero** on every tier without a secondary index. `dml.rs`'s
/// index probe and index put run once per index entry, and a table whose only key is its primary
/// key has none — so the phase does not happen, which is a different statement from "it was free".
fn print_phase_report(tiers: &[TierProfile]) {
    // **Every field of `Counts`, checked against the struct rather than chosen.** The first version
    // of this report printed the four phases I expected to matter and dropped the rest, which is
    // how a measured column (`other_get`) went missing from a page that concluded about the
    // remainder. The block below lists each field; the wide table after it stays because the
    // earlier tiers were reported in that shape and a comparison needs one.
    for tier in tiers {
        let stmts = u64::try_from(tier.statements.len()).unwrap_or(1).max(1);
        let wall_us = u64::try_from(tier.fill.as_micros())
            .unwrap_or(u64::MAX)
            .checked_div(stmts)
            .unwrap_or(0);
        let med = |of: fn(&cluster::profile::Counts) -> u64| median_phase(&tier.statements, of);
        let (row_us, row_n) = (med(|c| c.row_get_us), med(|c| c.row_get_n));
        let (idx_us, idx_n) = (med(|c| c.index_get_us), med(|c| c.index_get_n));
        let (oth_us, oth_n) = (med(|c| c.other_get_us), med(|c| c.other_get_n));
        let (unw_us, unw_n) = (med(|c| c.unwaited_get_us), med(|c| c.unwaited_get_n));
        let (scan_us, scan_n) = (med(|c| c.scan_us), med(|c| c.scan_n));
        let put_us = med(|c| c.put_us);
        let (put_row, put_idx, put_oth) = (
            med(|c| c.row_put_n),
            med(|c| c.index_put_n),
            med(|c| c.other_put_n),
        );
        let (commit_us, commit_n) = (med(|c| c.commit_us), med(|c| c.commit_n));
        let (rollback_n, begin_n) = (med(|c| c.rollback_n), med(|c| c.begin_n));
        let accounted = row_us
            .saturating_add(idx_us)
            .saturating_add(oth_us)
            .saturating_add(unw_us)
            .saturating_add(scan_us)
            .saturating_add(put_us)
            .saturating_add(commit_us);
        let remainder = wall_us.saturating_sub(accounted);
        // Share in integer tenths of a percent, for the reason every ratio here is integer.
        let share = remainder
            .saturating_mul(1000)
            .checked_div(wall_us)
            .unwrap_or(0);
        let indexed = if tier.indexed { "yes" } else { "no" };
        println!(
            "\n  tier {} rows, index {indexed}, fill {:?}, {stmts} statements — medians per INSERT:\n\
             \x20   get row      {row_us:>10} us  n={row_n}\n\
             \x20   get index    {idx_us:>10} us  n={idx_n}\n\
             \x20   get other    {oth_us:>10} us  n={oth_n}   (catalog and metadata keys)\n\
             \x20   get unwaited {unw_us:>10} us  n={unw_n}   (view_at's version counters)\n\
             \x20   scan         {scan_us:>10} us  n={scan_n}\n\
             \x20   put          {put_us:>10} us  row={put_row} index={put_idx} other={put_oth}\n\
             \x20   commit       {commit_us:>10} us  n={commit_n}\n\
             \x20   rollback                   n={rollback_n}\n\
             \x20   begin                      n={begin_n}\n\
             \x20   ---------------------------------------------\n\
             \x20   accounted    {accounted:>10} us\n\
             \x20   statement    {wall_us:>10} us  remainder {remainder} us ({}.{}%)",
            tier.rows,
            tier.fill,
            share / 10,
            share % 10
        );
    }

    println!(
        "rows      index  fill          rows/s      stmts  pk-probe us (n)   idx-probe us (n)  \
         put us (row/idx)   commit us   commit n"
    );
    for tier in tiers {
        let stmts = tier.statements.len();
        let pk_us = median_phase(&tier.statements, |counts| counts.row_get_us);
        let pk_n = median_phase(&tier.statements, |counts| counts.row_get_n);
        let put_us = median_phase(&tier.statements, |counts| counts.put_us);
        let row_put_n = median_phase(&tier.statements, |counts| counts.row_put_n);
        let commit_us = median_phase(&tier.statements, |counts| counts.commit_us);
        let commit_n = median_phase(&tier.statements, |counts| counts.commit_n);
        let idx_put_n = median_phase(&tier.statements, |counts| counts.index_put_n);
        let (probe, puts) = if tier.indexed {
            let idx_us = median_phase(&tier.statements, |counts| counts.index_get_us);
            let idx_n = median_phase(&tier.statements, |counts| counts.index_get_n);
            (
                format!("{idx_us} ({idx_n})"),
                format!("{put_us} ({row_put_n}/{idx_put_n})"),
            )
        } else {
            (
                "n/a (no index)".to_owned(),
                format!("{put_us} ({row_put_n}/n/a)"),
            )
        };
        // Rows a second in integer tenths, the idiom `print_scale_report` uses and for its reason:
        // a count over a time prints one decimal without ever becoming a float.
        let rate_tenths = (tier.rows * 10_000)
            .checked_div(i64::try_from(tier.fill.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let rate = format!("{}.{}", rate_tenths / 10, rate_tenths % 10);
        let fill = format!("{:?}", tier.fill);
        let indexed = if tier.indexed { "yes" } else { "no" };
        let (rows, pk) = (tier.rows, format!("{pk_us} ({pk_n})"));
        println!(
            "{rows:<10}{indexed:<7}{fill:<14}{rate:<12}{stmts:<7}{pk:<18}{probe:<18}{puts:<19}\
             {commit_us:<12}{commit_n}"
        );
    }
}

/// What a tier of `rows` rows should be expected to cost, from what the tiers before it **actually**
/// cost.
///
/// **Doubled, on purpose.** The fill rate falls as the table grows — 657 rows a second into an
/// empty store, 280 into one already holding 100k — so the last tier's per-row cost is a floor and
/// not a forecast. A projection that trusted it would start a tier it cannot finish, which is the
/// failure the stop rule exists to prevent; doubling makes the estimate conservative in the only
/// direction that matters. With no tier behind it there is nothing to project from, and the answer
/// is zero: the first tier always runs.
fn projected_fill(tiers: &[TierProfile], rows: i64) -> Duration {
    let Some(last) = tiers.last() else {
        return Duration::ZERO;
    };
    let per_row_us = u64::try_from(last.fill.as_micros())
        .unwrap_or(u64::MAX)
        .checked_div(u64::try_from(last.rows).unwrap_or(1))
        .unwrap_or(0);
    Duration::from_micros(
        per_row_us
            .saturating_mul(2)
            .saturating_mul(u64::try_from(rows).unwrap_or(0)),
    )
}

/// **One tier of [`what_an_insert_spends_its_time_on_as_the_table_grows`]**, for re-measuring the
/// phases without paying for the curve.
///
/// The three-tier run costs about twenty-seven minutes of filling, which is the right price for a
/// slope and the wrong one for a question about *which phases exist* — and that question is what
/// changed: `get_without_waiting` was forwarded untimed, so the catalog's version-counter reads
/// were in no column at all, and the report printed four phases of the eighteen `Counts` holds.
/// Both are fixed above; this runs the smallest tier so the fix can be read in a minute rather
/// than in half an hour. **It is not a substitute for the curve**: one tier has an intercept and
/// no slope, and every growth claim in `esker-coord/s1-fill-window-2026-09-16/q109-data.md` comes
/// from the three-tier run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#109's phases at one tier: a real cluster and about two minutes"]
async fn what_an_insert_spends_its_time_on_at_the_smallest_tier() {
    let rows = PROFILE_TIERS[0];
    let gate = Gate::start().await;
    let (fill, statements) = gate.fill_profiled("t", rows, false).await;
    let tiers = vec![TierProfile {
        rows,
        indexed: false,
        fill,
        statements,
    }];
    print_phase_report(&tiers);
    gate.stop().await;
}

/// **Where an `INSERT` spends itself, as the table grows** — debt #109's profile.
///
/// The phases are the `Backend` seam's own method boundaries (`tests/cluster/profile.rs`), which is
/// why this needs no product change to measure. What it can and cannot see is written there, and
/// two of its limits belong in every reading of this table:
///
/// * **`put` buffers.** The put column is memory, not I/O; the network is entirely inside `commit`.
/// * **Round trips are not counted, only timed.** `esker-client` keeps no counter of its prewrite
///   and commit requests, so the number of them is inferred from the regions a statement touched
///   and is reported as an inference or not at all.
///
/// The columnar tee and region routing are below this seam, inside the store's apply path. They are
/// invisible here, which the report says rather than rounding to zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#109's profile: three real clusters and most of a window"]
async fn what_an_insert_spends_its_time_on_as_the_table_grows() {
    let began = Instant::now();
    let mut tiers: Vec<TierProfile> = Vec::new();

    for rows in PROFILE_TIERS {
        // **A stop rule that cannot stop is not a stop rule.** Asking only whether the budget has
        // run out would happily start a quarter-hour tier at minute thirty-one, and nothing here
        // can abort a fill once it is running. The question has to be whether this tier can
        // *finish*, so it is asked before the tier starts and answered with `projected_fill`.
        let projected = projected_fill(&tiers, rows);
        if began.elapsed() + projected > PROFILE_BUDGET {
            println!(
                "not starting the {rows}-row tier: {:?} gone of {PROFILE_BUDGET:?}, and this tier \
                 projects to {projected:?} at twice the last tier's per-row cost. The tiers above \
                 are what ran; nothing is extrapolated past them.",
                began.elapsed()
            );
            break;
        }
        // A cluster of its own, started and stopped inside the loop: see `PROFILE_TIERS`.
        let gate = Gate::start().await;
        let (fill, statements) = gate.fill_profiled("t", rows, false).await;
        tiers.push(TierProfile {
            rows,
            indexed: false,
            fill,
            statements,
        });
        // **Printed as each tier finishes**, for the reason the tier loop above it learned the hard
        // way: a later tier's panic must not be able to take a measured one with it.
        print_phase_report(&tiers);
        gate.stop().await;
    }

    // The indexed tier is last and is the first thing the budget gives up, because it answers a
    // different question from the three above and cannot be compared with them.
    //
    // **Its projection is deliberately the wrong way round**: `projected_fill` reads the *last*
    // tier, which by now is the largest, so a twenty-thousand-row table is priced at a
    // two-hundred-thousand-row table's per-row cost and doubled on top. That over-estimates, and
    // over-estimating can only skip a tier that would have fitted — never overrun the window. Given
    // that this tier is the one the ruling gives up first, erring toward skipping it is the
    // intended direction rather than an oversight.
    let rows = PROFILE_TIERS[0];
    if began.elapsed() + projected_fill(&tiers, rows) <= PROFILE_BUDGET {
        let gate = Gate::start().await;
        let (fill, statements) = gate.fill_profiled("t", rows, true).await;
        tiers.push(TierProfile {
            rows,
            indexed: true,
            fill,
            statements,
        });
        print_phase_report(&tiers);
        gate.stop().await;
    } else {
        println!("the indexed tier did not run: the budget was spent on the comparable ones");
    }

    assert!(
        !tiers.is_empty(),
        "no tier ran, so there is nothing to report"
    );
}

/// Rows the ratio measurement fills before it strands anything. Small on purpose: the question is
/// about locks, and every second spent filling is a second not spent meeting them.
const RATIO_ROWS: i64 = 2_000;
/// Scans per arm.
///
/// **Twenty, and sixty was the mistake.** With the planter running, every scan meets a lock — the
/// first run with it measured 60 encounters in 60 scans — so sixty scans took ten minutes, and a
/// ten-minute arm outlives any lease a fixture is willing to plant: the control's own lock, given
/// six hundred seconds, was read `expired 17 ms ago`. Twenty scans still clears the floor of twenty
/// encounters the reading was pre-registered against, and keeps an arm inside its lease.
const RATIO_SCANS: usize = 20;
/// The stranded arm's lease, `joint_gate.rs`'s number. Short, because the arm waits it out.
const STRANDED_TTL_MS: u64 = 300;
/// The control arm's lease. Long enough that every read in this test lands **inside** it, which is
/// what makes the lock read as alive rather than as stranded.
///
/// **Ten minutes, and the first number was sixty seconds, which was wrong.** The control plants its
/// lock and *then* scans sixty times; that loop took most of a minute, so the lease expired inside
/// it and the census read the control's own lock as `expired 35 ms ago` with `ttl_ms=60000` beside
/// it. The arithmetic was right — `physical_ms` is applied to both sides — and the fixture's
/// parameter was wrong. The lease has to outlast the arm that reads it.
const ALIVE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Encounters below which a share is not reported (pre-registered in `q117-ratio-design.md` §④).
const RATIO_FLOOR: usize = 20;

/// One arm's tally: how each lock **this fixture planted** was classified when a fragment met it.
#[derive(Debug, Default)]
struct Verdicts {
    alive: usize,
    stranded: usize,
    replaced: usize,
    gone: usize,
    /// Refusals whose detail names a lock, i.e. #86's check firing.
    blocked: usize,
    /// Refusals that carried `TooFarBehind` for the other reason — a learner simply behind.
    behind: usize,
    scans: usize,
}

/// Scans `table` `scans` times, draining the encounters **after every scan**.
///
/// **Not [`scan_a_table_n`]**, and the difference is the whole point: that one calls
/// `counted.reset()` at the top of each scan, and `reset` now clears `encounters` — so reusing it
/// would throw every sample away between scans. That is exactly the shape #107 lost a window to,
/// where the discriminator was collected and then discarded before it was read.
fn scan_and_collect(
    gate: &Gate,
    counted: &Arc<CountingFragments>,
    table: &str,
    table_id: u64,
    scans: usize,
    tally: &mut Verdicts,
    planted: &Arc<std::sync::Mutex<Vec<u64>>>,
) {
    let mut session = gate.session_asking(Arc::clone(counted) as Arc<dyn FragmentSource>);
    for _ in 0..scans {
        counted.reset();
        let _ = session.run(&format!("SELECT count(*) FROM {table}"));
        tally.scans += 1;
        let (_, too_far, _, _) = counted.taken();
        // **This scan's count, not the running one.** `too_far` is per scan, because `reset` runs
        // at the top of the loop; subtracting the cumulative total from it would drive the other
        // tally negative and `saturating_sub` would hide that as a zero.
        let drained = match counted.encounters.lock() {
            Ok(mut met) => {
                let n = met.len();
                met.clear();
                n
            }
            Err(_) => 0,
        };
        tally.blocked += drained;
        // A `TooFarBehind` that named no lock is the other cause sharing the tag.
        tally.behind += usize::try_from(too_far)
            .unwrap_or(0)
            .saturating_sub(drained);
        // **The first look is taken here, at the encounter, not after the arm.** The first run of
        // this test classified nothing at all — four zeroes in both arms — because it sampled once
        // the sixty scans were done, and by then the row path had had sixty chances to roll the
        // planted lock forward. `report_stranded` prints a line per lock and nothing for an empty
        // census, so the silence in that log was an absence of locks rather than a failure to
        // match them. The design said "forensics at the encounter"; this is the line that was
        // missing.
        // **Both looks, at every encounter, while whatever is planting still is.** The first
        // version took one look for the whole arm and left the second to the caller, who took it
        // after stopping the planter — so the second census always saw a stopped system with `gap`
        // milliseconds to roll the last pair forward, and `gone` was the only reachable verdict.
        // On known inputs the classifier produces `alive` and `stranded` both
        // (`q117-planter-result.md` §6b), so the fault was never the classifier: it was that one
        // instant's snapshot holds one pair, and the second one was taken too late.
        if drained > 0 {
            let first = gate.sample_locks(table, table_id, "at the encounter");
            // **A third of a lease, derived rather than passed.** The interval is only meaningful
            // against the lease it samples: at a whole lease every planted lock is gone or expired
            // by the second look, which is what three runs produced. A caller free to choose both
            // could set an interval longer than the lease and see nothing but `gone` without the
            // number ever looking wrong.
            std::thread::sleep(Duration::from_millis(STRANDED_TTL_MS / 3));
            let second = gate.sample_locks(table, table_id, "a gap later");
            let seen = planted.lock().map_or_else(|_| Vec::new(), |s| s.clone());
            classify_planted(&seen, tally, &first, &second);
        }
    }
}

/// Classifies the locks this fixture planted, by taking two looks a fraction of `ttl_ms` apart.
///
/// **The interval is the arm's own `ttl_ms`, not a lease of the client's.** Nothing heartbeats
/// here: the control arm is alive because it is *inside* the lease it chose, so a second look a
/// full `LOCK_TTL_MS` later would find it expired and call it stranded — and the pre-registered
/// reading treats a control that is not `ALIVE` as a broken classifier.
/// Classifies one encounter's pair of looks.
///
/// **Both censuses come from the caller now, and that is the fix.** This used to take the second
/// look itself, which meant it ran wherever the caller happened to call it — and the caller called
/// it after `planting.store(false)`, so the second look always saw a system that had stopped
/// planting and had been given `gap` milliseconds to roll the last pair forward. Every lock was
/// therefore `gone`, in three runs, in both arms, and no other verdict was reachable.
///
/// The classifier itself was never the problem: on known inputs it produces both
/// `alive` and `stranded` (`q117-planter-result.md` §6b — a lock inside a ten-minute lease reads
/// unexpired, one past a 300 ms lease reads expired 915 ms later and is still there). What was
/// wrong was when the looks were taken, so taking them is now the caller's job.
fn classify_planted(
    planted: &[u64],
    tally: &mut Verdicts,
    first: &[StrandedLock],
    second: &[StrandedLock],
) {
    for before in first.iter().filter(|lock| planted.contains(&lock.start_ts)) {
        let same = |later: &&StrandedLock| later.at == before.at && later.key == before.key;
        match second.iter().find(same) {
            None => tally.gone += 1,
            Some(later) if later.start_ts != before.start_ts => tally.replaced += 1,
            Some(later) if later.expired_by_ms.is_some() => tally.stranded += 1,
            Some(_) => tally.alive += 1,
        }
    }
}

/// Rows the classifier check fills, and where it puts its own keys.
///
/// Clear of everything else: the fill owns `1..=RATIO_ROWS`, the planter starts at `RATIO_ROWS + 1`
/// and steps upward without end, and the ratio test's control sits at `RATIO_ROWS + 1_000_001`.
const CHECK_ROWS: i64 = 100;
const CHECK_BASE: i64 = RATIO_ROWS + 5_000_001;

/// Prints what a census found about one planted transaction, and nothing else.
///
/// **A print, not a bound**, which is this file's rule for a clock and is the right rule here for a
/// stronger reason: one of the three outcomes this check can produce is a *finding* rather than a
/// failure. If the stranded arm's lock cannot be sampled after its lease, that says stranded locks
/// are resolved the moment they expire — half of ADR 0117's answer — and an assertion would dress
/// that up as a broken fixture and invite somebody to "fix" it.
fn report_census(arm: &str, found: &[StrandedLock], planted: u64) {
    let mine: Vec<&StrandedLock> = found.iter().filter(|l| l.start_ts == planted).collect();
    let inside = mine.iter().filter(|l| l.expired_by_ms.is_none()).count();
    let past = mine.len() - inside;
    println!(
        "  {arm:<14} rows {} | mine {} | inside its lease {inside} | past it {past}",
        found.len(),
        mine.len()
    );
}

/// **Can the classifier produce `ALIVE` and `STRANDED` at all, on inputs whose answer is known?**
///
/// Three runs of the ratio measurement reported `gone` for eight locks and nothing else, in both
/// arms, every time — two keys times four stores, identical each run. A number that constant is
/// structural, and reading the code found two causes that each suffice on their own
/// (`q117-planter-result.md` §5): the second census is taken after the planter has been stopped, so
/// nothing can still be there; and the first census is a single snapshot, so it holds whatever one
/// instant holds, which is one pair.
///
/// Both of those are about *when* the census is taken. Neither says whether the classifier works,
/// and the rule written before any of this ran says a control not classified alive voids the round.
/// So this asks the narrow question directly, with no planter and no scan loop: plant one lock whose
/// answer is known and look at it once.
///
/// **The readings are pre-registered** in `q117-planter-result.md` §6: both arms find their lock and
/// disagree about the lease, and the classifier is sound and the fault is timing; the alive arm
/// finds its lock and the stranded arm finds nothing, and expired locks are being resolved on sight,
/// which is half of 0117's answer rather than a defect; neither arm finds anything, and the census
/// itself is what to fix first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "0117's classifier check: a real cluster and about a minute"]
async fn a_planted_lock_reads_alive_inside_its_lease_and_stranded_past_it() {
    let gate = Gate::start().await;
    let fill = gate.fill_one_at_scale("t", CHECK_ROWS).await;
    let table_id = tokio::task::block_in_place(|| gate.table_id("t")).expect("the table has an id");
    println!("\n0117 classifier check: {CHECK_ROWS} rows in {fill:?}, table {table_id}");

    tokio::task::block_in_place(|| {
        // Inside its lease and never committed: the census should find it and call it unexpired.
        let alive = strand_a_secondary(
            gate.client.router(),
            &gate.oracle,
            table_id,
            CHECK_BASE + 1,
            CHECK_BASE + 2,
            ALIVE_TTL_MS,
            false,
        );
        report_census("alive", &gate.sample_locks("t", table_id, "alive"), alive);

        // Primary committed, secondary left, and read only after four leases have passed: the
        // census should find it and call it expired — unless something resolved it first, which is
        // the outcome that answers 0117 rather than accusing the fixture.
        let stranded = strand_a_secondary(
            gate.client.router(),
            &gate.oracle,
            table_id,
            CHECK_BASE + 3,
            CHECK_BASE + 4,
            STRANDED_TTL_MS,
            true,
        );
        std::thread::sleep(Duration::from_millis(STRANDED_TTL_MS * 4));
        report_census(
            "stranded",
            &gate.sample_locks("t", table_id, "stranded"),
            stranded,
        );
    });

    gate.stop().await;
}

/// **What share of the locks a fragment meets belong to a transaction that is already finished** —
/// ADR 0117's third question, and the number that decides whether (b′)+(c) is worth building.
///
/// Two arms from one helper, as `q117-ratio-design.md` §① sets out: a **stranded** arm whose
/// primary is committed and whose secondary's lock is past its lease, and an **alive** control
/// whose lease is long and whose commit never comes. The control is not decoration — a rig that
/// can only produce `STRANDED` cannot show that it recognises `ALIVE`, and §④ says a control that
/// is not classified alive voids the round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "0117's ratio: a real cluster, planted locks, and a few minutes"]
async fn what_share_of_met_locks_are_already_finished() {
    let began = Instant::now();
    let gate = Gate::start().await;
    let fill = gate.fill_one_at_scale("t", RATIO_ROWS).await;
    let table_id = tokio::task::block_in_place(|| gate.table_id("t")).expect("the table has an id");
    let counted = Arc::new(CountingFragments::new(Arc::clone(&gate.fragments)));
    println!("\n0117 ratio: {RATIO_ROWS} rows filled in {fill:?}, table {table_id}");

    // **The stranded arm.** Commit the primary, leave the secondary, and wait past the lease: a
    // lock inside its lease is one the row path waits on rather than rolls forward, and then
    // neither engine answers and there is nothing to disagree about (`joint_gate.rs`).
    let mut stranded = Verdicts::default();
    // **A planter, not one pair.** The 12:09 run met a lock three times in a hundred and twenty
    // scans because it planted once and then scanned: a single pair is met a handful of times and
    // the row path rolls it forward. The encounter rate was capped by the fixture, so the fixture
    // is what changes — nothing about what is being measured moves.
    let planting = Arc::new(AtomicBool::new(true));
    let planted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let planting_thread = spawn_planter(
        &gate,
        table_id,
        RATIO_ROWS + 1,
        STRANDED_TTL_MS,
        true,
        &planting,
        &planted,
    );
    // Long enough that the first pairs are past their lease before the scans start: a lock inside
    // its lease is one the row path waits on rather than rolls forward, and then neither engine
    // answers and there is nothing to disagree about (`joint_gate.rs`).
    std::thread::sleep(Duration::from_millis(STRANDED_TTL_MS * 4));
    tokio::task::block_in_place(|| {
        scan_and_collect(
            &gate,
            &counted,
            "t",
            table_id,
            RATIO_SCANS,
            &mut stranded,
            &planted,
        );
    });
    // **Stopped after the scans, not before the classifying.** It used to stop here and *then* the
    // caller took its second census, so that census always saw a system that had quit planting —
    // which is why `gone` was the only verdict three runs running. Classifying happens inside the
    // loop now, while this is still placing pairs.
    planting.store(false, Ordering::Relaxed);
    let pairs = planted.lock().map_or(0, |seen| seen.len());
    planting_thread.join().expect("the planting thread ends");
    println!("  stranded arm planted {pairs} pairs");
    print_verdicts("stranded", &stranded);

    // **The control.** A long lease and no commit at all, read well inside it.
    let mut alive = Verdicts::default();
    let held = tokio::task::block_in_place(|| {
        strand_a_secondary(
            gate.client.router(),
            &gate.oracle,
            table_id,
            // **Clear of the planter's range, not next to it.** The planter starts at
            // `RATIO_ROWS + 1` and steps by two without end, so it reaches `+ 3` within a few
            // milliseconds: the control's pair has to sit somewhere the planter will not arrive,
            // or the two fixtures write the same rows and the control stops meaning anything.
            RATIO_ROWS + 1_000_001,
            RATIO_ROWS + 1_000_002,
            ALIVE_TTL_MS,
            false,
        )
    });
    // One planted lock, where the stranded arm has many: the control's job is a single
    // recognisable ALIVE, not volume. Wrapped the same way the planter's list is, so both arms
    // hand `scan_and_collect` the same type — a control that took a different shape is how a
    // fixture ends up with one arm wired and the other not.
    let held_only = Arc::new(std::sync::Mutex::new(vec![held]));
    tokio::task::block_in_place(|| {
        scan_and_collect(
            &gate,
            &counted,
            "t",
            table_id,
            RATIO_SCANS,
            &mut alive,
            &held_only,
        );
    });
    print_verdicts("alive (control)", &alive);

    let met = stranded.blocked + alive.blocked;
    println!(
        "\n0117: {met} encounters over {} scans in {:?}",
        stranded.scans + alive.scans,
        began.elapsed()
    );
    if met < RATIO_FLOOR {
        println!(
            "**sample too small**: {met} encounters is under the pre-registered floor of \
             {RATIO_FLOOR}. Raw counts above; **no share is reported and nothing is extrapolated**, \
             and this neither confirms nor refutes #88's one-in-30, whose workload is not this one."
        );
    }
    assert!(
        alive.alive > 0 || alive.blocked == 0,
        "the control's lock was planted with a {ALIVE_TTL_MS} ms lease and read inside it, so a \
         classifier that did not call it alive is the thing to fix before any share is believed: \
         {alive:?}"
    );
    gate.stop().await;
}

/// One arm's line, counts first and shares only where the floor allows.
fn print_verdicts(arm: &str, tally: &Verdicts) {
    println!(
        "  {arm:<16} scans {} | met {} | behind {} | ALIVE {} STRANDED {} replaced {} gone {}",
        tally.scans,
        tally.blocked,
        tally.behind,
        tally.alive,
        tally.stranded,
        tally.replaced,
        tally.gone
    );
}

fn median_u64(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// Rows the probe fills at each batch size. Large enough to swamp the one-off costs (the table, the
/// columnar replica, the first flush), small enough that three batch sizes fit in minutes.
const PROBE_ROWS: i64 = 20_000;

/// The batch sizes, run **forward and then back**: 500, 2000, 5000, 5000, 2000, 500.
///
/// The probe's first run could not answer its own question. The three fills shared one cluster and
/// ran in order, so "a bigger batch is slower" and "a fill into a store that already holds more is
/// slower" were the same measurement — 657, 406 and 275 rows a second, and no way to say which
/// effect that was. Giving each batch **two positions symmetric about the middle** separates them:
/// the store only grows, so a position effect largely cancels in a pair's mean while a batch effect
/// does not. If the two halves disagree about the ranking, the ranking was never about the batch.
const PROBE_ORDER: [usize; 6] = [500, 2_000, 5_000, 5_000, 2_000, 500];

/// **What a bigger batch buys the fill** — the number that decides the next window's shape.
///
/// #88's curve is blocked on filling, not on scanning: 100k rows took 356.5 s, which is 280 rows a
/// second and 1.78 s for one 500-row `INSERT`, so 500k costs half an hour of fill and 1M costs an
/// hour. This times the same 20k rows at three batch sizes rather than refilling 100k three times —
/// the ratio is the answer, and the ratio is what a fixed row count measures.
///
/// The columnar replica is awaited for every fill afterwards, because a fill so large that the
/// replica is only built at the end would not be the same thing under test.
///
/// Prints, and asserts only that every batch finished: a threshold here would be a claim about this
/// box rather than about the system.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#88's fill rate: a real cluster and a few minutes"]
async fn what_a_bigger_batch_buys_the_fill() {
    let gate = Gate::start().await;
    let mut filled: Vec<(usize, Duration)> = Vec::new();

    for (position, batch) in PROBE_ORDER.into_iter().enumerate() {
        let table = format!("fill{position}_{batch}");
        let took = tokio::task::block_in_place(|| {
            let mut session = gate.session();
            settle(
                &mut session,
                &format!("CREATE TABLE {table} (id int8 PRIMARY KEY, n int8, note text)"),
            );
            settle(
                &mut session,
                &format!("ALTER TABLE {table} SET (columnar_replicas = 1)"),
            );
            let began = Instant::now();
            for chunk in (1..=PROBE_ROWS).collect::<Vec<i64>>().chunks(batch) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("({id}, {id}, 'n{id}')"))
                    .collect();
                settle(
                    &mut session,
                    &format!("INSERT INTO {table} VALUES {}", values.join(", ")),
                );
            }
            began.elapsed()
        });
        // Rows a second in integer tenths: a count over a time needs no float to print one decimal.
        let tenths = (PROBE_ROWS * 10_000)
            .checked_div(i64::try_from(took.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        println!(
            "position {position} batch {batch:<6} {PROBE_ROWS} rows in {took:?}  ({}.{} rows/s)",
            tenths / 10,
            tenths % 10
        );
        filled.push((batch, took));
    }

    // The pair means are the comparison the design exists for: same batch, symmetric positions.
    for batch in [500_usize, 2_000, 5_000] {
        let pair: Vec<Duration> = filled
            .iter()
            .filter(|(b, _)| *b == batch)
            .map(|(_, took)| *took)
            .collect();
        let total: Duration = pair.iter().sum();
        let mean = total / u32::try_from(pair.len().max(1)).unwrap_or(1);
        println!("batch {batch:<6} mean over its two positions: {mean:?}  (samples {pair:?})");
    }

    for (position, batch) in PROBE_ORDER.into_iter().enumerate() {
        gate.wait_for_a_learner_that_answers(&format!("fill{position}_{batch}"))
            .await;
    }
    println!("every fill's columnar learner answered");

    assert_eq!(filled.len(), PROBE_ORDER.len(), "a fill did not finish");
}
