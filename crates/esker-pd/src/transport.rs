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
//! [`crate::driver::PdTransport::send`] is infallible by design. Raft already
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
use std::sync::{Arc, Mutex};

use esker_proto::pd::{PdRaftBatch, PdReq};
use esker_proto::transport::RpcTls;
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

/// One connection per member pair, and the membership may move under it.
#[derive(Debug)]
pub struct PdTcpTransport {
    id: NodeId,
    group_id: u64,
    /// One entry per other member, sorted by id — a `Vec` rather than a map because this is on a
    /// decision path and its iteration order should be defined.
    ///
    /// Behind a lock because [`PdTcpTransport::reconfigure`] adds and drops entries while
    /// [`PdTransport::send`] is reading them. A `Mutex` rather than an `RwLock`: the critical
    /// section is a binary search over at most a handful of members, and a writer is an operator
    /// action rather than anything on a hot path.
    peers: Mutex<Vec<Peer>>,
    /// How connections are built, kept so a member added later gets the same ones.
    config: TransportConfig,
    /// The TLS a new member's link uses, kept for the same reason.
    tls: RpcTls,
    /// Where a new member's delivery task is spawned.
    ///
    /// **Held rather than taken from the ambient runtime**, because `reconfigure` is called from
    /// the *driver thread* — a plain OS thread with no runtime, where `tokio::spawn` is a panic.
    /// `spawn` captures the handle while it still has one.
    runtime: tokio::runtime::Handle,
}

/// One member's queue, and what it was built from.
#[derive(Debug)]
struct Peer {
    id: NodeId,
    /// The address its task is connected to. Kept so that a member whose *address* moved is
    /// noticed: the id alone would call that "unchanged" and leave a task dialling the old host.
    address: SocketAddr,
    queue: mpsc::Sender<Message>,
    task: tokio::task::JoinHandle<()>,
}

impl PdTcpTransport {
    /// Connects to every member but `id`, spawning a task each.
    ///
    /// Must be called from inside a `tokio` runtime: the tasks are where the async lives, and the
    /// driver thread that feeds them is deliberately not async at all.
    pub fn spawn(id: NodeId, members: &MemberList, config: TransportConfig) -> Result<Arc<Self>> {
        Self::spawn_with_tls(id, members, config, &RpcTls::disabled())
    }

    /// [`PdTcpTransport::spawn`], with the TLS every member link uses.
    ///
    /// A placement-driver group is the clearest case for mTLS in this project: every end is ours,
    /// the membership is static, and the traffic is the Raft log that decides where every region
    /// lives. A `tls` that is disabled connects in the clear, exactly as before.
    ///
    /// # Errors
    ///
    /// A member address that is not an address, as [`PdTcpTransport::spawn`].
    pub fn spawn_with_tls(
        id: NodeId,
        members: &MemberList,
        config: TransportConfig,
        tls: &RpcTls,
    ) -> Result<Arc<Self>> {
        let group_id = members.group_id();
        let transport = Self {
            id,
            group_id,
            peers: Mutex::new(Vec::new()),
            config,
            tls: tls.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        transport.reconfigure(members)?;
        Ok(Arc::new(transport))
    }

    /// Moves the connections to match `members`, keeping every link that did not change.
    ///
    /// **Add and drop, never rebuild.** A membership change touches one member; dropping the other
    /// connections with it would cost a reconnect and an election timeout's worth of silence on
    /// links nobody asked about. A member is "unchanged" only if both its id *and* its address are
    /// — an address that moved is a new link, because the old task is dialling the old host.
    ///
    /// Safe to call from the driver thread, which is where a configuration change is applied and
    /// which has no runtime of its own: the handle captured at [`PdTcpTransport::spawn`] is what
    /// the new tasks are spawned on.
    ///
    /// # Errors
    ///
    /// A member address that is not an address. The transport is left as it was: the addresses are
    /// parsed before anything is added or dropped, so a bad configuration cannot half-apply.
    pub fn reconfigure(&self, members: &MemberList) -> Result<()> {
        // Parsed first, so a bad address is an error rather than a transport with some of the
        // change applied. This is the whole reason the loop below cannot fail.
        let mut wanted = Vec::new();
        for member in members
            .members()
            .iter()
            .filter(|member| member.id != self.id)
        {
            let address: SocketAddr = member.address.parse().map_err(|error| {
                PdError::invalid(format!(
                    "placement driver {}'s address `{}` is not an address: {error}",
                    member.id, member.address
                ))
            })?;
            wanted.push((member.id, address));
        }
        wanted.sort_by_key(|(id, _)| *id);

        let mut peers = self.peers();
        // Anything no longer wanted, or wanted at a different address, goes: dropping the sender
        // ends the task's loop, and the abort is for a task parked mid-connect.
        peers.retain(|peer| {
            let keep = wanted
                .iter()
                .any(|(id, address)| *id == peer.id && *address == peer.address);
            if !keep {
                peer.task.abort();
                tracing::debug!(
                    id = self.id,
                    to = peer.id,
                    "dropped a placement-driver link"
                );
            }
            keep
        });

        for (id, address) in wanted {
            if peers.iter().any(|peer| peer.id == id) {
                continue;
            }
            let (queue, receiver) = mpsc::channel(MEMBER_SEND_QUEUE);
            let task = self.runtime.spawn(deliver_to(
                id,
                address,
                self.group_id,
                self.id,
                receiver,
                self.config,
                self.tls.clone(),
            ));
            peers.push(Peer {
                id,
                address,
                queue,
                task,
            });
            tracing::debug!(id = self.id, to = id, %address, "added a placement-driver link");
        }
        peers.sort_by_key(|peer| peer.id);
        Ok(())
    }

    /// The peer table.
    fn peers(&self) -> std::sync::MutexGuard<'_, Vec<Peer>> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let mut peers = self.peers();
        for peer in peers.iter() {
            peer.task.abort();
        }
        peers.clear();
    }
}

