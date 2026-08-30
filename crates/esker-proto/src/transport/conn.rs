//! What a client connection and a server connection share: the writer task, the frame reader
//! with its keepalive, and the table of requests in flight.
//!
//! The shape is one task per direction. A **writer task** owns the write half and a bounded
//! queue of already-encoded frames; a **reader task** owns the read half and hands whole frames
//! to whichever side it belongs to. Encoding happens at the call site rather than in the writer
//! so that an oversized frame fails as [`ProtoError::NotSent`] — the caller learns the request
//! never left, which is a different fact from "no answer came back"
//! ([`crate::RequestOutcome`]).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::{Frame, FrameDecoder, FrameKind, ProtoError, Response};

use super::TransportConfig;

/// A boxed future, defined here because `futures` is not on the dependency allowlist and this
/// is the whole of it we need.
///
/// It is what makes [`super::Transport`] dyn-compatible: the client keeps transports behind an
/// `Arc<dyn Transport>` and swaps in a fake for its tests, neither of which an `async fn` in
/// trait allows.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The sending half of a connection: a bounded queue into the writer task.
///
/// Cloning it is cheap and is how many request tasks share one socket.
#[derive(Debug, Clone)]
pub(crate) struct FrameSink {
    frames: mpsc::Sender<Bytes>,
    max_frame_size: usize,
}

impl FrameSink {
    /// Encodes `frame` and queues it, waiting if the writer is behind.
    ///
    /// Waiting is the point: the queue is bounded, so a peer that reads slowly slows this side
    /// down instead of filling memory with frames nobody has asked for.
    pub(crate) async fn send(&self, frame: &Frame) -> Result<(), ProtoError> {
        let bytes = frame.encode(self.max_frame_size)?;
        self.frames
            .send(bytes)
            .await
            .map_err(|_| ProtoError::not_sent("the connection's writer has stopped"))
    }

    /// Queues `frame` without waiting, failing when the writer is behind.
    ///
    /// For frames that exist to keep a connection healthy — pings, pongs, the error that
    /// answers an unreadable request. Blocking the reader on a full writer queue would stop
    /// the connection from draining responses, which is the thing that empties the queue.
    pub(crate) fn try_send(&self, frame: &Frame) -> Result<(), ProtoError> {
        let bytes = frame.encode(self.max_frame_size)?;
        self.frames.try_send(bytes).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ProtoError::ServerIsBusy {
                reason: "the connection's writer queue is full".to_owned(),
            },
            mpsc::error::TrySendError::Closed(_) => {
                ProtoError::not_sent("the connection's writer has stopped")
            }
        })
    }

    /// The largest frame this connection will send.
    pub(crate) fn max_frame_size(&self) -> usize {
        self.max_frame_size
    }

    /// The same sink, narrowed to whichever limit is smaller.
    ///
    /// A client narrows its sink to the peer's advertised `max_frame_size` once the handshake
    /// has said what that is. An oversized request then fails *here*, before a byte goes out,
    /// rather than being refused by the peer's frame reader — which cannot answer it, because a
    /// bad length means the reader no longer knows where the next frame starts, so it closes
    /// the connection and every other request on it. Narrowing turns a lost connection into one
    /// refused call.
    ///
    /// Only the sending side narrows. What this end will *accept* stays its own configuration.
    pub(crate) fn narrowed_to(mut self, limit: usize) -> Self {
        self.max_frame_size = self.max_frame_size.min(limit);
        self
    }

    /// Whether the writer task has stopped, which means the connection is finished.
    pub(crate) fn is_closed(&self) -> bool {
        self.frames.is_closed()
    }
}

