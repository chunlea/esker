//! The transport over a real socket on an ephemeral port.
//!
//! Everything here runs against a `Server` bound to `127.0.0.1:0` with a test service behind
//! it, so the frames really do go through the kernel. The headline case is the one the phase
//! prompt asks for: a thousand requests in flight at once, answered in a shuffled order, every
//! one of which must come back to its own caller.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_base::rng::Pcg32;
use esker_proto::messages::HelloAck;
use esker_proto::transport::{ChunkStream, Reply, Server, ServerHandle, Service, TransportConfig};
use esker_proto::{
    BoxFuture, Epoch, Frame, FrameKind, ProtoError, RawKvReq, RawKvResp, Request, RequestHeader,
    RequestOutcome, Response, TcpTransport, Transport, WIRE_VERSION,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// A service that answers what it is asked, with the delays a test needs.
///
/// `Get` echoes the key back as the value, which is what lets a caller check that the answer
/// it received is the answer to the question it asked. `Scan` replies with a stream.
#[derive(Debug)]
struct Echo {
    /// Milliseconds each request sleeps before answering, drawn per request so replies come
    /// back in an order unrelated to the order they were asked.
    jitter: std::sync::Mutex<Pcg32>,
    /// How many requests have been handled.
    served: AtomicUsize,
    /// Requests that must never finish, so a caller can hold the in-flight table full.
    block: Option<Arc<tokio::sync::Semaphore>>,
}

impl Echo {
    fn new(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            jitter: std::sync::Mutex::new(Pcg32::from_seed(seed)),
            served: AtomicUsize::new(0),
            block: None,
        })
    }

    fn blocking(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            jitter: std::sync::Mutex::new(Pcg32::from_seed(seed)),
            served: AtomicUsize::new(0),
            block: Some(Arc::new(tokio::sync::Semaphore::new(0))),
        })
    }
}

impl Service for Echo {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        Box::pin(async move {
            self.served.fetch_add(1, Ordering::Relaxed);
            let Request::RawKv { request, .. } = request else {
                return Err(ProtoError::invalid("the echo service only speaks RawKv"));
            };

            if let Some(block) = &self.block {
                // Never granted: the caller uses this to fill the in-flight table.
                let _ = block.acquire().await;
            }

            match request {
                RawKvReq::Get { key } => {
                    let delay = {
                        let mut rng = self.jitter.lock().unwrap();
                        u64::from(rng.below(8))
                    };
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    Ok(Reply::Unary(Response::RawKv(RawKvResp::Get {
                        value: Some(key),
                    })))
                }
                RawKvReq::Scan { limit, .. } => {
                    let (sender, stream) = ChunkStream::channel(2);
                    tokio::spawn(async move {
                        for index in 0..limit {
                            let chunk = Bytes::from(format!("chunk-{index}"));
                            if sender.send(chunk).await.is_err() {
                                return;
                            }
                        }
                    });
                    Ok(Reply::Stream(stream))
                }
                RawKvReq::Put { .. } => Err(ProtoError::ServerIsBusy {
                    reason: "the echo service refuses writes".to_owned(),
                }),
                other => Err(ProtoError::Unsupported {
                    detail: format!("{:?}", other.method()),
                }),
            }
        })
    }

    fn store_id(&self) -> u64 {
        7
    }
}

async fn serve(service: Arc<dyn Service>, config: TransportConfig) -> ServerHandle {
    Server::bind("127.0.0.1:0", service, config)
        .await
        .unwrap()
        .spawn()
        .unwrap()
}

/// A configuration whose shutdown does not wait long.
///
/// The tests below deliberately wedge a handler for ever, which is exactly the case the
/// bounded drain exists for; without a short grace each of them would sit out the default ten
/// seconds.
fn impatient() -> TransportConfig {
    TransportConfig {
        shutdown_grace: Duration::from_millis(50),
        ..TransportConfig::new()
    }
}

fn header() -> RequestHeader {
    RequestHeader::new(1, Epoch::INITIAL, 1)
}

fn get(key: &str) -> Request {
    Request::raw_kv(header(), RawKvReq::get(Bytes::from(key.to_owned())))
}

