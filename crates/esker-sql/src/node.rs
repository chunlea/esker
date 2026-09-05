//! What a node hands each connection: one [`Executor`] over the state the node shares.
//!
//! **This lived in `bin/esker-sql.rs` and could not be tested from anywhere.** A binary's types are
//! not importable, so every harness that wanted a node built its own `Executors` by hand — and one
//! of them drifted: `tests/psql_smoke.rs`'s double never called
//! [`Executor::sharing_sequence_blocks`], so the only harness that spoke the wire protocol ran
//! with [ADR 0072](../../../docs/adr/0072-a-sequence-block-belongs-to-the-node-not-to-the-connection.md)
//! switched off and could not have shown a per-connection block if there had been one.
//!
//! That is the inverse of the usual failure — the product had the feature and the double did not —
//! and it is why a question about the *running* node took an instrumented pass of the real suite to
//! answer rather than a test. Moving the type here makes the thing the binary runs the same thing a
//! test constructs, so a claim about what a node shares is checkable in this repository.

use std::sync::Arc;

use crate::backend::Backend;
use crate::catalog::Catalog;
use crate::exec::Executor;
use crate::fragment::FragmentSource;
use crate::pd::ColumnarReport;
use crate::pgwire::server::Executors;
use crate::pgwire::session::Execute;

/// The store and the catalog cache, shared; one [`Executor`] per session over them.
///
/// `Debug` by hand because three of the five fields are trait objects with no `Debug` of their own,
/// and what a reader wants from this type is which of the optional wirings are attached rather than
/// the addresses behind them.
pub struct Sessions {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    /// This node's advisory locks, shared by every session it serves.
    ///
    /// Beside the catalog cache because it has the same lifetime and the same scope: node-wide, in
    /// memory, gone when the process is. A session that took one and never released it loses it
    /// when its connection closes, which is what a real server does too (`crate::advisory`).
    locks: Arc<crate::advisory::Locks>,
    /// The node's reserved sequence blocks, shared by every session it serves (ADR 0072).
    sequences: Arc<crate::sequence::Blocks>,
    /// Where an `ALTER … SET (columnar_replicas = N)` reports to, on a node that has a PD.
    columnar: Option<Arc<dyn ColumnarReport>>,
    /// Where a plan fragment goes, on a node that can send one (ADR 0022 milestone 4).
    ///
    /// `None` without `--pd`, and that is not a degraded node: routing needs to know which peer of
    /// a region is the columnar learner, and only the placement driver can say. A node without one
    /// plans every query on rows and `EXPLAIN` says why.
    fragments: Option<Arc<dyn FragmentSource>>,
}

impl Sessions {
    /// A node over one store and one catalog cache, with its own locks and sequence blocks.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, catalog: Arc<Catalog>) -> Self {
        Sessions {
            backend,
            catalog,
            locks: Arc::new(crate::advisory::Locks::new()),
            sequences: Arc::new(crate::sequence::Blocks::default()),
            columnar: None,
            fragments: None,
        }
    }

    /// Where an `ALTER … SET (columnar_replicas = N)` reports to.
    #[must_use]
    pub fn reporting_columnar_to(mut self, report: Arc<dyn ColumnarReport>) -> Self {
        self.columnar = Some(report);
        self
    }

    /// Where a plan fragment goes.
    #[must_use]
    pub fn asking_fragments_of(mut self, source: Arc<dyn FragmentSource>) -> Self {
        self.fragments = Some(source);
        self
    }

    /// The blocks every session it serves draws from — for a test that asks whether they really do.
    #[must_use]
    pub fn sequence_blocks(&self) -> Arc<crate::sequence::Blocks> {
        Arc::clone(&self.sequences)
    }
}

impl Executors for Sessions {
    fn for_session(
        &self,
        database: &str,
        identity: crate::session::Backend,
    ) -> crate::Result<Box<dyn Execute + Send>> {
        // **The directory decides the tenant**, and it is read once per connection rather than per
        // statement: the answer cannot change under a session, because dropping the database it is
        // serving is `55006` (ADR 0052).
        let txn = self.backend.begin()?;
        let tenant = crate::catalog::database_id(&*txn, database)?
            .ok_or_else(|| crate::SqlError::UndefinedDatabase(database.to_owned()))?;
        let _ = txn.rollback();
        let mut executor = Executor::new(
            Arc::clone(&self.backend),
            Arc::clone(&self.catalog),
            tenant,
            identity,
        )
        .serving_database(database)
        .sharing_advisory_locks(Arc::clone(&self.locks))
        // **The node's, not this connection's** — a pooled client is the normal client, and a
        // block per connection is what made five inserts answer 1, 33, 65, 97, 129 (ADR 0072).
        .sharing_sequence_blocks(Arc::clone(&self.sequences));
        if let Some(report) = &self.columnar {
            executor = executor.reporting_columnar_to(Arc::clone(report));
        }
        if let Some(source) = &self.fragments {
            executor = executor.asking_fragments_of(Arc::clone(source));
        }
        Ok(Box::new(executor))
    }
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Sessions")
            .field("columnar", &self.columnar.is_some())
            .field("fragments", &self.fragments.is_some())
            .finish_non_exhaustive()
    }
}
