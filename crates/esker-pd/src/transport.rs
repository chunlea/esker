//! The placement drivers' own Raft transport: one connection per member pair, a batch per tick.
//!
//! The shape is `esker_store::transport`'s, because the problem is the same one and it was solved
//! there first: a bounded queue per destination, one task draining it, everything that queued
//! between two wake-ups travelling in one frame, and a connection rebuilt on the next batch after
//! a failure. What differs is only how small it is — a placement driver has at most two peers and
//! exactly one group, so there is no address book to resolve and no region to stamp.
//!
//! # Fire and forget, and why that is not a compromise
//!
//! [`PdTransport::send`](crate::driver::PdTransport::send) is infallible by design. Raft already
//! retries everything it sends, so a lost message is indistinguishable from a slow one, and a
//! transport that reported failures would hand the driver a decision it has no better answer to
//! than "send it again next tick". Every failure here is therefore a dropped message and a log
//! line.
//!
//! A full queue drops too, and that is what a congested network does. The bound is what makes it
//! safe: an unbounded queue in front of a member that is simply gone is a memory leak that ends
//! the process instead of the connection (`docs/DESIGN.md` §9).
//!
//! # There is no reconnect backoff
//!
//! The tick is one. A member that is down costs one failed connect per batch, and a batch only
//! exists when there is something to say.

use std::net::SocketAddr;
use std::sync::Arc;

use esker_proto::pd::{PdRaftBatch, PdReq};
use esker_proto::{Request, TcpTransport, Transport, TransportConfig};
use esker_raft::{Message, NodeId};
use tokio::sync::mpsc;

use crate::driver::PdTransport;
use crate::error::{PdError, Result};
use crate::member::MemberList;

/// Messages that may queue for one member before the transport starts dropping.
///
/// Smaller than a store's, and deliberately: a store's queue carries every region's traffic and
/// this one carries a single group's, so the same bound would be three orders of magnitude of
/// slack nobody asked for. Two hundred is a hundred ticks' worth of heartbeats.
pub const MEMBER_SEND_QUEUE: usize = 256;

