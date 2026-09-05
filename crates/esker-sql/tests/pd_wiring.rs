//! The one cable: a SQL node's connection to the placement driver, and what it turns on.
//!
//! Three finished features are inert in a node that has no PD — the schema lease, the re-driver's
//! interval, and `ALTER TABLE ... SET (columnar_replicas = N)` — and every one of them comes alive
//! through [`esker_sql::pd`] (`docs/plans/phase-8-learner.md` §wiring). These tests drive the
//! **real** wiring: the real `PdConn` over a real socket, the real `PdLease` attached to a real
//! `StoreBackend` over three real stores, and the real executor above it.
//!
//! # Why the placement driver here is a stand-in and the lease source is not
//!
//! `esker-sql` does not depend on `esker-pd` and must not — they are peers that meet on the wire —
//! so the driver is a `Mutex` behind the real framing, exactly as `esker-store`'s own PD test does
//! it. What matters is that **nothing on this side is faked**: ADR 0028 makes the lease a trait so
//! that *"the test that proves fail closed has to be able to stop answering"*, and the way it stops
//! answering here is the real one — PD is shut down, and the real refresher fails to renew.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;
mod standin_pd;

use std::sync::Arc;
use std::time::Duration;

use esker_proto::pd::ColumnarWish;
use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource, StepInterval};
use esker_sql::exec::Executor;
use esker_sql::pd::{ColumnarReport, LeaseRefresher, PdConn, PdLease};
use esker_sql::pgwire::session::Execute;

use cluster::{Session, TENANT};
use standin_pd::{REMOVAL_EXTRA_MS, STEP_MS};

/// The whole node, as the binary builds it: a lease, a backend holding it, and a refresher.
struct Node {
    backend: Arc<dyn Backend>,
    conn: Arc<PdConn>,
    lease: Arc<PdLease>,
}

impl Node {
    /// Builds the node and fetches its first lease, which is what the binary does before it
    /// serves anything.
    fn start(cluster: &cluster::Cluster, address: std::net::SocketAddr) -> (Self, LeaseRefresher) {
        let lease = Arc::new(PdLease::new());
        let backend = cluster.backend_holding(Arc::clone(&lease) as Arc<dyn SchemaLeaseSource>);
        let conn = Arc::new(PdConn::new(address));
        let refresher = LeaseRefresher::new(Arc::clone(&conn), Arc::clone(&lease))
            .asserting_columnar_for(Arc::clone(&backend), TENANT);
        refresher
            .refresh()
            .expect("a node fetches its lease before it serves");
        (
            Node {
                backend,
                conn,
                lease,
            },
            refresher,
        )
    }

    fn session(&self, cluster: &cluster::Cluster) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&cluster.catalog),
                TENANT,
                esker_sql::session::register(),
            )
            .reporting_columnar_to(Arc::clone(&self.conn) as Arc<dyn ColumnarReport>),
        }
    }
}

/// Fail closed, driven the only honest way: by stopping the placement driver.
///
/// A node that cannot renew stops **writing** and keeps **reading** — the asymmetry is ADR 0028's
/// and ADR 0020's as amended, not a convenience: a reader's snapshot already agrees with the rows
/// it can see, so gating reads adds stalls and closes no hole.
#[tokio::test(flavor = "multi_thread")]
async fn a_lapsed_lease_refuses_writes_and_still_serves_reads() {
    let (_pd, pd_handle, address) = standin_pd::serve().await;
    let cluster = cluster::Cluster::start_on_this_runtime().await;

    // Everything below the socket is synchronous, so it runs off the reactor: `block_in_place`
    // rather than `spawn_blocking`, because the cluster is borrowed and a pool task would need it
    // for `'static`.
    let (node, refresher) = tokio::task::block_in_place(|| Node::start(&cluster, address));

    tokio::task::block_in_place(|| {
        let mut session = node.session(&cluster);
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .expect("a node holding a lease writes");
        session
            .run("INSERT INTO t VALUES (1, 'ada'), (2, 'grace')")
            .expect("a node holding a lease writes");

        // The re-driver's interval is PD's, not this node's.
        assert_eq!(
            node.backend.schema_step_interval(),
            Some(StepInterval {
                step_ms: STEP_MS,
                removal_extra_ms: REMOVAL_EXTRA_MS,
            }),
            "the step interval a node re-drives on comes from the lease answer",
        );
    });

    // The real refresher, on the thread the binary gives it.
    std::thread::Builder::new()
        .name("schema-lease".to_owned())
        .spawn(move || refresher.run())
        .unwrap();

    // Stop the placement driver. Nothing else changes: the stores are up, the client is
    // connected, and this node is simply no longer able to renew.
    pd_handle.shutdown().await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while node.backend.schema_lease_remaining().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the lease never lapsed with PD stopped",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tokio::task::block_in_place(|| {
        let mut session = node.session(&cluster);
        let refused = session
            .run("INSERT INTO t VALUES (3, 'edsger')")
            .expect_err("a node past its lease must not write");
        assert_eq!(
            refused.sqlstate(),
            esker_sql::sqlstate::READ_ONLY_SQL_TRANSACTION,
            "{refused}",
        );
        assert!(
            refused.to_string().contains("schema lease has expired"),
            "the refusal says which half is refused: {refused}",
        );

        // And the other half is untouched.
        assert_eq!(
            session.rows("SELECT name FROM t ORDER BY id"),
            vec![vec![Some("ada".to_owned())], vec![Some("grace".to_owned())]],
            "reads are never gated by the lease",
        );
        assert_eq!(
            node.backend.schema_step_interval(),
            None,
            "a node that has lost PD does not know the step interval either",
        );
        assert!(
            node.lease.refresh_period().is_some(),
            "it still knows how often to try again: the cadence is PD's last word, not a guess",
        );
    });

    drop(cluster);
}

