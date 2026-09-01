//! The placement driver, from a SQL node: the schema lease, and the columnar report.
//!
//! Exactly two methods travel this way — `Pd::SchemaLease` and `Pd::ReportColumnar` — and both are
//! a *SQL node's*. That is why this connection is here rather than in `esker-client`, whose raw-KV
//! callers have no use for either, and why `esker-store`'s `PdClient` trait (bootstrap, `alloc_id`,
//! `get_region`, the two heartbeats) is untouched: a store has no business holding a lease it does
//! not use (`docs/plans/phase-8-learner.md` §wiring).
//!
//! # What this cable turns on
//!
//! Three finished features are inert in a node with no PD, and all three come alive here:
//!
//! * the **schema lease** (ADR 0028), so that fail-closed arms — a node that cannot renew stops
//!   *writing* and keeps reading;
//! * the **re-driver's interval**, which is the same PD answer's second number
//!   ([`crate::exec::redrive`]);
//! * **`ReportColumnar`** (ADR 0022 Decision 5), so that `ALTER TABLE ... SET (columnar_replicas
//!   = N)` becomes placement rather than a durable record nobody reads.
//!
//! # Blocking, on purpose
//!
//! [`BlockingTransport`] owns a runtime of its own and every call carries a deadline — *"a
//! blocking call with no deadline is a hang"*. Its two callers here are a refresher **thread**,
//! which sleeps between passes and would hold a runtime worker for the whole of one, and a
//! statement on `tokio`'s blocking pool, which is where a synchronous client belongs
//! (`crate::pgwire::server`).
//!
//! # No retry queue, anywhere
//!
//! A failed call drops the connection; the next one builds a new connection. There is no backoff
//! loop because the caller's own cadence *is* the backoff, and no acknowledgement protocol for a
//! report because [`columnar_wishes`] is a **full assertion**: whatever is lost is repaired by the
//! next refresh, which re-sends the whole set (ADR 0022 Decision 5).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::pd::ColumnarWish;
use esker_proto::{BlockingTransport, PdReq, PdResp, ProtoError, TransportConfig};

use crate::backend::{Backend, SchemaLease, StepInterval};
use crate::exec::for_each_page;

/// A connection to the placement driver, held by one SQL node.
///
/// **Lazily connected**, like the store's own PD client: a node whose PD is not up yet fails its
/// first *call* rather than its construction, so a cluster can be started in any order. The
/// binary's first call is the lease fetch, which is what makes "PD is unreachable at startup" a
/// startup failure rather than a node that serves without a lease.
#[derive(Debug)]
pub struct PdConn {
    address: SocketAddr,
    config: TransportConfig,
    /// The live connection, or `None` before the first call and after a failed one.
    ///
    /// An `Arc` so a call can leave the lock before it blocks: the refresher and a session's
    /// `ALTER` share this connection, and holding the mutex across a call would let a slow PD
    /// stall a statement for a whole request timeout.
    transport: Mutex<Option<Arc<BlockingTransport>>>,
    /// The cluster PD said it serves, or `0` before it has said.
    cluster_id: AtomicU64,
}

impl PdConn {
    /// A connection to the placement driver at `address`, with the project's transport defaults.
    #[must_use]
    pub fn new(address: SocketAddr) -> Self {
        Self::with_config(address, TransportConfig::new())
    }

    /// The same, configured explicitly.
    #[must_use]
    pub fn with_config(address: SocketAddr, config: TransportConfig) -> Self {
        Self {
            address,
            config,
            transport: Mutex::new(None),
            cluster_id: AtomicU64::new(0),
        }
    }

    /// Where the placement driver is.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// This node's lease, as PD's three numbers.
    ///
    /// They arrive **together** because PD computes the interval from the lease: a node holding
    /// one without the other would be holding half an arithmetic (ADR 0028).
    pub fn schema_lease(&self) -> Result<Lease, ProtoError> {
        match self.call(&PdReq::SchemaLease)? {
            PdResp::SchemaLease {
                lease_ms,
                step_interval_ms,
                removal_extra_ms,
            } => Ok(Lease {
                lease_ms,
                step: StepInterval {
                    step_ms: step_interval_ms,
                    removal_extra_ms,
                },
            }),
            other => Err(mismatch("SchemaLease", &other)),
        }
    }

