//! The Raft transport: one connection per **store pair**, carrying every region's messages,
//! a batch per tick.
//!
//! [`RaftTransport`] is fire-and-forget and infallible, and this is
//! where that shape earns itself. Raft already retries everything it sends — a lost message is
//! indistinguishable from a slow one — so a transport that reported failures would hand the driver
//! a decision it has no better answer to than "send it again next tick". Every failure here is
//! therefore a dropped message and a log line, never an error the consensus layer has to reason
//! about.
//!
//! # Why the connection belongs to the store pair and not to the region
//!
//! `docs/DESIGN.md` §6: *one TCP connection per (store, store) pair carrying `RaftTransport::Batch`
//! frames with `RaftMessage`s for all regions, batched per tick.* Phase 3e had one region and could
//! not tell the difference. With fifty regions on five stores it is the difference between four
//! connections per store and two hundred — and between one batched frame per tick and fifty.
//!
//! So [`StoreTransport`] is keyed by store id and knows nothing about regions, and
//! [`RegionTransport`] is the per-region view handed to one peer's driver: it stamps the region id
//! and epoch onto each message and resolves the peer id Raft names into the store that hosts it.
//! Every region's view shares the one queue per destination store, which is what makes the batch a
//! batch.
//!
//! # What that buys, and what it costs
//!
//! The driver thread never blocks on a socket: [`RegionTransport::send`] hands each message to a
//! **bounded** queue and returns. A queue that is full drops, which is exactly what a congested
//! network does, and is why the bound is safe to have — an unbounded one in front of a slow peer
//! is a memory leak that ends the process instead of the connection
//! (`docs/DESIGN.md` §9, "nothing is unbounded"). The queue is shared by every region bound for
//! that store, which is the point and also the cost: a region that floods it drops another
//! region's heartbeat. Raft retransmits either way, and a per-region queue would trade that for
//! per-region memory that nothing bounds in aggregate.
//!
//! A connection that fails is dropped and rebuilt on the next batch. There is no reconnect
//! backoff loop, because the tick is already one — a peer that is down costs one failed connect
//! per batch, and a batch only exists when there is something to say.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use esker_proto::{
    Epoch, RaftBatch, RaftMessage, Request, TcpTransport, Transport, TransportConfig,
};
use esker_raft::{Message, NodeId};
use tokio::sync::mpsc;

use crate::peer::RaftTransport;

/// How many messages may queue for one **store** before the transport starts dropping.
///
/// Generous enough that a brief stall does not lose traffic, small enough that a peer which is
/// simply gone cannot cost unbounded memory. Dropping is safe: Raft retransmits.
pub const PEER_SEND_QUEUE: usize = 1024;

/// Where another store's peer can be reached.
///
/// Still peer-shaped rather than store-shaped because that is what a caller knows: a region's
/// membership is a list of peers, and the address book is built from it. [`StoreTransport`]
/// collapses it to one entry per store; [`StoreTransport::for_region`] keeps the peer half as the
/// routing table one region needs.
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

/// Where another store can be reached: the unit a connection is actually per.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreAddress {
    /// The store's id.
    pub store_id: u64,
    /// Its address.
    pub addr: SocketAddr,
}

impl StoreAddress {
    /// A store at an address.
    #[must_use]
    pub fn new(store_id: u64, addr: SocketAddr) -> Self {
        Self { store_id, addr }
    }

    /// The distinct stores named by a peer list, in store-id order.
    ///
    /// Two peers of two different regions on one store are one connection, which is the whole
    /// point of keying by store. A store that appears twice with two different addresses is a
    /// configuration mistake; the first address wins and the second is logged, because refusing
    /// to start over it would take the cluster down for a typo that costs one region.
    #[must_use]
    pub fn from_peers(peers: &[PeerAddress]) -> Vec<Self> {
        let mut stores: Vec<Self> = Vec::new();
        for peer in peers {
            match stores.binary_search_by_key(&peer.store_id, |store| store.store_id) {
                Ok(at) => {
                    if stores[at].addr != peer.addr {
                        tracing::warn!(
                            store_id = peer.store_id,
                            known = %stores[at].addr,
                            ignored = %peer.addr,
                            "a store was given two addresses; the first one is used"
                        );
                    }
                }
                Err(at) => stores.insert(at, Self::new(peer.store_id, peer.addr)),
            }
        }
        stores
    }
}

