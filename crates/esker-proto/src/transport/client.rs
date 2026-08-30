//! The calling side of a connection: [`TcpTransport`], the demultiplexer behind it, and the
//! [`BlockingTransport`] wrapper for callers that are threads rather than futures.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

use crate::messages::HelloAck;
use crate::{Frame, FrameKind, Hello, ProtoError, Request, Response, WIRE_VERSION};

use super::conn::{
    BoxFuture, FrameAction, FrameSink, InFlight, PING_REQUEST_ID, Waiter, WriterStop, read_frames,
    spawn_writer,
};
use super::{Transport, TransportConfig};

/// A connection to one peer.
///
/// **One transport is one connection to one address.** Mapping a store id to an address is
/// routing, which lives in the client's region cache (`docs/DESIGN.md` §10) and in the
/// placement driver (§7); putting a table of them in here would put routing below the wire.
/// From phase 4 a store keeps one of these per peer store, which is what `docs/DESIGN.md` §6
/// describes.
///
/// Cheap to clone: every clone speaks over the same socket, multiplexed by request id.
#[derive(Debug, Clone)]
pub struct TcpTransport {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    sink: FrameSink,
    in_flight: Arc<InFlight>,
    next_id: AtomicU64,
    config: TransportConfig,
    peer: SocketAddr,
    ack: HelloAck,
    /// Dropped with the last clone of the transport, which closes the socket. A connection
    /// nobody holds any more is a connection nobody wants.
    _stop: WriterStop,
}

impl TcpTransport {
    /// Connects to `addr` and negotiates the wire version.
    pub async fn connect(addr: SocketAddr) -> Result<Self, ProtoError> {
        Self::connect_with(addr, TransportConfig::new()).await
    }

    /// Connects with an explicit configuration.
    ///
    /// The [`Hello`] exchange happens here, before the connection is handed back, so a peer at
    /// another version fails at `connect` rather than at the first request that mattered.
    pub async fn connect_with(
        addr: SocketAddr,
        config: TransportConfig,
    ) -> Result<Self, ProtoError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|error| ProtoError::not_sent(format!("connecting to {addr}: {error}")))?;
        // Requests are small and latency matters more than packet count; the writer already
        // batches whatever is queued into one write.
        let _ = stream.set_nodelay(true);

        let (source, sink) = stream.into_split();
        let (sink, stop) = spawn_writer(sink, &config);

        let in_flight = Arc::new(InFlight::new(config.max_in_flight));
        let (hello_tx, hello_rx) = oneshot::channel();
        let demux = Arc::new(Demux {
            in_flight: Arc::clone(&in_flight),
            hello: std::sync::Mutex::new(Some(hello_tx)),
        });

        let reader_demux = Arc::clone(&demux);
        let reader_sink = sink.clone();
        tokio::spawn(async move {
            let exit = read_frames(source, reader_sink.clone(), config, |frame| {
                let demux = Arc::clone(&reader_demux);
                let sink = reader_sink.clone();
                async move { demux.dispatch(frame, &sink).await }
            })
            .await;
            // Everyone still waiting learns at once, and with the same reason.
            let error = exit.into_error();
            reader_demux.fail_hello(&error);
            reader_demux.in_flight.close(&error);
        });

        // The handshake uses the ordinary frame path, so a version mismatch is answered by an
        // error frame the reader routes like any other.
        sink.send(&Frame::new(
            FrameKind::Request,
            HELLO_REQUEST_ID,
            Bytes::from(Request::Hello(Hello::current()).encode()),
        ))
        .await?;

        let ack = match tokio::time::timeout(config.request_timeout, hello_rx).await {
            Ok(Ok(Ok(ack))) => ack,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => {
                return Err(ProtoError::Closed {
                    detail: format!("{addr} closed the connection during the handshake"),
                });
            }
            Err(_elapsed) => {
                return Err(ProtoError::Timeout {
                    detail: format!("{addr} did not answer the handshake"),
                });
            }
        };
        if ack.version != WIRE_VERSION {
            return Err(ProtoError::WireVersion {
                expected: WIRE_VERSION,
                actual: ack.version,
            });
        }

        // The peer said what it will accept; honour it, so an oversized request is one failed
        // call rather than a framing error that closes the connection.
        let sink = sink.narrowed_to(usize::try_from(ack.max_frame_size).unwrap_or(usize::MAX));

        Ok(Self {
            shared: Arc::new(Shared {
                sink,
                in_flight,
                // Zero is the keepalive id and one is the handshake's, so requests start at two.
                next_id: AtomicU64::new(HELLO_REQUEST_ID + 1),
                config,
                peer: addr,
                ack,
                _stop: stop,
            }),
        })
    }

    /// The address of the peer.
    #[must_use]
    pub fn peer(&self) -> SocketAddr {
        self.shared.peer
    }

    /// What the peer said about itself when the connection opened.
    #[must_use]
    pub fn hello_ack(&self) -> HelloAck {
        self.shared.ack
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.in_flight.is_closed() || self.shared.sink.is_closed()
    }

    /// How many requests are outstanding on this connection.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.len()
    }

    /// Sends `request` under a request id the caller chooses.
    ///
    /// Ids are the client's to assign, so this exists for callers that have their own
    /// numbering. An id already in flight is [`ProtoError::DuplicateRequestId`].
    pub fn call_with_id(
        &self,
        request_id: u64,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, ProtoError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move { shared.call(request_id, request).await })
    }

    /// Sends `request` and reads its answer as a stream of chunks.
    ///
    /// The caller says which shape it expects, rather than the first frame deciding: a
    /// response where a stream was asked for is a typed error, not a surprise. Phase 4's
    /// snapshot transfer is the caller this exists for (`docs/DESIGN.md` §6).
    pub async fn call_stream(&self, request: Request) -> Result<StreamResponse, ProtoError> {
        let request_id = self.shared.next_request_id();
        let (chunks, receiver) = mpsc::channel(STREAM_QUEUE);
        self.shared
            .in_flight
            .register(request_id, Waiter::Stream(chunks))?;

        let frame = Frame::new(
            FrameKind::Request,
            request_id,
            Bytes::from(request.encode()),
        );
        if let Err(error) = self.shared.sink.send(&frame).await {
            self.shared.in_flight.take(request_id);
            return Err(error);
        }
        Ok(StreamResponse {
            request_id,
            chunks: receiver,
        })
    }
}