    /// Tells PD **the whole** set of ranges that want columnar replicas.
    ///
    /// Never a delta: every SQL node reads the same catalog, so every report has the same content
    /// and the last writer is right whoever it was.
    pub fn report_columnar(&self, wishes: Vec<ColumnarWish>) -> Result<(), ProtoError> {
        match self.call(&PdReq::ReportColumnar { wishes })? {
            PdResp::ReportColumnar => Ok(()),
            other => Err(mismatch("ReportColumnar", &other)),
        }
    }

    /// One call, addressed to the cluster this connection has learned about.
    ///
    /// A SQL node never bootstraps, so it learns the cluster id the only other way PD offers: the
    /// refusal names the cluster PD serves, so the first call adopts that id and retries, and
    /// every call afterwards carries it. Retried **once**, and only on a mismatch naming a
    /// different cluster, so a PD that refused the id it had just given is reported rather than
    /// looped on (the same rule `esker-cli`'s `region` commands follow).
    fn call(&self, request: &PdReq) -> Result<PdResp, ProtoError> {
        let connection = self.connection()?;
        let deadline = || Instant::now() + self.config.request_timeout;
        let known = self.cluster_id.load(Ordering::Relaxed);
        let answer =
            match connection.call(esker_proto::pd::encode(known, request.clone()), deadline()) {
                Err(ProtoError::ClusterMismatch { expected, .. }) if expected != known => {
                    self.cluster_id.store(expected, Ordering::Relaxed);
                    connection.call(
                        esker_proto::pd::encode(expected, request.clone()),
                        deadline(),
                    )
                }
                other => other,
            };
        match answer {
            Ok(response) => esker_proto::pd::decode(response),
            Err(error) => {
                // The connection is suspect after any failure, and the next call builds a fresh
                // one. Dropped by identity rather than unconditionally, so a call that failed
                // while another had already replaced the connection does not throw away a
                // working one.
                self.forget(&connection);
                Err(error)
            }
        }
    }

    /// The live connection, or a new one.
    fn connection(&self) -> Result<Arc<BlockingTransport>, ProtoError> {
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = slot.as_ref()
            && !existing.is_closed()
        {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(BlockingTransport::connect_with(self.address, self.config)?);
        *slot = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Drops `used`, if it is still the connection this holds.
    fn forget(&self, used: &Arc<BlockingTransport>) {
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_some_and(|live| Arc::ptr_eq(live, used)) {
            *slot = None;
        }
    }
}

fn mismatch(asked: &str, got: &PdResp) -> ProtoError {
    ProtoError::invalid(format!(
        "asked the placement driver for {asked} and it answered {}",
        got.method().name()
    ))
}

/// PD's `SchemaLease` answer: how long this node may write, and how long a schema step waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    /// How long a node may serve **writes** from a cached schema, from when the answer arrived.
    pub lease_ms: u64,
    /// The step interval and the removal term, which the same answer carries.
    pub step: StepInterval,
}

/// A lease held by this node, refreshed by [`LeaseRefresher`].
///
/// The [`SchemaLease`] implementation the real binary attaches to its [`crate::backend::Backend`].
/// The trait is what makes fail-closed testable — *"the test that proves fail closed has to be
/// able to stop answering"* — and this is the implementation that stops answering for the real
/// reason: PD went away.
#[derive(Debug, Default)]
pub struct PdLease {
    /// The last answer, kept **after** it expires.
    ///
    /// Expiry is a question about `at + lease`, not a reason to forget: the same record still
    /// says what period to refresh on, which is how a node that has lost PD keeps trying at the
    /// cadence PD asked for rather than at one this crate invented.
    held: Mutex<Option<Held>>,
}

/// One PD answer, and when it arrived.
#[derive(Debug, Clone, Copy)]
struct Held {
    at: Instant,
    lease: Lease,
}

impl PdLease {
    /// A lease nobody has fetched yet, which answers `None` to everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a fresh answer from PD.
    pub fn record(&self, lease: Lease) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(Held {
            at: Instant::now(),
            lease,
        });
    }

