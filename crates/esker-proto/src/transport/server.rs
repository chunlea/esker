//! The answering side: a [`Service`] that handles requests, and the [`Server`] that puts one
//! behind a TCP listener.
//!
//! The split is deliberate. This module knows about frames, connections, version negotiation
//! and backpressure; it knows nothing about keys, regions or engines. `esker-store` implements
//! [`Service`] and knows nothing about sockets.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;

use crate::messages::HelloAck;
use crate::{Frame, FrameKind, ProtoError, Request, Response, WIRE_VERSION};

use super::TransportConfig;
use super::conn::{BoxFuture, FrameAction, FrameSink, read_frames, spawn_writer};

/// What a server does with a request.
///
/// Implemented by `esker-store`. [`crate::Hello`] never reaches it: version negotiation is the
/// connection's business, and a service that had to remember to handle it would be a service
/// that could forget.
///
/// `Debug` is required for the same reason the engine's seams require it: the workspace denies
/// a public type without a `Debug` implementation, and everything holding a service is one
/// (phase-1 plan §10.2).
pub trait Service: Send + Sync + std::fmt::Debug + 'static {
    /// Handles one request. An `Err` becomes an `Error` frame carrying the typed error.
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>>;

    /// Which store is answering, for the handshake.
    fn store_id(&self) -> u64;
}

/// A service's answer: one response, or a stream of chunks.
#[derive(Debug)]
pub enum Reply {
    /// One `Response` frame.
    Unary(Response),
    /// A run of `Stream` frames followed by a `StreamEnd`. Phase 4's snapshot transfer
    /// (`docs/DESIGN.md` §6) is what this is for.
    Stream(ChunkStream),
}

impl From<Response> for Reply {
    fn from(response: Response) -> Self {
        Self::Unary(response)
    }
}

/// The producing half of a streamed reply.
///
/// The channel is bounded, so a producer that outruns the network waits rather than buffering
/// a whole snapshot in memory.
#[derive(Debug)]
pub struct ChunkSender(mpsc::Sender<Result<Bytes, ProtoError>>);

impl ChunkSender {
    /// Queues one chunk, waiting while the connection catches up.
    ///
    /// `Err` means the reader is gone — the connection closed, or the caller stopped reading —
    /// and the producer should stop.
    pub async fn send(&self, chunk: Bytes) -> Result<(), ProtoError> {
        self.0
            .send(Ok(chunk))
            .await
            .map_err(|_| ProtoError::Closed {
                detail: "the stream's reader has gone".to_owned(),
            })
    }

    /// Ends the stream with an error instead of a [`FrameKind::StreamEnd`].
    ///
    /// A stream that stopped because something failed must not look like one that finished.
    pub async fn fail(self, error: ProtoError) {
        let _ = self.0.send(Err(error)).await;
    }
}

/// The consuming half, read by the connection and turned into frames.
#[derive(Debug)]
pub struct ChunkStream(mpsc::Receiver<Result<Bytes, ProtoError>>);

impl ChunkStream {
    /// A stream and the handle that fills it, buffering `capacity` chunks.
    #[must_use]
    pub fn channel(capacity: usize) -> (ChunkSender, Self) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (ChunkSender(sender), Self(receiver))
    }
}

/// A listener with a service behind it.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    service: Arc<dyn Service>,
    config: TransportConfig,
}

impl Server {
    /// Binds to `addr`. Port 0 asks the operating system for a free one, which is what the
    /// integration tests use.
    pub async fn bind(
        addr: impl ToSocketAddrs,
        service: Arc<dyn Service>,
        config: TransportConfig,
    ) -> Result<Self, ProtoError> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            service,
            config,
        })
    }

    /// The address actually bound, which is how a caller learns an ephemeral port.
    pub fn local_addr(&self) -> Result<SocketAddr, ProtoError> {
        Ok(self.listener.local_addr()?)
    }

    /// Serves until `shutdown` resolves, then finishes what is in flight.
    ///
    /// Graceful means three things in order: stop accepting, let every connection finish the
    /// requests it has already started, and only then return — so the caller can close the
    /// engine knowing no handler is still writing to it.
    pub async fn serve(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), ProtoError> {
        let (stop, stopping) = watch::channel(false);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let service = Arc::clone(&self.service);
                            let config = self.config;
                            let stopping = stopping.clone();
                            connections.spawn(async move {
                                if let Err(error) = serve_connection(stream, service, config, stopping).await {
                                    tracing::debug!(%peer, %error, "connection ended");
                                }
                            });
                        }
                        // One failed accept is not a reason to stop serving the connections
                        // that are already up: a per-process descriptor limit passes.
                        Err(error) => tracing::warn!(%error, "accept failed"),
                    }
                    // Finished connections are reaped here rather than accumulating in the set.
                    while connections.try_join_next().is_some() {}
                }
                () = &mut shutdown => break,
            }
        }

        tracing::info!("shutting down: no longer accepting, draining in-flight requests");
        let _ = stop.send(true);
        while connections.join_next().await.is_some() {}
        Ok(())
    }

    /// Runs the server on the current runtime and hands back a handle to stop it.
    ///
    /// For tests, and for `esker-cli server`, which stops it on ctrl-c.
    pub fn spawn(self) -> Result<ServerHandle, ProtoError> {
        let addr = self.local_addr()?;
        let (stop, mut stopping) = watch::channel(false);
        let join = tokio::spawn(async move {
            let signal = async move {
                // A dropped sender means the handle went away, which is also a stop.
                let _ = stopping.changed().await;
            };
            self.serve(signal).await
        });
        Ok(ServerHandle { addr, stop, join })
    }
}

