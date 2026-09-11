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
use esker_client::region_cache::{RegionResolver, Route};
use esker_proto::pd::ColumnarWish;
use esker_proto::{
    BlockingTransport, LeaderBook, PdReq, PdResp, ProtoError, Redirects, TransportConfig,
};

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
    /// Who is in the group, and which member this node believes leads it.
    book: LeaderBook,
    config: TransportConfig,
    /// The live connection and the member it is to, or `None` before the first call and after a
    /// failed one.
    ///
    /// An `Arc` so a call can leave the lock before it blocks: the refresher and a session's
    /// `ALTER` share this connection, and holding the mutex across a call would let a slow PD
    /// stall a statement for a whole request timeout.
    ///
    /// **The address travels with it** because the believed member moves: a connection to the
    /// member this node has just stopped believing is of no use for the call that redirected it.
    transport: Mutex<Option<(SocketAddr, Arc<BlockingTransport>)>>,
    /// The cluster PD said it serves, or `0` before it has said.
    cluster_id: AtomicU64,
}

impl PdConn {
    /// A connection to the placement driver at `address`, with the project's transport defaults.
    ///
    /// One address is a group of one, which is what a cluster with a single driver is and what
    /// every caller had before there could be more than one.
    #[must_use]
    pub fn new(address: SocketAddr) -> Self {
        Self::with_config(address, TransportConfig::new())
    }

    /// The same, configured explicitly.
    #[must_use]
    pub fn with_config(address: SocketAddr, config: TransportConfig) -> Self {
        Self::over(LeaderBook::lone(address), config)
    }

    /// A connection to a placement-driver **group**, given every member's address.
    ///
    /// Only the leader answers, so a node is given the whole list and moves between them as it is
    /// told to — or as it finds a member it cannot reach
    /// ([`esker_proto::LeaderBook`],
    /// [ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
    pub fn to_group(endpoints: &[SocketAddr], config: TransportConfig) -> Result<Self, ProtoError> {
        Ok(Self::over(LeaderBook::new(endpoints)?, config))
    }

    fn over(book: LeaderBook, config: TransportConfig) -> Self {
        Self {
            book,
            config,
            transport: Mutex::new(None),
            cluster_id: AtomicU64::new(0),
        }
    }