/// Chunks buffered for a stream reader before the connection slows down.
///
/// Small on purpose: a stream is a bulk transfer, and buffering more of it in memory than the
/// reader has asked for is how a snapshot receiver runs out of it.
const STREAM_QUEUE: usize = 4;

/// The request id the handshake uses. Never reused, so a late `HelloAck` cannot be mistaken
/// for an answer to a real request.
const HELLO_REQUEST_ID: u64 = 1;

impl Shared {
    fn next_request_id(&self) -> u64 {
        // Wrapping is unreachable in practice — it needs 2^64 requests on one connection — but
        // if it ever happened, skipping the two reserved ids keeps the invariant true.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id <= HELLO_REQUEST_ID {
            self.next_id
                .fetch_add(HELLO_REQUEST_ID + 1 - id, Ordering::Relaxed);
            return HELLO_REQUEST_ID + 1;
        }
        id
    }

    async fn call(&self, request_id: u64, request: Request) -> Result<Response, ProtoError> {
        let (sender, receiver) = oneshot::channel();
        self.in_flight.register(request_id, Waiter::Unary(sender))?;

        let frame = Frame::new(
            FrameKind::Request,
            request_id,
            Bytes::from(request.encode()),
        );
        // Until the frame is queued, nothing has been sent — an encoding failure or a stopped
        // writer is `NotSent`, and the caller may safely try again. Once it is queued, the
        // outcome of a later failure is unknown, which is what `Closed` and `Timeout` say.
        if let Err(error) = self.sink.send(&frame).await {
            self.in_flight.take(request_id);
            return Err(error);
        }

        match tokio::time::timeout(self.config.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ProtoError::Closed {
                detail: "the connection ended before the answer arrived".to_owned(),
            }),
            Err(_elapsed) => {
                self.in_flight.take(request_id);
                Err(ProtoError::Timeout {
                    detail: format!(
                        "no answer from {} in {:?}",
                        self.peer, self.config.request_timeout
                    ),
                })
            }
        }
    }
}

/// The reader task's view of a connection: where answers go, and the handshake it is waiting
/// for before ordinary requests can start.
#[derive(Debug)]
struct Demux {
    in_flight: Arc<InFlight>,
    hello: std::sync::Mutex<Option<oneshot::Sender<Result<HelloAck, ProtoError>>>>,
}