/// The headline case: a thousand requests in flight at once, answered in an order unrelated to
/// the order they were asked, every one routed to the caller that asked it.
///
/// The service echoes the key back as the value, so a misrouted answer is not just a count
/// being wrong — it is a caller receiving another caller's data, which is what the
/// demultiplexer exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_concurrent_requests_each_reach_their_own_caller() {
    let server = serve(Echo::new(0xE5_1234), TransportConfig::new()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let mut calls = Vec::with_capacity(1_000);
    for index in 0..1_000u32 {
        let transport = transport.clone();
        calls.push(tokio::spawn(async move {
            let key = format!("key-{index}");
            let response = transport.call(get(&key)).await?;
            match response.into_raw_kv()? {
                RawKvResp::Get { value } => Ok::<_, ProtoError>((key, value)),
                other => Err(ProtoError::invalid(format!("{other:?}"))),
            }
        }));
    }

    for call in calls {
        let (key, value) = call.await.unwrap().unwrap();
        assert_eq!(
            value,
            Some(Bytes::from(key.clone())),
            "request `{key}` was answered with someone else's value"
        );
    }

    assert_eq!(transport.in_flight(), 0, "a waiter was left in the table");
    server.shutdown().await.unwrap();
}

/// Request ids belong to the client, so a duplicate is the client's bug — and it has to be
/// told, because the alternative is replacing a waiter that then never hears anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_request_id_is_refused_while_the_first_is_in_flight() {
    let server = serve(Echo::blocking(1), impatient()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let held = transport.clone();
    let first = tokio::spawn(async move { held.call_with_id(99, get("first")).await });
    // Wait until the first call is registered rather than guessing at a sleep.
    while transport.in_flight() == 0 {
        tokio::task::yield_now().await;
    }

    let error = transport
        .call_with_id(99, get("second"))
        .await
        .expect_err("the duplicate was accepted");
    assert_eq!(error, ProtoError::DuplicateRequestId { request_id: 99 });
    assert_eq!(
        error.outcome(),
        RequestOutcome::NotApplied,
        "a refused request never went out"
    );

    first.abort();
    server.shutdown().await.unwrap();
}

/// The bound is a bound. Past it a caller is told the connection is busy, which is retryable,
/// rather than being added to a queue with no end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_flight_limit_sheds_load_instead_of_queueing() {
    let config = TransportConfig {
        max_in_flight: 2,
        ..impatient()
    };
    let server = serve(Echo::blocking(2), config).await;
    let transport = TcpTransport::connect_with(server.local_addr(), config)
        .await
        .unwrap();

    let mut held = Vec::new();
    for index in 0..2 {
        let transport = transport.clone();
        held.push(tokio::spawn(async move {
            transport.call(get(&format!("held-{index}"))).await
        }));
    }
    while transport.in_flight() < 2 {
        tokio::task::yield_now().await;
    }

    let error = transport
        .call(get("one too many"))
        .await
        .expect_err("the limit did not hold");
    assert!(
        matches!(error, ProtoError::ServerIsBusy { .. }),
        "expected ServerIsBusy, got {error:?}"
    );
    assert!(error.is_retryable());
    assert_eq!(error.outcome(), RequestOutcome::NotApplied);
    assert_eq!(transport.in_flight(), 2, "the table grew past its limit");

    for call in held {
        call.abort();
    }
    server.shutdown().await.unwrap();
}

/// The frame kinds phase 4 needs for snapshot transfer, exercised now so they are not written
/// for the first time under a snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_answer_arrives_in_order_and_ends_at_stream_end() {
    let server = serve(Echo::new(3), TransportConfig::new()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let request = Request::raw_kv(header(), RawKvReq::scan(&b""[..], &b""[..], 32));
    let mut stream = transport.call_stream(request).await.unwrap();

    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        chunks.push(chunk.unwrap());
    }
    let expected: Vec<Bytes> = (0..32).map(|i| Bytes::from(format!("chunk-{i}"))).collect();
    assert_eq!(chunks, expected, "a stream arrived out of order or short");
    assert_eq!(transport.in_flight(), 0, "the stream stayed registered");

    server.shutdown().await.unwrap();
}

/// A stream with no chunks still ends, or a receiver would wait for ever on an empty snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_stream_still_ends() {
    let server = serve(Echo::new(4), TransportConfig::new()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let request = Request::raw_kv(header(), RawKvReq::scan(&b""[..], &b""[..], 0));
    let stream = transport.call_stream(request).await.unwrap();
    assert!(stream.collect().await.unwrap().is_empty());

    server.shutdown().await.unwrap();
}

