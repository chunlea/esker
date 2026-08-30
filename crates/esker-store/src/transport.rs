//! The Raft transport: one connection per store pair, a batch per tick.
//!
//! [`RaftTransport`](crate::peer::RaftTransport) is fire-and-forget and infallible, and this is
//! where that shape earns itself. Raft already retries everything it sends — a lost message is
//! indistinguishable from a slow one — so a transport that reported failures would hand the driver
//! a decision it has no better answer to than "send it again next tick". Every failure here is
//! therefore a dropped message and a log line, never an error the consensus layer has to reason
//! about.
//!
//! # What that buys, and what it costs
//!
//! The driver thread never blocks on a socket: [`StoreTransport::send`] hands each message to a
//! **bounded** queue and returns. A queue that is full drops, which is exactly what a congested
//! network does, and is why the bound is safe to have — an unbounded one in front of a slow peer
//! is a memory leak that ends the process instead of the connection
//! (`docs/DESIGN.md` §9, "nothing is unbounded").
//!
//! One task per peer owns that peer's connection, batches whatever has queued since it last woke,
//! and sends it as a single [`RaftBatch`] frame. Batching is not an optimisation here: without it
//! a cluster spends a frame per heartbeat per region per tick (`docs/DESIGN.md` §6).
//!
//! A connection that fails is dropped and rebuilt on the next batch. There is no reconnect
//! backoff loop, because the tick is already one — a peer that is down costs one failed connect
//! per batch, and a batch only exists when there is something to say.

use std::net::SocketAddr;
use std::sync::Arc;

use esker_proto::{
    Epoch, RaftBatch, RaftMessage, Request, TcpTransport, Transport, TransportConfig,
};
use esker_raft::{Message, NodeId};
use tokio::sync::mpsc;

use crate::peer::RaftTransport;

/// How many messages may queue for one peer before the transport starts dropping.
///
/// Generous enough that a brief stall does not lose traffic, small enough that a peer which is
/// simply gone cannot cost unbounded memory. Dropping is safe: Raft retransmits.
pub const PEER_SEND_QUEUE: usize = 1024;

/// Where another store's peer can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddress {
    /// The Raft peer id, which is what a `Message` names.
    pub peer_id: NodeId,
    /// The store hosting it.
    pub store_id: u64,
    /// Its address.
    pub addr: SocketAddr,
}

impl PeerAddress {
    /// A peer at an address.
    #[must_use]
    pub fn new(peer_id: NodeId, store_id: u64, addr: SocketAddr) -> Self {
        Self {
            peer_id,
            store_id,
            addr,
        }
    }
}

/// Sends one region's Raft messages to the peers that are not this store.
#[derive(Debug)]
pub struct StoreTransport {
    region_id: u64,
    epoch: Epoch,
    /// One queue per peer, sorted by peer id — a `Vec` rather than a map for the same reason the
    /// core uses one: this is on a decision path and its iteration order should be defined.
    peers: Vec<(NodeId, mpsc::Sender<RaftMessage>)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl StoreTransport {
    /// Connects to every peer but this one, spawning a task each.
    ///
    /// Must be called from inside a `tokio` runtime: the tasks are where the async lives, and the
    /// driver thread that feeds them is deliberately not async at all.
    #[must_use]
    pub fn spawn(
        region_id: u64,
        epoch: Epoch,
        self_peer: NodeId,
        peers: &[PeerAddress],
        config: TransportConfig,
    ) -> Arc<Self> {
        let mut queues = Vec::new();
        let mut tasks = Vec::new();
        for peer in peers.iter().filter(|peer| peer.peer_id != self_peer) {
            let (sender, receiver) = mpsc::channel(PEER_SEND_QUEUE);
            queues.push((peer.peer_id, sender));
            tasks.push(tokio::spawn(deliver_to(
                peer.clone(),
                region_id,
                receiver,
                config,
            )));
        }
        queues.sort_by_key(|(id, _)| *id);
        Arc::new(Self {
            region_id,
            epoch,
            peers: queues,
            tasks,
        })
    }

    /// Stops every peer task. Anything still queued is dropped, which is what a peer going away
    /// looks like from the other end anyway.
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl RaftTransport for StoreTransport {
    fn send(&self, messages: Vec<Message>) {
        for message in messages {
            let to = message.recipient();
            let Ok(at) = self.peers.binary_search_by_key(&to, |(id, _)| *id) else {
                // A message for a peer this store has no address for. In 3e the membership is
                // static, so this is a configuration mistake rather than a race — worth saying.
                tracing::warn!(
                    region_id = self.region_id,
                    peer = to,
                    "no address for a peer; the message was dropped"
                );
                continue;
            };
            let wrapped = RaftMessage::new(self.region_id, self.epoch, message);
            if self.peers[at].1.try_send(wrapped).is_err() {
                // Full or closed. Dropping is the honest outcome and Raft will resend; blocking
                // the driver thread on a slow socket would stall consensus for every region.
                tracing::debug!(
                    region_id = self.region_id,
                    peer = to,
                    "the send queue is full or closed; the message was dropped"
                );
            }
        }
    }
}

impl Drop for StoreTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One peer's connection: batch what has queued, send it, keep the socket if it worked.
async fn deliver_to(
    peer: PeerAddress,
    region_id: u64,
    mut queue: mpsc::Receiver<RaftMessage>,
    config: TransportConfig,
) {
    let mut connection: Option<TcpTransport> = None;
    while let Some(first) = queue.recv().await {
        // Everything queued since the last wake-up travels together. This is the per-tick
        // batching `docs/DESIGN.md` §6 asks for: without it a heartbeat is a frame.
        let mut batch = vec![first];
        while let Ok(next) = queue.try_recv() {
            batch.push(next);
        }

        if connection.as_ref().is_some_and(TcpTransport::is_closed) {
            connection = None;
        }
        if connection.is_none() {
            match TcpTransport::connect_with(peer.addr, config).await {
                Ok(transport) => connection = Some(transport),
                Err(error) => {
                    tracing::debug!(
                        region_id,
                        peer = peer.peer_id,
                        store = peer.store_id,
                        %error,
                        "could not reach a peer; its messages were dropped"
                    );
                    continue;
                }
            }
        }

        let Some(transport) = connection.as_ref() else {
            continue;
        };
        let count = batch.len();
        if let Err(error) = transport.call(Request::Raft(RaftBatch::new(batch))).await {
            tracing::debug!(
                region_id,
                peer = peer.peer_id,
                count,
                %error,
                "a Raft batch did not reach its peer"
            );
            // The connection is suspect; the next batch builds a new one.
            connection = None;
        }
    }
}
