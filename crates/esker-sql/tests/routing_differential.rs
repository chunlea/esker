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

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use esker_client::region_cache::RegionResolver;
use esker_client::router::{ClientOptions, Router};
use esker_client::{TcpStores, TimestampOracle, TxnClient};
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{PeerRole, ProtoError, Server, ServerHandle, Service, TransportConfig};
use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{ClientFragments, FragmentSource};
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

    assert_eq!(
        routed, by_rows,
        "the two engines disagree{note} on `{query}` at {at}\n\
         routed: {routed:?}\n\
         rows:   {by_rows:?}\n\
         plan:\n{plan}"
    );
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
}

struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Gate {
    async fn start() -> Self {
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
            nodes.push(open_store(*address, at as u64 + 1, pd_address, &peers).await);
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

        let oracle: Arc<dyn TimestampOracle> = Arc::new(WallClockOracle::new());
        let (backend, conn, fragments) =
            tokio::task::block_in_place(|| sql_node(&addresses, pd_address, Arc::clone(&oracle)));

        Gate {
            pd,
            oracle,
            pd_handle: Some(pd_handle),
            nodes,
            backend,
            catalog: Arc::new(Catalog::new()),
            conn,
            fragments,
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
        wait_for("PD to place a columnar learner", 60, || {
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

        let deadline = Instant::now() + Duration::from_secs(60);
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
                "the columnar learner never answered a fragment over {table}"
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

    async fn stop(mut self) {
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
fn sql_node(
    addresses: &[SocketAddr],
    pd_address: SocketAddr,
    oracle: Arc<dyn TimestampOracle>,
) -> (Arc<dyn Backend>, Arc<PdConn>, Arc<dyn FragmentSource>) {
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
        esker_sql::backend::StoreBackend::new(client, oracle)
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
    (backend, conn, fragments)
}

async fn open_store(
    address: SocketAddr,
    store_id: u64,
    pd_address: SocketAddr,
    peers: &[PeerAddress],
) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(peers.to_vec(), 20_260_901);
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

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Physical milliseconds in the high bits and a counter in the low ones — the shape PD's TSO hands
/// out.
///
/// `CountingOracle` cannot be used on a cluster that can strand a lock: `esker_client::is_expired`
/// judges a lock by the *physical half* of a timestamp, which under a plain counter is zero for
/// ever, so a lock left behind by a write whose answer was lost can never expire. Nothing here
/// reads a clock to order anything (`CLAUDE.md` invariant 6); this stands in for PD's TSO, whose
/// job is to turn a clock into timestamps.
#[derive(Debug)]
struct WallClockOracle {
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
        let issued = (*next).max(now_ms << esker_client::TSO_LOGICAL_BITS);
        *next = issued.saturating_add(u64::from(count.max(1)));
        Ok(issued)
    }
}

/// A statement that has to succeed, retried through the leadership gap a saturated machine
/// produces (`docs/plans/phase-9-rails.md` §8).
fn settle(session: &mut Session, sql: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
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
                assert!(Instant::now() < deadline, "`{sql}` never settled: {error}");
                std::thread::sleep(Duration::from_millis(100));
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
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
                assert!(Instant::now() < deadline, "`{sql}` never settled: {error}");
                std::thread::sleep(Duration::from_millis(100));
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
