//! Three stores, one region, real TCP.
//!
//! The unit tests in the crate prove each piece against a scripted counterpart: the driver
//! against an auditing transport, the log against the engine, the apply loop against a batch. This
//! file proves the pieces are wired to each other — that a message which leaves one store's driver
//! thread crosses a socket, is decoded by the wire, reaches another store's peer, and comes back
//! as an acknowledgement the leader counts.
//!
//! Nothing here is stubbed. Three real databases in three temporary directories, three real
//! servers on ephemeral ports, and Raft electing a leader among them with no help.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::{
    Epoch, RawKvReq, RawKvResp, Request, RequestHeader, RequestOutcome, Server, ServerHandle,
    TcpTransport, Transport, TransportConfig,
};
use esker_raft::Role;
use esker_store::apply::Command;
use esker_store::server::RaftOptions;
use esker_store::{Applied, PeerAddress, Store, StoreOptions, StoreService};
use tempfile::TempDir;

/// One store: its database, its server, and the directory that must outlive both.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    #[allow(dead_code)]
    dir: TempDir,
}

/// Reserves `count` ports and **keeps holding them**.
///
/// The stores have to know every peer's address *before* any server exists, so the addresses
/// cannot come from the servers. This used to bind a socket, read the port and release it, with a
/// comment saying the window was microseconds — and under a parallel suite run something else
/// took the port in that window and the rebind was `Address already in use`. The listeners are
/// returned instead of their addresses and handed to `Server::from_listener`, so each port is
/// held from the moment it is allocated until the server is serving on it.
fn reserve_ports(count: usize) -> Vec<std::net::TcpListener> {
    (0..count)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect()
}

/// Starts `count` stores replicating one region among themselves.
#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn start_cluster(count: usize) -> Vec<Node> {
    let listeners = reserve_ports(count);
    let addrs: Vec<SocketAddr> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap())
        .collect();
    let peers: Vec<PeerAddress> = (0..count)
        .map(|at| PeerAddress::new(at as u64 + 1, at as u64 + 1, addrs[at]))
        .collect();

    let mut nodes = Vec::new();
    for (at, listener) in listeners.into_iter().enumerate() {
        let id = at as u64 + 1;
        let dir = TempDir::new().unwrap();
        let mut raft = RaftOptions::new(peers.clone(), 20_260_830);
        // Ticks are 100 ms in production; here they are shorter so an election takes a fraction
        // of a second rather than seconds. Nothing about the algorithm changes — it counts ticks
        // — but the interval cannot be arbitrarily small: these tests run in parallel, and a
        // whole file's worth of clusters in one process is enough contention that a 5 ms tick
        // gets delayed past an election timeout and leadership churns. 25 ms leaves headroom.
        raft.tick = Duration::from_millis(25);
        let options = StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: 1,
            raft: Some(raft),
            ..StoreOptions::new()
        };
        let store = Store::open(dir.path(), options).unwrap();
        let server = Server::from_listener(
            listener,
            StoreService::new(Arc::clone(&store)),
            TransportConfig::new(),
        )
        .unwrap();
        let handle = server.spawn().unwrap();
        nodes.push(Node { store, handle, dir });
    }
    nodes
}

async fn shutdown(nodes: Vec<Node>) {
    for node in nodes {
        node.store.stop();
        let _ = node.handle.shutdown().await;
    }
}

