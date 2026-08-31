//! The one place that touches a socket.
//!
//! [`Transport`] is a single method, and that is the point: `docs/DESIGN.md` §13 asked that the
//! transport be a trait so the TLS decision stays local to one module, and
//! [ADR 0025](../../../docs/adr/0025-s3-transport-and-tls.md) then chose plain HTTP for the
//! first milestone. When TLS lands it is a second implementor here, not a change anywhere else.
//!
//! # Why blocking
//!
//! `CLAUDE.md` puts async at the network edge and keeps the engine synchronous and `std`-only.
//! The uploader runs on the engine's side of that line, so it blocks — on its own thread,
//! where blocking is what threads are for. A `tokio` transport is a legitimate second
//! implementor for a caller that already has a runtime; it is not this one.

use std::fmt;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::http::{Response, read_response};

/// How long to wait for a connection, and then for each read or write.
///
/// Separate knobs because they fail differently: a connect timeout catches an endpoint that is
/// not there, and an I/O timeout catches one that accepted and then stopped talking. The second
/// is the one that would otherwise hang an uploader thread forever.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Time allowed to establish the TCP connection.
    pub connect: Duration,
    /// Time allowed for any single read or write once connected.
    pub io: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            io: Duration::from_secs(30),
        }
    }
}

/// Sends one request and reads one response.
///
/// Implementations must not retry: retrying is [`crate::client::S3Client`]'s job, because only
/// it knows whether the operation is idempotent and only it holds the backoff state
/// ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 2).
pub trait Transport: Send + Sync + fmt::Debug {
    /// Connects to `host:port`, writes `request`, and reads the response.
    fn round_trip(&self, host: &str, port: u16, request: &[u8]) -> Result<Response>;
}

/// Plain HTTP over a fresh TCP connection per request.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpTransport {
    /// The timeouts every connection gets.
    pub timeouts: Timeouts,
}

impl TcpTransport {
    /// A transport with the default timeouts.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A transport with explicit timeouts.
    #[must_use]
    pub fn with_timeouts(timeouts: Timeouts) -> Self {
        Self { timeouts }
    }
}

impl Transport for TcpTransport {
    fn round_trip(&self, host: &str, port: u16, request: &[u8]) -> Result<Response> {
        let target = (host, port);
        // Resolution can return several addresses — a host with both an A and a AAAA record is
        // ordinary — and connecting to the first one that answers is the whole of our
        // happy-eyeballs story. The last error is the one reported.
        let addresses: Vec<_> = target
            .to_socket_addrs()
            .map_err(|source| Error::io("resolving the endpoint", source))?
            .collect();
        if addresses.is_empty() {
            return Err(Error::Config(format!(
                "{host}:{port} resolved to no address"
            )));
        }

        let mut last = None;
        let mut stream = None;
        for address in &addresses {
            match TcpStream::connect_timeout(address, self.timeouts.connect) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(err) => last = Some(err),
            }
        }
        let Some(mut stream) = stream else {
            let err = last.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address answered")
            });
            return Err(Error::io("connecting to the endpoint", err));
        };

        stream
            .set_read_timeout(Some(self.timeouts.io))
            .and_then(|()| stream.set_write_timeout(Some(self.timeouts.io)))
            // Small writes are the request head; a delayed ACK plus Nagle would add 40 ms to
            // every call for no benefit, since we write the whole request in one go anyway.
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|source| Error::io("configuring the connection", source))?;

        stream
            .write_all(request)
            .map_err(|source| Error::io("sending the request", source))?;
        stream
            .flush()
            .map_err(|source| Error::io("sending the request", source))?;

        read_response(&mut stream)
    }
}
