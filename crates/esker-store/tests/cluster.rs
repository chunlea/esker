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

/// Reserves `count` ports by binding and releasing them.
///
/// The stores have to know every peer's address *before* any server exists, so the addresses
/// cannot come from the servers. Releasing a port and rebinding it races in principle; in practice
/// the window is microseconds, and a peer that cannot be reached yet simply has its messages
/// dropped and retried — which is the transport's normal behaviour, not a special case.
fn reserve_ports(count: usize) -> Vec<SocketAddr> {
    let listeners: Vec<std::net::TcpListener> = (0..count)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap())
        .collect()
}

/// Starts `count` stores replicating one region among themselves.
async fn start_cluster(count: usize) -> Vec<Node> {
    let addrs = reserve_ports(count);
    let peers: Vec<PeerAddress> = (0..count)
        .map(|at| PeerAddress::new(at as u64 + 1, at as u64 + 1, addrs[at]))
        .collect();

    let mut nodes = Vec::new();
    for (at, addr) in addrs.iter().enumerate() {
        let id = at as u64 + 1;
        let dir = TempDir::new().unwrap();
        let mut raft = RaftOptions::new(peers.clone(), 20_260_830);
        // Ticks are 100 ms in production; here they are fast, so an election takes milliseconds
        // rather than seconds. Nothing about the algorithm changes — it counts ticks.
        raft.tick = Duration::from_millis(5);
        let options = StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: 1,
            raft: Some(raft),
            ..StoreOptions::new()
        };
        let store = Store::open(dir.path(), options).unwrap();
        let server = Server::bind(
            *addr,
            StoreService::new(Arc::clone(&store)),
            TransportConfig::new(),
        )
        .await
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

/// Waits for exactly one leader, and returns its index.
async fn wait_for_leader(nodes: &[Node]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let mut leaders = Vec::new();
        for (at, node) in nodes.iter().enumerate() {
            let status = node.store.peer().unwrap().status().await.unwrap();
            if status.role == Role::Leader {
                leaders.push((at, status.term));
            }
        }
        // More than one is legal for an instant — a deposed leader that has not found out yet —
        // so the test waits for the cluster to agree rather than asserting on the first sighting.
        if leaders.len() == 1 {
            return leaders[0].0;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no single leader within the deadline");
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_stores_elect_a_leader_over_real_tcp() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;

    let status = nodes[leader].store.peer().unwrap().status().await.unwrap();
    assert_eq!(status.role, Role::Leader);
    assert!(status.term >= 1);

    // And the followers agree who it is, which only the leader's heartbeats can have told them.
    let deadline = Instant::now() + Duration::from_secs(10);
    let leader_id = status.id;
    loop {
        let mut agreed = 0;
        for node in &nodes {
            if node.store.peer().unwrap().leader() == Some(leader_id) {
                agreed += 1;
            }
        }
        if agreed == nodes.len() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the followers never agreed on a leader"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown(nodes).await;
}

/// A write proposed on the leader reaches every peer's data column family. This is the whole
/// stack: propose, replicate over TCP, commit on a quorum, apply on each peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_on_the_leader_reaches_every_peer() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_refuses_a_proposal_and_names_the_leader() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_of_writes_replicates_in_order() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_over_the_wire_is_served_by_the_leader_and_redirected_by_a_follower() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compare_and_swap_over_the_wire_is_ordered_by_raft() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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

/// A linearizable read on the leader comes back at an index the state machine has *already*
/// applied — the property `ReadIndex` exists for, checked across a real cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_index_on_the_leader_is_answered_past_its_apply() {
    let nodes = start_cluster(3).await;
    let leader = wait_for_leader(&nodes).await;
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