/// Version negotiation happens once, at connect, and a mismatch fails there — not at the first
/// request that mattered (`docs/DESIGN.md` §9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_at_the_wrong_wire_version_is_refused_at_connect() {
    let server = serve(Echo::new(5), TransportConfig::new()).await;

    // Hand-rolled, because a `TcpTransport` cannot be made to speak the wrong version.
    let mut socket = TcpStream::connect(server.local_addr()).await.unwrap();
    let hello = Request::Hello(esker_proto::Hello {
        version: WIRE_VERSION + 1,
    });
    let frame = Frame::new(FrameKind::Request, 1, Bytes::from(hello.encode()));
    socket
        .write_all(&frame.encode(esker_proto::MAX_FRAME_SIZE).unwrap())
        .await
        .unwrap();

    let mut buffer = bytes::BytesMut::new();
    let mut decoder = esker_proto::FrameDecoder::new(esker_proto::MAX_FRAME_SIZE);
    loop {
        tokio::io::AsyncReadExt::read_buf(&mut socket, &mut buffer)
            .await
            .unwrap();
        decoder.push(&buffer.split());
        if let Some(frame) = decoder.next_frame().unwrap() {
            assert_eq!(frame.kind, FrameKind::Error);
            assert_eq!(
                ProtoError::decode(&frame.body).unwrap(),
                ProtoError::WireVersion {
                    expected: WIRE_VERSION,
                    actual: WIRE_VERSION + 1,
                }
            );
            break;
        }
    }

    server.shutdown().await.unwrap();
}

/// The handshake is not optional: a connection that starts talking without one is refused, so
/// a peer can never skip the one exchange that establishes what the other speaks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_before_the_handshake_is_refused() {
    let server = serve(Echo::new(6), TransportConfig::new()).await;

    let mut socket = TcpStream::connect(server.local_addr()).await.unwrap();
    let frame = Frame::new(FrameKind::Request, 1, Bytes::from(get("early").encode()));
    socket
        .write_all(&frame.encode(esker_proto::MAX_FRAME_SIZE).unwrap())
        .await
        .unwrap();

    let mut buffer = bytes::BytesMut::new();
    let mut decoder = esker_proto::FrameDecoder::new(esker_proto::MAX_FRAME_SIZE);
    loop {
        tokio::io::AsyncReadExt::read_buf(&mut socket, &mut buffer)
            .await
            .unwrap();
        decoder.push(&buffer.split());
        if let Some(frame) = decoder.next_frame().unwrap() {
            assert_eq!(frame.kind, FrameKind::Error);
            let error = ProtoError::decode(&frame.body).unwrap();
            assert!(
                matches!(error, ProtoError::InvalidRequest { .. }),
                "{error:?}"
            );
            break;
        }
    }

    server.shutdown().await.unwrap();
}

/// A typed error from the service reaches the caller as that error, with its redirect
/// information, rather than as a string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_error_survives_the_round_trip() {
    let server = serve(Echo::new(7), TransportConfig::new()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let request = Request::raw_kv(header(), RawKvReq::put(&b"k"[..], &b"v"[..]));
    let error = transport
        .call(request)
        .await
        .expect_err("the put succeeded");
    assert_eq!(
        error,
        ProtoError::ServerIsBusy {
            reason: "the echo service refuses writes".to_owned()
        }
    );
    assert!(error.is_retryable());

    // And the connection carries on afterwards: an error ends a request, not a connection.
    let response = transport.call(get("after")).await.unwrap();
    assert_eq!(
        response.into_raw_kv().unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"after"))
        }
    );

    server.shutdown().await.unwrap();
}

/// The handshake tells a client what the peer will accept, so an oversized request can be
/// refused before it costs a round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handshake_reports_the_store_and_its_frame_limit() {
    let config = TransportConfig {
        max_frame_size: 64 * 1024,
        ..TransportConfig::new()
    };
    let server = serve(Echo::new(8), config).await;
    let transport = TcpTransport::connect_with(server.local_addr(), config)
        .await
        .unwrap();

    assert_eq!(
        transport.hello_ack(),
        HelloAck {
            version: WIRE_VERSION,
            store_id: 7,
            max_frame_size: 64 * 1024,
        }
    );
    assert_eq!(transport.max_frame_size(), 64 * 1024);

    server.shutdown().await.unwrap();
}

