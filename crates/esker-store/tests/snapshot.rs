//! A region moving to a store that never had it: the membership change, the transfer, and the
//! crash in the middle of both.
//!
//! This is the first sub-phase where a region exists somewhere it was not created, so it is the
//! first where "which store holds what" can be wrong in a way no earlier test could produce. The
//! two things checked hardest are the two that would be silent: a peer that is added but never
//! filled, and a peer that is filled but only half way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::{
    Epoch, Operator, PeerRole, RawKvReq, RawKvResp, Region, RequestHeader, Server, ServerHandle,
    Service, TransportConfig, TxnKvReq, TxnKvResp, TxnMutation,
};
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

/// A store on a socket, with everything needed to keep it alive.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = within("the server to shut down", self.handle.shutdown()).await;
    }
}

fn raft_options(
    peers: Vec<PeerAddress>,
    compaction: LogCompaction,
    bootstrap_voters: Option<Vec<u64>>,
) -> RaftOptions {
    let mut raft = RaftOptions::new(peers, 20_260_830);
    // **25 ms and not 5.** `esker-raft` counts ticks and never reads a clock, so the election
    // timeout is 10-20 of these: 250-500 ms here against production's 1-2 s (`TICK_MS` = 100).
    // At 5 ms it was 50-100 ms, and a 50 ms election timeout is a bet that the box will schedule
    // this thread within 50 ms. Under a saturated `--workspace` run it will not, and the trace is
    // unmistakable — a two-voter region racing its term 22 -> 97 in fifteen seconds, both peers
    // alternately campaigning, no leader for long enough to apply anything. That is correct Raft
    // on a machine that has been taken away from it, not a bug to find; the bug was compressing
    // the timeout twentyfold while scheduling jitter did not compress with it
    // (`docs/plans/debt-c1.md` section 3).
    raft.tick = Duration::from_millis(25);
    raft.compaction = compaction;
    raft.bootstrap_voters = bootstrap_voters;
    raft
}

/// Turns the store's own tracing on when `RUST_LOG` is set. A transfer that does not happen is
/// always "something was dropped somewhere", and only the log says which something.
fn trace() {
    use tracing_subscriber::fmt;
    let _ = fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// The deadline every store-reaching await in this file gets, so a wedge becomes a failure.
///
/// Sixty seconds is far past anything here needs — this file runs in about two seconds on a
/// quiet box and under thirty on a saturated one — so it can only fire on a wait that was never
/// going to end.
const AWAIT_DEADLINE: Duration = Duration::from_secs(60);

/// Awaits `future`, failing with `what` rather than wedging on it.
///
/// **The store's own awaits have no upper bound.** [`esker_store::peer::RaftPeer::propose`]
/// waits on a oneshot that only `complete_proposal` resolves, and that runs when an entry
/// applies at the proposal's index — so a proposal whose index never applies, on a peer nobody
/// stops, waits for ever. A test that inherits that wait does not fail slowly; it does not fail
/// at all.
///
/// Observed rather than theorised: a build of this test from before the tick change sat on
/// `a_region_reaches_a_store_that_never_had_it` for **twenty-one hours**, its main thread parked
/// in `block_on` while the store's tickers kept polling beside it. A flake costs a re-run; a
/// wedge costs a CI agent until a human notices. The deadline does not fix the underlying
/// unbounded wait — that is product code and is written up in `docs/plans/debt-c1.md` section 7
/// — it makes the symptom a diagnosis instead of a silence.
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(AWAIT_DEADLINE, future)
        .await
        .unwrap_or_else(|_: tokio::time::error::Elapsed| {
            panic!(
                "timed out after {AWAIT_DEADLINE:?} waiting for {what}. This is a wait with no \
                 end rather than a slow one: most likely a proposal whose index never applied, \
                 which the store has no timeout of its own for."
            )
        })
}

/// Takes a free port and releases it, so two stores can be told each other's addresses before
/// either is listening. A loopback port is not reused between this and the bind that follows.
/// A port, **held** until the server that will serve on it adopts the socket.
///
/// Returning the address and dropping the listener leaves the port belonging to nobody until the
/// rebind, and under a parallel suite run something else takes it — `Address already in use`.
/// `Server::from_listener` takes the socket itself, so there is no window.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn open(
    address_listener: std::net::TcpListener,
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
    bootstrap_region: u64,
) -> Node {
    let address = address_listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: bootstrap_region,
            raft: Some(raft),
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            // An operator reaches a store on the answer to a region heartbeat and has no other
            // way in, so the region interval is also the repair latency. Short here for the same
            // reason the raft tick is.
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                // Nothing in this file is about splitting; a region that split under the test
                // would move the ranges it is asserting about.
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
    Node {
        store,
        handle,
        _dir: dir,
    }
}

async fn wait_for<F: FnMut() -> bool>(what: &str, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:05}"))
}

/// The `default` column family's tag in a snapshot chunk, which is where a `RawKV` pair goes.
///
/// The number rather than a name because the tag is the format's and the tests that use it are
/// about the bytes: a snapshot's pairs chunk names its column family (`esker_store::snapshot`).
const DEFAULT_CF: u8 = 1;

/// The engine key a `RawKV` put of `user_key` writes: the `'r'` namespace, which a snapshot now
/// carries as it finds it rather than stripping and re-adding.
fn raw(user_key: &[u8]) -> Bytes {
    Bytes::from(esker_keys::prefix::raw_key(user_key))
}