impl PdTransport for PdTcpTransport {
    /// The group changed; reach these members instead.
    ///
    /// **This is the method the driver calls**, and it has to be here rather than left to the
    /// trait's default. `PdCore::learn_routes` reaches a transport through [`PdTransport`], so a
    /// type that implemented only `send` would compile, pass every test of its own inherent
    /// `reconfigure`, and silently never rewire — a member added at run time would be in the
    /// configuration, in the log, and unreachable from every member that was already there.
    ///
    /// Infallible, like `send` and for the same reason. A failure costs reachability to one
    /// member, which Raft keeps retrying and an operator can fix by restarting this one; it is not
    /// a reason to fail an apply, because the log is the truth and this is a cache of where to
    /// send it.
    fn reconfigure(&self, members: &MemberList) {
        if let Err(error) = PdTcpTransport::reconfigure(self, members) {
            tracing::error!(id = self.id, %error, "could not rewire for the new group");
        }
    }

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
            let peers = self.peers();
            let Ok(at) = peers.binary_search_by_key(&to, |peer| peer.id) else {
                // Either a configuration mistake, or the one-`Ready` window a membership change
                // opens: a configuration is in force from the moment its entry is appended, and
                // this transport learns the address from `reconfigure` in the same `Ready`. Raft
                // retransmits, so the window costs a heartbeat rather than a member.
                tracing::warn!(
                    id = self.id,
                    to,
                    "no address for a placement driver; the message was dropped"
                );
                continue;
            };
            if peers[at].queue.try_send(message).is_err() {
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
    tls: RpcTls,
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
            // The peer is verified against its configured address, which is what a member list of
            // addresses can offer; a certificate for a placement driver therefore carries its IP.
            match TcpTransport::connect_with_tls(address, config, &tls, None).await {
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

    /// The members this transport currently holds a queue for.
    fn peer_ids(transport: &PdTcpTransport) -> Vec<esker_raft::NodeId> {
        transport.peers().iter().map(|peer| peer.id).collect()
    }

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
        assert_eq!(peer_ids(&transport), vec![1, 3]);
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
        assert!(peer_ids(&transport).is_empty());
    }

    /// A membership change adds and drops one link and **leaves the rest connected**.
    ///
    /// The assertion that matters is the last one: the queue for member 3 is the same channel it
    /// was before the change. A transport that rebuilt itself would pass every other assertion
    /// here and fail that one, and the cost — a reconnect and an election timeout of silence on a
    /// **The path the driver actually takes.**
    ///
    /// `PdCore::learn_routes` reaches this through [`PdTransport`], not through the inherent
    /// method above, and the trait's `reconfigure` has a **default no-op** — so a type that
    /// implements only `send` compiles, passes every test of its inherent method, and silently
    /// never rewires. The consequence is not subtle: a member added at run time is in the
    /// configuration and in the log, and no existing member ever opens a connection to it.
    #[tokio::test]
    async fn the_trait_method_is_the_one_the_driver_calls_and_it_rewires() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        assert_eq!(peer_ids(&transport), vec![1, 3]);

        let four = MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:32379"),
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:32381"),
            PdMember::new(4, "127.0.0.1:32382"),
        ])
        .unwrap();
        // Through the trait, exactly as the driver does — `PdTransport::reconfigure`, not
        // `PdTcpTransport::reconfigure`.
        PdTransport::reconfigure(&*transport, &four);
        assert_eq!(
            peer_ids(&transport),
            vec![1, 3, 4],
            "the trait's reconfigure did not reach the transport's; a member added at run time \
             would be unreachable from here"
        );
    }

    /// link nobody touched — would show up only under a membership change in production.
    #[tokio::test]
    async fn reconfigure_keeps_the_links_that_did_not_change() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        let kept = transport
            .peers()
            .iter()
            .find(|peer| peer.id == 3)
            .map(|peer| peer.queue.clone())
            .expect("member 3 has a queue");

        let four = MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:32379"),
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:32381"),
            PdMember::new(4, "127.0.0.1:32382"),
        ])
        .unwrap();
        transport.reconfigure(&four).unwrap();
        assert_eq!(peer_ids(&transport), vec![1, 3, 4], "4 was added");

        let two = MemberList::new(vec![
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:32381"),
        ])
        .unwrap();
        transport.reconfigure(&two).unwrap();
        assert_eq!(peer_ids(&transport), vec![3], "1 and 4 were dropped");

        let still = transport
            .peers()
            .iter()
            .find(|peer| peer.id == 3)
            .map(|peer| peer.queue.clone())
            .expect("member 3 still has one");
        assert!(
            kept.same_channel(&still),
            "member 3's link survived two changes it was not part of"
        );
    }

    /// A member whose **address** moved is a new link, not an unchanged one.
    ///
    /// Comparing ids alone would call this unchanged and leave a task dialling the old host for
    /// ever — the failure would look like one member being unreachable for no reason.
    #[tokio::test]
    async fn a_member_that_moved_gets_a_new_link() {
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        let before = transport
            .peers()
            .iter()
            .find(|peer| peer.id == 3)
            .map(|peer| peer.queue.clone())
            .unwrap();

        let moved = MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:32379"),
            PdMember::new(2, "127.0.0.1:32380"),
            PdMember::new(3, "127.0.0.1:42381"),
        ])
        .unwrap();
        transport.reconfigure(&moved).unwrap();

        let after = transport
            .peers()
            .iter()
            .find(|peer| peer.id == 3)
            .map(|peer| peer.queue.clone())
            .unwrap();
        assert!(
            !before.same_channel(&after),
            "an address change must replace the link, not keep it"
        );
        assert_eq!(peer_ids(&transport), vec![1, 3]);
    }

    /// A bad address cannot reach `reconfigure` at all — `MemberList` refuses to hold one.
    ///
    /// Which is where that guarantee belongs, and worth pinning: the parse inside `reconfigure` is
    /// therefore belt-and-braces rather than the check. It stays because the invariant lives in
    /// another module and a `Result` is what lets it stay honest if that ever changes; this test
    /// says why it is never seen.
    #[tokio::test]
    async fn a_bad_address_never_reaches_the_transport() {
        assert!(
            MemberList::new(vec![
                PdMember::new(2, "127.0.0.1:32380"),
                PdMember::new(3, "not-an-address"),
            ])
            .is_err(),
            "the member list is where an address is validated"
        );

        // And the transport it did build is untouched by the attempt.
        let transport = PdTcpTransport::spawn(2, &three(), TransportConfig::new()).unwrap();
        assert_eq!(peer_ids(&transport), vec![1, 3]);
    }
}