/// A request too big for the connection never reaches the socket, so the caller is told
/// `NotSent` and may safely send something else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_request_is_refused_before_it_is_sent() {
    let config = TransportConfig {
        max_frame_size: 1024,
        ..TransportConfig::new()
    };
    let server = serve(Echo::new(9), config).await;
    let transport = TcpTransport::connect_with(server.local_addr(), config)
        .await
        .unwrap();

    let huge = Request::raw_kv(header(), RawKvReq::get(Bytes::from(vec![b'k'; 4096])));
    let error = transport.call(huge).await.expect_err("the frame went out");
    assert_eq!(
        error.outcome(),
        RequestOutcome::NotApplied,
        "an unsent request cannot have applied"
    );
    assert_eq!(transport.in_flight(), 0, "a waiter was left behind");

    // The connection is unharmed: a refused encode is a caller error, not a broken socket.
    assert!(transport.call(get("still here")).await.is_ok());
    server.shutdown().await.unwrap();
}

/// A connection that dies with a request in flight is *ambiguous*, and the client is told so:
/// the store may have applied the request before it went away
/// ([`RequestOutcome::Unknown`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_dies_mid_request_is_ambiguous() {
    let server = serve(Echo::blocking(10), impatient()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let calling = {
        let transport = transport.clone();
        tokio::spawn(async move { transport.call(get("in flight")).await })
    };
    while transport.in_flight() == 0 {
        tokio::task::yield_now().await;
    }

    server.shutdown().await.unwrap();
    let error = calling
        .await
        .unwrap()
        .expect_err("the request was answered");
    assert_eq!(
        error.outcome(),
        RequestOutcome::Unknown,
        "a request that went out and was never answered is ambiguous, got {error:?}"
    );
    assert!(
        !error.is_retryable(),
        "an ambiguous write must not auto-retry"
    );
}

/// A connection to nothing fails as `NotSent`, which is the one failure a client may safely
/// retry with a write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_connection_is_not_sent() {
    // Bind and drop, so the port is almost certainly closed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let error = TcpTransport::connect(addr)
        .await
        .expect_err("connected to a closed port");
    assert_eq!(error.outcome(), RequestOutcome::NotApplied, "{error:?}");
}

/// A deadline that passes is ambiguous, not a failure to send: the peer may be slow rather
/// than absent, and giving up on the answer says nothing about whether it applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deadline_that_passes_is_ambiguous() {
    let server = serve(Echo::blocking(11), impatient()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let deadline = Instant::now() + Duration::from_millis(50);
    let error = transport
        .call_with_deadline(get("slow"), deadline)
        .await
        .expect_err("the blocked service answered");
    assert!(matches!(error, ProtoError::Timeout { .. }), "{error:?}");
    assert_eq!(error.outcome(), RequestOutcome::Unknown);

    server.shutdown().await.unwrap();
}

/// Keepalive both ways: a connection with nothing on it stays up, and a request sent after a
/// long silence still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_is_kept_alive_by_pings() {
    let config = TransportConfig {
        keepalive_interval: Duration::from_millis(20),
        idle_timeout: Duration::from_millis(400),
        ..TransportConfig::new()
    };
    let server = serve(Echo::new(12), config).await;
    let transport = TcpTransport::connect_with(server.local_addr(), config)
        .await
        .unwrap();

    // Several keepalive intervals of silence — more than `missed_keepalives` would tolerate if
    // the pings were not being answered.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!transport.is_closed(), "an idle connection was dropped");

    let response = transport.call(get("after the silence")).await.unwrap();
    assert_eq!(
        response.into_raw_kv().unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"after the silence"))
        }
    );

    server.shutdown().await.unwrap();
}

/// A peer that stops answering is dropped rather than waited on for ever, and every caller
/// waiting on it is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_stops_answering_is_dropped() {
    let config = TransportConfig {
        keepalive_interval: Duration::from_millis(20),
        idle_timeout: Duration::from_millis(60),
        request_timeout: Duration::from_secs(30),
        ..TransportConfig::new()
    };

    // A listener that accepts and then says nothing at all — not even a handshake reply.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mute = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // Hold it open, reading nothing and writing nothing.
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(stream);
    });

    let started = Instant::now();
    let error = TcpTransport::connect_with(addr, config)
        .await
        .expect_err("a mute peer completed a handshake");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the connect waited for the peer instead of the idle timeout"
    );
    assert_eq!(error.outcome(), RequestOutcome::Unknown, "{error:?}");

    mute.abort();
}

