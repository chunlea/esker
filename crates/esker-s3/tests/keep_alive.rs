//! The transport keeps its connection, and knows when it may not.
//!
//! `docs/bench/phase-6b.md` §3 measured 692 µs for one cold ranged `GET` over loopback and said
//! where it goes: a fresh TCP handshake per request. The saving is invisible in a response — the
//! bytes are identical either way — so what is asserted here is
//! [`TcpTransport::connections_opened`], against a server that counts what it accepts.
//!
//! The three shapes are the three a server can produce:
//!
//! 1. a well-behaved HTTP/1.1 endpoint, which one connection must carry all of;
//! 2. one that says `Connection: close`, which must not be reused even though it is still open;
//! 3. one that closes an idle connection without saying so, which is a server's idle timeout and
//!    is the case a pool gets wrong if it trusts what it holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use esker_s3::transport::{TcpTransport, Timeouts, Transport};

/// What a server does after answering a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// Keep the connection and wait for the next request.
    Keep,
    /// Answer with `Connection: close` and close.
    Close,
    /// Answer as if keeping it, then close anyway — a server's idle timeout, which announces
    /// nothing.
    KeepThenDropIt,
}

/// A server that speaks just enough HTTP to answer, and counts what it accepts.
struct Server {
    address: SocketAddr,
    accepted: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn start(after: After) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&accepted);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // Checked **before** counting, so the sentinel connect that unblocks this loop at
                // shutdown is not mistaken for a request. A count that included it would make
                // every assertion here one too many.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                counter.fetch_add(1, Ordering::Relaxed);
                serve(stream, after);
            }
        });
        Self {
            address,
            accepted,
            stopping,
            handle: Some(handle),
        }
    }

    fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The flag, then a connect to unblock `incoming()` — the loop reads the flag and ends.
        // Without both, `join` waits on a thread parked in `accept` for ever, which is exactly
        // what this test file did on its first run.
        self.stopping.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Answers every request on one connection until the peer goes away, or until `after` says stop.
fn serve(mut stream: TcpStream, after: After) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // The requests here have no body, so the blank line ends one.
        while find(&buffer, b"\r\n\r\n").is_none() {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let end = find(&buffer, b"\r\n\r\n").unwrap() + 4;
        buffer.drain(..end);

        let body = b"hello";
        let head = match after {
            After::Close => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            After::Keep | After::KeepThenDropIt => {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
            }
        };
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(body).is_err() {
            return;
        }
        let _ = stream.flush();
        if after != After::Keep {
            return;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn transport() -> TcpTransport {
    TcpTransport::with_timeouts(Timeouts {
        connect: std::time::Duration::from_secs(2),
        io: std::time::Duration::from_secs(2),
    })
}

fn get(transport: &TcpTransport, address: SocketAddr) -> Vec<u8> {
    let request = b"GET /esker/block HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
    let response = transport
        .round_trip(&address.ip().to_string(), address.port(), request)
        .unwrap();
    assert_eq!(response.status, 200);
    response.body
}

/// **The unit.** Ten requests, one connection.
#[test]
fn ten_requests_share_one_connection() {
    let server = Server::start(After::Keep);
    let transport = transport();
    for _ in 0..10 {
        assert_eq!(get(&transport, server.address), b"hello");
    }
    assert_eq!(
        transport.connections_opened(),
        1,
        "ten requests opened {} connections",
        transport.connections_opened(),
    );
    assert_eq!(server.accepted(), 1, "the server saw more than one connect");
}

/// A server that says `Connection: close` is believed, even though the socket is still open at the
/// instant the answer is read.
#[test]
fn a_server_that_says_close_gets_a_new_connection_each_time() {
    let server = Server::start(After::Close);
    let transport = transport();
    for _ in 0..5 {
        assert_eq!(get(&transport, server.address), b"hello");
    }
    assert_eq!(
        transport.connections_opened(),
        5,
        "a connection the server closed was handed out again",
    );
}

/// **The idle timeout**, which announces nothing: the server answers as if keeping the connection
/// and then drops it. A pool that trusts what it holds writes into a socket the peer has finished
/// with; this one looks first.
#[test]
fn a_connection_the_peer_dropped_is_replaced_rather_than_used() {
    let server = Server::start(After::KeepThenDropIt);
    let transport = transport();

    assert_eq!(get(&transport, server.address), b"hello");
    // The peer's FIN has to arrive before the next request, or this test proves nothing about the
    // check — it would be testing a race. One connect's worth of loopback is plenty.
    for _ in 0..100 {
        if server.accepted() >= 1 {
            break;
        }
        std::thread::yield_now();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert_eq!(
        get(&transport, server.address),
        b"hello",
        "the second request went into a socket the peer had closed",
    );
    assert_eq!(transport.connections_opened(), 2);
}

/// Closing the idle connections leaves the transport working, from a fresh one.
#[test]
fn closing_the_idle_connections_costs_only_a_reconnect() {
    let server = Server::start(After::Keep);
    let transport = transport();
    assert_eq!(get(&transport, server.address), b"hello");
    transport.close_idle();
    assert_eq!(get(&transport, server.address), b"hello");
    assert_eq!(transport.connections_opened(), 2);
    assert_eq!(server.accepted(), 2);
}