/// Waits until every node agrees who leads, and returns that node's index.
///
/// "Exactly one node says it is the leader" is not enough to act on: a follower learns the leader
/// from an `AppendEntries`, so there is a window in which one has taken office and nobody else
/// knows. A test that asserts on a redirect hint — or that proposes and expects it to be ordered
/// — is racing that window. Waiting for unanimity closes it, and costs a few milliseconds.
async fn settled_leader(nodes: &[Node]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut beliefs = Vec::new();
        for node in nodes {
            beliefs.push(node.store.peer().unwrap().leader());
        }
        if let Some(Some(leader)) = beliefs.first().copied() {
            let unanimous = beliefs.iter().all(|belief| *belief == Some(leader));
            // Found by peer id rather than by arithmetic on the index: a test that has removed a
            // node no longer has `index == id - 1`, and assuming it does is how this helper
            // spins for ever looking at the wrong node.
            let at = nodes
                .iter()
                .position(|node| node.store.peer().unwrap().peer_id() == leader);
            if let Some(at) = at {
                if unanimous
                    && nodes[at].store.peer().unwrap().status().await.unwrap().role == Role::Leader
                {
                    return at;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the cluster never agreed on a leader");
}

/// Waits for every node's applied index to reach `index`.
async fn wait_for_applied(nodes: &[Node], index: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut caught_up = 0;
        for node in nodes {
            let status = node.store.peer().unwrap().status().await.unwrap();
            if status.applied >= index {
                caught_up += 1;
            }
        }
        if caught_up == nodes.len() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("not every node applied through {index} within the deadline");
}

/// Three stores with no help elect one leader — which means the transport carried the votes and
/// the wire decoded them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_stores_elect_a_leader_over_real_tcp() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;

    let status = nodes[leader].store.peer().unwrap().status().await.unwrap();
    assert_eq!(status.role, Role::Leader);
    assert!(status.term >= 1);
    // `settled_leader` returned only once every node agreed, which only the leader's heartbeats
    // can have told them.
    for node in &nodes {
        assert_eq!(node.store.peer().unwrap().leader(), Some(status.id));
    }
    shutdown(nodes).await;
}

/// A write proposed on the leader reaches every peer's data column family. This is the whole
/// stack: propose, replicate over TCP, commit on a quorum, apply on each peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_on_the_leader_reaches_every_peer() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let peer = nodes[leader].store.peer().unwrap();

    let command = Command::Put {
        key: Bytes::from_static(b"replicated"),
        value: Bytes::from_static(b"yes"),
    };
    assert_eq!(peer.propose(&command).await.unwrap(), Applied::Done);

    let index = peer.status().await.unwrap().applied;
    wait_for_applied(&nodes, index).await;

    for (at, node) in nodes.iter().enumerate() {
        let stored = esker_store::rawkv::get(node.store.db(), b"replicated").unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(&b"yes"[..]),
            "node {} does not have the write",
            at + 1
        );
    }
    shutdown(nodes).await;
}

/// A follower cannot order a write, and says so with the redirect a client acts on rather than
/// an opaque failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_refuses_a_proposal_and_names_the_leader() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let follower = (leader + 1) % nodes.len();

    let command = Command::Put {
        key: Bytes::from_static(b"k"),
        value: Bytes::from_static(b"v"),
    };
    let error = nodes[follower]
        .store
        .peer()
        .unwrap()
        .propose(&command)
        .await
        .unwrap_err();
    match error {
        esker_proto::ProtoError::NotLeader {
            region_id,
            leader_hint,
        } => {
            assert_eq!(region_id, 1);
            assert_eq!(
                leader_hint,
                Some((leader + 1) as u64),
                "a follower that knows the leader should say so"
            );
        }
        other => panic!("expected NotLeader, got {other:?}"),
    }
    shutdown(nodes).await;
}

/// Several writes in a row all land, in order, on every peer — which exercises the transport's
/// batching rather than a single message crossing once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_of_writes_replicates_in_order() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let peer = nodes[leader].store.peer().unwrap();

    for index in 0..16_u32 {
        let command = Command::Put {
            key: Bytes::from(format!("key-{index:02}").into_bytes()),
            value: Bytes::from(index.to_be_bytes().to_vec()),
        };
        peer.propose(&command).await.unwrap();
    }

    let applied = peer.status().await.unwrap().applied;
    wait_for_applied(&nodes, applied).await;

    for (at, node) in nodes.iter().enumerate() {
        for index in 0..16_u32 {
            let key = format!("key-{index:02}");
            let stored = esker_store::rawkv::get(node.store.db(), key.as_bytes()).unwrap();
            assert_eq!(
                stored.as_deref(),
                Some(&index.to_be_bytes()[..]),
                "node {} is missing {key}",
                at + 1
            );
        }
    }
    shutdown(nodes).await;
}

