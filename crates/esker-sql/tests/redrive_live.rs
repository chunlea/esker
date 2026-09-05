//! The re-driver with a **real** interval: a job orphaned on one node is finished by the other.
//!
//! `tests/redrive.rs` proves the logic — idleness counted in passes, the wait a removing step
//! takes, two re-drivers racing — by driving `pass()` by hand over one process's storage. This
//! proves the *wiring*, which is the half that was missing until the SQL node had a placement
//! driver: with no PD, `Backend::schema_step_interval` answers `None`, `ReDriver::run` finds no
//! interval and waits for ever, and every one of those passing tests is about a mechanism that
//! never ran in the real binary (`docs/plans/phase-8-learner.md` §wiring).
//!
//! So nothing here is driven by hand. Two SQL nodes, each with its own client, its own catalog
//! cache and its own lease, over three real stores; node A abandons a schema change and node B's
//! `run()` loop finishes it on the cadence **PD published**.
//!
//! # What a timeout here would mean
//!
//! The test runs for several lease terms, so the refresher thread is load-bearing: a node whose
//! lease lapsed would stop re-driving mid-change (ADR 0028, and `ReDriver::pass` checks it before
//! doing any work). A hang is therefore a renewal that stopped, and not only a step that did not
//! take.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;
mod standin_pd;

use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource, StepInterval};
use esker_sql::catalog::SchemaState;
use esker_sql::exec::Executor;
use esker_sql::exec::redrive::ReDriver;
use esker_sql::pd::{LeaseRefresher, PdConn, PdLease};

use cluster::{Session, TENANT};
use standin_pd::{REMOVAL_EXTRA_MS, STEP_MS};

/// Rows to backfill. One batch (`esker_sql::exec::BATCH_ROWS` is 256), because what is under test
/// is the schedule and not the backfill — `tests/redrive.rs` owns the cursor's own case.
const ROWS: i64 = 40;

/// One SQL node: its own client, catalog and lease, over the cluster's stores.
struct Node {
    backend: Arc<dyn Backend>,
    catalog: Arc<esker_sql::catalog::Catalog>,
}

impl Node {
    /// Builds a node, fetches its lease, and starts the refresher — in that order, as the binary
    /// does.
    fn start(cluster: &cluster::Cluster, pd: std::net::SocketAddr) -> Self {
        let lease = Arc::new(PdLease::new());
        let backend = cluster.backend_for(
            cluster.another_client(),
            Arc::clone(&lease) as Arc<dyn SchemaLeaseSource>,
        );
        let refresher = LeaseRefresher::new(Arc::new(PdConn::new(pd)), Arc::clone(&lease));
        refresher.refresh().expect("a node fetches its lease");
        std::thread::Builder::new()
            .name("schema-lease".to_owned())
            .spawn(move || refresher.run())
            .unwrap();
        Node {
            backend,
            catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        }
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

    fn redriver(&self) -> ReDriver {
        ReDriver::new(Arc::clone(&self.backend), Arc::clone(&self.catalog), TENANT)
    }

    /// The state of the one index on `t`, read fresh.
    fn index_state(&self) -> Option<SchemaState> {
        let txn = self.backend.begin().unwrap();
        let view = self.catalog.view(&*txn, TENANT).unwrap();
        let table = view.table("t").unwrap().unwrap();
        let state = table.indexes.first().map(|index| index.state);
        let _ = txn.rollback();
        state
    }
}

/// A `CREATE INDEX CONCURRENTLY` whose node walks away is finished by another node, on PD's
/// interval, with nobody calling `esker_schema_step`.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_orphaned_on_one_node_is_finished_by_the_other() {
    let (_pd, pd_handle, address) = standin_pd::serve().await;
    let cluster = cluster::Cluster::start_on_this_runtime().await;

    let (a, b) = tokio::task::block_in_place(|| {
        (
            Node::start(&cluster, address),
            Node::start(&cluster, address),
        )
    });

    // The interval both nodes re-drive on is the one PD published, not one either invented.
    let published = Some(StepInterval {
        step_ms: STEP_MS,
        removal_extra_ms: REMOVAL_EXTRA_MS,
    });
    assert_eq!(a.backend.schema_step_interval(), published);
    assert_eq!(b.backend.schema_step_interval(), published);

    tokio::task::block_in_place(|| {
        let mut session = a.session();
        session
            .run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
            .unwrap();
        let values: Vec<String> = (1..=ROWS)
            .map(|id| format!("({id}, {})", id * 10))
            .collect();
        session
            .run(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
        session
            .run("CREATE INDEX CONCURRENTLY ti ON t (a)")
            .unwrap();
        assert_eq!(
            session.rows("SELECT esker_schema_step('ti')")[0][0],
            Some("delete-only".to_owned()),
            "the node that started it took one step",
        );
        // And then it walks away, holding nothing: the record and its cursor are in the catalog.
        assert_eq!(a.index_state(), Some(SchemaState::DeleteOnly));
    });

    // Node B's re-driver, as the binary runs it: a thread, on PD's cadence, with nothing driving
    // it from this test.
    let redriver = b.redriver();
    assert_eq!(redriver.interval(), published);
    std::thread::Builder::new()
        .name("schema-redriver".to_owned())
        .spawn(move || redriver.run())
        .unwrap();

    // Several lease terms' worth of patience: what this waits on is a background thread on a
    // 300 ms cadence, and a machine running three stores beside it is not a quiet one.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::task::block_in_place(|| b.index_state()) == Some(SchemaState::Public) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the second node never finished the orphaned job; it reached {:?}",
            tokio::task::block_in_place(|| b.index_state()),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tokio::task::block_in_place(|| {
        // The index is complete: every row that predated it is in it, which is what makes the
        // difference between a job that finished and a job that was merely marked finished.
        let mut session = b.session();
        assert_eq!(
            session.rows("SELECT id FROM t WHERE a = 300"),
            [[Some("30".to_owned())]],
        );
        // And the node that abandoned it sees the same, because the state is the cluster's.
        assert_eq!(a.index_state(), Some(SchemaState::Public));
    });

    pd_handle.shutdown().await.unwrap();
    drop(cluster);
}