/// Writes one key, retrying while the answer is one the caller is told to retry.
///
/// **Not `unwrap`.** A one-shot write makes "leadership does not move, and no epoch changes
/// underneath us" a silent precondition of every test in this file, and that precondition is not
/// this file's subject — a region *arriving* is. It is also false: once `AddPeer`'s learner is
/// promoted the region has two voters, and a two-voter group on a box that will not schedule its
/// threads legitimately elects the other one. Under saturation that is what happened, fifteen
/// times in twenty runs, as `NotLeader { leader_hint: Some(2) }` out of a `put` three lines
/// after the region had arrived exactly as the test wanted (`docs/plans/debt-c1.md` section 3).
///
/// Retrying is the honest reading of a retryable error, and a non-retryable one still fails the
/// test on the spot. The epoch is re-read each time round, because the reason to retry is that
/// something moved.
async fn put(group: &[&Arc<Store>], region: &Region, key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = Instant::now();
    let mut attempts = 0u32;
    let mut at = 0usize;
    loop {
        attempts += 1;
        let store = group[at % group.len()];
        // The region as it stands now, not as the caller last saw it.
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        let request = RawKvReq::put(key.clone(), Bytes::copy_from_slice(value));
        match within("a put to be applied", store.serve(header, request)).await {
            Ok(_) => return,
            Err(error) => {
                // **An ambiguous answer is not a retryable one in general.** A leader that
                // stepped down with this proposal in its log answers `Unknown`: the entry may
                // still commit, so a client that repeated the write could apply it twice
                // (`esker-store`'s pending-proposal invariant). Repeating is safe *here*, and
                // only here, because every write in this test is an idempotent `Put` of one
                // fixed value from a single writer, so a second apply cannot be observed.
                // Before the store answered this case at all, it was a hang.
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "writing {key:?}: {error}");
                assert!(
                    Instant::now() < deadline,
                    "writing {key:?} never succeeded after {attempts} attempts in {:?}; \
                     last answer {error}; last asked store {}, whose peer says leader={:?}",
                    started.elapsed(),
                    store.store_id(),
                    store.peer_of(region.id).map(|peer| peer.leader())
                );
                // **A hint that points outside the group is the caller's mistake, and it is
                // fatal here rather than at the deadline.** Following a redirect can only reach
                // a store this helper was handed. When the office is on one the caller left
                // out, every remaining attempt re-asks a follower that has already answered, so
                // the deadline is spent re-proving what the first refusal said. That is what
                // 9046 attempts in 30.000411395s were, all to store 1, which answered
                // `leader=Some(2)` every time (`docs/plans/debt-c7.md` section 11). The peer
                // list of the store that refused says where the office went, so say it.
                if let esker_proto::ProtoError::NotLeader {
                    leader_hint: Some(peer_id),
                    ..
                } = &error
                    && let Some(elsewhere) = store.regions().get(region.id).and_then(|state| {
                        state
                            .region()
                            .peers
                            .iter()
                            .find(|peer| peer.peer_id == *peer_id)
                            .map(|peer| peer.store_id)
                    })
                {
                    assert!(
                        group
                            .iter()
                            .any(|candidate| candidate.store_id() == elsewhere),
                        "writing {key:?}: the office is peer {peer_id} on store {elsewhere}, \
                         and this put was given only stores {:?}; retrying cannot reach a store \
                         it was not handed",
                        group
                            .iter()
                            .map(|candidate| candidate.store_id())
                            .collect::<Vec<_>>()
                    );
                }

                // **Follow the hint, or this is a livelock rather than a retry.** An election
                // moves no epoch, so re-sending to the peer that just disclaimed leadership at
                // the same epoch asks a question already answered, and in a two-voter group the
                // office does not come back on its own. `NotLeader` names the peer that has it;
                // anything else advances round the group, because a store that cannot answer is
                // not made able to by being asked again.
                if let esker_proto::ProtoError::NotLeader {
                    leader_hint: Some(peer_id),
                    ..
                } = &error
                    && let Some(next) = group.iter().position(|candidate| {
                        candidate.regions().get(region.id).is_some_and(|state| {
                            state.region().peers.iter().any(|peer| {
                                peer.peer_id == *peer_id && peer.store_id == candidate.store_id()
                            })
                        })
                    })
                {
                    at = next;
                } else {
                    at += 1;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// Two voters for region 1 with the office deliberately on the **second** store.
///
/// The state this file's two redirect tests need and neither should race for. Both of the
/// sightings behind them arrive here by being unlucky — a peer starved for the 250-500 ms this
/// file's tick budget allows — which is why neither reproduced under load and both reproduce
/// instantly when the office is moved on purpose.
///
/// Returns once the second store leads *and* the first does not, so anything a caller then aims
/// at `first` is aimed at a store that has stopped leading.
async fn two_voters_with_the_office_on_the_second() -> (Node, Node) {
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address_listener,
        1,
        &pd,
        raft_options(peers.clone(), LogCompaction::new(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = first.store.regions().regions()[0].clone();
    put(&[&first.store], &region, key(0), b"before").await;

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers, LogCompaction::new(), Some(vec![2])),
        2,
    )
    .await;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch: first.store.regions().regions()[0].epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the second store to become a voter", || {
        first.store.regions().get(1).is_some_and(|state| {
            state
                .region()
                .peers
                .iter()
                .any(|peer| peer.peer_id == 2 && peer.role == PeerRole::Voter)
        })
    })
    .await;

    // The office moves, deliberately. Everything above is setup; this is the state the two
    // sightings arrive at by being unlucky.
    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::TransferLeader {
        region_id: 1,
        epoch,
        to_peer_id: 2,
    });
    wait_for("the second store to lead", || {
        second.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    assert!(
        !first.store.peer_of(1).is_some_and(|peer| peer.is_leader()),
        "the first store still leads, so the office did not move and this proves nothing"
    );

    (first, second)
}

/// **A write aimed at a store that has stopped leading is never answered, however long it waits.**
///
/// The mechanism behind two recorded sightings of `snapshot.rs:228`
/// (`a_region_reaches_a_store_that_never_had_it`,
/// `a_snapshot_replacing_a_held_region_routes_through_a_retire`), driven rather than waited for.
/// Neither reproduced under load — not under twenty-four spinning threads, 8 runs each, nor under
/// six full runs of this crate's 305 tests — because both need an *election*, and an election
/// needs a peer starved for the 250-500 ms this file's tick budget allows. Moving the office on
/// purpose reaches the same state in a second and always.
///
/// [`put`] retries what the store tells it to retry, which is right, and re-reads the region's
/// epoch each time round, which is also right and is not enough: **an election moves no epoch**.
/// So a `NotLeader` is re-sent to the peer that just disclaimed leadership, at the same epoch,
/// until the deadline — and in a two-voter group the office does not come back on its own. That is
/// a livelock, not a slow write, and thirty seconds of it is indistinguishable from a hang.
///
/// The fix is to follow the hint the refusal already carries, which is what a real client does
/// (`esker-client`'s router, `docs/DESIGN.md` §10). This test is red without it: it fails at
/// [`put`]'s deadline having spent thirty seconds re-asking a follower.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_follows_the_office_when_it_moves() {
    trace();
    let (first, second) = two_voters_with_the_office_on_the_second().await;

    // Aimed at the store that has *stopped* leading, which is exactly what the sightings do.
    // Without following the hint this spends the whole deadline and fails.
    let region = first.store.regions().regions()[0].clone();
    put(&[&first.store, &second.store], &region, key(1), b"after").await;

    let header = RequestHeader::new(region.id, second.store.regions().regions()[0].epoch, 0);
    assert_eq!(
        within(
            "the new leader to answer",
            second.store.serve(header, RawKvReq::get(key(1)))
        )
        .await
        .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"after"))
        },
        "the write did not reach the store that now leads"
    );

    first.stop().await;
    second.stop().await;
}