/// Closes a connection when dropped: the writer flushes what is queued and shuts the socket.
///
/// It exists because a [`FrameSink`] clone can outlive the connection — a handler wedged on a
/// stuck disk still holds one — and while any clone lives the writer has no way to know the
/// connection is over. Whoever owns the connection owns one of these, and dropping it is what
/// actually ends the socket rather than waiting for the last straggler to let go.
#[derive(Debug)]
// The sender is never read from: dropping it is the signal, which is what a `oneshot` gives us
// for free and what makes "the owner went away" and "the owner said stop" the same event.
pub(crate) struct WriterStop(#[allow(dead_code)] oneshot::Sender<()>);

impl WriterStop {
    /// Ends the connection now, flushing whatever is already queued.
    pub(crate) fn stop(self) {
        drop(self);
    }
}

/// Starts the writer task for one connection.
pub(crate) fn spawn_writer<W>(mut sink: W, config: &TransportConfig) -> (FrameSink, WriterStop)
where
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let (frames, mut queue) = mpsc::channel::<Bytes>(config.write_queue);
    let (closer, mut stop) = oneshot::channel();
    let max_frame_size = config.max_frame_size;

    tokio::spawn(async move {
        let mut batch = BytesMut::new();
        let mut closing = false;
        loop {
            let next = if closing {
                queue.recv().await
            } else {
                tokio::select! {
                    frame = queue.recv() => frame,
                    _ = &mut stop => {
                        // The connection is closing. Refuse new frames and flush what is
                        // already queued, so an answer that was produced before the shutdown
                        // still reaches the caller who is waiting for it.
                        closing = true;
                        queue.close();
                        queue.recv().await
                    }
                }
            };
            let Some(first) = next else { break };

            // Whatever is already queued goes out in one write. Under load this is what turns
            // a thousand small responses into a handful of syscalls; when there is no load it
            // does nothing, because `try_recv` finds an empty queue.
            batch.clear();
            batch.extend_from_slice(&first);
            while let Ok(more) = queue.try_recv() {
                batch.extend_from_slice(&more);
            }
            if sink.write_all(&batch).await.is_err() {
                break;
            }
            if sink.flush().await.is_err() {
                break;
            }
        }
        // Closing the write half tells the peer no more frames are coming, which is what turns
        // a shutdown into an orderly end of stream rather than a reset.
        let _ = sink.shutdown().await;
    });

    (
        FrameSink {
            frames,
            max_frame_size,
        },
        WriterStop(closer),
    )
}

/// Why a connection's reader stopped.
#[derive(Debug)]
pub(crate) enum ReaderExit {
    /// The peer closed its write half cleanly.
    PeerClosed,
    /// The peer said nothing for `idle_timeout`, so it is presumed gone.
    Idle,
    /// The socket failed, or the bytes were not frames.
    Failed(ProtoError),
    /// The frame handler asked to stop, carrying the reason.
    Stopped(ProtoError),
}

impl ReaderExit {
    /// The error every in-flight request is failed with when the connection ends this way.
    ///
    /// All of them are ambiguous: the request went out and no answer came back, so whether the
    /// peer applied it is unknown ([`crate::RequestOutcome`]).
    pub(crate) fn into_error(self) -> ProtoError {
        match self {
            Self::PeerClosed => ProtoError::Closed {
                detail: "the peer closed the connection".to_owned(),
            },
            Self::Idle => ProtoError::Closed {
                detail: "the peer stopped answering keepalives".to_owned(),
            },
            Self::Failed(error) | Self::Stopped(error) => error,
        }
    }
}

/// What a connection's reader does with one frame.
pub(crate) enum FrameAction {
    /// Keep reading.
    Continue,
    /// Stop, for the stated reason.
    Stop(ProtoError),
}