/// The blocking wrapper the CLI and the benchmark driver use: no runtime in the caller, a
/// deadline on every call.
#[test]
fn the_blocking_transport_works_from_an_ordinary_thread() {
    // The server needs a runtime of its own, on another thread, because the caller below has
    // none — which is the situation the wrapper exists for.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let handle = runtime.block_on(async { serve(Echo::new(13), TransportConfig::new()).await });
    let addr = handle.local_addr();

    let transport = esker_proto::BlockingTransport::connect(addr).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = transport.call(get("from a thread"), deadline).unwrap();
    assert_eq!(
        response.into_raw_kv().unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"from a thread"))
        }
    );
    assert_eq!(transport.hello_ack().store_id, 7);
    assert!(transport.max_frame_size() > 0);

    drop(transport);
    runtime.block_on(handle.shutdown()).unwrap();
}

/// Blocking inside a runtime is what `tokio` panics on. Here it is an error value instead
/// (`CLAUDE.md` invariant 9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_blocking_transport_refuses_to_run_inside_a_runtime() {
    let server = serve(Echo::new(14), TransportConfig::new()).await;
    let addr = server.local_addr();

    let transport =
        tokio::task::spawn_blocking(move || esker_proto::BlockingTransport::connect(addr).unwrap())
            .await
            .unwrap();

    let error = transport
        .call(get("nope"), Instant::now() + Duration::from_secs(1))
        .expect_err("blocking inside a runtime was allowed");
    assert!(matches!(error, ProtoError::Internal { .. }), "{error:?}");

    // And dropping it here — on a runtime thread — must not panic either, which is why it
    // shuts its runtime down in the background rather than waiting for it.
    drop(transport);
    server.shutdown().await.unwrap();
}

/// Many connections at once, each with its own handshake and its own request ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_connections_are_served_at_once() {
    let server = serve(Echo::new(15), TransportConfig::new()).await;
    let addr = server.local_addr();

    let mut clients = Vec::new();
    for index in 0..16u32 {
        clients.push(tokio::spawn(async move {
            let transport = TcpTransport::connect(addr).await?;
            for round in 0..8 {
                let key = format!("client-{index}-round-{round}");
                let response = transport.call(get(&key)).await?;
                assert_eq!(
                    response.into_raw_kv()?,
                    RawKvResp::Get {
                        value: Some(Bytes::from(key))
                    }
                );
            }
            Ok::<(), ProtoError>(())
        }));
    }
    for client in clients {
        client.await.unwrap().unwrap();
    }

    server.shutdown().await.unwrap();
}

/// Shutdown is graceful: it stops accepting and lets what is already running finish, so the
/// caller can close the database knowing no handler is still writing to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_lets_in_flight_requests_finish() {
    let service = Echo::new(16);
    let counter = Arc::clone(&service);
    let server = serve(service, TransportConfig::new()).await;
    let transport = TcpTransport::connect(server.local_addr()).await.unwrap();

    let mut calls = Vec::new();
    for index in 0..32u32 {
        let transport = transport.clone();
        calls.push(tokio::spawn(async move {
            transport.call(get(&format!("k-{index}"))).await
        }));
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }

    server.shutdown().await.unwrap();
    assert_eq!(counter.served.load(Ordering::Relaxed), 32);
}

/// Graceful cannot mean "for ever". A handler that never finishes is abandoned when the grace
/// expires, so a stuck disk cannot hold the process open past any patience.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_handler_does_not_hold_shutdown_open() {
    let config = TransportConfig {
        shutdown_grace: Duration::from_millis(100),
        ..TransportConfig::new()
    };
    let server = serve(Echo::blocking(17), config).await;
    let transport = TcpTransport::connect_with(server.local_addr(), config)
        .await
        .unwrap();

    let wedged = {
        let transport = transport.clone();
        tokio::spawn(async move { transport.call(get("never finishes")).await })
    };
    while transport.in_flight() == 0 {
        tokio::task::yield_now().await;
    }

    let started = Instant::now();
    server.shutdown().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the drain waited on a handler that will never finish"
    );
    wedged.abort();
}