/// **A write cannot follow the office to a store it was never handed.**
///
/// [`put`] follows `NotLeader`'s hint, which is right and is not sufficient: the hint names a
/// *peer*, and this helper can only ask the stores its caller passed. Where those disagree the
/// loop has nothing it can do with the answer it is given, and re-asks the follower until the
/// deadline.
///
/// That is the mechanism behind `a_snapshot_replacing_a_held_region_routes_through_a_retire`
/// failing in g1's gate on 2026-09-04 at 35.854 s — well under the 60 s deadline it was blamed
/// on twice: `writing b"k00058" never succeeded after 9046 attempts in 30.000411395s`, every one
/// of them to store 1, which answered `leader=Some(Some(2))` every time. The announcement
/// helper wrote keys 40..60 through a one-store group *after* `AddPeer` had made a second voter,
/// so there was a redirect and nowhere to follow it to.
///
/// The deadline was never the mechanism and lengthening it fixes nothing. This pins the
/// diagnosis instead: a hint the group cannot honour fails the write at once and names the store
/// the caller left out, rather than spending thirty seconds proving the first refusal.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(
    expected = "the office is peer 2 on store 2, and this put was given only stores [1]"
)]
async fn a_put_says_which_store_it_was_not_given() {
    trace();
    let (first, _second) = two_voters_with_the_office_on_the_second().await;

    // Deliberately the group the announcement helper used to pass: the store that has stopped
    // leading, and only it.
    let region = first.store.regions().regions()[0].clone();
    put(&[&first.store], &region, key(1), b"after").await;
}

/// Commits one key through Percolator on `store`, as a client would: prewrite, then commit.
///
/// The replicated path, so both entries go through the log and apply on every peer — which is
/// what makes "the peer that joins later" a question about the transfer rather than about a
/// write that never replicated. Retried on the same terms as [`put`], and for the same reason.
async fn commit_one(
    store: &Arc<Store>,
    region: &Region,
    key: Bytes,
    value: &'static [u8],
    start_ts: u64,
    commit_ts: u64,
) {
    let requests = [
        TxnKvReq::Prewrite {
            start_ts,
            primary: key.clone(),
            ttl_ms: 3_000,
            mutations: vec![TxnMutation::Put {
                key: key.clone(),
                value: Bytes::from_static(value),
                read_ts: None,
            }],
        },
        TxnKvReq::Commit {
            start_ts,
            commit_ts,
            keys: vec![key.clone()],
        },
    ];
    for request in requests {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let epoch = store
                .regions()
                .get(region.id)
                .map_or(region.epoch, |state| state.region().epoch);
            let header = RequestHeader::new(region.id, epoch, 0);
            match within(
                "a transactional write to be applied",
                store.serve_txn(header, request.clone()),
            )
            .await
            {
                Ok(_) => break,
                Err(error) => {
                    // A prewrite and a commit are both idempotent for a single writer of one
                    // fixed value, so an ambiguous answer may be repeated here — the same
                    // argument `put` makes above, and no wider.
                    if error.is_ambiguous() {
                        continue;
                    }
                    assert!(error.is_retryable(), "committing {key:?}: {error}");
                    assert!(
                        Instant::now() < deadline,
                        "committing {key:?} never succeeded; last answer {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }
}

/// Any well-formed command; the routing test only needs the proposal to be refused or accepted.
fn put_command() -> esker_store::apply::Command {
    esker_store::apply::Command::Put {
        key: key(0),
        value: Bytes::from_static(b"after"),
    }
}

// -- membership -------------------------------------------------------------------------

/// The placement driver asks for a replica; the leader proposes a **learner**, and the region's
/// peer list and `conf_ver` move with it. A learner and not a voter, because a voter that is not
/// caught up raises the bar for a quorum while it is catching up.
#[tokio::test(flavor = "multi_thread")]
async fn an_add_peer_operator_makes_a_learner() {
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let node = open(
        address_listener,
        1,
        &pd,
        raft_options(
            vec![PeerAddress::new(1, 1, address)],
            LogCompaction::new(),
            None,
        ),
        1,
    )
    .await;

    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let before = node.store.regions().regions()[0].clone();
    assert_eq!(before.peers.len(), 1);

    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch: before.epoch,
        store_id: 9,
        peer_id: 90,
    });
    wait_for("the learner to appear", || {
        node.store.regions().regions()[0].peers.len() == 2
    })
    .await;

    let after = node.store.regions().regions()[0].clone();
    let learner = after
        .peers
        .iter()
        .find(|peer| peer.peer_id == 90)
        .expect("the operator's peer");
    assert_eq!(
        learner.role,
        PeerRole::Learner,
        "a voter was added directly"
    );
    assert_eq!(learner.store_id, 9, "the store id came through the entry");
    assert_eq!(
        after.epoch,
        Epoch::new(before.epoch.conf_ver + 1, before.epoch.version),
        "a membership change moves conf_ver and not version"
    );

    // And it is on disk: a restart recovers the membership rather than the one it started with.
    node.store.stop();
    node.store.flush().unwrap();
    node.stop().await;
}

/// An operator decided against an epoch the region has moved past is dropped. Applying it anyway
/// is how two half-informed schedulers take a region below quorum between them.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_against_a_stale_epoch_is_dropped() {
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let node = open(
        address_listener,
        1,
        &pd,
        raft_options(
            vec![PeerAddress::new(1, 1, address)],
            LogCompaction::new(),
            None,
        ),
        1,
    )
    .await;
    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let stale = Epoch::new(99, 99);
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch: stale,
        store_id: 9,
        peer_id: 90,
    });
    // **Waited for by evidence, not by clock.** `FakePd` hands an operator out once, removing it
    // as it answers the heartbeat, so `operator_pending` going false is proof the store has
    // *taken* it; and one further region heartbeat proves the loop iteration that ran it has
    // finished, because `run_operator` is called in the same iteration as the beat that returned
    // it. Before this the test slept 200 ms — a **negative** assertion behind a wall clock, which
    // under load does not go red, it goes vacuously green: the store may not have fetched the
    // operator at all, and "nothing was applied" is then true of a store that never looked.
    wait_for("the stale operator to be taken by the store", || {
        !pd.operator_pending(1)
    })
    .await;
    let carried = pd.region_beats().len();
    wait_for("the round that ran it to finish", || {
        pd.region_beats().len() > carried
    })
    .await;
    assert_eq!(
        node.store.regions().regions()[0].peers.len(),
        1,
        "an operator from a stale epoch was applied"
    );

    // A `TransferLeader` naming a peer this region does not have is dropped too, and it never
    // moves the epoch either way: who leads is not part of a region's identity, which is why a
    // client learns it from a `NotLeader` hint rather than from its cache.
    let epoch = node.store.regions().regions()[0].epoch;
    pd.issue(Operator::TransferLeader {
        region_id: 1,
        epoch,
        to_peer_id: 90,
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(node.store.regions().regions()[0].epoch, epoch);
    assert!(
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader()),
        "a transfer to a peer that does not exist unseated the leader"
    );
    node.stop().await;
}