/// Reads frames until the connection ends, pinging a silent peer and giving up on a dead one.
///
/// The keepalive is folded into the read rather than run as a second task: a read that has not
/// produced a byte for `keepalive_interval` *is* the idle signal, and a peer that has ignored
/// [`TransportConfig::missed_keepalives`] of them is not going to answer.
pub(crate) async fn read_frames<R, H, F>(
    mut source: R,
    sink: FrameSink,
    config: TransportConfig,
    mut handle: H,
) -> ReaderExit
where
    R: AsyncReadExt + Unpin,
    H: FnMut(Frame) -> F,
    F: Future<Output = FrameAction>,
{
    let mut decoder = FrameDecoder::new(config.max_frame_size);
    let mut silent_intervals = 0u32;
    let allowed = config.missed_keepalives();

    loop {
        let read = tokio::time::timeout(
            config.keepalive_interval,
            source.read_buf(decoder.buffer_mut()),
        )
        .await;

        match read {
            Ok(Ok(0)) => return ReaderExit::PeerClosed,
            Ok(Ok(_)) => silent_intervals = 0,
            Ok(Err(error)) => return ReaderExit::Failed(error.into()),
            Err(_elapsed) => {
                silent_intervals += 1;
                if silent_intervals >= allowed {
                    return ReaderExit::Idle;
                }
                // A ping that cannot be queued means the writer is backed up, which is itself
                // proof the connection is alive. Dropping it is correct.
                let _ = sink.try_send(&Frame::empty(FrameKind::Ping, PING_REQUEST_ID));
                continue;
            }
        }

        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let FrameAction::Stop(reason) = handle(frame).await {
                        return ReaderExit::Stopped(reason);
                    }
                }
                Ok(None) => break,
                Err(error) => return ReaderExit::Failed(error),
            }
        }
    }
}

/// The request id keepalive frames carry.
///
/// Zero is never a request id — [`InFlight::register`] refuses it — so a ping can never be
/// mistaken for an answer to something.
pub(crate) const PING_REQUEST_ID: u64 = 0;

/// Where a response to one request id is delivered.
#[derive(Debug)]
pub(crate) enum Waiter {
    /// A caller waiting for one response frame.
    Unary(oneshot::Sender<Result<Response, ProtoError>>),
    /// A caller reading a stream of chunks.
    Stream(mpsc::Sender<Result<Bytes, ProtoError>>),
}

/// The requests outstanding on one connection, keyed by request id.
///
/// Two rules it exists to enforce. A **duplicate id that is still in flight is refused**:
/// replacing the waiter would leave the first caller waiting for an answer that can never
/// arrive, and the ids are the client's to choose, so a collision is a bug worth naming. And
/// the table is **bounded**: past `max_in_flight` a caller is told the connection is busy
/// rather than being added to a queue that has no end.
#[derive(Debug)]
pub(crate) struct InFlight {
    waiters: Mutex<Option<HashMap<u64, Waiter>>>,
    limit: usize,
}