impl Demux {
    fn fail_hello(&self, error: &ProtoError) {
        let sender = self
            .hello
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(Err(error.clone()));
        }
    }

    async fn dispatch(&self, frame: Frame, sink: &FrameSink) -> FrameAction {
        match frame.kind {
            FrameKind::Ping => {
                let _ = sink.try_send(&Frame::empty(FrameKind::Pong, frame.request_id));
                FrameAction::Continue
            }
            FrameKind::Pong => FrameAction::Continue,
            FrameKind::Request => {
                FrameAction::Stop(ProtoError::invalid("a client received a request frame"))
            }
            FrameKind::Response | FrameKind::Error => self.finish(&frame),
            FrameKind::Stream | FrameKind::StreamEnd => self.chunk(frame).await,
        }
    }

    /// Delivers a terminal frame — a response or an error — to whoever is waiting for it.
    fn finish(&self, frame: &Frame) -> FrameAction {
        if frame.request_id == HELLO_REQUEST_ID {
            return self.finish_hello(frame);
        }
        let delivered = match frame.kind {
            FrameKind::Error => ProtoError::decode(&frame.body)
                .map_err(|error| ProtoError::corrupt("error frame", error.to_string()))
                .and_then(Err),
            _ => Response::decode(&frame.body).map_err(Into::into),
        };

        match self.in_flight.take(frame.request_id) {
            Some(Waiter::Unary(sender)) => {
                let _ = sender.send(delivered);
            }
            Some(Waiter::Stream(sender)) => {
                // A stream was asked for and a unary answer arrived. An error frame is a legal
                // way to end a stream; a response frame is not.
                let ending = match delivered {
                    Err(error) => Err(error),
                    Ok(_) => Err(ProtoError::invalid(
                        "a response frame arrived for a streaming request",
                    )),
                };
                let _ = sender.try_send(ending);
            }
            // Nobody is waiting: the caller gave up, or the peer answered twice. Dropping it is
            // right, and it is worth seeing in a log.
            None => tracing::debug!(
                request_id = frame.request_id,
                "no waiter for an answer; the caller gave up or the peer answered twice"
            ),
        }
        FrameAction::Continue
    }

    fn finish_hello(&self, frame: &Frame) -> FrameAction {
        let result = match frame.kind {
            FrameKind::Error => ProtoError::decode(&frame.body)
                .map_err(|error| ProtoError::corrupt("error frame", error.to_string()))
                .and_then(Err),
            _ => Response::decode(&frame.body)
                .map_err(ProtoError::from)
                .and_then(|response| match response {
                    Response::Hello(ack) => Ok(ack),
                    other @ Response::RawKv(_) => Err(ProtoError::invalid(format!(
                        "expected a Hello acknowledgement, got {}",
                        other.method().name()
                    ))),
                }),
        };
        let failed = result.is_err();
        let sender = self
            .hello
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match sender {
            Some(sender) => {
                let _ = sender.send(result);
            }
            None => {
                return FrameAction::Stop(ProtoError::invalid("a second Hello acknowledgement"));
            }
        }
        if failed {
            // A refused handshake means nothing else on this connection can be understood.
            return FrameAction::Stop(ProtoError::invalid("the handshake was refused"));
        }
        FrameAction::Continue
    }

    /// Delivers one chunk of a stream, or ends it.
    async fn chunk(&self, frame: Frame) -> FrameAction {
        let Some(sender) = self.in_flight.stream_sender(frame.request_id) else {
            tracing::debug!(
                request_id = frame.request_id,
                "a stream chunk with no reader"
            );
            return FrameAction::Continue;
        };
        if frame.kind == FrameKind::StreamEnd {
            self.in_flight.take(frame.request_id);
            // Dropping the sender is what ends the receiver's loop.
            drop(sender);
            return FrameAction::Continue;
        }
        // Waiting here is the backpressure: a reader that has stopped taking chunks stops the
        // whole connection from reading more of them, which is what a bounded buffer means.
        if sender.send(Ok(frame.body)).await.is_err() {
            self.in_flight.take(frame.request_id);
        }
        FrameAction::Continue
    }
}