// -- the transfer -----------------------------------------------------------------------

/// The whole of 4c in one run: a region on one store, a learner added on another, the leader's log
/// compacted past what the learner needs, and the region arriving on the second store by snapshot
/// with its data intact.
#[tokio::test(flavor = "multi_thread")]
async fn a_region_reaches_a_store_that_never_had_it() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    // The leader's log is compacted aggressively, so the follower is past its start almost at
    // once — which is the only way a snapshot is ever needed.
    let compaction = LogCompaction {
        threshold: 8,
        keep: 2,
        ..LogCompaction::new()
    };
    let first = open(
        first_address_listener,
        1,
        &pd,
        // The address book has both stores — the leader must know how to reach a peer it is about
        // to be told it has — while region 1 bootstraps with **one** voter, so this store can
        // commit on its own and the second joins as a learner.
        raft_options(peers.clone(), compaction, Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let region = first.store.regions().regions()[0].clone();
    for n in 0..40 {
        put(&[&first.store], &region, key(n), b"value").await;
    }

    // The second store is told the cluster already exists, so it hosts nothing of its own.
    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), compaction, Some(vec![2])),
        2,
    )
    .await;
    assert!(
        second.store.regions().is_empty(),
        "the second store bootstrapped a region of its own"
    );

    // Now add it to the region. The leader proposes a learner, replicates to it, finds its own log
    // has been compacted past what the learner needs, and offers a snapshot.
    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the learner in the membership", || {
        first.store.regions().regions()[0].peers.len() == 2
    })
    .await;

    // Keep writing, so the leader's log compacts past the learner and a snapshot becomes the only
    // way to catch it up. **Both stores**, because the learner is promotable from here and a
    // two-voter group can move the office mid-batch; aiming at `first` alone is the livelock this
    // file's `a_write_follows_the_office_when_it_moves` pins.
    for n in 40..120 {
        let region = first.store.regions().regions()[0].clone();
        put(&[&first.store, &second.store], &region, key(n), b"value").await;
    }

    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;

    // It arrived with the range, the epoch and the membership the sender had.
    let arrived = second.store.regions().get(1).unwrap().region().clone();
    assert_eq!(arrived.start_key, Bytes::new());
    assert_eq!(arrived.end_key, Bytes::new());
    assert_eq!(arrived.peers.len(), 2);

    // And with the data. Read it through the direct path rather than the replicated one: the
    // second store's peer is a learner and does not serve reads, which is the point of a learner.
    let header = RequestHeader::new(arrived.id, arrived.epoch, 0);
    for n in 0..40 {
        assert_eq!(
            second.store.handle(header, RawKvReq::get(key(n))).unwrap(),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"value"))
            },
            "key {n} did not arrive"
        );
    }

    first.stop().await;
    second.stop().await;
}

/// A region that arrives by snapshot arrives with its **transactional** records, not only its
/// raw pairs.
///
/// # The trace this was written from
///
/// A columnar learner placed by PD on a live four-store cluster published apply index 22 with
/// **two** `write` records where its leader had ten (`docs/plans/phase-8-learner.md` §store,
/// "THE BLOCKER"). Two, not zero, because two commits happened *after* the transfer and came
/// down the log; everything committed before it was missing. That is the shape of a snapshot
/// that carries one column family out of three, and it is what this reproduces in two seconds
/// with no cluster: the region moves, the raw pair goes with it, and the Percolator records do
/// not.
///
/// A **row** learner hides this. Its store is caught up by the log whenever the leader has not
/// compacted, and when it is caught up by snapshot instead, promotion waits on `matched` — a
/// number the transfer moves whether or not the bytes were complete. Nothing downstream reads
/// its `write` records until it leads, by which time a later snapshot or a full log has usually
/// covered the hole. A columnar learner is never promoted and its whole job is to read those
/// records, so it is the first replica in this system for which an incomplete transfer is
/// visible rather than merely true.
#[tokio::test(flavor = "multi_thread")]
async fn a_region_arrives_with_its_transactional_records() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    // **No aggressive compaction.** The leader's log is intact, so a snapshot here is not the
    // repair of a peer that fell behind — it is the ordinary way a store that never had the
    // region receives it, which is the path every placed replica takes.
    let first = open(
        first_address_listener,
        1,
        &pd,
        raft_options(peers.clone(), LogCompaction::new(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    // Four keys through Percolator and one through `RawKV`. Both halves before the peer exists,
    // so both halves are the transfer's job — and a failure that reports one and not the other
    // says which column family was dropped rather than that "data is missing".
    let region = first.store.regions().regions()[0].clone();
    for n in 0..4 {
        commit_one(
            &first.store,
            &region,
            key(n),
            b"committed",
            10 + u64::from(n) * 2,
            11 + u64::from(n) * 2,
        )
        .await;
    }
    put(&[&first.store], &region, key(100), b"raw").await;

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), LogCompaction::new(), Some(vec![2])),
        2,
    )
    .await;
    assert!(
        second.store.regions().is_empty(),
        "the second store bootstrapped a region of its own"
    );

    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;

    // The raw half, which has always arrived.
    let arrived = second.store.regions().get(1).unwrap().region().clone();
    let header = RequestHeader::new(arrived.id, arrived.epoch, 0);
    assert_eq!(
        second
            .store
            .handle(header, RawKvReq::get(key(100)))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"raw"))
        },
        "the raw pair did not arrive, so this is not the failure this test is about"
    );

    // The transactional half. Counted first, because a count says how much was lost where a
    // read says only that one key was.
    let state = second.store.regions().get(1).unwrap();
    let missing: Vec<u32> = (0..4)
        .filter(|n| second.store.write_records(&key(*n)).unwrap() == 0)
        .collect();
    assert!(
        missing.is_empty(),
        "the region arrived without the `write` records of {missing:?}: a snapshot that ships \
         one column family out of three"
    );
    for n in 0..4 {
        assert_eq!(
            second
                .store
                .handle_txn(
                    &state,
                    TxnKvReq::Get {
                        key: key(n),
                        ts: 100,
                    },
                )
                .unwrap(),
            TxnKvResp::Get {
                value: Some(Bytes::from_static(b"committed"))
            },
            "key {n} committed before the transfer is not readable after it"
        );
    }

    first.stop().await;
    second.stop().await;
}