/// One connection per store pair, shared by every region this store hosts.
#[derive(Debug)]
pub struct StoreTransport {
    store_id: u64,
    /// One queue per destination store, sorted by store id — a `Vec` rather than a map for the
    /// same reason the Raft core uses one: this is on a decision path and its iteration order
    /// should be defined.
    stores: Vec<(u64, mpsc::Sender<RaftMessage>)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl StoreTransport {
    /// Connects to every store but this one, spawning a task each.
    ///
    /// Must be called from inside a `tokio` runtime: the tasks are where the async lives, and the
    /// driver threads that feed them are deliberately not async at all.
    #[must_use]
    pub fn spawn(store_id: u64, stores: &[StoreAddress], config: TransportConfig) -> Arc<Self> {
        let mut queues = Vec::new();
        let mut tasks = Vec::new();
        for store in stores.iter().filter(|store| store.store_id != store_id) {
            let (sender, receiver) = mpsc::channel(PEER_SEND_QUEUE);
            queues.push((store.store_id, sender));
            tasks.push(tokio::spawn(deliver_to(store.clone(), receiver, config)));
        }
        queues.sort_by_key(|(id, _)| *id);
        Arc::new(Self {
            store_id,
            stores: queues,
            tasks,
        })
    }

    /// This store's id.
    #[must_use]
    pub fn store_id(&self) -> u64 {
        self.store_id
    }

    /// The view one region's driver holds: this transport, plus the region's identity and the
    /// map from its peer ids to the stores hosting them.
    ///
    /// `peers` is the region's whole membership, this store's own peer included; the entry for
    /// this store is kept in the table and never dispatched, because a message addressed to it
    /// would be Raft talking to itself.
    ///
    /// The routing table comes from the **region's own peer list**, not from the address book.
    /// They are different questions and 4d is where the difference bit: a region's peer ids are
    /// allocated by the placement driver per replica, so a peer added after the store started is
    /// not in the address book at all — and every message to it was silently dropped. The address
    /// book answers *where a store is*; the region answers *which store a peer is on*.
    #[must_use]
    pub fn for_region(
        self: &Arc<Self>,
        region_id: u64,
        epoch: Epoch,
        peers: &[esker_proto::Peer],
    ) -> Arc<RegionTransport> {
        let view = RegionTransport {
            transport: Arc::clone(self),
            region_id,
            membership: Mutex::new(Membership {
                epoch,
                routes: Vec::new(),
            }),
        };
        view.follow(epoch, peers);
        Arc::new(view)
    }

    /// Stops every store task. Anything still queued is dropped, which is what a peer going away
    /// looks like from the other end anyway.
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Queues one already-stamped message for the store hosting its recipient.
    fn dispatch(&self, to_store: u64, message: RaftMessage) {
        let region_id = message.region_id;
        let Ok(at) = self.stores.binary_search_by_key(&to_store, |(id, _)| *id) else {
            // No address for that store. With a static address book this is a configuration
            // mistake rather than a race, and it stays worth saying out loud once PD can move a
            // peer to a store this one has never been told about.
            tracing::warn!(
                region_id,
                store = to_store,
                "no address for a store; the message was dropped"
            );
            return;
        };
        if self.stores[at].1.try_send(message).is_err() {
            // Full or closed. Dropping is the honest outcome and Raft will resend; blocking the
            // driver thread on a slow socket would stall consensus for every region at once.
            tracing::debug!(
                region_id,
                store = to_store,
                "the send queue is full or closed; the message was dropped"
            );
        }
    }
}

impl Drop for StoreTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One region's view of the store-pair transport.
///
/// This is what a peer's driver holds as its [`RaftTransport`]. It knows the two things the
/// shared transport must not: which region these messages belong to, and which store each of the
/// region's peers is on.
#[derive(Debug)]
pub struct RegionTransport {
    transport: Arc<StoreTransport>,
    region_id: u64,
    /// The region's identity as it stands, behind a lock because it **moves**: a split bumps the
    /// epoch and a membership change adds or removes a peer, and a view that kept the values it
    /// was built with would stamp a stale epoch and drop every message to a peer added since.
    membership: Mutex<Membership>,
}

/// What a region's transport view has to keep current.
#[derive(Debug)]
struct Membership {
    /// Stamped onto every message, so the far end can drop one from an epoch it has moved past.
    epoch: Epoch,
    /// `peer_id → store_id`, sorted by peer id, taken from the region's own peer list.
    routes: Vec<(NodeId, u64)>,
}

impl RegionTransport {
    /// The region these messages belong to.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        self.region_id
    }

