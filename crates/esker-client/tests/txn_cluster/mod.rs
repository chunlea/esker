//! A real transactional cluster: several regions, each replicated by its own Raft group, one
//! oracle shared by every client, and a switch that takes a node away.
//!
//! `tests/chaos_linearizability.rs` builds the same shape for `RawKv` and one region. Phase 5
//! needs two things that one does not have, and both are load-bearing:
//!
//! * **More than one region, replicated.** A transaction's primary and its secondaries have to
//!   land in *different Raft groups*, or "the commit was lost on a secondary" is not a state
//!   the cluster can reach: one group's log commits every key of the transaction at once. The
//!   whole of Percolator's roll-forward exists for the case where they are separate facts.
//! * **An oracle whose timestamps carry real milliseconds.** A lock's lease is judged in the
//!   *physical* half of a timestamp (`docs/txn-spec.md` §5.5), so an oracle that counts by one
//!   mints a million timestamps inside the same millisecond and no lock ever expires. A test
//!   whose crashed clients leave locks that nothing may clean up is not testing what it thinks.
//!
//! So the oracle here is **PD's own** ([`esker_pd::tso::Oracle`]) rather than a second
//! implementation of the same idea: one instance, shared by every client thread, driven by a
//! real clock. `CountingOracle` is correct for one client and wrong for two
//! (`CLAUDE.md` invariant 6), and both of its failures — a repeated timestamp and a frozen
//! physical clock — are exactly what these tests are about.
//!
//! # What a kill is, and is not
//!
//! [`Cluster::kill`] stops a node's server and its store: it stops answering, stops
//! replicating, and comes back on the same directory, so what it recovers is what it had made
//! durable. It is the in-process stand-in for a `SIGKILL`, with the same bound
//! `chaos_linearizability.rs` states — a thread cannot be shot, so anything already inside the
//! engine finishes. The unbounded version, with a real signal and a real process, is
//! `esker-cli/tests/cluster_chaos.rs`.
//!
//! A killed node is also this harness's **partition**: a node that answers nobody is, to every
//! other node, on the far side of a cut. What it does not model is a *symmetric* partition that
//! leaves both halves running, which needs control over the transport rather than over the
//! process.

#![allow(
    dead_code,
    reason = "each test file uses a different part of the harness"
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::wire::ProtoError;
use esker_client::{TcpStores, TimestampOracle, TxnClient};
use esker_proto::{Epoch, Peer, Region, Server, ServerHandle, TransportConfig};
use esker_raft::Role;
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};
use tempfile::TempDir;

/// How the key space is divided, and how many stores hold each part.
#[derive(Debug, Clone)]
pub(crate) struct Topology {
    /// The keys the regions are divided at. `n` boundaries make `n + 1` regions.
    pub(crate) boundaries: Vec<&'static [u8]>,
    /// Stores per region. Three is the smallest that survives losing one.
    pub(crate) replicas: usize,
    /// The seed every node's election timer is drawn from.
    pub(crate) seed: u64,
}

impl Topology {
    /// Two regions divided at `m`, each replicated three ways: six stores.
    ///
    /// `m` is the divider the phase-5 tests are written around — accounts live under `acct/`
    /// and everything that witnesses a transfer under `wit/`, so a transaction that touches
    /// both has its primary in one Raft group and a secondary in another.
    #[must_use]
    pub(crate) fn two_regions(seed: u64) -> Self {
        Self {
            boundaries: vec![b"m"],
            replicas: 3,
            seed,
        }
    }

    /// One store per region: no replication, so nothing survives a kill.
    ///
    /// For the runs that are about *transactions* rather than about Raft — a thousand seeds of
    /// contention and crashed clients, where six engines per seed would be the whole cost.
    #[must_use]
    pub(crate) fn unreplicated(seed: u64) -> Self {
        Self {
            boundaries: vec![b"m"],
            replicas: 1,
            seed,
        }
    }

    fn regions(&self) -> usize {
        self.boundaries.len() + 1
    }

    fn nodes(&self) -> usize {
        self.regions() * self.replicas
    }
}