    /// How long until this node must stop writing, or `None` if it already must.
    ///
    /// Measured on a monotonic [`Instant`], which is a *duration since a local event* and not an
    /// ordering: `CLAUDE.md` invariant 6 keeps wall clocks out of ordering, and nothing here
    /// orders anything — the timestamps this node writes still come only from the oracle.
    fn left(&self) -> Option<Duration> {
        let held = (*self.held.lock().unwrap_or_else(PoisonError::into_inner))?;
        Duration::from_millis(held.lease.lease_ms).checked_sub(held.at.elapsed())
    }

    /// How long to wait before renewing: **a third of the lease**.
    ///
    /// Derived from PD's number rather than configured, because a tunable here would be a way to
    /// be wrong independently of the bound it exists to respect. A third leaves two more attempts
    /// after a lost round trip, so a single dropped answer cannot expire a lease.
    ///
    /// `None` before the first answer — there is no number yet to take a third of.
    #[must_use]
    pub fn refresh_period(&self) -> Option<Duration> {
        let held = (*self.held.lock().unwrap_or_else(PoisonError::into_inner))?;
        // The floor is a spin guard and never a cadence: a lease that short refuses every write
        // whatever this does, and a PD answering `lease_ms = 0` must not turn a refresher into a
        // busy loop against it.
        Some((Duration::from_millis(held.lease.lease_ms) / 3).max(MIN_REFRESH_PERIOD))
    }
}

/// The shortest a refresh cadence may become, whatever PD says the lease is.
const MIN_REFRESH_PERIOD: Duration = Duration::from_millis(100);

impl SchemaLease for PdLease {
    fn remaining(&self) -> Option<Duration> {
        self.left()
    }

    /// The step interval, **only while the lease is live**.
    ///
    /// Both or neither, as the answer arrives: a node that has lost PD does not know how long a
    /// schema step must wait any more than it knows it may still write, and answering with the
    /// number from an expired answer would be re-driving on a bound nobody is renewing.
    fn step_interval(&self) -> Option<StepInterval> {
        let held = (*self.held.lock().unwrap_or_else(PoisonError::into_inner))?;
        self.left().map(|_| held.lease.step)
    }
}

/// Where this node's columnar wishes go.
///
/// A trait so that the executor holds a *sink* rather than a socket: the report is a full
/// assertion sent after a commit, and what it is sent over is not the executor's business.
/// [`PdConn`] is the implementation the binary attaches.
pub trait ColumnarReport: std::fmt::Debug + Send + Sync {
    /// Asserts the whole set of ranges that want columnar replicas.
    fn report(&self, wishes: Vec<ColumnarWish>) -> Result<(), ProtoError>;
}

impl ColumnarReport for PdConn {
    fn report(&self, wishes: Vec<ColumnarWish>) -> Result<(), ProtoError> {
        self.report_columnar(wishes)
    }
}

/// Every range that wants columnar replicas, read from the catalog.
///
/// **The scan is the message.** A report is a full assertion, so this is what is sent — never a
/// delta of what one `ALTER` changed. A table set to `0` is *absent* rather than present with a
/// zero, which is how a cleared flag travels and why removal needs no message of its own.
///
/// Ranges rather than table ids, because a range is PD's own vocabulary: it acts on this without
/// learning that a table exists (`CLAUDE.md` invariant 7), and a table that later splits into four
/// regions is still one range that every overlapping region inherits.
pub fn columnar_wishes(
    backend: &dyn Backend,
    tenant: u64,
) -> crate::error::Result<Vec<ColumnarWish>> {
    let mut txn = backend.begin()?;
    let (start, end) = crate::catalog::columnar_range(tenant);
    let mut wishes = Vec::new();
    let walked = for_each_page(&mut *txn, &start, &end, |_, page| {
        for (key, value) in page {
            let (table_id, replicas) = crate::catalog::decode_columnar(tenant, key, value)?;
            if replicas == 0 {
                continue;
            }
            let (start_key, end_key) = esker_keys::row::table_row_range(tenant, table_id);
            wishes.push(ColumnarWish {
                start_key: Bytes::from(start_key),
                end_key: Bytes::from(end_key),
                replicas,
            });
        }
        Ok(())
    });
    // A read, so there is nothing to commit and nothing to lose by rolling back.
    let _ = txn.rollback();
    walked?;
    Ok(wishes)
}

