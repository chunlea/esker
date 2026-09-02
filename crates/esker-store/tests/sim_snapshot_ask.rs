//! The real snapshot sender, put in front of every membership state it can be asked from.
//!
//! `esker_sim::mech::ask` owns the answer table and the checker; this file drives a real store
//! into each state and reports what came back. `tests/snapshot.rs` holds one ordering still — a
//! voter added on a store that does not exist, so the change can never commit — and asserts which
//! refusal it gets. This puts the same sender in front of the whole table, and in particular
//! **times** every answer, because "not a member, on sight" and "not applied here, after the wait"
//! are the pre-fix and post-fix answers to the same question.
//!
//! Copy this file and `crates/esker-sim/` into a detached worktree at `c062f91` — `1502d0f`'s
//! parent — and the member-in-the-gap case fails: the sender reads only the applied record, so it
//! answers "is not a member of region", on sight.
//!
//! `docs/plans/phase-11-engine.md` §10 holds the recorded red.
//!
//! Single-node stores throughout: nothing here needs a second store, and a case that costs one
//! process is a case that can be run six times.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::{
    PeerRole, RawKvReq, Region, RequestHeader, Server, ServerHandle, Service, TransportConfig,
};
use esker_sim::mech::ask::{Answer, AskCase, Membership, Outcome, cases, check};
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

/// What "the sender waited for the record" means observably.
///
/// `RECORD_CATCHUP_WAIT` is 500 ms and the poll is 2 ms, so a sender that consulted the record
/// more than once cannot answer in under 400 ms, and one that answered on sight cannot take that
/// long. The gap between the two is what tells a waited answer from an immediate one, and without
/// it a pre-fix sender that happened to phrase its refusal well would pass.
const WAIT_FLOOR_MS: u64 = 400;

struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = tokio::time::timeout(Duration::from_secs(30), self.handle.shutdown()).await;
    }
}

/// A port, **held** until the server that will serve on it adopts the socket.
///
/// Returning the address and dropping the listener leaves the port belonging to nobody until the
/// rebind, and under a parallel suite run something else takes it — `Address already in use`.
/// `Server::from_listener` takes the socket itself, so there is no window.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .unwrap_or_else(|_: tokio::time::error::Elapsed| panic!("timed out waiting for {what}"))
}

async fn wait_for<F: FnMut() -> bool>(what: &str, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One store, alone in its cluster, leading region 1.
#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn open(address_listener: std::net::TcpListener, pd: &Arc<FakePd>) -> Node {
    let address = address_listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(vec![PeerAddress::new(1, 1, address)], 20_261_102);
    raft.tick = Duration::from_millis(25);
    raft.compaction = LogCompaction::new();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            raft: Some(raft),
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                region_split_size: u64::MAX,
                max_sampled_keys: 1024,
            },
            ..StoreOptions::new()
        },
    )
    .unwrap();
    let server = Server::from_listener(
        address_listener,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .unwrap();
    let handle = server.spawn().unwrap();
    Node { store, handle, dir }
}