/// A streamed answer: chunks in the order the peer sent them, ending at `StreamEnd`.
#[derive(Debug)]
pub struct StreamResponse {
    request_id: u64,
    chunks: mpsc::Receiver<Result<Bytes, ProtoError>>,
}

impl StreamResponse {
    /// The next chunk, or `None` when the stream has ended.
    ///
    /// An error ends the stream too: a stream that stopped because the connection died must
    /// not look like one that finished.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, ProtoError>> {
        self.chunks.recv().await
    }

    /// The request id this stream answers.
    #[must_use]
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Reads the whole stream into one buffer.
    ///
    /// For tests and for callers whose payload is known to be small. A snapshot receiver uses
    /// [`StreamResponse::next_chunk`] and writes each chunk to disk instead.
    pub async fn collect(mut self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }
}

impl Transport for TcpTransport {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response, ProtoError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            let request_id = shared.next_request_id();
            shared.call(request_id, request).await
        })
    }

    fn max_frame_size(&self) -> usize {
        self.shared.sink.max_frame_size()
    }
}

/// A [`Transport`] for callers that are threads rather than futures.
///
/// The CLI and the benchmark driver are ordinary blocking code, and `CLAUDE.md` keeps async at
/// the network edge — so the runtime lives in here, owned by the connection, and the caller
/// never sees a future. Every call carries a deadline, because a blocking call with no deadline
/// is a hang.
#[derive(Debug)]
pub struct BlockingTransport {
    /// `None` only while dropping. Dropping a `Runtime` blocks until its tasks stop, and
    /// blocking inside an async context is a panic, so [`Drop`] takes it out and shuts it down
    /// in the background instead.
    runtime: Option<Runtime>,
    transport: TcpTransport,
}

impl BlockingTransport {
    /// Connects to `addr`, building the runtime this connection lives on.
    pub fn connect(addr: SocketAddr) -> Result<Self, ProtoError> {
        Self::connect_with(addr, TransportConfig::new())
    }

    /// Connects with an explicit configuration.
    pub fn connect_with(addr: SocketAddr, config: TransportConfig) -> Result<Self, ProtoError> {
        // One worker thread, so the reader, the writer and the keepalive keep running between
        // calls. On a current-thread runtime they would only advance while a caller was
        // blocked inside `block_on`, and a connection that only lives during a call cannot
        // notice a dead peer.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| ProtoError::internal(format!("building a runtime: {error}")))?;
        let transport = runtime.block_on(TcpTransport::connect_with(addr, config))?;
        Ok(Self {
            runtime: Some(runtime),
            transport,
        })
    }

    /// Sends `request` and waits for its answer, up to `deadline`.
    ///
    /// Calling this from inside a `tokio` runtime is an error rather than a panic, which is
    /// what `tokio` itself would do (`CLAUDE.md` invariant 9).
    pub fn call(&self, request: Request, deadline: Instant) -> Result<Response, ProtoError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(ProtoError::internal(
                "BlockingTransport::call was used inside an async runtime; use the async \
                 Transport there",
            ));
        }
        let Some(runtime) = self.runtime.as_ref() else {
            return Err(ProtoError::internal("the transport is shutting down"));
        };
        runtime.block_on(self.transport.call_with_deadline(request, deadline))
    }

    /// The largest frame this connection will send.
    #[must_use]
    pub fn max_frame_size(&self) -> usize {
        self.transport.max_frame_size()
    }

    /// What the peer said when the connection opened.
    #[must_use]
    pub fn hello_ack(&self) -> HelloAck {
        self.transport.hello_ack()
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.transport.is_closed()
    }

    /// The asynchronous transport underneath, for a caller that has a runtime of its own.
    #[must_use]
    pub fn inner(&self) -> &TcpTransport {
        &self.transport
    }
}

impl Drop for BlockingTransport {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // `Runtime::drop` waits for its threads, and waiting is a panic inside an async
            // context — which is where a caller that built this on a worker thread would drop
            // it. Shutting down in the background is correct from anywhere, and the connection
            // is finished either way (`CLAUDE.md` invariant 9).
            runtime.shutdown_background();
        }
    }
}

/// Never a request id: [`PING_REQUEST_ID`] is the keepalive's and is checked here so the two
/// constants cannot drift apart.
const _: () = assert!(PING_REQUEST_ID < HELLO_REQUEST_ID);