/// The wire path, end to end: a `RawKv` request to the **leader's** socket is ordered by Raft and
/// answered from this peer's own apply, and the same request to a **follower** comes back as
/// `NotLeader` naming the leader — which is what a client's region cache learns from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_over_the_wire_is_served_by_the_leader_and_redirected_by_a_follower() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let follower = (leader + 1) % nodes.len();
    let header = RequestHeader::new(1, Epoch::INITIAL, 0);

    let to_leader = TcpTransport::connect(nodes[leader].handle.local_addr())
        .await
        .unwrap();
    let answer = to_leader
        .call(Request::raw_kv(
            header,
            RawKvReq::put(&b"wire"[..], &b"value"[..]),
        ))
        .await
        .unwrap();
    assert_eq!(answer.into_raw_kv().unwrap(), RawKvResp::Put);

    // A linearizable read on the leader sees it.
    let answer = to_leader
        .call(Request::raw_kv(header, RawKvReq::get(&b"wire"[..])))
        .await
        .unwrap();
    assert_eq!(
        answer.into_raw_kv().unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"value"))
        }
    );

    // The same request to a follower is refused with the hint a client redirects on.
    let to_follower = TcpTransport::connect(nodes[follower].handle.local_addr())
        .await
        .unwrap();
    let error = to_follower
        .call(Request::raw_kv(
            header,
            RawKvReq::put(&b"wire"[..], &b"other"[..]),
        ))
        .await
        .unwrap_err();
    match error {
        esker_proto::ProtoError::NotLeader {
            region_id,
            leader_hint,
        } => {
            assert_eq!(region_id, 1);
            assert_eq!(leader_hint, Some(leader as u64 + 1));
        }
        other => panic!("expected NotLeader from a follower, got {other:?}"),
    }
    // And it is the retryable, provably-not-applied kind, so a client may follow the hint.
    let error = to_follower
        .call(Request::raw_kv(header, RawKvReq::get(&b"wire"[..])))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.outcome(), RequestOutcome::NotApplied);

    shutdown(nodes).await;
}

/// A `CompareAndSwap` over the wire is decided at apply time on every peer, and its answer comes
/// back to the caller that proposed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compare_and_swap_over_the_wire_is_ordered_by_raft() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let header = RequestHeader::new(1, Epoch::INITIAL, 0);
    let transport = TcpTransport::connect(nodes[leader].handle.local_addr())
        .await
        .unwrap();

    let swap = |expected: Option<&'static [u8]>, value: Option<&'static [u8]>| {
        Request::raw_kv(
            header,
            RawKvReq::CompareAndSwap {
                key: Bytes::from_static(b"cas"),
                expected: expected.map(Bytes::from_static),
                value: value.map(Bytes::from_static),
                sync: true,
            },
        )
    };

    let answer = transport.call(swap(None, Some(b"first"))).await.unwrap();
    assert_eq!(
        answer.into_raw_kv().unwrap(),
        RawKvResp::CompareAndSwap {
            swapped: true,
            previous: None
        }
    );
    let answer = transport.call(swap(None, Some(b"second"))).await.unwrap();
    assert_eq!(
        answer.into_raw_kv().unwrap(),
        RawKvResp::CompareAndSwap {
            swapped: false,
            previous: Some(Bytes::from_static(b"first"))
        }
    );

    // Every peer agrees, because the comparison happened at apply time on each of them.
    let index = nodes[leader]
        .store
        .peer()
        .unwrap()
        .status()
        .await
        .unwrap()
        .applied;
    wait_for_applied(&nodes, index).await;
    for node in &nodes {
        let stored = esker_store::rawkv::get(node.store.db(), b"cas").unwrap();
        assert_eq!(stored.as_deref(), Some(&b"first"[..]));
    }
    shutdown(nodes).await;
}

