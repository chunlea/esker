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
/// PD's `lease_ms + lock_ttl_ms`, as PD computes it — the wait between the states of a schema
/// change. Short here for the same reason PD's own tests shorten it: the arithmetic is PD's, and
/// what a node must not do is hold an opinion of its own about the number.
pub const STEP_MS: u64 = 300;
/// The retention term a *removing* step waits on top of the interval.
pub const REMOVAL_EXTRA_MS: u64 = 0;

/// The placement driver these tests point a SQL node at.
#[derive(Debug, Default)]
pub struct StandInPd {
    /// Every columnar report it has been sent, in order. The whole set each time: a report is a
    /// full assertion, so what is recorded here is what PD would have replaced its record with.
    reports: Mutex<Vec<Vec<ColumnarWish>>>,
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
                    lease_ms: LEASE_MS,
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
                // A SQL node has exactly two methods, and sending a third would mean this node
                // had grown a vocabulary that belongs to a store.
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

/// Starts one on an ephemeral port.
pub async fn serve() -> (Arc<StandInPd>, ServerHandle, std::net::SocketAddr) {
    let pd = Arc::new(StandInPd::default());
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