/// A running server: where it is listening, and how to stop it.
#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    join: tokio::task::JoinHandle<Result<(), ProtoError>>,
}

impl ServerHandle {
    /// The address it is listening on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting and waits for in-flight requests to finish.
    pub async fn shutdown(self) -> Result<(), ProtoError> {
        let _ = self.stop.send(true);
        match self.join.await {
            Ok(result) => result,
            Err(error) => Err(ProtoError::internal(format!(
                "the server task failed: {error}"
            ))),
        }
    }
}

/// Serves one connection: handshake, then requests until the peer or the server stops.
async fn serve_connection(
    stream: TcpStream,
    service: Arc<dyn Service>,
    config: TransportConfig,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), ProtoError> {
    let _ = stream.set_nodelay(true);
    let (source, sink) = stream.into_split();
    let (sink, stop) = spawn_writer(sink, &config);

    // The permits *are* the in-flight limit: a request that cannot take one is shed with a
    // typed error rather than queued, and draining on shutdown is waiting for them all back.
    let permits = Arc::new(Semaphore::new(config.max_in_flight));
    let state = Arc::new(ConnectionState {
        service,
        permits: Arc::clone(&permits),
        greeted: std::sync::atomic::AtomicBool::new(false),
        config,
    });

    let reader_state = Arc::clone(&state);
    let reader_sink = sink.clone();
    let reading = read_frames(source, reader_sink.clone(), config, move |frame| {
        let state = Arc::clone(&reader_state);
        let sink = reader_sink.clone();
        async move { state.dispatch(frame, sink).await }
    });

    tokio::select! {
        exit = reading => tracing::debug!(?exit, "connection reader finished"),
        _ = stopping.changed() => tracing::debug!("connection asked to stop"),
    }

    // Every permit back means every handler has finished and written its answer; the writer
    // then drains its queue and closes the socket when `sink` is dropped. The wait is bounded,
    // because a handler wedged on a stuck disk must not hold the process open for ever.
    let all = u32::try_from(config.max_in_flight).unwrap_or(u32::MAX);
    if tokio::time::timeout(config.shutdown_grace, permits.acquire_many(all))
        .await
        .is_err()
    {
        tracing::warn!(
            grace = ?config.shutdown_grace,
            abandoned = config.max_in_flight - permits.available_permits(),
            "requests were still running when the shutdown grace expired"
        );
    }
    // Explicitly, rather than by dropping the last `FrameSink`: an abandoned handler still
    // holds one, and a peer waiting on a socket that nobody will ever write to again would
    // sit there until its own request timeout.
    stop.stop();
    Ok(())
}

#[derive(Debug)]
struct ConnectionState {
    service: Arc<dyn Service>,
    permits: Arc<Semaphore>,
    greeted: std::sync::atomic::AtomicBool,
    config: TransportConfig,
}

impl ConnectionState {
    // Not `async` itself, but the reader's handler must return a future, so it is written as
    // one. Nothing here waits: a request is answered on its own task.
    #[allow(clippy::unused_async)]
    async fn dispatch(self: Arc<Self>, frame: Frame, sink: FrameSink) -> FrameAction {
        match frame.kind {
            FrameKind::Request => self.on_request(&frame, sink),
            FrameKind::Ping => {
                let _ = sink.try_send(&Frame::empty(FrameKind::Pong, frame.request_id));
                FrameAction::Continue
            }
            FrameKind::Pong => FrameAction::Continue,
            // A server never receives these. Rather than guess what the peer meant, say so and
            // close: a connection carrying frames in the wrong direction is not understood.
            other => {
                let error = ProtoError::invalid(format!("a server received a {other:?} frame"));
                let _ = sink.try_send(&error_frame(frame.request_id, &error));
                FrameAction::Stop(error)
            }
        }
    }

