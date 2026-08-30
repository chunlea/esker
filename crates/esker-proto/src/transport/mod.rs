//! The runtime side: `tokio` TCP, a per-connection writer task, a demultiplexer keyed by
//! request id, ping/pong keepalive and chunked streams (`docs/DESIGN.md` §9).
//!
//! This is the **only** async code in the project. `CLAUDE.md` puts the runtime at the network
//! edge and nowhere else: `esker-engine` and `esker-raft` are synchronous, and a caller that
//! reaches them from here does it through `spawn_blocking` so that an `fsync` never stalls the
//! reactor.
//!
//! # What a connection guarantees
//!
//! * **One version negotiation, on connect.** The first frame is a [`crate::Hello`]; a
//!   mismatch is [`ProtoError::WireVersion`] and the connection closes. There is no downgrade.
//! * **Request ids are client-assigned and unique while in flight.** A duplicate is
//!   [`ProtoError::DuplicateRequestId`], never a silently replaced waiter — replacing one
//!   leaves the first caller waiting for a response that can never arrive.
//! * **Nothing is unbounded.** The writer queue, the in-flight table and the stream channels
//!   all have limits, and a peer at its limit answers [`ProtoError::ServerIsBusy`] rather than
//!   growing until it is killed.
//! * **A failure says whether the request may have applied.** [`ProtoError::NotSent`] means the
//!   bytes provably never left; [`ProtoError::Closed`] and [`ProtoError::Timeout`] mean they
//!   may have. See [`crate::RequestOutcome`].
//! * **A dead peer is noticed.** A connection with nothing on it is pinged every
//!   `keepalive_interval`, and one silent for `idle_timeout` is dropped with every waiter
//!   failed. Nothing waits for ever.

mod client;
mod conn;
mod server;

use std::time::Duration;

pub use client::{BlockingTransport, StreamResponse, TcpTransport};
pub use conn::BoxFuture;
pub use server::{ChunkSender, ChunkStream, Reply, Server, ServerHandle, Service};

use crate::{MAX_FRAME_SIZE, ProtoError, Request, Response};

/// The knobs a connection is built with (`docs/DESIGN.md` §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportConfig {
    /// Largest frame accepted or sent, counting the length field.
    pub max_frame_size: usize,
    /// Requests that may be outstanding on one connection at once. Past it, a caller gets
    /// [`ProtoError::ServerIsBusy`] and a server sheds the request instead of queueing it.
    pub max_in_flight: usize,
    /// Encoded frames the writer task will hold before a sender waits. This is the
    /// backpressure that stops a slow socket from becoming an unbounded queue.
    pub write_queue: usize,
    /// How long a connection may be silent before it is pinged.
    pub keepalive_interval: Duration,
    /// How long a connection may be silent before it is declared dead.
    pub idle_timeout: Duration,
    /// How long [`Transport::call`] waits before giving up on a response.
    pub request_timeout: Duration,
    /// How long a shutdown waits for in-flight requests before closing anyway.
    ///
    /// Graceful cannot mean "for ever". A handler wedged on a stuck disk would otherwise hold
    /// the process open past any patience, so the drain is bounded and what is still running
    /// when it expires is abandoned — with a log line saying how much.
    pub shutdown_grace: Duration,
}

impl TransportConfig {
    /// The defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_frame_size: MAX_FRAME_SIZE,
            // Above the 1,000 concurrent requests the phase's loopback test drives, so that
            // the limit is a limit rather than a thing normal use runs into.
            max_in_flight: 4096,
            write_queue: 256,
            keepalive_interval: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(10),
        }
    }

    /// How many keepalive intervals of silence are tolerated before the peer is dead.
    ///
    /// At least one, so a misconfiguration that makes the timeout shorter than the interval
    /// gives a peer one chance to answer rather than killing every connection immediately.
    #[must_use]
    pub fn missed_keepalives(&self) -> u32 {
        let interval = self.keepalive_interval.max(Duration::from_millis(1));
        u32::try_from(self.idle_timeout.as_millis() / interval.as_millis().max(1))
            .unwrap_or(u32::MAX)
            .max(1)
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Something that can carry a request to a peer and bring back its answer.
///
/// Dyn-compatible on purpose. The client holds one per store behind an `Arc` and swaps in a
/// scripted fake for its retry tests; an `async fn` in trait would forbid both. A synchronous
/// caller — the CLI, the benchmark driver — uses [`BlockingTransport`] instead of this.
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// Sends `request` and waits for its answer, up to the configured request timeout.
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response, ProtoError>>;

    /// The largest frame this transport will send, so a caller can refuse an oversized request
    /// without spending a round trip discovering it.
    fn max_frame_size(&self) -> usize;

    /// Sends `request` and waits until `deadline`.
    ///
    /// A deadline that passes is [`ProtoError::Timeout`], whose outcome is
    /// [`crate::RequestOutcome::Unknown`]: the request went out, and giving up on the answer
    /// says nothing about whether the peer applied it.
    fn call_with_deadline(
        &self,
        request: Request,
        deadline: std::time::Instant,
    ) -> BoxFuture<'_, Result<Response, ProtoError>> {
        let call = self.call(request);
        Box::pin(async move {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), call).await {
                Ok(result) => result,
                Err(_elapsed) => Err(ProtoError::Timeout {
                    detail: "the deadline passed before the peer answered".to_owned(),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TransportConfig;
    use std::time::Duration;

    /// The loopback test drives a thousand concurrent requests. If the default limit were
    /// below that, the test would be measuring the limit rather than the demultiplexer.
    #[test]
    fn the_default_in_flight_limit_is_above_normal_use() {
        assert!(TransportConfig::new().max_in_flight > 1_000);
    }

    #[test]
    fn a_peer_gets_several_chances_before_it_is_declared_dead() {
        assert!(TransportConfig::new().missed_keepalives() >= 2);
    }

    /// A configuration whose timeout is shorter than its interval must still give the peer one
    /// chance, rather than killing every connection on the first tick.
    #[test]
    fn an_inverted_keepalive_configuration_still_allows_one_probe() {
        let config = TransportConfig {
            keepalive_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(1),
            ..TransportConfig::new()
        };
        assert_eq!(config.missed_keepalives(), 1);
    }

    /// A zero interval would divide by zero on the way to the answer.
    #[test]
    fn a_zero_keepalive_interval_does_not_divide_by_zero() {
        let config = TransportConfig {
            keepalive_interval: Duration::ZERO,
            ..TransportConfig::new()
        };
        assert!(config.missed_keepalives() >= 1);
    }
}