/// **The user-facing shape.** A voter caught up by snapshot, given the office, answers a read of
/// a row committed before it joined.
///
/// The other two tests in this pair look at a learner's column families, which is where the
/// defect was found; this one asks the question a client asks, through the front door, of the
/// store a client would be routed to. On the version-1 stream it answered `None` — a committed
/// row read as absent, by a leader, with no error anywhere. That is a lost acknowledged write as
/// far as anybody outside the store can tell, and it needed no columnar anything: a peer added
/// after the log was compacted, promoted, and elected is a sequence phases 4 and 5 already had.
///
/// # Could the acceptance suites have caught it
///
/// No, and the reason is a gap between two files rather than a weak assertion in either.
/// `tests/snapshot.rs` is the only place that installs a snapshot on a peer and reads it back,
/// and every write in it was `RawKV` — the one namespace version 1 shipped. `tests/txnkv.rs` and
/// `esker-txn`'s matrix are the only places that write Percolator records, and both run against a
/// single store that never transfers a region. `tests/promotion.rs` drives learners to voters
/// under load, which is this test's first half, but its load generator writes `RawKV` too and it
/// asserts about roles rather than about values. So the two halves — a transfer, and a
/// transactional value read afterwards — had never met.
#[tokio::test(flavor = "multi_thread")]
async fn a_voter_caught_up_by_snapshot_can_lead_and_answer_an_old_row() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];
    // Compacted hard, so the log cannot catch the second store up and a snapshot is the only way
    // it ever holds the region — the case a store that joins a running cluster is in.
    let compaction = LogCompaction {
        threshold: 8,
        keep: 2,
        ..LogCompaction::new()
    };

    let first = open(
        first_address_listener,
        1,
        &pd,
        raft_options(peers.clone(), compaction, Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    // The old row: committed before the second store exists, and never written again.
    let region = first.store.regions().regions()[0].clone();
    commit_one(&first.store, &region, key(0), b"committed", 10, 11).await;

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), compaction, Some(vec![2])),
        2,
    )
    .await;
    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch,
        store_id: 2,
        peer_id: 2,
    });

    // Keep writing, so the leader's log compacts past the new peer and the snapshot is what
    // catches it up rather than the entries. Both stores, for the reason above.
    for n in 1..80 {
        let region = first.store.regions().regions()[0].clone();
        put(&[&first.store, &second.store], &region, key(n), b"value").await;
    }
    wait_for("the second store to hold the region", || {
        second.store.regions().get(1).is_some()
    })
    .await;
    wait_for("the second store's peer to be a voter", || {
        first.store.regions().regions()[0]
            .peers
            .iter()
            .any(|peer| peer.peer_id == 2 && peer.role == PeerRole::Voter)
    })
    .await;

    // The office moves. A transfer only completes if the target is caught up, so reaching this
    // point at all is the cluster's own statement that the second store has the region.
    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::TransferLeader {
        region_id: 1,
        epoch,
        to_peer_id: 2,
    });
    wait_for("the second store to lead", || {
        second.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    // And the read a client makes, of a row committed long before this store existed, served by
    // the replicated path on the store that now leads.
    let region = second.store.regions().get(1).unwrap().region().clone();
    let header = RequestHeader::new(region.id, region.epoch, 0);
    let answer = within(
        "the new leader to answer a read",
        second.store.serve_txn(
            header,
            TxnKvReq::Get {
                key: key(0),
                ts: 100,
            },
        ),
    )
    .await
    .expect("the new leader refused a read of its own region");
    assert_eq!(
        answer,
        TxnKvResp::Get {
            value: Some(Bytes::from_static(b"committed"))
        },
        "a row committed before this peer joined reads as absent from the leader: an \
         acknowledged write lost by a transfer that carried one column family"
    );

    first.stop().await;
    second.stop().await;
}

/// **The blocker, as PD creates it.** A columnar learner placed by an operator reaches the
/// leader's applied index holding the leader's data.
///
/// The regression `docs/plans/phase-8-learner.md` §store asks for. It differs from the test
/// above in the one way that matters for how the defect was found: nothing here is promoted and
/// nothing here is read through the front door, so the *only* evidence that the transfer was
/// complete is what the learner's own column families hold — which is exactly the position the
/// fragment service is in when it is asked for a table.
#[tokio::test(flavor = "multi_thread")]
async fn a_placed_columnar_learner_holds_what_the_leader_holds() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address_listener,
        1,
        &pd,
        raft_options(peers.clone(), LogCompaction::new(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let region = first.store.regions().regions()[0].clone();
    for n in 0..6 {
        commit_one(
            &first.store,
            &region,
            key(n),
            b"committed",
            10 + u64::from(n) * 2,
            11 + u64::from(n) * 2,
        )
        .await;
    }

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), LogCompaction::new(), Some(vec![2])),
        2,
    )
    .await;

    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::AddLearner {
        region_id: 1,
        epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the columnar learner in the membership", || {
        first.store.regions().regions()[0]
            .peers
            .iter()
            .any(|peer| peer.role == PeerRole::ColumnarLearner)
    })
    .await;
    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;

    // One more commit after the placement, so "caught up" means the stream as well as the
    // transfer — a learner that received nothing and then followed the log perfectly would pass
    // an assertion about the tail alone, and that is the state the cluster was found in.
    let region = first.store.regions().regions()[0].clone();
    let leader = first.store.peer_of(1).unwrap();
    commit_one(&first.store, &region, key(6), b"committed", 30, 31).await;

    // **The bar is asked of the driver, not read from what the peer publishes.**
    //
    // `RaftPeer::applied_index` reads `Published::applied`, which the driver refreshes at the end
    // of a batch — *after* `complete_proposal` has already told `commit_one` the entry applied. So
    // between the acknowledgement and the next publish, the leader holds key 6 and still names the
    // entry before it, and a learner that has merely caught up to *that* satisfies `>=` while
    // holding nothing of the commit. `status()` is answered inside the driver from the core's own
    // `applied`, so it cannot name an index whose data has not landed.
    //
    // Measured rather than reasoned: publishing two entries behind on the leader alone —
    // everything else untouched, so replication keeps its timing — turned this test red 5 times
    // out of 5 with the identical `(6, 1, 0)`, and reading the bar here instead makes it green
    // under the same injection. The `>=` on the learner's side stays a *published* read on
    // purpose: a learner that looks behind only makes this wait longer, which is the safe
    // direction, and it is the same value the fragment service reads.
    let bar = leader
        .status()
        .await
        .expect("the leader answers its own status")
        .applied;
    wait_for(
        "the learner to reach the index the leader acknowledged",
        || {
            second
                .store
                .peer_of(1)
                .is_some_and(|peer| peer.applied_index() >= bar)
        },
    )
    .await;

    let short: Vec<(u32, u64, u64)> = (0..7)
        .map(|n| {
            (
                n,
                first.store.write_records(&key(n)).unwrap(),
                second.store.write_records(&key(n)).unwrap(),
            )
        })
        .filter(|(_, leader, learner)| leader != learner)
        .collect();
    assert!(
        short.is_empty(),
        "the columnar learner is at the leader's applied index without the leader's data; \
         (key, leader versions, learner versions) = {short:?}; the bar was {bar}, the leader \
         publishes {}, the learner publishes {:?}",
        leader.applied_index(),
        second.store.peer_of(1).map(|peer| peer.applied_index()),
    );

    first.stop().await;
    second.stop().await;
}

