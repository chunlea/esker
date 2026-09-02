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
//!
//! # Why the connection is kept
//!
//! [ADR 0039](../../../docs/adr/0039-a-kept-alive-s3-connection.md). A cold tiered read is one
//! ranged `GET` per block, and `docs/bench/phase-6b.md` §3 measured 692 µs for one of them over
//! loopback — which is not the network, it is a fresh TCP handshake per request. [`TcpTransport`]
//! now keeps idle connections and hands one back out, per endpoint.
//!
//! Two rules keep that from turning a saving into a wrong answer:
//!
//! * **a connection goes back in the pool only after a framed response was read whole.** Never
//!   mid-body, never after an error, never when the answer said `Connection: close` — so what is
//!   in the pool is always a socket at a message boundary;
//! * **the transport still does not retry.** ADR 0024 decision 2 puts retrying in
//!   [`crate::client::S3Client`], because only it knows whether an operation is idempotent. What
//!   this does instead is *check before reusing*: an idle connection the peer has finished with is
//!   detected by a non-blocking peek and dropped, so the ordinary stale-connection case never
//!   becomes a failed request at all. The rare loss — a peer that closes between the peek and the
//!   write — is an `Error::Io`, which is retryable, and the layer that owns idempotency decides.

use std::fmt;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::http::{Response, read_response};

/// How many idle connections are kept per endpoint.
///
/// One per thread that reads blocks, with headroom. Above this an idle connection is closed rather
/// than kept, because a pool that grows without a bound is a file-descriptor leak wearing a hat.
const MAX_IDLE: usize = 16;

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

/// Plain HTTP over a pool of kept-alive TCP connections, one pool per endpoint.
#[derive(Debug, Default)]
pub struct TcpTransport {
    /// The timeouts every connection gets.
    pub timeouts: Timeouts,
    /// Connections at a message boundary, waiting for the next request.
    idle: Mutex<Vec<Idle>>,
    /// How many TCP connections this transport has opened, ever.
    ///
    /// The observable the keep-alive is asserted against: reuse is invisible from the responses,
    /// which are the same responses either way.
    opened: AtomicU64,
}

/// One pooled connection and the endpoint it belongs to.
#[derive(Debug)]
struct Idle {
    host: String,
    port: u16,
    stream: TcpStream,
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
        Self {
            timeouts,
            ..Self::default()
        }
    }

    /// How many TCP connections this transport has opened since it was created.
    ///
    /// With keep-alive working this is far below the number of requests; without it the two are
    /// equal. Nothing branches on it — it is here so a test and an operator can see the difference,
    /// which no response reveals.
    #[must_use]
    pub fn connections_opened(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }

    /// Closes every idle connection, keeping none.
    pub fn close_idle(&self) {
        self.pool().clear();
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, Vec<Idle>> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A pooled connection to `host:port` that still looks alive, if there is one.
    ///
    /// Newest first: a connection that was used a moment ago is the one least likely to have been
    /// closed by the server's idle timeout, so taking from the end keeps the survivors busy and
    /// lets the stale ones fall out.
    fn take_idle(&self, host: &str, port: u16) -> Option<TcpStream> {
        loop {
            let stream = {
                let mut pool = self.pool();
                let at = pool
                    .iter()
                    .rposition(|idle| idle.port == port && idle.host == host)?;
                pool.remove(at).stream
            };
            if is_alive(&stream) {
                return Some(stream);
            }
            tracing::trace!(
                host,
                port,
                "an idle S3 connection had been closed by the peer"
            );
        }
    }

    /// Puts a connection back at a message boundary.
    fn put_idle(&self, host: &str, port: u16, stream: TcpStream) {
        let mut pool = self.pool();
        if pool.len() >= MAX_IDLE {
            return;
        }
        pool.push(Idle {
            host: host.to_string(),
            port,
            stream,
        });
    }

    /// Opens and configures a fresh connection.
    fn connect(&self, host: &str, port: u16) -> Result<TcpStream> {
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
        let Some(stream) = stream else {
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
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(stream)
    }
}

impl Transport for TcpTransport {
    fn round_trip(&self, host: &str, port: u16, request: &[u8]) -> Result<Response> {
        let mut stream = match self.take_idle(host, port) {
            Some(kept) => kept,
            None => self.connect(host, port)?,
        };

        // A failure anywhere below drops the connection rather than pooling it: the socket's
        // position in the message stream is no longer known, and a socket at an unknown position
        // is the one thing that must never come back out of the pool.
        stream
            .write_all(request)
            .map_err(|source| Error::io("sending the request", source))?;
        stream
            .flush()
            .map_err(|source| Error::io("sending the request", source))?;

        let response = read_response(&mut stream)?;
        if response.may_reuse_connection() {
            self.put_idle(host, port, stream);
        }
        Ok(response)
    }
}

/// Whether an idle connection still looks usable.
///
/// A non-blocking peek, which is the only question a socket can be asked without spending a round
/// trip on it: `WouldBlock` means the peer has said nothing, which for an idle HTTP connection is
/// exactly right. Anything else — an orderly close (`Ok(0)`), bytes nobody asked for, or an error —
/// means this connection is not at the boundary the pool claims, and it goes.
///
/// This is not a guarantee and cannot be one: the peer may close between the peek and the write.
/// It converts the *common* stale-connection case, a server's idle timeout, from a failed request
/// into a new connection, and leaves the rare one to the retry policy that owns it.
fn is_alive(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let alive = matches!(stream.peek(&mut byte), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock);
    if stream.set_nonblocking(false).is_err() {
        return false;
    }
    alive
}