/// `ALTER TABLE ... SET (columnar_replicas = N)` reaches PD, and what reaches it is the whole set.
///
/// ADR 0022 Decision 5: PD acts on the flag and cannot read it, so the node that ran the `ALTER`
/// reports — as **key ranges**, which is PD's own vocabulary, and as a full assertion rather than
/// a delta, which is what makes a lost report cost nothing and a cleared flag need no message of
/// its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_alter_reports_every_range_that_wants_columnar_replicas() {
    // **A lease that cannot lapse inside this test**, because this test cannot renew one: a
    // refresher thread asserts the columnar set on every renewal, and what is counted below is
    // exactly those assertions. Held on the one lease `serve()` hands out, the later `ALTER`s
    // here are refused on a loaded machine — correctly, by ADR 0028 — and the failure is the
    // machine's speed rather than anything this test is about (`standin_pd::NO_LAPSE_MS`).
    let (pd, pd_handle, address) = standin_pd::serve_with_lease(standin_pd::NO_LAPSE_MS).await;
    let cluster = cluster::Cluster::start_on_this_runtime().await;
    let (node, refresher) = tokio::task::block_in_place(|| Node::start(&cluster, address));

    // The startup report: this node asserted an empty set, because nothing wants a copy yet.
    assert_eq!(
        pd.reports(),
        vec![Vec::new()],
        "a node asserts on startup, which is what repairs a PD that restarted",
    );

    let (_first, second) = tokio::task::block_in_place(|| {
        let mut session = node.session(&cluster);
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, name text)")
            .unwrap();
        session
            .run("CREATE TABLE u (id int8 PRIMARY KEY, name text)")
            .unwrap();
        assert_eq!(
            pd.reports().len(),
            1,
            "a CREATE TABLE says nothing about columnar placement",
        );

        session
            .run("ALTER TABLE t SET (columnar_replicas = 2)")
            .unwrap();
        let first = table_range(&node, &cluster, "t");
        assert_eq!(
            pd.last_report().unwrap(),
            vec![wish(&first, 2)],
            "the ALTER reported the table's row range and its count",
        );

        // A second table, and the report carries **both**: the scan is the message.
        session
            .run("ALTER TABLE u SET (columnar_replicas = 1)")
            .unwrap();
        let second = table_range(&node, &cluster, "u");
        assert_eq!(
            pd.last_report().unwrap(),
            vec![wish(&first, 2), wish(&second, 1)],
            "a report is the whole catalog, never the delta of one ALTER",
        );
        (first, second)
    });

    tokio::task::block_in_place(|| {
        let mut session = node.session(&cluster);
        // Zero is removal, and it arrives as an absence rather than as a zero.
        session
            .run("ALTER TABLE t SET (columnar_replicas = 0)")
            .unwrap();
        assert_eq!(
            pd.last_report().unwrap(),
            vec![wish(&second, 1)],
            "a table set to zero is absent from the assertion, which is how removal travels",
        );
        assert!(
            pd.last_report()
                .unwrap()
                .iter()
                .all(|wish| wish.replicas != 0),
            "a wish never carries a zero",
        );

        // A rolled-back ALTER reports nothing: what is asserted is what the cluster can read.
        session.executor.begin(false).unwrap();
        session
            .run("ALTER TABLE u SET (columnar_replicas = 3)")
            .unwrap();
        session.executor.rollback().unwrap();
        assert_eq!(
            pd.last_report().unwrap(),
            vec![wish(&second, 1)],
            "an ALTER that was rolled back was never true, so it is never reported",
        );

        // And the same one inside a block that commits does report, at the commit.
        session.executor.begin(false).unwrap();
        session
            .run("ALTER TABLE u SET (columnar_replicas = 3)")
            .unwrap();
        let before = pd.reports().len();
        session.executor.commit().unwrap();
        assert_eq!(pd.reports().len(), before + 1, "the block's commit reports");
        assert_eq!(pd.last_report().unwrap(), vec![wish(&second, 3)]);
    });

    // The anti-entropy sweep: a refresh re-asserts the same content, which is why there is no
    // acknowledgement protocol and no retry queue.
    let before = pd.reports().len();
    tokio::task::block_in_place(|| refresher.refresh().unwrap());
    assert_eq!(pd.reports().len(), before + 1);
    assert_eq!(
        pd.last_report().unwrap(),
        vec![wish(&second, 3)],
        "every refresh re-asserts the whole set",
    );

    pd_handle.shutdown().await.unwrap();
    drop(cluster);
}

/// The row range of a table, which is what a wish names.
fn table_range(node: &Node, cluster: &cluster::Cluster, name: &str) -> (Vec<u8>, Vec<u8>) {
    let txn = node.backend.begin().unwrap();
    let view = cluster.catalog.view(&*txn, TENANT).unwrap();
    let table = view.table(name).unwrap().unwrap();
    let range = esker_keys::row::table_row_range(TENANT, table.id);
    let _ = txn.rollback();
    range
}

fn wish(range: &(Vec<u8>, Vec<u8>), replicas: u8) -> ColumnarWish {
    ColumnarWish {
        start_key: bytes::Bytes::from(range.0.clone()),
        end_key: bytes::Bytes::from(range.1.clone()),
        replicas,
    }
}