/// One connection per member pair.
#[derive(Debug)]
pub struct PdTcpTransport {
    id: NodeId,
    group_id: u64,
    /// One queue per other member, sorted by id — a `Vec` rather than a map because this is on a
    /// decision path and its iteration order should be defined.
    peers: Vec<(NodeId, mpsc::Sender<Message>)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl PdTcpTransport {
    /// Connects to every member but `id`, spawning a task each.
    ///
    /// Must be called from inside a `tokio` runtime: the tasks are where the async lives, and the
    /// driver thread that feeds them is deliberately not async at all.
    pub fn spawn(id: NodeId, members: &MemberList, config: TransportConfig) -> Result<Arc<Self>> {
        let group_id = members.group_id();
        let mut peers = Vec::new();
        let mut tasks = Vec::new();
        for member in members.members().iter().filter(|member| member.id != id) {
            let address: SocketAddr = member.address.parse().map_err(|error| {
                PdError::invalid(format!(
                    "placement driver {}'s address `{}` is not an address: {error}",
                    member.id, member.address
                ))
            })?;
            let (sender, receiver) = mpsc::channel(MEMBER_SEND_QUEUE);
            peers.push((member.id, sender));
            tasks.push(tokio::spawn(deliver_to(
                member.id, address, group_id, id, receiver, config,
            )));
        }
        peers.sort_by_key(|(id, _)| *id);
        Ok(Arc::new(Self {
            id,
            group_id,
            peers,
            tasks,
        }))
    }

    /// This member's id.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The group whose traffic this carries.
    #[must_use]
    pub fn group_id(&self) -> u64 {
        self.group_id
    }

    /// Stops the delivery tasks. Idempotent, and called on drop.
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl PdTransport for PdTcpTransport {
    fn send(&self, messages: Vec<Message>) {
        for message in messages {
            let to = message.recipient();
            if to == self.id {
                // Raft talking to itself, which the core does not do and which would be a loop if
                // it did. Loud, because it would mean the configuration and the core disagree.
                tracing::error!(
                    id = self.id,
                    "a placement driver addressed a message to itself"
                );
                continue;
            }
            let Ok(at) = self.peers.binary_search_by_key(&to, |(id, _)| *id) else {
                // Membership is static, so this is a configuration mistake rather than a race:
                // the core is addressing a member this process was never told about.
                tracing::warn!(
                    id = self.id,
                    to,
                    "no address for a placement driver; the message was dropped"
                );
                continue;
            };
            if self.peers[at].1.try_send(message).is_err() {
                // Full or closed. Dropping is the honest outcome and Raft will resend; blocking
                // the driver thread on a slow socket would stall the whole group.
                tracing::debug!(
                    id = self.id,
                    to,
                    "the send queue is full or closed; the message was dropped"
                );
            }
        }
    }
}

impl Drop for PdTcpTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One member's connection: batch what has queued, send it, keep the socket if it worked.
async fn deliver_to(
    to: NodeId,
    address: SocketAddr,
    group_id: u64,
    from: NodeId,
    mut queue: mpsc::Receiver<Message>,
    config: TransportConfig,
) {
    let mut connection: Option<TcpTransport> = None;
    while let Some(first) = queue.recv().await {
        // Everything queued since the last wake-up travels together. Without it a group spends a
        // frame per heartbeat per tick, which is the same arithmetic that made a store's batching
        // worth having, one order of magnitude down.
        let mut messages = vec![first];
        while let Ok(next) = queue.try_recv() {
            messages.push(next);
        }

        if connection.as_ref().is_some_and(TcpTransport::is_closed) {
            connection = None;
        }
        if connection.is_none() {
            match TcpTransport::connect_with(address, config).await {
                Ok(transport) => connection = Some(transport),
                Err(error) => {
                    tracing::debug!(
                        to,
                        %error,
                        "could not reach a placement driver; its messages were dropped"
                    );
                    continue;
                }
            }
        }

        let Some(transport) = connection.as_ref() else {
            continue;
        };
        let count = messages.len();
        // **Cluster id zero, on purpose.** This frame is not a question about a cluster — the
        // group has to elect a leader before `Bootstrap` has minted a cluster id at all — so the
        // service exempts it from the cluster check and the group id is what guards it instead
        // ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)). Sending a real one would
        // imply a check that is not being made.
        let request = Request::Pd {
            cluster_id: 0,
            request: PdReq::Raft(PdRaftBatch::new(group_id, from, messages)),
        };
        if let Err(error) = transport.call(request).await {
            tracing::debug!(to, count, %error, "a placement-driver batch did not arrive");
            // The connection is suspect; the next batch builds a new one.
            connection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MEMBER_SEND_QUEUE, PdTcpTransport};
    use crate::driver::PdTransport;
    use crate::member::{MemberList, PdMember};
    use esker_proto::TransportConfig;
    use esker_raft::Message;

    fn three() -> MemberList {
        MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:32379"),
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:32381"),
        ])
        .unwrap()
    }

    /// A member connects to the others and not to itself: a queue for itself would be a loop the
    /// first heartbeat fell into.
    #[tokio::test]
    async fn a_member_holds_a_queue_for_every_member_but_itself() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        assert_eq!(transport.id(), 2);
        assert_eq!(
            transport
                .peers
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(transport.group_id(), three().group_id());
    }

    /// Every failure is a dropped message and a log line, never an error the consensus layer has
    /// to reason about — including the two that are configuration mistakes rather than races.
    #[tokio::test]
    async fn a_message_nobody_can_take_is_dropped_rather_than_returned() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        transport.send(vec![
            // To a member this process has never been told about.
            Message::TimeoutNow {
                from: 2,
                to: 9,
                term: 1,
            },
            // To itself.
            Message::TimeoutNow {
                from: 2,
                to: 2,
                term: 1,
            },
        ]);
    }

    /// A queue that filled would otherwise be a memory leak in front of a member that is gone.
    /// Nothing is listening on these ports, so every message piles up and then starts dropping —
    /// and `send` still returns.
    #[tokio::test]
    async fn a_full_queue_drops_rather_than_blocking_the_driver() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        for _ in 0..MEMBER_SEND_QUEUE * 4 {
            transport.send(vec![Message::TimeoutNow {
                from: 2,
                to: 1,
                term: 1,
            }]);
        }
    }

    /// A group of one has nobody to reach, and building a transport for it is not an error — it
    /// is what `Pd::open` does when nothing else is configured.
    #[tokio::test]
    async fn a_group_of_one_has_no_peers() {
        let transport =
            PdTcpTransport::spawn(1, &MemberList::alone(1), TransportConfig::new()).unwrap();
        assert!(transport.peers.is_empty());
    }
}