impl InFlight {
    /// An empty table with the given ceiling.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            waiters: Mutex::new(Some(HashMap::new())),
            limit,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HashMap<u64, Waiter>>> {
        // A panic while holding this lock cannot leave the map in a state that matters: every
        // operation on it is a single insert or remove. Recovering is better than propagating
        // a panic into every later caller.
        self.waiters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a waiter, or says why it could not.
    pub(crate) fn register(&self, request_id: u64, waiter: Waiter) -> Result<(), ProtoError> {
        if request_id == PING_REQUEST_ID {
            return Err(ProtoError::invalid(
                "request id 0 is reserved for keepalive frames",
            ));
        }
        let mut guard = self.lock();
        let Some(waiters) = guard.as_mut() else {
            return Err(ProtoError::not_sent("the connection is closed"));
        };
        if waiters.contains_key(&request_id) {
            return Err(ProtoError::DuplicateRequestId { request_id });
        }
        if waiters.len() >= self.limit {
            return Err(ProtoError::ServerIsBusy {
                reason: format!(
                    "{} requests already in flight on this connection",
                    self.limit
                ),
            });
        }
        waiters.insert(request_id, waiter);
        Ok(())
    }

    /// Removes a waiter — because its exchange finished, or because the caller gave up.
    pub(crate) fn take(&self, request_id: u64) -> Option<Waiter> {
        self.lock().as_mut()?.remove(&request_id)
    }

    /// Clones the stream sender for `request_id`, leaving it registered.
    ///
    /// The clone is what lets a chunk be delivered *without* the table's lock held: a stream
    /// channel is bounded, so delivering into it may wait, and waiting under a lock every
    /// other request needs is how one slow reader stalls a whole connection.
    pub(crate) fn stream_sender(
        &self,
        request_id: u64,
    ) -> Option<mpsc::Sender<Result<Bytes, ProtoError>>> {
        match self.lock().as_ref()?.get(&request_id)? {
            Waiter::Stream(sender) => Some(sender.clone()),
            Waiter::Unary(_) => None,
        }
    }

    /// How many requests are outstanding.
    pub(crate) fn len(&self) -> usize {
        self.lock().as_ref().map_or(0, HashMap::len)
    }

    /// Fails every waiter and refuses any new one.
    ///
    /// Called once, when the connection ends. Every caller learns the same thing at the same
    /// time, which is the difference between a closed connection and a hung one.
    pub(crate) fn close(&self, error: &ProtoError) {
        let Some(waiters) = self.lock().take() else {
            return;
        };
        for (_, waiter) in waiters {
            match waiter {
                Waiter::Unary(sender) => {
                    let _ = sender.send(Err(error.clone()));
                }
                Waiter::Stream(sender) => {
                    // The receiver may already be gone; a stream that nobody is reading needs
                    // no ending.
                    let _ = sender.try_send(Err(error.clone()));
                }
            }
        }
    }

    /// Whether the connection has been closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{InFlight, PING_REQUEST_ID, Waiter};
    use crate::{ProtoError, RequestOutcome};
    use tokio::sync::oneshot;

    fn unary() -> (
        Waiter,
        oneshot::Receiver<Result<crate::Response, ProtoError>>,
    ) {
        let (sender, receiver) = oneshot::channel();
        (Waiter::Unary(sender), receiver)
    }

    /// The rule the demultiplexer exists for: two callers cannot share an id, because the
    /// second would silently take the first one's answer.
    #[test]
    fn a_duplicate_id_is_refused_rather_than_replacing_the_waiter() {
        let table = InFlight::new(8);
        let (first, mut receiver) = unary();
        table.register(1, first).unwrap();

        let (second, _) = unary();
        assert_eq!(
            table.register(1, second).unwrap_err(),
            ProtoError::DuplicateRequestId { request_id: 1 }
        );

        // And the first caller is still there, waiting for an answer that can still arrive.
        assert_eq!(table.len(), 1);
        assert!(receiver.try_recv().is_err(), "the first waiter was dropped");
    }

    #[test]
    fn the_table_is_bounded_and_says_so() {
        let table = InFlight::new(2);
        for id in 1..=2 {
            table.register(id, unary().0).unwrap();
        }
        let error = table.register(3, unary().0).unwrap_err();
        assert!(
            matches!(error, ProtoError::ServerIsBusy { .. }),
            "{error:?}"
        );
        assert!(error.is_retryable(), "shedding load must be retryable");
        assert_eq!(
            error.outcome(),
            RequestOutcome::NotApplied,
            "a request that was never sent cannot have applied"
        );

        // Finishing one makes room for the next.
        assert!(table.take(1).is_some());
        table.register(3, unary().0).unwrap();
    }

    /// Zero is a keepalive's id. Letting a request use it would make a pong look like an answer.
    #[test]
    fn request_id_zero_is_reserved() {
        let table = InFlight::new(8);
        assert!(table.register(PING_REQUEST_ID, unary().0).is_err());
    }

    /// When a connection ends, every caller finds out at once and nobody is left waiting.
    #[test]
    fn closing_fails_every_waiter_and_refuses_new_ones() {
        let table = InFlight::new(8);
        let (waiter, receiver) = unary();
        table.register(1, waiter).unwrap();

        table.close(&ProtoError::Closed {
            detail: "peer went away".to_owned(),
        });

        let delivered = receiver
            .blocking_recv()
            .expect("the waiter was not answered");
        assert!(matches!(delivered, Err(ProtoError::Closed { .. })));
        assert!(table.is_closed());

        let error = table.register(2, unary().0).unwrap_err();
        assert_eq!(
            error.outcome(),
            RequestOutcome::NotApplied,
            "a request refused by a closed connection never went out"
        );
    }

    #[test]
    fn taking_a_waiter_removes_it() {
        let table = InFlight::new(8);
        table.register(5, unary().0).unwrap();
        assert!(table.take(5).is_some());
        assert!(table.take(5).is_none());
        assert_eq!(table.len(), 0);
    }
}
