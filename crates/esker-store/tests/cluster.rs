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

/// One peer's whole consensus position, for a failure message that has to distinguish a write
/// that never arrived from one that has not been applied yet.
///
/// `last_index` is the claim about *durability*: an entry at or below it is in this peer's log, so
/// a quorum held it and nothing was lost. `applied` is the claim about *visibility*: only at or
/// below it can `rawkv::get` see the value.
async fn digest(node: &Node) -> String {
    let status = node.store.peer().unwrap().status().await.unwrap();
    format!(
        "store {} {:?} term={} commit={} applied={} last={}",
        node.store.store_id(),
        status.role,
        status.term,
        status.commit,
        status.applied,
        status.last_index,
    )
}

/// **Invariant 1, where it lives.** A quorum holds `index` in its log, durably.
///
/// The write was acknowledged, so this was true before the call was made: the follower persists
/// with `WriteOptions::synced()` before it answers an `AppendEntries`, the leader advances
/// `matched` only on that answer, and it commits only when a majority of `matched` covers the
/// index (`esker_raft`'s `maybe_commit`, with §5.4.2's term condition).
///
/// Asserted directly rather than inferred from a later read of the state machine, because these
/// are two different claims: this one is about the **log** and is true the instant the
/// acknowledgement returns, and it is the one a leader kill can actually break. Reading the data
/// back tests it only through a state machine that may not have caught up, which is a race — and
/// that race reported a lost write when nothing had been lost.
async fn assert_quorum_holds(nodes: &[Node], index: u64, before: &[String]) {
    let mut holders = 0;
    for node in nodes {
        if node
            .store
            .peer()
            .unwrap()
            .status()
            .await
            .unwrap()
            .last_index
            >= index
        {
            holders += 1;
        }
    }
    assert!(
        holders > nodes.len() / 2,
        "only {holders} of {} peers hold the acknowledged entry {index} in their log, which is \
         not a quorum -- the acknowledgement did not mean what it says: {}",
        nodes.len(),
        before.join(" | "),
    );
}