/// PD's oracle, shared by every client in the process.
///
/// Not a re-implementation: [`esker_pd::tso::Oracle`] is a pure state machine precisely so that
/// something other than a PD server can drive it, and driving it here means the timestamps
/// these tests run on are composed by the code production composes them with. The mark it asks
/// to persist is dropped — this is one process and a restart of it is not what is under test —
/// and that is the one thing a real PD does that this does not.
#[derive(Debug)]
pub(crate) struct SharedOracle {
    state: Mutex<esker_pd::tso::Oracle>,
    /// Wall-clock milliseconds at construction, and the monotonic instant they were read at:
    /// the physical half has to advance with real time or no lease ever runs out.
    epoch_ms: u64,
    started: Instant,
    /// Timestamps handed out, so a test can report how much oracle traffic a run made.
    issued: AtomicU64,
}

impl SharedOracle {
    #[must_use]
    pub(crate) fn new() -> Self {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0));
        Self {
            state: Mutex::new(esker_pd::tso::Oracle::load(None, epoch_ms, 3_000)),
            epoch_ms,
            started: Instant::now(),
            issued: AtomicU64::new(0),
        }
    }

    /// The millisecond the oracle would stamp a timestamp with right now.
    #[must_use]
    pub(crate) fn now_ms(&self) -> u64 {
        self.epoch_ms
            .saturating_add(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    #[must_use]
    pub(crate) fn issued(&self) -> u64 {
        self.issued.load(Ordering::Relaxed)
    }

    /// One timestamp, for a test driving the phases of a transaction by hand.
    ///
    /// The same call [`TimestampOracle::timestamp`] makes, as an inherent method so a caller
    /// does not have to bring the trait into scope to ask for a number.
    #[must_use]
    pub(crate) fn tso_one(&self) -> u64 {
        self.tso(1).expect("the oracle answers")
    }
}

impl Default for SharedOracle {
    fn default() -> Self {
        Self::new()
    }
}

impl TimestampOracle for SharedOracle {
    fn tso(&self, count: u32) -> Result<u64, ProtoError> {
        let now = self.now_ms();
        let mut oracle = self
            .state
            .lock()
            .map_err(|_| ProtoError::invalid("the oracle's lock was poisoned"))?;
        // The persist callback is where a real PD makes its high-water mark durable before
        // handing out a timestamp above it. Nothing here restarts, so there is nothing to be
        // durable against, and swallowing it keeps that difference in one visible place.
        let ts = oracle
            .allocate(count.max(1), now, |_mark| Ok(()))
            .map_err(|error| ProtoError::invalid(error.to_string()))?;
        self.issued
            .fetch_add(u64::from(count.max(1)), Ordering::Relaxed);
        Ok(ts)
    }
}

/// One store: its server and the handle that stops it.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
}

/// A cluster of `regions × replicas` stores, addressable and killable by node index.
pub(crate) struct Cluster {
    runtime: tokio::runtime::Runtime,
    topology: Topology,
    /// The address book, one entry per node, in node-index order.
    addrs: Vec<SocketAddr>,
    /// Each region's address book, for the Raft groups.
    peers: Vec<Vec<PeerAddress>>,
    dirs: Vec<TempDir>,
    /// `None` while that node is down.
    nodes: Vec<Mutex<Option<Node>>>,
    /// The reserved listener each node has not yet taken, in node-index order.
    ///
    /// Emptied by the first `start_node` for that index. A **restart** finds `None` and binds the
    /// address again, which is the one window a reservation cannot close: the killed server let
    /// the port go, and nothing can hold it in the gap.
    reserved: Vec<Mutex<Option<std::net::TcpListener>>>,
    resolver: Arc<dyn RegionResolver>,
    oracle: Arc<SharedOracle>,
}

impl Cluster {
    /// Starts every node and returns once they are running — not once they have elected.
    pub(crate) fn start(topology: Topology) -> Arc<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime");
        let reserved = reserve_ports(topology.nodes());
        let addrs: Vec<SocketAddr> = reserved
            .iter()
            .map(|listener| listener.local_addr().expect("its address"))
            .collect();
        let peers: Vec<Vec<PeerAddress>> = (0..topology.regions())
            .map(|group| {
                (0..topology.replicas)
                    .map(|replica| {
                        let at = group * topology.replicas + replica;
                        let id = at as u64 + 1;
                        PeerAddress::new(id, id, addrs[at])
                    })
                    .collect()
            })
            .collect();
        let dirs: Vec<TempDir> = (0..topology.nodes())
            .map(|_| TempDir::new().expect("a temporary directory"))
            .collect();
        let resolver = routing(&topology, &peers);