/// The thread that renews this node's lease, and re-asserts its columnar wishes.
///
/// One thread doing both, because they are one PD answer's worth of work: the lease is what says
/// when to come back, and the re-assertion is the anti-entropy sweep that ADR 0022 Decision 5
/// relies on instead of an acknowledgement protocol.
#[derive(Debug)]
pub struct LeaseRefresher {
    conn: Arc<PdConn>,
    lease: Arc<PdLease>,
    /// What to read the wishes from, or `None` for a refresher that only renews.
    wishes: Option<(Arc<dyn Backend>, u64)>,
}

impl LeaseRefresher {
    /// A refresher for `lease`, which renews and nothing else.
    #[must_use]
    pub fn new(conn: Arc<PdConn>, lease: Arc<PdLease>) -> Self {
        Self {
            conn,
            lease,
            wishes: None,
        }
    }

    /// The same refresher, re-asserting `tenant`'s columnar wishes on every round.
    ///
    /// The binary always attaches this; a test that is only about the lease does not have to.
    #[must_use]
    pub fn asserting_columnar_for(mut self, backend: Arc<dyn Backend>, tenant: u64) -> Self {
        self.wishes = Some((backend, tenant));
        self
    }

    /// One round: renew the lease, then re-assert the wishes.
    ///
    /// The lease first, because it is the half that must not be late — a report that misses a
    /// round is repaired by the next one, and a lease that misses enough of them stops this node
    /// writing.
    ///
    /// # Errors
    ///
    /// The renewal's failure. A **report** that fails is logged and not returned: the lease is
    /// still good, so this node keeps writing, and PD hears the same content on the next round.
    pub fn refresh(&self) -> Result<Lease, ProtoError> {
        let lease = self.conn.schema_lease()?;
        self.lease.record(lease);
        self.assert_wishes();
        Ok(lease)
    }

    /// Sends the whole set, or logs why it could not read it.
    ///
    /// **A failed read sends nothing**, which is the one thing that must not go wrong here: an
    /// empty report is a valid assertion meaning "no table wants a columnar copy", so a node that
    /// reported `[]` because its own store was unreachable would retire every learner in the
    /// cluster.
    fn assert_wishes(&self) {
        let Some((backend, tenant)) = &self.wishes else {
            return;
        };
        match columnar_wishes(&**backend, *tenant) {
            Ok(wishes) => {
                let ranges = wishes.len();
                if let Err(error) = self.conn.report_columnar(wishes) {
                    tracing::warn!(
                        %error,
                        "could not report columnar placement; the next refresh re-asserts it"
                    );
                } else {
                    tracing::debug!(ranges, "asserted columnar placement");
                }
            }
            Err(error) => tracing::warn!(
                %error,
                "could not read this tenant's columnar settings; reporting nothing rather than \
                 asserting an empty set"
            ),
        }
    }

    /// Renews for as long as this node runs.
    ///
    /// Never returns, and never gives up: a node that has lost PD has stopped writing, and the
    /// only way back is to keep asking. The cadence stays the one PD last published
    /// ([`PdLease::refresh_period`]).
    pub fn run(self) {
        loop {
            let period = self.lease.refresh_period().unwrap_or(MIN_REFRESH_PERIOD);
            std::thread::sleep(period);
            match self.refresh() {
                Ok(lease) => tracing::trace!(
                    lease_ms = lease.lease_ms,
                    step_ms = lease.step.step_ms,
                    "renewed the schema lease"
                ),
                // Not an error the node can act on: writes fail closed on their own when the
                // lease runs out, and reads are unaffected either way.
                Err(error) => tracing::warn!(
                    %error,
                    "could not renew the schema lease from the placement driver"
                ),
            }
        }
    }
}