/// **Killing the leader loses nothing.** Writes are acknowledged, the leader is stopped, the
/// remaining two elect a new one — and every write that was acknowledged is still there.
///
/// This is `CLAUDE.md` invariant 1 at the consensus layer. An acknowledgement means the command
/// applied on this peer, which means it committed, which means a quorum had it durably — so the
/// two survivors are a quorum and at least one of them must hold every acknowledged write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_acknowledged_write_is_lost_when_the_leader_is_killed() {
    let mut nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;

    // Acknowledged writes: each `propose` returns only once the command has applied.
    let mut acknowledged = Vec::new();
    for index in 0..12_u32 {
        let key = format!("acked-{index:02}");
        nodes[leader]
            .store
            .peer()
            .unwrap()
            .propose(&Command::Put {
                key: Bytes::from(key.clone().into_bytes()),
                value: Bytes::from(index.to_be_bytes().to_vec()),
            })
            .await
            .unwrap();
        acknowledged.push((key, index));
    }

    // Stop the leader: its driver thread, its ticker and its connections all go away, which is
    // what a store dying looks like to the other two.
    nodes[leader].store.stop();
    let dead = nodes.remove(leader);
    // The directory has to outlive the assertions below — the survivors do not need it, but
    // dropping it here would delete a database a still-running peer might touch.
    let Node { handle, dir, .. } = dead;
    let _ = handle.shutdown().await;

    // Two of three is still a quorum, so the survivors elect one of themselves.
    let new_leader = settled_leader(&nodes).await;
    assert_eq!(nodes.len(), 2);

    // Every acknowledged write is on both survivors.
    for node in &nodes {
        for (key, value) in &acknowledged {
            let stored = esker_store::rawkv::get(node.store.db(), key.as_bytes()).unwrap();
            assert_eq!(
                stored.as_deref(),
                Some(&value.to_be_bytes()[..]),
                "store {} lost the acknowledged write {key}",
                node.store.store_id()
            );
        }
    }

    // And the cluster still takes writes.
    nodes[new_leader]
        .store
        .peer()
        .unwrap()
        .propose(&Command::Put {
            key: Bytes::from_static(b"after"),
            value: Bytes::from_static(b"the-kill"),
        })
        .await
        .unwrap();
    assert_eq!(
        esker_store::rawkv::get(nodes[new_leader].store.db(), b"after")
            .unwrap()
            .as_deref(),
        Some(&b"the-kill"[..])
    );

    drop(dir);
    shutdown(nodes).await;
}

/// A minority cannot elect a leader or accept a write. Two of three stops is one survivor, which
/// is not a quorum — and a store that kept serving on its own would be the split brain every
/// other rule here exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lone_survivor_cannot_serve() {
    let mut nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    nodes[leader]
        .store
        .peer()
        .unwrap()
        .propose(&Command::Put {
            key: Bytes::from_static(b"before"),
            value: Bytes::from_static(b"the-kill"),
        })
        .await
        .unwrap();

    // Stop two, keeping whichever was not the leader last.
    let survivor_at = (leader + 2) % 3;
    let mut survivor = None;
    for (at, node) in nodes.drain(..).enumerate() {
        if at == survivor_at {
            survivor = Some(node);
        } else {
            node.store.stop();
            let _ = node.handle.shutdown().await;
        }
    }
    let survivor = survivor.unwrap();

    // It campaigns and never wins: one of three is not a majority.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = survivor.store.peer().unwrap().status().await.unwrap();
    assert_ne!(status.role, Role::Leader, "one of three elected itself");

    // And it refuses a write rather than accepting one it could never commit.
    let error = survivor
        .store
        .peer()
        .unwrap()
        .propose(&Command::Put {
            key: Bytes::from_static(b"lonely"),
            value: Bytes::from_static(b"v"),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, esker_proto::ProtoError::NotLeader { .. }));

    survivor.store.stop();
    let _ = survivor.handle.shutdown().await;
}

/// A linearizable read on the leader comes back at an index the state machine has *already*
/// applied — the property `ReadIndex` exists for, checked across a real cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_index_on_the_leader_is_answered_past_its_apply() {
    let nodes = start_cluster(3).await;
    let leader = settled_leader(&nodes).await;
    let peer = nodes[leader].store.peer().unwrap();

    peer.propose(&Command::Put {
        key: Bytes::from_static(b"k"),
        value: Bytes::from_static(b"v"),
    })
    .await
    .unwrap();

    let index = peer.read_index().await.unwrap();
    let status = peer.status().await.unwrap();
    assert!(
        status.applied >= index,
        "a read was answered at {index} with only {} applied",
        status.applied
    );
    assert!(
        index > 0,
        "the read index should be past the leader's own no-op"
    );
    shutdown(nodes).await;
}