/// Announces a snapshot the learner cannot already have, and waits for its old peer to be retired.
///
/// # The assumption, named because this used to race for it
///
/// `receive_raft` retires the held peer only when the announced index is **above** the learner's
/// own applied index — that gap is the whole precondition of the scenario. The first version of
/// this opened it by *hoping*: eighty writes to push the leader ahead, an announcement carrying the
/// **leader's** applied index, five seconds of polling, and another round if the learner had
/// already caught up. A wall clock decided how many rounds there was room for.
///
/// It waits for it now, and announces once. **The index must also be one the leader can serve**,
/// which is what the writes were really for — an index a sender has nothing at is declined, so
/// inventing one (`applied + 1`) makes the announcement a no-op, measured. So the loop pushes the
/// leader forward until a gap exists and then *stops*: losing a round costs twenty more writes
/// rather than five seconds and a fresh announcement, and the only clock left is the file's wedge
/// net, which now fails with "no gap ever opened" instead of "the peer was never retired".
///
/// Measured before the change, one test, four machine loads: **0.95 s idle, 24.7 s under fourteen
/// busy threads, 78.5 s under forty, 11.25 s under eighty** — and a failure at 65 s in the
/// workspace gate, twice. Not a slow test: a race whose outcome flips, because starving the learner
/// *harder* widens the gap and the first round wins, while a middling load keeps it just close
/// enough to lose several. Numbers that go up and then down with load are the signature.
async fn announce_a_snapshot_and_await_the_retire(
    first: &Node,
    second: &Node,
    old: &Arc<esker_store::RaftPeer>,
) {
    let leader = first.store.peer_of(1).expect("the leader");
    let Some(state) = second.store.regions().get(1) else {
        // No region to replace: the scenario is already past what it was going to observe.
        return;
    };

    // **Wait for the precondition, then announce.** The guard in `Store::receive_raft` is
    // `!peer.is_leader() && peer.applied_index() < index`, and the index also has to be one the
    // *sender* can serve — which is why the writes are here at all. So the loop below waits for a
    // gap to exist and stops; it does not retry the scenario, and losing a round costs one more
    // write rather than five seconds and a fresh announcement.
    let behind_by = within(
        "a gap between the learner's applied index and the leader's",
        async {
            loop {
                let region = first.store.regions().regions()[0].clone();
                for n in 40..60 {
                    // **Both stores, because by here there are two voters.** `AddPeer` has
                    // promoted peer 2, so the office can move while this loop runs, and a
                    // one-store group cannot follow the redirect it is then given.
                    put(&[&first.store, &second.store], &region, key(n), b"value").await;
                }
                let ahead = leader.applied_index();
                if old.applied_index() < ahead {
                    return ahead;
                }
            }
        },
    )
    .await;

    let announcement = esker_proto::RaftMessage::new(
        1,
        state.region().epoch,
        1,
        esker_raft::Message::InstallSnapshot {
            from: 1,
            to: 2,
            term: leader.status().await.unwrap().term,
            snapshot: esker_raft::Snapshot {
                meta: esker_raft::SnapshotMeta {
                    index: behind_by,
                    term: 1,
                    conf: esker_raft::ConfState::default(),
                },
                data: Bytes::new(),
            },
        },
    );
    second
        .store
        .receive_raft(esker_proto::RaftBatch::new(vec![announcement]))
        .await
        .expect("an announcement is accepted");

    // Then the event itself, under the file's own wedge net rather than a budget of its own: the
    // old peer is gone from the region **and** refuses a proposal, which is what "retired" means
    // to anything that still holds a handle to it.
    within(
        "the old peer to be retired after a snapshot announcement above its applied index",
        async {
            loop {
                let replaced = second
                    .store
                    .peer_of(1)
                    .is_none_or(|now| !Arc::ptr_eq(&now, old));
                if replaced && old.propose(&put_command()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;
}

/// **A snapshot that replaces a region this store already holds retires the old peer first.**
///
/// `Store::fetch_snapshot` step 1 calls `retire_region_now` before it writes a byte, and that is
/// what answers anything still outstanding on the peer being replaced — the snapshot half of the
/// rule stated on [`esker_store::peer`]'s pending queue: *a pending proposal is resolved on every
/// path that can make its index unreachable*. The step-down half has its own regressions next to
/// that queue; this pins the routing, because a refactor of `fetch_snapshot` that dropped the
/// retire would put the leak back without failing anything else.
///
/// Driven by handing the store the announcement directly rather than by arranging for a leader to
/// send one: `receive_raft` is the entry point either way, and naming an index the learner cannot
/// have reached makes the held-region branch a fact rather than a race.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_replacing_a_held_region_routes_through_a_retire() {
    trace();
    let pd = Arc::new(FakePd::new());
    let first_address_listener = reserve();
    let first_address = first_address_listener.local_addr().unwrap();
    let second_address_listener = reserve();
    let second_address = second_address_listener.local_addr().unwrap();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];
    let compaction = LogCompaction {
        threshold: 8,
        keep: 2,
        ..LogCompaction::new()
    };
    let first = open(
        first_address_listener,
        1,
        &pd,
        raft_options(peers.clone(), compaction, Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = first.store.regions().regions()[0].clone();
    for n in 0..40 {
        put(&[&first.store], &region, key(n), b"value").await;
    }

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), compaction, Some(vec![2])),
        2,
    )
    .await;
    let epoch = first.store.regions().regions()[0].epoch;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;

    // The peer that is about to be replaced.
    let old = second
        .store
        .peer_of(1)
        .expect("the arrived region has a peer");

    // `receive_raft` acts only when the learner's apply index is **below** the announced one, so
    // the announcement is built one past what this peer has applied — see
    // [`announce_a_snapshot_and_await_the_retire`], which used to race for that gap and now
    // constructs it.
    announce_a_snapshot_and_await_the_retire(&first, &second, &old).await;

    first.stop().await;
    second.stop().await;
}

/// Whether a snapshot request was refused.
///
/// The refusal arrives *in* the stream rather than instead of it: a streamed reply is opened
/// before the service has decided anything, so a stream that fails is the shape a caller has to
/// handle either way.
async fn snapshot_refused(
    connection: &esker_proto::TcpTransport,
    region_id: u64,
    peer_id: u64,
) -> bool {
    snapshot_refusal(connection, region_id, peer_id)
        .await
        .is_some()
}

/// What a refused snapshot ask said, or `None` if it was served.
///
/// **A refusal arrives in one of two places** and a test that reads only one of them is testing
/// the framing rather than the decision: a store that refuses before the stream is opened fails
/// the call, and one that refuses after — which is what a *waited* refusal does — sends the error
/// as the stream's first chunk. Both are the same answer to the caller, so both are read here.
async fn snapshot_refusal(
    connection: &esker_proto::TcpTransport,
    region_id: u64,
    peer_id: u64,
) -> Option<String> {
    match connection
        .call_stream(esker_proto::Request::Snapshot(
            esker_proto::SnapshotRequest {
                region_id,
                index: 1,
                peer_id,
            },
        ))
        .await
    {
        Err(error) => Some(error.to_string()),
        Ok(mut stream) => match stream.next_chunk().await {
            None => Some("the stream ended before its header".to_owned()),
            Some(Err(error)) => Some(error.to_string()),
            Some(Ok(_)) => None,
        },
    }
}

/// A snapshot is refused to a store that is not a member of the region. A snapshot hands over a
/// region wholesale, and a store with no claim to the range has no claim to a copy of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_outside_the_region_is_refused_a_copy() {
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let node = open(
        address_listener,
        1,
        &pd,
        raft_options(
            vec![PeerAddress::new(1, 1, address)],
            LogCompaction::new(),
            None,
        ),
        1,
    )
    .await;
    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = node.store.regions().regions()[0].clone();
    put(&[&node.store], &region, key(0), b"v").await;

    let connection = esker_proto::TcpTransport::connect(address).await.unwrap();
    assert!(
        snapshot_refused(&connection, 1, 77).await,
        "a stranger was handed a region"
    );
    assert!(
        snapshot_refused(&connection, 42, 1).await,
        "a region this store does not host was served"
    );

    // The member itself is served, and the first chunk is a header naming the region.
    let mut stream = connection
        .call_stream(esker_proto::Request::Snapshot(
            esker_proto::SnapshotRequest {
                region_id: 1,
                index: 1,
                peer_id: 1,
            },
        ))
        .await
        .expect("a member is served");
    let first = stream.next_chunk().await.unwrap().unwrap();
    let header = esker_store::snapshot::SnapshotHeader::decode(&first).unwrap();
    assert_eq!(header.region.id, 1);
    assert!(header.meta.index > 0);

    node.stop().await;
}

