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
fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn open(
    address: std::net::SocketAddr,
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
    bootstrap_region: u64,
) -> Node {
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

    let server = Server::bind(
        address,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
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
async fn put(store: &Arc<Store>, region: &Region, key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
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
                    "writing {key:?} never succeeded; last answer {error}"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
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
    let address = reserve();
    let node = open(
        address,
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
    let address = reserve();
    let node = open(
        address,
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
    // Long enough for several heartbeat rounds to have carried it.
    tokio::time::sleep(Duration::from_millis(200)).await;
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
    let first_address = reserve();
    let second_address = reserve();
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
        first_address,
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
        put(&first.store, &region, key(n), b"value").await;
    }

    // The second store is told the cluster already exists, so it hosts nothing of its own.
    let second = open(
        second_address,
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
    // way to catch it up.
    for n in 40..120 {
        let region = first.store.regions().regions()[0].clone();
        put(&first.store, &region, key(n), b"value").await;
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
    let first_address = reserve();
    let second_address = reserve();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    // **No aggressive compaction.** The leader's log is intact, so a snapshot here is not the
    // repair of a peer that fell behind — it is the ordinary way a store that never had the
    // region receives it, which is the path every placed replica takes.
    let first = open(
        first_address,
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
    put(&first.store, &region, key(100), b"raw").await;

    let second = open(
        second_address,
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
    let first_address = reserve();
    let second_address = reserve();
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
        first_address,
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
        second_address,
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
    // catches it up rather than the entries.
    for n in 1..80 {
        let region = first.store.regions().regions()[0].clone();
        put(&first.store, &region, key(n), b"value").await;
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
    let first_address = reserve();
    let second_address = reserve();
    let peers = vec![
        PeerAddress::new(1, 1, first_address),
        PeerAddress::new(2, 2, second_address),
    ];

    let first = open(
        first_address,
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
        second_address,
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
    commit_one(&first.store, &region, key(6), b"committed", 30, 31).await;

    let leader = first.store.peer_of(1).unwrap();
    wait_for("the learner to reach the leader's applied index", || {
        second
            .store
            .peer_of(1)
            .is_some_and(|peer| peer.applied_index() >= leader.applied_index())
    })
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
         (key, leader versions, learner versions) = {short:?}"
    );

    first.stop().await;
    second.stop().await;
}

/// Announces a snapshot to `second` for the region it holds, until its old peer is retired.
///
/// Returns whether the retire was observed. See the caller for why this is a loop.
async fn announce_until_retired(
    first: &Node,
    second: &Node,
    old: &Arc<esker_store::RaftPeer>,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        for n in 40..120 {
            let region = first.store.regions().regions()[0].clone();
            put(&first.store, &region, key(n), b"value").await;
        }
        let leader = first.store.peer_of(1).expect("the leader");
        let Some(state) = second.store.regions().get(1) else {
            return true;
        };
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
                        index: leader.applied_index(),
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

        let settle = Instant::now() + Duration::from_secs(5);
        while Instant::now() < settle {
            let replaced = second
                .store
                .peer_of(1)
                .is_none_or(|now| !Arc::ptr_eq(&now, old));
            if replaced && old.propose(&put_command()).await.is_err() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    false
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
    let first_address = reserve();
    let second_address = reserve();
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
        first_address,
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
        put(&first.store, &region, key(n), b"value").await;
    }

    let second = open(
        second_address,
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

    // Two conditions have to hold at once and they pull in opposite directions: `receive_raft`
    // acts only when the learner's apply index is **below** the announced one, and the sender
    // refuses anything **above** what it can offer. The index has to sit in the gap, and the gap
    // only exists while the learner is catching up — so [`announce_until_retired`] opens one with
    // a burst of writes and takes losing the race as another round rather than as a failure.
    let retired = announce_until_retired(&first, &second, &old).await;
    assert!(
        retired,
        "the old peer was never retired, so the snapshot did not route through one"
    );

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
        Err(_) => true,
        Ok(mut stream) => matches!(stream.next_chunk().await, None | Some(Err(_))),
    }
}

/// A snapshot is refused to a store that is not a member of the region. A snapshot hands over a
/// region wholesale, and a store with no claim to the range has no claim to a copy of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_outside_the_region_is_refused_a_copy() {
    let pd = Arc::new(FakePd::new());
    let address = reserve();
    let node = open(
        address,
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
    put(&node.store, &region, key(0), b"v").await;

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
        db.write(batch, &esker_engine::WriteOptions { sync: true })
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