async fn put(store: &Arc<Store>, region: &Region, key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        let request = RawKvReq::put(key.clone(), Bytes::copy_from_slice(value));
        match within("a put", store.serve(header, request)).await {
            Ok(_) => return,
            Err(error) => {
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "{error}");
                assert!(Instant::now() < deadline, "the put never landed");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// Asks for a snapshot and classifies what came back, with how long it took.
///
/// **A refusal arrives in one of two places** and a test that reads only one of them is testing
/// the framing rather than the decision: a store that refuses before the stream is opened fails
/// the call, and one that refuses *after waiting* sends the error as the stream's first chunk.
/// `1502d0f`'s own test found that the hard way — its first version read one place and reported a
/// refusal as a snapshot served.
async fn ask(connection: &esker_proto::TcpTransport, region_id: u64, peer_id: u64) -> Outcome {
    let asked_at = Instant::now();
    let request = esker_proto::Request::Snapshot(esker_proto::SnapshotRequest {
        region_id,
        index: 1,
        peer_id,
    });
    let said = match connection.call_stream(request).await {
        Err(error) => Err(error.to_string()),
        Ok(mut stream) => match stream.next_chunk().await {
            None => Err("the stream ended before its header".to_owned()),
            Some(Err(error)) => Err(error.to_string()),
            Some(Ok(_)) => Ok(()),
        },
    };
    let waited_ms = u64::try_from(asked_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let answer = match said {
        Ok(()) => Answer::Served,
        Err(message) if message.contains("has not applied here") => Answer::NotAppliedHere,
        Err(message) if message.contains("is not a member") => Answer::NotAMember,
        Err(message)
            if message.contains("no such region")
                || message.contains("region 42")
                || message.contains("not replicated on this store") =>
        {
            Answer::NoSuchRegion
        }
        Err(message) => Answer::Other(message),
    };
    Outcome { answer, waited_ms }
}

/// Drives a store into `case`'s state and asks.
///
/// Returns `None` for a state a real store cannot be held in; the caller records the skip.
async fn observe(case: &AskCase) -> Option<Outcome> {
    if !case.constructible {
        return None;
    }
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let node = open(address_listener, &pd).await;
    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = node.store.regions().regions()[0].clone();
    put(&node.store, &region, Bytes::from_static(b"k"), b"v").await;

    let peer = node.store.peer_of(1).unwrap();
    let mut proposing = None;
    let (region_id, peer_id) = if case.region_hosted {
        match case.membership {
            // The peer this store already is. Its own record names it, so nothing is waited for.
            Membership::InRecord => (1, 1),
            Membership::Stranger => (1, 77),
            Membership::InCoreOnly { commits: false } => {
                // **Holding the ordering still**, as `1502d0f`'s own test does it: a voter added
                // on a store that does not exist. The core takes the change when it appends it,
                // and the commit then needs a quorum of the *new* configuration — which the
                // absent peer is half of — so it never commits, never applies, and the record
                // never moves. Stable rather than a window to hit.
                let handle = tokio::spawn({
                    let peer = Arc::clone(&peer);
                    async move {
                        peer.propose_conf_change(
                            esker_raft::ConfChangeKind::AddVoter,
                            2,
                            2,
                            PeerRole::Voter,
                        )
                        .await
                    }
                });
                // Polled rather than `wait_for`, because `status()` is async and the helper is
                // not: the core has to be seen taking the change before the ask goes out, or the
                // case is asking from a state it has not reached.
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let conf = within("the core's status", peer.status())
                        .await
                        .unwrap()
                        .conf;
                    if conf.is_voter(2) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the core never took the conf change, so this case is not the state it \
                         claims to be"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(
                    !node
                        .store
                        .regions()
                        .get(1)
                        .unwrap()
                        .region()
                        .peers
                        .iter()
                        .any(|member| member.peer_id == 2),
                    "the record applied the change, so this is not the ordering the case is about"
                );
                proposing = Some(handle);
                (1, 2)
            }
            Membership::InCoreOnly { commits: true } => unreachable!("not constructible"),
        }
    } else {
        // A region this store does not host. Which peer is asked about does not matter and the
        // case table carries both, because a sender that looked the peer up before the region
        // would answer the wrong question.
        (
            42,
            if case.membership == Membership::InRecord {
                1
            } else {
                77
            },
        )
    };

    let connection = esker_proto::TcpTransport::connect(address).await.unwrap();
    let outcome = within("the snapshot ask", ask(&connection, region_id, peer_id)).await;
    if let Some(handle) = proposing {
        handle.abort();
    }
    node.stop().await;
    Some(outcome)
}

/// Every membership state the sender can be asked from.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_is_served_only_to_a_peer_the_record_names() {
    let mut ran = 0;
    let mut served = 0;
    let mut waited = 0;
    let mut skipped: Vec<&'static str> = Vec::new();

    for case in cases() {
        let Some(outcome) = observe(&case).await else {
            skipped.push(case.name);
            continue;
        };
        if let Err(violation) = check(&case, &outcome, WAIT_FLOOR_MS) {
            panic!(
                "{violation}\n\nADR 0035: the core's membership decides whether the caller is a \
                 stranger, and the applied record decides when it is served — because a \
                 snapshot's header *is* the applied record, so serving on the core's word ships a \
                 header that does not name the peer receiving it."
            );
        }
        ran += 1;
        if outcome.answer == Answer::Served {
            served += 1;
        }
        if outcome.waited_ms >= WAIT_FLOOR_MS {
            waited += 1;
        }
    }

    assert_eq!(
        skipped,
        vec!["the core has the peer and the change commits"],
        "exactly one state is not constructible: a learner's addition commits on the existing \
         voters alone, so the gap it opens is microseconds wide and cannot be held from outside \
         the store. The record can never know more than the core, so the mirror state does not \
         exist at all."
    );
    assert_eq!(ran, 5, "the table shrank without the checker noticing");
    assert_eq!(
        served, 1,
        "exactly one state permits serving; a run where none did never exercised the serving path"
    );
    assert_eq!(
        waited, 1,
        "exactly one state is waited for. A run where none was is a sender that refuses on sight, \
         which is the pre-1502d0f answer; a run where more than one was is a sender that makes \
         every wrong ask cost the bound."
    );
}