/// The sender tells a **stranger** from a member it has not caught up to, and waits for the
/// second rather than refusing it — but it will not serve a snapshot whose record omits the peer
/// receiving it.
///
/// A conf change takes effect in the Raft core when it is *appended*, and in the region record
/// when it is *applied*. Between those two the leader already knows about the new peer and its own
/// record does not — and that gap is exactly where the new peer asks, because the traffic that
/// tells it the region exists is the traffic the leader started sending the moment it appended.
/// The refusal it met, *"peer N is not a member of region M and may not have a copy of it"*, is a
/// correct sentence about the wrong membership (`docs/plans/phase-8-learner.md` §close bullet 4).
///
/// **What this test is really guarding is the fix that was tried first.** Answering the ask
/// straight from the core's membership — the fix the plan named — ships a header carrying the
/// *applied* record, which does not list the peer receiving it; the receiver then writes the
/// record and the whole region and `host_region` declines to start it, so the transfer "succeeds"
/// and nothing is hosted, for ever. `tests/promotion.rs` fails 3 of 3 that way with a learner
/// stranded for the length of the run. So the sender waits for the record instead, which is to say
/// it waits for the change to **commit**, and refuses only if that does not happen.
///
/// # Holding the ordering still
///
/// A voter is added on a store that does not exist. The core takes the change when it appends it,
/// and the commit then needs a quorum of the **new** configuration — which the absent peer is half
/// of — so it never commits, never applies, and the region record never moves. The state is stable
/// rather than a window to hit: the core has peer 2 for the rest of the process and the applied
/// record does not. The leader steps down one election window later, having lost its quorum, and
/// that changes nothing here — `Status::conf` is the latest configuration *in the log*, committed
/// or not.
///
/// That is also a conf change that **can still be rolled back**, and the answer this asserts is
/// the one such a peer must get: not a copy of the region.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_the_core_has_and_the_log_has_not_committed_is_waited_for_and_then_refused() {
    trace();
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let node = open(
        address_listener,
        1,
        &pd,
        raft_options(
            vec![PeerAddress::new(1, 1, address)],
            LogCompaction::new(),
            None,
        ),
        1,
    )
    .await;
    wait_for("a leader", || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;
    let region = node.store.regions().regions()[0].clone();
    put(&[&node.store], &region, key(0), b"v").await;

    let peer = node.store.peer_of(1).unwrap();
    // Never awaited: this proposal is one the group cannot commit, which is the whole point.
    let proposing = tokio::spawn({
        let peer = Arc::clone(&peer);
        async move {
            peer.propose_conf_change(esker_raft::ConfChangeKind::AddVoter, 2, 2, PeerRole::Voter)
                .await
        }
    });
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
            "the core never took the conf change"
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
        "the record applied the change, so this is not the ordering the test is about"
    );

    let connection = esker_proto::TcpTransport::connect(address).await.unwrap();
    let asked_at = Instant::now();
    let refusal = within("the snapshot ask", snapshot_refusal(&connection, 1, 2))
        .await
        .expect("a region was shipped to a peer whose conf change can still be rolled back");
    // **Which refusal it is, is the whole assertion.** "Not a member" is the old answer and means
    // the sender read the applied record and stopped there; this one means it read the core, found
    // the peer, and waited for the change to apply.
    assert!(
        refusal.contains("has not applied here"),
        "the sender refused as if peer 2 were a stranger: {refusal}"
    );
    assert!(
        asked_at.elapsed() >= Duration::from_millis(400),
        "the sender refused on sight rather than waiting for the record: {:?}",
        asked_at.elapsed()
    );

    // A stranger is refused too — and told the other thing, on sight. The check reads a wider
    // membership; it does not read none.
    let stranger_at = Instant::now();
    assert!(
        snapshot_refused(&connection, 1, 77).await,
        "widening the membership check let a stranger in"
    );
    assert!(
        stranger_at.elapsed() < Duration::from_millis(400),
        "a stranger was waited for as if it were a member"
    );

    proposing.abort();
    node.stop().await;
}