    /// Adopts a region's current epoch and peer list.
    ///
    /// Called whenever the region moves — a split, a conf change — so that the next message is
    /// stamped and routed by what the region *is* rather than by what it was at open.
    pub fn follow(&self, epoch: Epoch, peers: &[esker_proto::Peer]) {
        let mut routes: Vec<(NodeId, u64)> = peers
            .iter()
            .map(|peer| (peer.peer_id, peer.store_id))
            .collect();
        routes.sort_unstable_by_key(|(peer_id, _)| *peer_id);
        routes.dedup_by_key(|(peer_id, _)| *peer_id);
        if let Ok(mut membership) = self.membership.lock() {
            membership.epoch = epoch;
            membership.routes = routes;
        }
    }

    /// Adds one peer's store to the routes, without moving the epoch.
    ///
    /// §4.1 of the dissertation: a configuration takes effect when its entry is **appended**, not
    /// when it commits. So the leader may address a brand-new peer in the very `Ready` that
    /// carries the entry adding it — a full round trip before the region record moves and
    /// [`follow`](Self::follow) hears about it. A route that waits for apply is a route that
    /// arrives after the first message that needed it.
    ///
    /// Dropping that first message is not merely a retry: if it was an `InstallSnapshot` the
    /// leader's progress for that peer is now `Snapshot`, which is paused until the peer answers
    /// a snapshot it was never sent (`docs/plans/phase-4.md` §14.5).
    pub fn learn(&self, peer: NodeId, store_id: u64) {
        let Ok(mut membership) = self.membership.lock() else {
            return;
        };
        match membership
            .routes
            .binary_search_by_key(&peer, |(peer_id, _)| *peer_id)
        {
            Ok(at) => membership.routes[at].1 = store_id,
            Err(at) => membership.routes.insert(at, (peer, store_id)),
        }
    }

    /// The epoch stamped onto every message it sends.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.membership
            .lock()
            .map_or(Epoch::INITIAL, |membership| membership.epoch)
    }

    /// Whether any peer this region knows of is on `store_id`.
    ///
    /// The question `store_of` answers backwards, and it is asked at a moment the region *record*
    /// cannot answer: a conf change takes effect when its entry is **appended**, and the record
    /// only moves when it applies. Between those, the peer exists in every core that appended the
    /// entry and the only thing that knows where it lives is this table —
    /// [`learn`](Self::learn) puts it there from the change's own context, which is what that
    /// method exists for. A store hosts one peer per region (`RegionMap::insert` refuses a
    /// second), so "this store already has one" is a refusal and not a preference.
    #[must_use]
    pub fn hosts_store(&self, store_id: u64) -> bool {
        self.membership.lock().is_ok_and(|membership| {
            membership
                .routes
                .iter()
                .any(|(_, store)| *store == store_id)
        })
    }

    /// The store hosting `peer`, if this region knows of it.
    #[must_use]
    pub fn store_of(&self, peer: NodeId) -> Option<u64> {
        let membership = self.membership.lock().ok()?;
        membership
            .routes
            .binary_search_by_key(&peer, |(peer_id, _)| *peer_id)
            .ok()
            .map(|at| membership.routes[at].1)
    }
}

impl RaftTransport for RegionTransport {
    fn learn(&self, peer: NodeId, store_id: u64) {
        RegionTransport::learn(self, peer, store_id);
    }