        let cluster = Arc::new(Self {
            runtime,
            nodes: (0..topology.nodes()).map(|_| Mutex::new(None)).collect(),
            reserved: reserved.into_iter().map(|l| Mutex::new(Some(l))).collect(),
            topology,
            addrs,
            peers,
            dirs,
            resolver,
            oracle: Arc::new(SharedOracle::new()),
        });
        for at in 0..cluster.topology.nodes() {
            cluster.start_node(at);
        }
        cluster
    }

    /// The region group node `at` belongs to.
    #[must_use]
    pub(crate) fn group_of(&self, at: usize) -> usize {
        at / self.topology.replicas
    }

    #[must_use]
    pub(crate) fn regions(&self) -> usize {
        self.topology.regions()
    }

    #[must_use]
    pub(crate) fn nodes(&self) -> usize {
        self.topology.nodes()
    }

    #[must_use]
    pub(crate) fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    #[must_use]
    pub(crate) fn oracle(&self) -> &Arc<SharedOracle> {
        &self.oracle
    }

    /// Starts node `at` on its own address and directory, reopening whatever it had.
    pub(crate) fn start_node(&self, at: usize) {
        let id = at as u64 + 1;
        let group = self.group_of(at);
        let mut raft = RaftOptions::new(self.peers[group].clone(), self.topology.seed + id);
        // Shorter than production's tick so an election takes a fraction of a second, for the
        // reason `chaos_linearizability.rs` gives: the algorithm counts ticks, so nothing about
        // it changes, and a run that kills a node every few hundred milliseconds would
        // otherwise spend most of its time with nobody leading.
        raft.tick = Duration::from_millis(25);
        let options = StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: group as u64 + 1,
            raft: (self.topology.replicas > 1).then_some(raft),
            ..StoreOptions::new()
        };
        // Inside the runtime: opening a store spawns the transport's per-peer tasks.
        let store = {
            let _guard = self.runtime.enter();
            Store::open(self.dirs[at].path(), options).expect("the store opens")
        };
        let addr = self.addrs[at];
        let service = StoreService::new(Arc::clone(&store));
        // The reservation if this node has not started before, and a fresh bind if it is coming
        // back from a kill — see `Cluster::reserved`.
        let held = self.reserved[at]
            .lock()
            .expect("the reservation lock")
            .take();
        let handle = self.runtime.block_on(async move {
            let server = match held {
                Some(listener) => Server::from_listener(listener, service, TransportConfig::new())
                    .expect("the reserved listener is adopted"),
                None => Server::bind(addr, service, TransportConfig::new())
                    .await
                    .unwrap_or_else(|error| {
                        panic!("the server binds {addr} after a restart: {error}")
                    }),
            };
            server.spawn().expect("the server starts")
        });
        *self.nodes[at].lock().expect("the node lock") = Some(Node { store, handle });
    }

    /// Takes node `at` away. See this module's header for what that models.
    pub(crate) fn kill(&self, at: usize) {
        let node = self.nodes[at].lock().expect("the node lock").take();
        if let Some(node) = node {
            node.store.stop();
            self.runtime.block_on(async {
                let _ = node.handle.shutdown().await;
            });
        }
    }

    /// The node index that believes it leads region group `group`, if one does.
    #[must_use]
    pub(crate) fn leader_of(&self, group: usize) -> Option<usize> {
        let first = group * self.topology.replicas;
        for at in first..first + self.topology.replicas {
            let guard = self.nodes[at].lock().expect("the node lock");
            let Some(node) = guard.as_ref() else { continue };
            let Some(peer) = node.store.peer() else {
                // An unreplicated store has no Raft peer and always answers for itself.
                return Some(at);
            };
            let role = self.runtime.block_on(peer.status()).ok().map(|s| s.role);
            if role == Some(Role::Leader) {
                return Some(at);
            }
        }
        None
    }

    /// Waits until every region has a leader that its live peers agree on.
    pub(crate) fn settle(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if (0..self.topology.regions()).all(|group| self.group_settled(group)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn group_settled(&self, group: usize) -> bool {
        let Some(at) = self.leader_of(group) else {
            return false;
        };
        let id = at as u64 + 1;
        let first = group * self.topology.replicas;
        (first..first + self.topology.replicas).all(|other| {
            let guard = self.nodes[other].lock().expect("the node lock");
            guard.as_ref().is_none_or(|node| {
                node.store
                    .peer()
                    .is_none_or(|peer| peer.leader() == Some(id))
            })
        })
    }

    /// A client with its own connections, sharing the cluster's one oracle.
    ///
    /// Rebuilt rather than repaired after a kill, for the reason `chaos_linearizability.rs`
    /// gives: `TcpStores` opens its connections once, so a client that has lost the store it
    /// was talking to has to build a new book to find another.
    pub(crate) fn client(&self, jitter_seed: u64) -> Option<TxnClient> {
        Some(TxnClient::on_router(
            self.router(jitter_seed)?,
            Arc::clone(&self.oracle) as Arc<dyn TimestampOracle>,
        ))
    }

    /// The routing half of [`Cluster::client`], for a test that drives the phases by hand.
    pub(crate) fn router(&self, jitter_seed: u64) -> Option<Arc<Router>> {
        let stores = TcpStores::connect_all(&self.addrs, TransportConfig::new()).ok()?;
        Some(Arc::new(Router::with_options(
            Arc::new(stores),
            Arc::clone(&self.resolver),
            ClientOptions {
                jitter_seed: Some(jitter_seed),
                ..ClientOptions::default()
            },
        )))
    }

    /// A client, retrying the connection until the cluster has someone listening.
    pub(crate) fn client_within(&self, jitter_seed: u64, within: Duration) -> Option<TxnClient> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(client) = self.client(jitter_seed) {
                return Some(client);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn shutdown(&self) {
        for at in 0..self.topology.nodes() {
            self.kill(at);
        }
    }
}

/// The routing table a client is given: one region per group, tiling the key space.
///
/// Each store nominally bootstraps a region covering everything and it is this table that
/// divides it, exactly as `tests/txn_multi_region.rs` does. What a client observes is the same
/// either way — an answer bounded by what one group holds — and the alternative, a real split,
/// would put phase 4's machinery in the middle of a phase 5 test.
fn routing(topology: &Topology, peers: &[Vec<PeerAddress>]) -> Arc<dyn RegionResolver> {
    let routes = (0..topology.regions()).map(|group| {
        let start = if group == 0 {
            Bytes::new()
        } else {
            Bytes::from_static(topology.boundaries[group - 1])
        };
        let end = topology
            .boundaries
            .get(group)
            .map_or_else(Bytes::new, |bound| Bytes::from_static(bound));
        Route {
            region: Region {
                id: group as u64 + 1,
                start_key: start,
                end_key: end,
                peers: peers[group]
                    .iter()
                    .map(|peer| Peer::voter(peer.store_id, peer.peer_id))
                    .collect(),
                epoch: Epoch::INITIAL,
            },
            // No opinion: the first request goes to whichever peer is listed first and, if it
            // is a follower, comes back with the hint that fixes the cache.
            leader: None,
        }
    });
    Arc::new(RegionTable::from_routes(routes))
}

/// Reserves `count` ports and **keeps holding them**.
///
/// Every store has to know every peer's address before any server exists, so the addresses cannot
/// come from the servers. This used to bind each port, read its number and drop the listener —
/// which left every address free from the moment it was chosen until each `Server` bound it, and
/// under a parallel suite run another test's cluster took one. The symptom was a panic at
/// `.expect("the server binds")` in a transaction test, in a workspace run nobody could reproduce
/// on its own.
///
/// The listeners are returned instead of their addresses and handed to `Server::from_listener`, so
/// each port is held from the moment it is allocated until the server is serving on it.
/// `esker-store`'s cluster harness made this same fix for this same reason.
fn reserve_ports(count: usize) -> Vec<std::net::TcpListener> {
    (0..count)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("a port"))
        .collect()
}

#[cfg(test)]
mod tests {
    /// **A reserved port has to stay reserved until its server binds it.**
    ///
    /// `reserve_ports` binds each port to learn its number. If it then drops the listener, every
    /// address it hands back is free from that moment until each `Server` gets around to binding
    /// it — and under a parallel suite run that window is long enough for another test's cluster
    /// to take one. The symptom is a panic at `.expect("the server binds")` in a test that has
    /// nothing to do with ports, in a run that nobody can reproduce on its own.
    ///
    /// `esker-store`'s cluster harness had the identical bug and fixed it the same way; its
    /// comment says the window was "microseconds" and something took it anyway.
    #[test]
    fn a_reserved_port_is_held_until_it_is_handed_over() {
        let reserved = super::reserve_ports(4);
        for listener in &reserved {
            let addr = listener.local_addr().expect("its address");
            assert!(
                std::net::TcpListener::bind(addr).is_err(),
                "{addr} was still bindable, so the reservation is holding nothing"
            );
        }
    }
}
