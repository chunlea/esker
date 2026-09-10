//! A placement driver in a `Mutex`, behind the real framing.
//!
//! `esker-sql` does not depend on `esker-pd` and must not — they are peers that meet on the wire —
//! so the driver these tests point a node at is a stand-in, exactly as `esker-store`'s own PD test
//! does it. **Nothing on the node's side is faked**: the connection, the lease, the refresher, the
//! backend and the executor are the real ones, and this answers them over a real socket.
//!
//! What it can do that a real PD cannot is *stop*, which is the point: ADR 0028 makes the lease a
//! trait so that "the test that proves fail closed has to be able to stop answering", and shutting
//! this down is how a test stops PD without stopping the cluster.

#![allow(
    dead_code,
    reason = "shared by several test binaries; each uses a subset"
)]
#![allow(
    unreachable_pub,
    reason = "a test-only module: `pub` is what makes it reachable from the binaries that include it"
)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, PoisonError};

use esker_proto::pd::ColumnarWish;
use esker_proto::{
    BoxFuture, PdReq, PdResp, ProtoError, Reply, Request, Response, Server, ServerHandle, Service,
};

/// A short lease, so a lapse is a second of test rather than five.
pub const LEASE_MS: u64 = 600;
/// A lease no test body can outlive, for the tests that are not about the lease.
///
/// A node renews on a thread ([`esker_sql::pd::LeaseRefresher::run`]), and a test that can spawn
/// one needs nothing from here. A test that *counts what the node sent PD* cannot spawn one — a
/// refresher asserts the columnar set on every renewal, so the thread it would need is the thread
/// that would spoil what it is counting — and it therefore holds the one lease it fetched at
/// startup for its whole body. With [`LEASE_MS`] that puts the machine's speed inside an
/// assertion about report content: `an_alter_reports_every_range_that_wants_columnar_replicas`
/// failed **4 of 48** under load with `SchemaLeaseExpired { command: "ALTER TABLE" }`, which is
/// ADR 0028 working exactly as designed and nothing to do with what the test asserts
/// (`docs/plans/phase-14-flakes.md` U3).
///
/// An hour, so that "the lease did not lapse" is a fact rather than a bet. Not a widened
/// deadline: the lapse itself is what `a_lapsed_lease_refuses_writes_and_still_serves_reads` is
/// for, and it keeps [`LEASE_MS`].
pub const NO_LAPSE_MS: u64 = 3_600_000;
/// PD's `lease_ms + lock_ttl_ms`, as PD computes it — the wait between the states of a schema
/// change. Short here for the same reason PD's own tests shorten it: the arithmetic is PD's, and
/// what a node must not do is hold an opinion of its own about the number.
pub const STEP_MS: u64 = 300;
/// The retention term a *removing* step waits on top of the interval.
pub const REMOVAL_EXTRA_MS: u64 = 0;

/// The placement driver these tests point a SQL node at.
#[derive(Debug)]
pub struct StandInPd {
    /// Every columnar report it has been sent, in order. The whole set each time: a report is a
    /// full assertion, so what is recorded here is what PD would have replaced its record with.
    reports: Mutex<Vec<Vec<ColumnarWish>>>,
    /// How long a lease this driver hands out. Per driver, because the two things a test can want
    /// from a lease are opposite: one watches it lapse, the others must not.
    lease_ms: u64,
    /// **The next timestamp**, and the whole reason one driver serves several nodes: this counter
    /// is what makes two nodes' transactions orderable against each other (`CLAUDE.md`
    /// invariant 6).
    next_ts: std::sync::atomic::AtomicU64,
}

impl Default for StandInPd {
    fn default() -> Self {
        StandInPd {
            reports: Mutex::new(Vec::new()),
            lease_ms: LEASE_MS,
            // Above zero, because a timestamp is never zero (`docs/txn-spec.md` §5.5) and a test
            // that started at it would be exercising a value the protocol excludes.
            next_ts: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl StandInPd {
    /// Every report, oldest first.
    pub fn reports(&self) -> Vec<Vec<ColumnarWish>> {
        self.reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The most recent report, which is the whole set PD would be acting on.
    pub fn last_report(&self) -> Option<Vec<ColumnarWish>> {
        self.reports().last().cloned()
    }
}

impl Service for StandInPd {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        Box::pin(async move {
            let Request::Pd { request, .. } = request else {
                return Err(ProtoError::invalid(
                    "this stand-in only answers Pd requests",
                ));
            };
            let response = match request {
                PdReq::SchemaLease => PdResp::SchemaLease {
                    lease_ms: self.lease_ms,
                    step_interval_ms: STEP_MS,
                    removal_extra_ms: REMOVAL_EXTRA_MS,
                },
                PdReq::ReportColumnar { wishes } => {
                    self.reports
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(wishes);
                    PdResp::ReportColumnar
                }
                // **The third method, added 2026-09-10.** A SQL node used to have exactly two,
                // and a node that asked for a timestamp was asking for something it had no
                // business asking — because it kept its own counter, which is the defect
                // `tests/two_nodes_one_clock.rs` pins. It now asks the driver, so `Tso` belongs
                // here; anything beyond these three is still a vocabulary that belongs to a store.
                PdReq::Tso { count } => PdResp::Tso {
                    start_ts: self
                        .next_ts
                        .fetch_add(u64::from(count.max(1)), std::sync::atomic::Ordering::SeqCst),
                    count: count.max(1),
                },
                other => {
                    return Err(ProtoError::invalid(format!(
                        "a SQL node sent {}, which is not one of its two methods",
                        other.method().name()
                    )));
                }
            };
            Ok(Reply::Unary(Response::Pd(response)))
        })
    }

    /// PD is not a store, and zero is not a store id anywhere in this codebase.
    fn store_id(&self) -> u64 {
        0
    }
}

/// Starts one on an ephemeral port, handing out [`LEASE_MS`] leases.
pub async fn serve() -> (Arc<StandInPd>, ServerHandle, std::net::SocketAddr) {
    serve_with_lease(LEASE_MS).await
}

/// The same, with the lease this test needs — [`NO_LAPSE_MS`] for one that is not about leases.
pub async fn serve_with_lease(
    lease_ms: u64,
) -> (Arc<StandInPd>, ServerHandle, std::net::SocketAddr) {
    let pd = Arc::new(StandInPd {
        lease_ms,
        ..StandInPd::default()
    });
    let server = Server::bind(
        "127.0.0.1:0",
        Arc::clone(&pd) as Arc<dyn Service>,
        esker_proto::TransportConfig::new(),
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    (pd, server.spawn().unwrap(), address)
}
