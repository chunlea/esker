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

use esker_sql::backend::{Backend, SchemaLease as SchemaLeaseSource, StepInterval};
use esker_sql::exec::Executor;
use esker_sql::pd::{LeaseRefresher, PdConn, PdLease};

use cluster::{Session, TENANT};
use standin_pd::{REMOVAL_EXTRA_MS, STEP_MS};

/// The whole node, as the binary builds it: a lease, a backend holding it, and a refresher.
struct Node {
    backend: Arc<dyn Backend>,
    lease: Arc<PdLease>,
}

impl Node {
    /// Builds the node and fetches its first lease, which is what the binary does before it
    /// serves anything.
    fn start(cluster: &cluster::Cluster, address: std::net::SocketAddr) -> (Self, LeaseRefresher) {
        let lease = Arc::new(PdLease::new());
        let backend = cluster.backend_holding(Arc::clone(&lease) as Arc<dyn SchemaLeaseSource>);
        let refresher = LeaseRefresher::new(Arc::new(PdConn::new(address)), Arc::clone(&lease));
        refresher
            .refresh()
            .expect("a node fetches its lease before it serves");
        (Node { backend, lease }, refresher)
    }

    fn session(&self, cluster: &cluster::Cluster) -> Session {
        Session {
            executor: Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&cluster.catalog),
                TENANT,
            ),
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