/// A snapshot announced but interrupted leaves keys no region covers. The restart clears them, so
/// the retry finds the empty range it is promised — without which the recovery path would be the
/// thing that wedged.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_receive_is_cleared_by_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let region = Region {
        id: 7,
        start_key: Bytes::from_static(b"d"),
        end_key: Bytes::from_static(b"m"),
        peers: vec![esker_proto::Peer::voter(1, 1)],
        epoch: Epoch::INITIAL,
    };

    {
        let db = esker_engine::Db::open_with(
            dir.path(),
            esker_engine::Options {
                create_if_missing: true,
                ..esker_engine::Options::default()
            },
            Arc::new(esker_engine::LocalFileSystem::new()),
            &esker_engine::cf::BUILTIN,
        )
        .unwrap();
        let cf_id = db.cf_id(esker_engine::cf::RAFT).unwrap();

        // The announcement, and then half a transfer's worth of keys — exactly what a crash
        // between steps 2 and 4 leaves.
        let mut batch = esker_engine::WriteBatch::new();
        esker_store::meta::stage_pending_snapshot(&mut batch, cf_id, &region, 42);
        db.write(batch, &esker_engine::WriteOptions::synced())
            .unwrap();
        esker_store::snapshot::stage_pairs(
            &db,
            DEFAULT_CF,
            &[
                (raw(b"e"), Bytes::from_static(b"half")),
                (raw(b"f"), Bytes::from_static(b"half")),
            ],
        )
        .unwrap();
        // A key outside the region, which the cleanup must not touch.
        esker_store::snapshot::stage_pairs(
            &db,
            DEFAULT_CF,
            &[(raw(b"z"), Bytes::from_static(b"other"))],
        )
        .unwrap();
        db.flush_all().unwrap();
    }

    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    assert!(
        store.regions().get(7).is_none(),
        "a half-received region was hosted"
    );

    // The range is clean again, so the retry may start.
    esker_store::snapshot::clear_range(store.db(), &region).expect("the range was not cleared");
    // And the announcement is gone, so the next open does not clear it a second time.
    assert!(
        esker_store::meta::load_pending_snapshots(store.db())
            .unwrap()
            .is_empty()
    );
    // The neighbour's key is untouched: the cleanup is the region's range and nothing else. Read
    // through the store's own region, which covers everything on a freshly bootstrapped store.
    let whole = store.regions().regions()[0].clone();
    assert_eq!(
        store
            .handle(
                RequestHeader::new(whole.id, whole.epoch, 0),
                RawKvReq::get(&b"z"[..])
            )
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"other"))
        }
    );
}

/// A store the snapshot would land on top of is refused, which is the v1 limitation stated
/// plainly: a range cannot be cleared and refilled while the engine has no range tombstones.
#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_range_refuses_a_snapshot_rather_than_half_applying_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let region = store.regions().regions()[0].clone();

    esker_store::snapshot::clear_range(store.db(), &region).expect("a fresh store is clean");
    store
        .handle(
            RequestHeader::new(region.id, region.epoch, 0),
            RawKvReq::put(key(0), Bytes::from_static(b"v")),
        )
        .unwrap();
    // It is emptied rather than refused now, which is what lets a peer that has fallen behind
    // its leader's compaction boundary be repaired at all (`docs/plans/phase-4.md` §18).
    esker_store::snapshot::clear_range(store.db(), &region).expect("the range was cleared");
    assert!(
        store
            .handle(
                RequestHeader::new(region.id, region.epoch, 0),
                RawKvReq::get(key(0)),
            )
            .is_ok_and(|answer| matches!(answer, RawKvResp::Get { value: None })),
        "a cleared range still served a key"
    );
}

/// The read side of a transfer is exactly the region's range, at one instant. A stream that
/// carried a neighbour's keys would hand a store data it has no claim to.
#[tokio::test(flavor = "multi_thread")]
async fn what_is_streamed_is_the_region_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
    let whole = store.regions().regions()[0].clone();
    for n in 0..20 {
        store
            .handle(
                RequestHeader::new(whole.id, whole.epoch, 0),
                RawKvReq::put(key(n), Bytes::from_static(b"v")),
            )
            .unwrap();
    }

    let narrow = Region {
        start_key: key(5),
        end_key: key(9),
        ..whole
    };
    let mut seen = Vec::new();
    esker_store::snapshot::read_pairs(
        store.db(),
        &narrow,
        store.db().snapshot(),
        esker_store::snapshot::CHUNK_TARGET_BYTES,
        |cf_tag, pairs| {
            if cf_tag == DEFAULT_CF {
                seen.extend(pairs);
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        seen.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
        (5..9).map(|n| raw(&key(n))).collect::<Vec<_>>()
    );

    // And a scan of the whole region still answers, so the pinned read did not disturb anything.
    let RawKvResp::Scan { pairs } = store
        .handle(
            RequestHeader::new(whole.id, whole.epoch, 0),
            RawKvReq::scan(&b""[..], &b""[..], 0),
        )
        .unwrap()
    else {
        panic!("not a scan");
    };
    assert_eq!(pairs.len(), 20);
}