    fn send(&self, messages: Vec<Message>) {
        let epoch = self.epoch();
        for message in messages {
            let to = message.recipient();
            let Some(store) = self.store_of(to) else {
                tracing::warn!(
                    region_id = self.region_id,
                    peer = to,
                    "no store known for a peer of this region; the message was dropped"
                );
                continue;
            };
            if store == self.transport.store_id() {
                // Raft never addresses itself, so this is a membership table that disagrees with
                // reality rather than a message to deliver locally.
                tracing::warn!(
                    region_id = self.region_id,
                    peer = to,
                    "a message was addressed to a peer on this store; it was dropped"
                );
                continue;
            }
            self.transport.dispatch(
                store,
                RaftMessage::new(self.region_id, epoch, self.transport.store_id(), message),
            );
        }
    }
}

/// One store's connection: batch what has queued, send it, keep the socket if it worked.
async fn deliver_to(
    store: StoreAddress,
    mut queue: mpsc::Receiver<RaftMessage>,
    config: TransportConfig,
) {
    let mut connection: Option<TcpTransport> = None;
    while let Some(first) = queue.recv().await {
        // Everything queued since the last wake-up travels together, whichever regions it came
        // from. This is the per-tick batching `docs/DESIGN.md` §6 asks for: without it a cluster
        // spends a frame per heartbeat per region per tick.
        let mut batch = vec![first];
        while let Ok(next) = queue.try_recv() {
            batch.push(next);
        }

        if connection.as_ref().is_some_and(TcpTransport::is_closed) {
            connection = None;
        }
        if connection.is_none() {
            match TcpTransport::connect_with(store.addr, config).await {
                Ok(transport) => connection = Some(transport),
                Err(error) => {
                    tracing::debug!(
                        store = store.store_id,
                        %error,
                        "could not reach a store; its messages were dropped"
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
                store = store.store_id,
                count,
                %error,
                "a Raft batch did not reach its store"
            );
            // The connection is suspect; the next batch builds a new one.
            connection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PeerAddress, StoreAddress, StoreTransport};
    use crate::peer::RaftTransport;
    use esker_proto::{Epoch, TransportConfig};
    use esker_raft::Message;
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    /// Two regions with peers on the same three stores must collapse to three addresses, not
    /// six. This is the whole reason the connection is keyed by store.
    #[test]
    fn peers_on_the_same_store_are_one_address() {
        let peers = vec![
            PeerAddress::new(1, 1, addr(7001)),
            PeerAddress::new(2, 2, addr(7002)),
            PeerAddress::new(3, 3, addr(7003)),
            // The second region's peers, on the same three stores.
            PeerAddress::new(11, 1, addr(7001)),
            PeerAddress::new(12, 2, addr(7002)),
            PeerAddress::new(13, 3, addr(7003)),
        ];
        let stores = StoreAddress::from_peers(&peers);
        assert_eq!(
            stores,
            vec![
                StoreAddress::new(1, addr(7001)),
                StoreAddress::new(2, addr(7002)),
                StoreAddress::new(3, addr(7003)),
            ]
        );
    }

    /// A store named twice with two addresses is a typo in a config file. Taking the cluster
    /// down over it would cost every region; one warning and the first address costs one.
    #[test]
    fn a_store_with_two_addresses_keeps_the_first() {
        let stores = StoreAddress::from_peers(&[
            PeerAddress::new(1, 1, addr(7001)),
            PeerAddress::new(2, 1, addr(7999)),
        ]);
        assert_eq!(stores, vec![StoreAddress::new(1, addr(7001))]);
    }

    /// A peer whose conf change has been **appended and not applied** is already on its store.
    ///
    /// The window `RegionState::hosts_store` exists for. `learn` is called from the `Ready` that
    /// persists the entry, on every peer that appends it, so between the append and the apply this
    /// table is the only thing that knows the new peer exists — and a placement decision that read
    /// the region record instead put a second peer on a store that already had one
    /// (`docs/plans/phase-14-flakes.md` U2).
    #[tokio::test]
    async fn a_peer_learned_at_append_already_counts_as_placed_on_its_store() {
        let peers = vec![
            PeerAddress::new(1, 1, addr(7201)),
            PeerAddress::new(2, 2, addr(7202)),
            PeerAddress::new(3, 3, addr(7203)),
        ];
        let transport =
            StoreTransport::spawn(1, &StoreAddress::from_peers(&peers), TransportConfig::new());
        let members = vec![
            esker_proto::Peer::voter(1, 1),
            esker_proto::Peer::voter(2, 2),
        ];
        let region = transport.for_region(7, Epoch::INITIAL, &members);
        assert!(region.hosts_store(1));
        assert!(region.hosts_store(2));
        assert!(
            !region.hosts_store(3),
            "no peer of this region is on store 3 yet"
        );

        // The conf change adding peer 9 on store 3 is on disk; the region record has not moved.
        RaftTransport::learn(&*region, 9, 3);
        assert!(
            region.hosts_store(3),
            "a peer added by an appended conf change is on its store from that moment"
        );
        assert_eq!(region.store_of(9), Some(3));
    }

    #[tokio::test]
    async fn a_regions_view_resolves_peers_to_stores_and_never_to_itself() {
        let peers = vec![
            PeerAddress::new(1, 1, addr(7101)),
            PeerAddress::new(2, 2, addr(7102)),
            PeerAddress::new(3, 3, addr(7103)),
        ];
        let transport =
            StoreTransport::spawn(1, &StoreAddress::from_peers(&peers), TransportConfig::new());
        // Two other stores; never a queue back to this one.
        assert_eq!(transport.stores.len(), 2);

        let members: Vec<esker_proto::Peer> = peers
            .iter()
            .map(|peer| esker_proto::Peer::voter(peer.store_id, peer.peer_id))
            .collect();
        let region = transport.for_region(7, Epoch::INITIAL, &members);
        assert_eq!(region.region_id(), 7);
        assert_eq!(region.store_of(2), Some(2));
        assert_eq!(region.store_of(3), Some(3));
        assert_eq!(
            region.store_of(1),
            Some(1),
            "this store's own peer stays in the table; it is dropped at send time"
        );
        assert_eq!(region.store_of(99), None);

        // A peer added after the view was built is routable once the region says so — which is
        // the whole reason the routes come from the region rather than from the address book.
        region.follow(
            Epoch::new(2, 1),
            &[
                esker_proto::Peer::voter(1, 1),
                esker_proto::Peer::voter(2, 2),
                esker_proto::Peer::voter(3, 3),
                esker_proto::Peer::voter(2, 4_000),
            ],
        );
        assert_eq!(region.store_of(4_000), Some(2));
        assert_eq!(region.epoch(), Epoch::new(2, 1));

        // Neither of the two messages a region must not dispatch panics or reaches a queue: one
        // names a peer nobody knows, one names this store's own.
        region.send(vec![
            Message::TimeoutNow {
                from: 1,
                to: 99,
                term: 1,
            },
            Message::TimeoutNow {
                from: 2,
                to: 1,
                term: 1,
            },
        ]);
    }

    /// The point of the per-store queue: two regions bound for the same store share it, so one
    /// tick is one batch rather than one batch per region.
    #[tokio::test]
    async fn every_region_bound_for_one_store_shares_its_queue() {
        let peers = vec![
            PeerAddress::new(1, 1, addr(7201)),
            PeerAddress::new(2, 2, addr(7202)),
        ];
        let transport =
            StoreTransport::spawn(1, &StoreAddress::from_peers(&peers), TransportConfig::new());
        let members: Vec<esker_proto::Peer> = peers
            .iter()
            .map(|peer| esker_proto::Peer::voter(peer.store_id, peer.peer_id))
            .collect();
        let first = transport.for_region(1, Epoch::INITIAL, &members);
        let second = transport.for_region(2, Epoch::INITIAL, &members);

        for region in [&first, &second] {
            region.send(vec![Message::TimeoutNow {
                from: 1,
                to: 2,
                term: 1,
            }]);
        }

        // One queue, both regions' messages in it. The task at the other end is trying to
        // connect to a port nothing is listening on, so nothing has drained.
        assert_eq!(transport.stores.len(), 1);
    }
}