    /// The member of the group this node currently believes leads.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.book.believed()
    }

    /// Every member this node knows of.
    #[must_use]
    pub fn endpoints(&self) -> Vec<SocketAddr> {
        self.book.endpoints()
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

    /// The region covering `key`, as PD has it, with the peer it believes leads.
    ///
    /// `GetRegion` is the only routing question PD answers, and this is a SQL node asking it —
    /// which until milestone 4 nothing did: the binary routed from a static one-region table, so a
    /// cluster that had split served every key from a region that no longer covered it. It is also
    /// the only way a node learns that a region has a **columnar learner**, because a learner joins
    /// through a conf change and the peer list is what carries it (ADR 0022 Decision 1).
    ///
    /// `Ok(None)` is *no region covers this key*, which is a routing failure the caller reports and
    /// does not retry; an `Err` is *PD could not say*, which usually is retryable. Collapsing the
    /// two would turn a momentary PD outage into a terminal error on every call in the process.
    pub fn get_region(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
        let PdResp::GetRegion {
            region,
            leader_peer_id,
            ..
        } = self.call(&PdReq::GetRegion {
            key: Bytes::copy_from_slice(key),
        })?
        else {
            return Err(mismatch("GetRegion", &PdResp::ReportColumnar));
        };
        Ok(region.map(|region| {
            let leader = (leader_peer_id != 0)
                .then(|| {
                    region
                        .peers
                        .iter()
                        .find(|peer| peer.peer_id == leader_peer_id)
                        .copied()
                })
                .flatten();
            Route { region, leader }
        }))
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

    /// One call, to whichever member of the group this node believes leads it.
    ///
    /// The rules are [`esker_proto::LeaderBook`]'s and the store's placement-driver client obeys
    /// the same ones: follow a hint that names a member, wait rather than spin when nobody will say
    /// who leads, and — the one that matters when a driver is **killed** rather than deposed —
    /// move past a member this node provably could not reach.
    ///
    /// **Sleeping here is right**, unusually. `PdConn`'s two callers are a refresher thread and a
    /// statement on `tokio`'s blocking pool; both are already blocking on purpose, which is the
    /// whole argument of this file's header.
    ///
    /// Every method a SQL node sends through here is safe to send again: `Tso` hands out fresh
    /// timestamps and never reuses one, `GetRegion` and `SchemaLease` are reads, and
    /// `ReportColumnar` is a full assertion whose last writer is right.
    fn call(&self, request: &PdReq) -> Result<PdResp, ProtoError> {
        let mut redirects = Redirects::new();
        loop {
            match self.attempt(request) {
                Ok(response) => return Ok(response),
                Err(ProtoError::PdNotLeader {
                    leader_id,
                    leader_address,
                }) => {
                    if !redirects.take() {
                        return Err(ProtoError::PdNotLeader {
                            leader_id,
                            leader_address,
                        });
                    }
                    if !leader_address.is_empty() && self.follow(&leader_address) {
                        continue;
                    }
                    // An election is in progress, or the hint is one this node cannot use. The
                    // member's answer will not change until the election ends, so ask another one
                    // — after a wait, because asking immediately is asking the same question.
                    self.book.advance();
                    std::thread::sleep(redirects.backoff());
                }
                // A member that was killed answers nothing at all, so nothing above moves this
                // node off it. `NotSent` is a request that provably never left this process.
                Err(other) if esker_proto::is_unreachable(&other) && redirects.take() => {
                    self.book.advance();
                    std::thread::sleep(redirects.backoff());
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Points this node at the member `hint` names, and says whether it moved.
    ///
    /// The decision is [`LeaderBook::follow`]'s; what this adds is the `Pd::Members` call it may
    /// need, which every member answers.
    fn follow(&self, hint: &str) -> bool {
        self.book
            .follow(hint, || match self.attempt(&PdReq::Members)? {
                PdResp::Members(membership) => Ok(membership),
                other => Err(mismatch("Members", &other)),
            })
    }

    /// One attempt, addressed to the cluster this connection has learned about.
    ///
    /// A SQL node never bootstraps, so it learns the cluster id the only other way PD offers: the
    /// refusal names the cluster PD serves, so the first call adopts that id and retries, and
    /// every call afterwards carries it. Retried **once**, and only on a mismatch naming a
    /// different cluster, so a PD that refused the id it had just given is reported rather than
    /// looped on (the same rule `esker-cli`'s `region` commands follow).
    fn attempt(&self, request: &PdReq) -> Result<PdResp, ProtoError> {
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

    /// The live connection to the member this node believes leads, or a new one.
    ///
    /// The **address** is what identifies it, not the slot being occupied: a redirect leaves a
    /// perfectly healthy connection to a member that has just told this node it is the wrong one.
    fn connection(&self) -> Result<Arc<BlockingTransport>, ProtoError> {
        let want = self.book.believed();
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((held, existing)) = slot.as_ref()
            && *held == want
            && !existing.is_closed()
        {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(BlockingTransport::connect_with(want, self.config)?);
        *slot = Some((want, Arc::clone(&fresh)));
        Ok(fresh)
    }

    /// Drops `used`, if it is still the connection this holds.
    fn forget(&self, used: &Arc<BlockingTransport>) {
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|(_, live)| Arc::ptr_eq(live, used))
        {
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
    ///
    /// **A renewal that lands more than half a lease after the previous one is a warning**, with
    /// both durations, because it is the only visible symptom of a renewal cadence that has come
    /// unstuck. Half rather than the whole: at the whole, the node has already refused a write and
    /// the log arrives after the client's error. Here, rather than in the refresher, so that every
    /// path which records a lease is measured — the startup grant included.
    pub fn record(&self, lease: Lease) {
        let now = Instant::now();
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = *held {
            let since = now.duration_since(previous.at);
            if since > Duration::from_millis(previous.lease.lease_ms) / 2 {
                tracing::warn!(
                    since_previous_ms = since.as_millis(),
                    lease_ms = previous.lease.lease_ms,
                    "a schema lease renewal landed more than half a lease after the previous one; \
                     a renewal this late is one lost round trip away from refusing writes"
                );
            }
        }
        *held = Some(Held { at: now, lease });
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

/// Routing, from the one thing that knows it.
///
/// A SQL node's region cache is a hint repaired by the refusals it causes (`esker-client`'s
/// invariant); this is the authority behind it. Nothing about the impl is specific to this crate —
/// it is `GetRegion` with the network in it — and it lives here rather than in `esker-client`
/// because that crate deliberately does not link a placement driver: it is handed a resolver.
impl RegionResolver for PdConn {
    fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
        self.get_region(key)
    }
}

/// **The driver's clock, which is the only one this system may order by** — `CLAUDE.md`
/// invariant 6, *"timestamps come only from PD's TSO. No node uses its wall clock for ordering."*
///
/// A node built without `--pd` keeps a local counter, and for one node that is a correct oracle:
/// it is monotonic and it is the only source. **For two it is not.** Two `esker-sql` processes each
/// counting from one hand the same `start_ts` to different transactions, and every MVCC decision in
/// this system — visibility, first-committer-wins, lock ownership — is made against that number.
/// `tests/two_nodes_one_clock.rs` is the arrangement and what it costs: a node reading at 1 cannot
/// see what another node committed at 2.
///
/// The same connection as the routing, deliberately. A second socket to the same driver would be a
/// second thing to notice had failed, and the driver answers both questions from the same leader.
impl esker_client::TimestampOracle for PdConn {
    fn tso(&self, count: u32) -> Result<u64, ProtoError> {
        // Counterfactual, for whoever changes this next: swapping the line below for a local
        // counter makes `two_nodes_on_one_driver_never_share_a_timestamp` fail on its first
        // comparison, because the two nodes then count independently from one.
        match self.call(&PdReq::Tso { count })? {
            PdResp::Tso { start_ts, .. } => Ok(start_ts),
            other => Err(ProtoError::invalid(format!(
                "the placement driver answered {other:?} to a timestamp request"
            ))),
        }
    }
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

/// Sends the whole set to PD, or logs why it could not read it.
///
/// **A failed read sends nothing**, which is the one thing that must not go wrong here: an empty
/// report is a valid assertion meaning "no table wants a columnar copy", so a node that reported
/// `[]` because its own store was unreachable would retire every learner in the cluster.
///
/// A free function rather than a method because it now has two callers on two threads — the
/// startup round and [`Reporter`] — and belongs to neither.
fn assert_wishes(conn: &PdConn, backend: &dyn Backend, tenant: u64) {
    match columnar_wishes(backend, tenant) {
        Ok(wishes) => {
            let ranges = wishes.len();
            if let Err(error) = conn.report_columnar(wishes) {
                tracing::warn!(
                    %error,
                    "could not report columnar placement; the next round re-asserts it"
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

    /// **The startup round**: renew the lease, then assert the wishes, both before serving.
    ///
    /// The one place the two still travel together, and the only place they may. A node has not
    /// opened its client socket yet, so this round's cost is paid by startup rather than by a
    /// lease that is meanwhile running out — which is precisely what makes it safe here and unsafe
    /// in a loop ([`LeaseRefresher::run`]).
    ///
    /// # Errors
    ///
    /// The renewal's failure, which the binary turns into a refusal to start. A **report** that
    /// fails is logged and not returned: the lease is still good, so this node serves, and PD
    /// hears the same content on the reporter's next round.
    pub fn refresh(&self) -> Result<Lease, ProtoError> {
        // **The report first here, and the renewal last.** The opposite of the order the loop used
        // to run, for the opposite reason: nothing is being kept alive across this round, so what
        // matters is that the lease be *fresh when this returns* — the caller opens the client
        // socket next. Recording it first and then reporting hands the socket a lease already aged
        // by the whole report, which on a slow one is a lease that has expired before the node has
        // served a single statement. That is the residual the deterministic test found once the
        // loop was fixed: 1 lapsed sample in 263, in exactly the window between serving and the
        // refresher's first renewal.
        if let Some((backend, tenant)) = &self.wishes {
            assert_wishes(&self.conn, &**backend, *tenant);
        }
        let lease = self.conn.schema_lease()?;
        self.lease.record(lease);
        Ok(lease)
    }

    /// Renews for as long as this node runs, and reports beside it on a thread of its own.
    ///
    /// Never returns, and never gives up: a node that has lost PD has stopped writing, and the
    /// only way back is to keep asking. The cadence stays the one PD last published
    /// ([`PdLease::refresh_period`]).
    ///
    /// # Why the report is not on this thread
    ///
    /// It was, and the two were one round: `sleep(lease / 3)` then renew then report. The renewal
    /// is recorded before the report, so *that* renewal is never late — which is what this comment
    /// used to claim, and it was true and it was not enough. **The next sleep did not begin until
    /// the report returned**, and the report is [`columnar_wishes`]: a transaction opened against
    /// the cluster and a catalog range scanned across it, whose cost is a region mid-split, a
    /// leader that has moved, or a store saturated by somebody else's load. A report costing more
    /// than the remaining two thirds of the lease let it expire, and nothing logged a thing —
    /// the renewal had succeeded, and a slow read is not an error.
    ///
    /// Measured on a real cluster: a round with `renew_ms=0` and `report_ms=3879` against a
    /// `period_ms=1666` on a 5 s lease, putting the next renewal 5,545 ms after the last, in the
    /// same run whose first `INSERT` came back `25006`. Reads kept working throughout, which is
    /// why it presents as a client problem and is not one.
    ///
    /// The two halves have different failure semantics — a lost report is repaired by the next
    /// one, a lost renewal stops this node writing — so they now keep different threads and
    /// different cadences, and neither can make the other late.
    pub fn run(self) {
        let Self {
            conn,
            lease,
            wishes,
        } = self;
        if let Some((backend, tenant)) = wishes {
            let reporter = Reporter {
                conn: Arc::clone(&conn),
                lease: Arc::clone(&lease),
                backend,
                tenant,
            };
            if let Err(error) = std::thread::Builder::new()
                .name("columnar-report".to_owned())
                .spawn(move || reporter.run())
            {
                // The lease is the half that must not be late, and it is this thread's. A node
                // that could not start a reporter renews correctly and asserts nothing, which is
                // a placement that stops being repaired — loud, and not fatal.
                tracing::warn!(
                    %error,
                    "could not start the columnar reporter; this node will renew its lease but \
                     assert no columnar placement"
                );
            }
        }
        renew_forever(&conn, &lease);
    }
}

/// Renews `lease` for ever, on a cadence that is a **deadline and not a delay**.
///
/// `sleep` until `round_start + period`, so the time a renewal itself takes comes out of the wait
/// rather than being added to it. With the report moved off this thread the renewal is a single
/// fast call, and this is what keeps that true if it ever stops being one.
fn renew_forever(conn: &PdConn, lease: &PdLease) {
    loop {
        let round_started = Instant::now();
        match conn.schema_lease() {
            Ok(answer) => {
                lease.record(answer);
                tracing::trace!(
                    lease_ms = answer.lease_ms,
                    step_ms = answer.step.step_ms,
                    "renewed the schema lease"
                );
            }
            // Not an error the node can act on: writes fail closed on their own when the lease
            // runs out, and reads are unaffected either way.
            Err(error) => tracing::warn!(
                %error,
                "could not renew the schema lease from the placement driver"
            ),
        }
        let period = lease.refresh_period().unwrap_or(MIN_REFRESH_PERIOD);
        let due = round_started + period;
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}

/// The columnar report, on its own thread and its own cadence.
///
/// One round at a time by construction — the loop does not start a report until the last has
/// returned — so a slow cluster read makes reports rarer and never makes them pile up. Its PD half
/// already carries a deadline ([`BlockingTransport`]); its backend half is a cluster read and its
/// duration is **not** bounded, which is exactly why it is no longer allowed near the renewal.
///
/// A report that misses a round is repaired by the next one: [`columnar_wishes`] is a full
/// assertion, not a delta (ADR 0022 Decision 5).
#[derive(Debug)]
struct Reporter {
    conn: Arc<PdConn>,
    lease: Arc<PdLease>,
    backend: Arc<dyn Backend>,
    tenant: u64,
}

impl Reporter {
    fn run(self) {
        loop {
            let round_started = Instant::now();
            assert_wishes(&self.conn, &*self.backend, self.tenant);
            // The lease's cadence, because it is the number PD publishes and a report has no
            // clock of its own to prefer. A deadline here too, so a slow round makes the next one
            // immediate rather than doubly late.
            let period = self.lease.refresh_period().unwrap_or(MIN_REFRESH_PERIOD);
            let due = round_started + period;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    }
}