    fn on_request(self: Arc<Self>, frame: &Frame, sink: FrameSink) -> FrameAction {
        let request = match Request::decode(&frame.body) {
            Ok(request) => request,
            Err(error) => {
                // The frame's checksum passed, so the bytes arrived as sent: this is a caller
                // error, not corruption, and the connection survives it.
                let error = ProtoError::from(error);
                let _ = sink.try_send(&error_frame(frame.request_id, &error));
                return FrameAction::Continue;
            }
        };

        if let Request::Hello(hello) = request {
            return self.on_hello(hello.version, frame.request_id, &sink);
        }

        if !self.greeted.load(std::sync::atomic::Ordering::Acquire) {
            let error = ProtoError::invalid("the first request on a connection must be a Hello");
            let _ = sink.try_send(&error_frame(frame.request_id, &error));
            return FrameAction::Stop(error);
        }

        // `try_acquire` and not `acquire`: at the limit the answer is "busy", not a queue.
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            let error = ProtoError::ServerIsBusy {
                reason: format!(
                    "{} requests already in flight on this connection",
                    self.config.max_in_flight
                ),
            };
            let _ = sink.try_send(&error_frame(frame.request_id, &error));
            return FrameAction::Continue;
        };

        let state = Arc::clone(&self);
        let request_id = frame.request_id;
        tokio::spawn(async move {
            state.answer(request_id, request, &sink).await;
            drop(permit);
        });
        FrameAction::Continue
    }

    fn on_hello(&self, version: u32, request_id: u64, sink: &FrameSink) -> FrameAction {
        if version != WIRE_VERSION {
            let error = ProtoError::WireVersion {
                expected: WIRE_VERSION,
                actual: version,
            };
            let _ = sink.try_send(&error_frame(request_id, &error));
            // There is no downgrade path, so nothing else on this connection can be trusted.
            return FrameAction::Stop(error);
        }
        if self.greeted.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let error = ProtoError::invalid("a second Hello on one connection");
            let _ = sink.try_send(&error_frame(request_id, &error));
            return FrameAction::Stop(error);
        }

        let ack = Response::Hello(HelloAck {
            version: WIRE_VERSION,
            store_id: self.service.store_id(),
            max_frame_size: self.config.max_frame_size as u64,
        });
        let _ = sink.try_send(&Frame::new(
            FrameKind::Response,
            request_id,
            Bytes::from(ack.encode()),
        ));
        FrameAction::Continue
    }

    async fn answer(&self, request_id: u64, request: Request, sink: &FrameSink) {
        match self.service.call(request).await {
            Ok(Reply::Unary(response)) => {
                let frame = Frame::new(
                    FrameKind::Response,
                    request_id,
                    Bytes::from(response.encode()),
                );
                if let Err(error) = sink.send(&frame).await {
                    tracing::debug!(request_id, %error, "could not send a response");
                }
            }
            Ok(Reply::Stream(stream)) => self.stream(request_id, stream, sink).await,
            Err(error) => {
                let _ = sink.send(&error_frame(request_id, &error)).await;
            }
        }
    }

    async fn stream(&self, request_id: u64, mut stream: ChunkStream, sink: &FrameSink) {
        while let Some(chunk) = stream.0.recv().await {
            let frame = match chunk {
                Ok(bytes) => Frame::new(FrameKind::Stream, request_id, bytes),
                Err(error) => {
                    // An error ends the stream in place of its `StreamEnd`, so a reader can
                    // tell a transfer that failed from one that finished.
                    let _ = sink.send(&error_frame(request_id, &error)).await;
                    return;
                }
            };
            if sink.send(&frame).await.is_err() {
                return;
            }
        }
        let _ = sink
            .send(&Frame::empty(FrameKind::StreamEnd, request_id))
            .await;
    }
}

fn error_frame(request_id: u64, error: &ProtoError) -> Frame {
    Frame::new(FrameKind::Error, request_id, Bytes::from(error.encode()))
}

#[cfg(test)]
mod tests {
    use super::{ChunkStream, Reply};
    use crate::{RawKvResp, Response};
    use bytes::Bytes;

    #[test]
    fn a_response_is_a_reply() {
        let reply: Reply = Response::RawKv(RawKvResp::Put).into();
        assert!(matches!(reply, Reply::Unary(_)));
    }

    /// A zero-capacity `tokio` channel panics on construction, and a caller asking for one
    /// means "as little buffering as possible", not "crash".
    #[tokio::test]
    async fn a_zero_capacity_stream_still_works() {
        let (sender, mut stream) = ChunkStream::channel(0);
        sender.send(Bytes::from_static(b"chunk")).await.unwrap();
        drop(sender);
        assert_eq!(
            stream.0.recv().await,
            Some(Ok(Bytes::from_static(b"chunk")))
        );
        assert_eq!(stream.0.recv().await, None);
    }
}