/// How long a value takes to appear on one node, or `None` if it never does — **the line that
/// tells a lost write from a slow one**.
///
/// A write the survivor never received is a durability failure and this answers `None`. One it
/// holds in its log and has not applied is a read that came too early, and this answers how much
/// too early. Only ever called on the failure path, which is why it may take ten seconds.
async fn eventually(node: &Node, key: &str, value: &[u8]) -> Option<Duration> {
    let waited = Instant::now();
    while waited.elapsed() < Duration::from_secs(10) {
        let again = esker_store::rawkv::get(node.store.db(), key.as_bytes()).unwrap();
        if again.as_deref() == Some(value) {
            return Some(waited.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
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
/// applied on this peer, which means it committed, which means a quorum had it **durably in their
/// log** — so the two survivors are a quorum and at least one of them must already hold every
/// acknowledged entry, and both must converge on it.
///
/// **Those are two claims and they are checked in two places**, because they live in two places
/// and fail differently. Durability is a fact about the *log* and is true the instant the
/// acknowledgement returns, so it is asserted before the kill, where nothing can race it.
/// Convergence is a fact about the *state machine* and is only eventual — a follower may hold an
/// entry it has not applied — so the survivors are given time to apply before their data is read.
///
/// Reading the state machine without that wait is what made this test fail about once in thirty
/// runs. The instrumentation below is what proved it was a lag and not a loss: the follower had
/// the entry in its log before the kill (`last=13` against a `commit=12`) and the value appeared
/// 22 ms later. Nothing was ever lost, and the assertion has not been weakened to say so — the
/// missing wait was added, and the durability half was made explicit rather than inferred from
/// the convergence half.
///
/// # What counts as acknowledged here, and what a lost write would look like
///
/// **A write is acknowledged exactly when its `propose` returned `Ok`, and nothing else is.** That
/// `Ok` has a proof behind it: [`RaftPeer::propose`] is answered by `complete_proposal`, whose one
/// call site is the end of the apply path, so it means *this entry applied on this peer* — which
/// means it committed, which means a majority held it in their logs (`maybe_commit` takes
/// `matched[quorum - 1]` under §5.4.2). Every other resolution answers `Err`.
///
/// So an `Err` is **not** an acknowledgement, and an *ambiguous* one least of all: `is_ambiguous`
/// is `RequestOutcome::Unknown`, the store saying it does not know whether the entry will commit.
/// A key whose propose answered that must never enter `acknowledged` — the list is the test's
/// entire claim, and one unearned entry in it turns this into an assertion the cluster never made.
///
/// **A real lost write shows up in one of two places, and they fail differently:**
///
/// * before the kill, [`assert_quorum_holds`] — fewer than a quorum hold the acknowledged entry in
///   their logs, so the acknowledgement did not mean what it says. This is the durability half and
///   it cannot be a lag, because it is a fact about the log at the instant the answer returned;
/// * after the election and `wait_for_applied`, the loop at the end — a survivor's `default`
///   column family does not hold `key -> value`, and still does not ten seconds later. This is the
///   convergence half.
///
/// The fix for an ambiguous answer therefore has exactly one shape: **retry until `Ok`, and record
/// only then**. Dropping the write instead would shorten the list and let the test pass while
/// checking less; recording it anyway would demand a value the cluster never promised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_acknowledged_write_is_lost_when_the_leader_is_killed() {
    let mut nodes = start_cluster(3).await;
    let mut leader = settled_leader(&nodes).await;

    // Acknowledged writes: each `propose` returns only once the command has applied.
    let mut acknowledged = Vec::new();
    for index in 0..12_u32 {
        let key = format!("acked-{index:02}");
        let command = Command::Put {
            key: Bytes::from(key.clone().into_bytes()),
            value: Bytes::from(index.to_be_bytes().to_vec()),
        };
        // **An ambiguous answer is not an acknowledgement, and this used to `unwrap` one.** A
        // leader that steps down with the proposal in its log answers `it may still commit`, which
        // is `Unknown` — seen once in twenty-five runs at fourteen busy threads, and read from
        // outside as this test failing, which reads as invariant 1. Retried rather than recorded:
        // repeating is safe *here* because every write is an idempotent `Put` of one fixed value
        // from a single writer, so a second apply cannot be observed — the same argument
        // `promotion.rs`'s `put` makes, and the same handling five other store tests already use.
        //
        // The leader is re-derived rather than reused: the answer means it is no longer leading,
        // and it is also the node this test kills later.
        loop {
            match nodes[leader].store.peer().unwrap().propose(&command).await {
                Ok(_) => break,
                Err(error) if error.is_ambiguous() || error.is_retryable() => {
                    leader = settled_leader(&nodes).await;
                }
                Err(error) => panic!("proposing {key}: {error}"),
            }
        }
        // The leader's log index for this write, so a failure can say whether a survivor is
        // missing the entry or merely has not applied it yet.
        let at = nodes[leader]
            .store
            .peer()
            .unwrap()
            .status()
            .await
            .unwrap()
            .applied;
        acknowledged.push((key, index, at));
    }
    // Every peer's state at the moment before the kill: the leader acked, so a quorum must hold
    // each entry — this records who actually did.
    let mut before_kill = Vec::new();
    for node in &nodes {
        before_kill.push(digest(node).await);
    }

    let last_acked = acknowledged.last().unwrap().2;
    assert_quorum_holds(&nodes, last_acked, &before_kill).await;

    // Stop the leader: its driver thread, its ticker and its connections all go away, which is
    // what a store dying looks like to the other two.
    nodes[leader].store.stop();
    let dead = nodes.remove(leader);
    // The directory has to outlive the assertions below — the survivors do not need it, but
    // dropping it here would delete a database a still-running peer might touch.
    let Node { handle, dir, .. } = dead;
    let _ = handle.shutdown().await;

    // Two of three is still a quorum, so the survivors elect one of themselves.
    let mut new_leader = settled_leader(&nodes).await;
    assert_eq!(nodes.len(), 2);

    // **The survivors' state machines have to catch up before their data is read.**
    // `settled_leader` waits for agreement on who leads, which says nothing about how far anyone
    // has applied — a follower can hold a committed entry it has not run yet, and reading its
    // column family in that window reports a write that is on disk in the log as missing. The
    // same wait the `CompareAndSwap` test above makes before it reads every node.
    wait_for_applied(&nodes, last_acked).await;

    // Every acknowledged write is on both survivors.
    let mut after_election = Vec::new();
    for node in &nodes {
        after_election.push(digest(node).await);
    }
    for node in &nodes {
        for (key, value, at) in &acknowledged {
            let stored = esker_store::rawkv::get(node.store.db(), key.as_bytes()).unwrap();
            if stored.as_deref() == Some(&value.to_be_bytes()[..]) {
                continue;
            }
            let arrived = eventually(node, key, &value.to_be_bytes()).await;
            let mut settled = Vec::new();
            for node in &nodes {
                settled.push(digest(node).await);
            }
            panic!(
                "store {} lost the acknowledged write {key}\n  \
                 entry index on the old leader: {at}\n  \
                 read back within 10s: {arrived:?}\n  \
                 before the kill:  {}\n  \
                 after the election: {}\n  \
                 after the wait:     {}",
                node.store.store_id(),
                before_kill.join(" | "),
                after_election.join(" | "),
                settled.join(" | "),
            );
        }
    }

    // And the cluster still takes writes.
    // **The second unwrapped propose, and it fails the same way.** The survivors have just
    // elected; the one this write is addressed to can step down again, or refuse as a non-leader,
    // before it applies. Seventy-five runs of the unfixed test at fourteen busy threads failed
    // twelve times — eight at the write loop above and **four here** — so fixing only the first
    // would have left a third of the failures and read as the fix not working.
    let after = Command::Put {
        key: Bytes::from_static(b"after"),
        value: Bytes::from_static(b"the-kill"),
    };
    loop {
        match nodes[new_leader]
            .store
            .peer()
            .unwrap()
            .propose(&after)
            .await
        {
            Ok(_) => break,
            Err(error) if error.is_ambiguous() || error.is_retryable() => {
                new_leader = settled_leader(&nodes).await;
            }
            Err(error) => panic!("writing after the kill: {error}"),
        }
    }
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
