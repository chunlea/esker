//! The three-node cluster the raw-KV chaos tests run against, and the client that talks to it.
//!
//! Shared by `chaos_linearizability.rs` and `refusals.rs` because both need the same thing: real
//! stores on real sockets, and a leader that can be taken away underneath a running client. It
//! is the raw-KV twin of `txn_cluster/mod.rs`, which does the same job for the transactional
//! tests and is not reusable here — that one builds a `TxnClient` around a timestamp oracle, and
//! these tests are about the layer underneath one.

#![allow(
    dead_code,
    reason = "each test file uses a different part of the harness"
)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};
use esker_proto::{Server, ServerHandle, TransportConfig};
use esker_raft::Role;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};
use tempfile::TempDir;

/// The one region every key in these tests falls in.
pub(crate) const REGION: u64 = 1;
/// The seed every node's election timer is drawn from.
pub(crate) const SEED: u64 = 20_260_830;

/// One store: its server and the directory that outlives it.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
}

/// A three-node cluster that can lose a node and get it back.
pub(crate) struct Cluster {
    runtime: tokio::runtime::Runtime,
    peers: Vec<PeerAddress>,
    pub(crate) addrs: Vec<SocketAddr>,
    dirs: Vec<TempDir>,
    /// `None` while that node is down.
    nodes: Vec<Mutex<Option<Node>>>,
    /// The reserved listener each node has not yet taken, in node-index order. See
    /// [`reserve_ports`]; emptied by that node's first start.
    reserved: Vec<Mutex<Option<std::net::TcpListener>>>,
}

/// Reserves `count` ports by binding and releasing them.
///
/// Every store has to know every peer's address before any server exists, so the addresses cannot
/// come from the servers.
///
/// **The listeners are held, not released.** This used to bind each port, read its number and drop
/// the listener, with a comment saying a released port is rebound microseconds later — and under a
/// parallel suite run something else takes it in that window and the rebind is `Address already in
/// use`, which surfaces as a panic in whatever test happened to be starting a cluster.
/// `esker-store`'s harnesses and this crate's `txn_cluster` both made this fix for that reason.
///
/// The reasoning that was here is still true of a *restart*: a killed node's port really is
/// released, and a peer that is briefly unreachable has its messages dropped and retried, which is
/// the transport's ordinary behaviour. What it did not cover is the first bind, where nothing had
/// gone wrong yet and the port was simply given away.
fn reserve_ports(count: usize) -> Vec<std::net::TcpListener> {
    (0..count)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect()
}

impl Cluster {
    pub(crate) fn start(count: usize) -> Arc<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let reserved = reserve_ports(count);
        let addrs: Vec<SocketAddr> = reserved
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        let peers: Vec<PeerAddress> = (0..count)
            .map(|at| PeerAddress::new(at as u64 + 1, at as u64 + 1, addrs[at]))
            .collect();
        let dirs: Vec<TempDir> = (0..count).map(|_| TempDir::new().unwrap()).collect();

        let cluster = Arc::new(Self {
            runtime,
            peers,
            addrs,
            dirs,
            nodes: (0..count).map(|_| Mutex::new(None)).collect(),
            reserved: reserved.into_iter().map(|l| Mutex::new(Some(l))).collect(),
        });
        for at in 0..count {
            cluster.start_node(at);
        }
        cluster
    }

    /// Starts node `at` on its own address and directory. A restart reopens the same database,
    /// which is the point: what it recovers is what it had made durable.
    pub(crate) fn start_node(&self, at: usize) {
        let id = at as u64 + 1;
        let mut raft = RaftOptions::new(self.peers.clone(), SEED);
        // Shorter than production's 100 ms so an election takes a fraction of a second. The
        // algorithm counts ticks, so nothing about it changes — but this file kills a node every
        // few hundred milliseconds, and a tick that is too short churns leadership on scheduler
        // noise alone.
        raft.tick = Duration::from_millis(25);
        let options = StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: REGION,
            raft: Some(raft),
            ..StoreOptions::new()
        };
        // Inside the runtime: opening a store spawns the transport's per-peer tasks, and
        // `tokio::spawn` needs a runtime to spawn onto.
        let store = {
            let _guard = self.runtime.enter();
            Store::open(self.dirs[at].path(), options).unwrap()
        };
        let addr = self.addrs[at];
        let service = StoreService::new(Arc::clone(&store));
        // The reservation on the first start, and a fresh bind on a restart — where the killed
        // node let the port go and nothing can hold it in the gap.
        let held = self.reserved[at].lock().unwrap().take();
        let handle = self.runtime.block_on(async move {
            let server = match held {
                Some(listener) => {
                    Server::from_listener(listener, service, TransportConfig::new()).unwrap()
                }
                None => Server::bind(addr, service, TransportConfig::new())
                    .await
                    .unwrap_or_else(|error| panic!("rebinding {addr} after a restart: {error}")),
            };
            server.spawn().unwrap()
        });
        *self.nodes[at].lock().unwrap() = Some(Node { store, handle });
    }

    /// Takes node `at` away: the server stops answering and the store stops replicating.
    ///
    /// This is the in-process stand-in for a `SIGKILL`. It is not one — a thread cannot be shot,
    /// and anything still inside the engine finishes — so what it proves is bounded: every
    /// acknowledged write is durable *and* recoverable across losing the process that
    /// acknowledged it. The unbounded version, with a real signal and a real process, is
    /// `esker-cli/tests/cluster_chaos.rs`.
    pub(crate) fn kill_node(&self, at: usize) {
        let node = self.nodes[at].lock().unwrap().take();
        if let Some(node) = node {
            node.store.stop();
            self.runtime.block_on(async {
                let _ = node.handle.shutdown().await;
            });
        }
    }

    /// The index of the node that believes it leads, if exactly one does.
    pub(crate) fn leader(&self) -> Option<usize> {
        for at in 0..self.nodes.len() {
            let guard = self.nodes[at].lock().unwrap();
            let Some(node) = guard.as_ref() else { continue };
            let Some(peer) = node.store.peer() else {
                continue;
            };
            let role = self.runtime.block_on(peer.status()).ok().map(|s| s.role);
            if role == Some(Role::Leader) {
                return Some(at);
            }
        }
        None
    }

    /// Waits until some node leads and every live node agrees, or the deadline passes.
    pub(crate) fn settle(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(at) = self.leader() {
                let id = at as u64 + 1;
                let agreed = (0..self.nodes.len()).all(|other| {
                    let guard = self.nodes[other].lock().unwrap();
                    guard.as_ref().is_none_or(|node| {
                        node.store
                            .peer()
                            .is_none_or(|peer| peer.leader() == Some(id))
                    })
                });
                if agreed {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub(crate) fn shutdown(&self) {
        for at in 0..self.nodes.len() {
            self.kill_node(at);
        }
    }
}

/// Connects a client to every store, retrying until the cluster has someone listening.
///
/// Rebuilt rather than repaired after a node is killed: `TcpStores` opens its connections once,
/// so a client that has lost one reconnects, which is what a real client does.
pub(crate) fn connect(addrs: &[SocketAddr], deadline: Instant) -> Option<RawClient> {
    while Instant::now() < deadline {
        if let Ok(stores) = TcpStores::connect_all(addrs, TransportConfig::new()) {
            let ids = stores.store_ids();
            return Some(RawClient::new(
                Arc::new(stores),
                Arc::new(StaticRegion::replicated(REGION, &ids)),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}
