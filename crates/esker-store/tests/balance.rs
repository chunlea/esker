//! Regions and leaders spreading across stores, at a size CI can afford.
//!
//! `prompts/04-multiraft-pd.md` 4d asks for one store, four more added, twenty gigabytes written,
//! and the regions and leaders spread within a bounded time. Twenty gigabytes is an acceptance
//! run; what belongs here is the same **shape** at a size a test can pay for — a low split
//! threshold, a few hundred kilobytes, and the two properties that would be broken by a real bug
//! rather than by the scale.
//!
//! The two are:
//!
//! * **no region is orphaned.** Every region a store hosts is one whose peer list names that
//!   store, and every region in the cluster has a leader that can serve it. A region nobody leads
//!   is data nobody can read, and it is the failure a balance operator can cause by moving the
//!   wrong replica;
//! * **the key space stays a contiguous partition** across every store, through every split and
//!   every membership change. This is the invariant the phase-4 simulator checks globally; here it
//!   is checked after the cluster has finished moving.
//!
//! The scheduler that *decides* to move a region is the placement-driver lane's. This drives the
//! store side with a fake driver that issues the operators a real one would, which is what makes
//! the test about this lane's code rather than about theirs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_proto::{
    Operator, ProtoError, RawKvReq, Region, RegionStatus, RequestHeader, Server, Service,
};
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

/// Small enough that a few hundred kilobytes make a dozen regions.
const TINY_SPLIT_SIZE: u64 = 16 * 1024;

struct Node {
    store: Arc<Store>,
    handle: esker_proto::ServerHandle,
    _dir: tempfile::TempDir,
}

/// Turns the store's own tracing on when `RUST_LOG` is set. The interesting failures in a
/// two-store cluster are all "something was dropped somewhere", and a dropped message says so.
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
    peers: &[PeerAddress],
    bootstrap_voters: Option<Vec<u64>>,
) -> Node {
    let address = address_listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(peers.to_vec(), 20_260_830);
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
    raft.compaction = LogCompaction {
        threshold: 32,
        keep: 8,
        ..LogCompaction::new()
    };
    // Two workers, so the pool is a pool: a store hosting a dozen regions has to interleave them,
    // which is the case one-thread-per-region never exercised.
    raft.driver_workers = 2;
    raft.bootstrap_voters = bootstrap_voters;

    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: store_id,
            raft: Some(raft),
            pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                region_split_size: TINY_SPLIT_SIZE,
                max_sampled_keys: 1024,
            },
            ..StoreOptions::new()
        },
    )
    .unwrap();

    let server = Server::from_listener(
        address_listener,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        esker_proto::TransportConfig::new(),
    )
    .unwrap();
    let handle = server.spawn().unwrap();
    Node {
        store,
        handle,
        _dir: dir,
    }
}

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:06}"))
}

/// Writes one key through whichever region currently owns it, retrying while the routing moves.
async fn put(store: &Arc<Store>, key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let Some(state) = store.regions().find(&key) else {
            assert!(Instant::now() < deadline, "no region ever covered {key:?}");
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        };
        let header = RequestHeader::new(state.id(), state.region().epoch, 0);
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
                assert!(Instant::now() < deadline, "writing {key:?} never succeeded");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// Every region every store hosts, deduplicated by id and in key order.
fn cluster_regions(nodes: &[&Node]) -> Vec<Region> {
    let mut all: std::collections::BTreeMap<u64, Region> = std::collections::BTreeMap::new();
    for node in nodes {
        for region in node.store.regions().regions() {
            all.insert(region.id, region);
        }
    }
    let mut regions: Vec<Region> = all.into_values().collect();
    regions.sort_by(|left, right| left.start_key.cmp(&right.start_key));
    regions
}

/// The key space must be one contiguous partition, whatever moved.
fn assert_contiguous(regions: &[Region]) {
    assert!(!regions.is_empty(), "the cluster owns no key space");
    assert_eq!(regions[0].start_key, Bytes::new(), "{regions:#?}");
    for pair in regions.windows(2) {
        assert_eq!(
            pair[0].end_key, pair[1].start_key,
            "regions {} and {} leave a gap or overlap",
            pair[0].id, pair[1].id
        );
    }
    assert_eq!(regions.last().unwrap().end_key, Bytes::new());
}

/// One store grows a dozen regions and a second joins; the regions reach it, every one of them
/// keeps a leader, and the key space is still one partition when everything has settled.
///
/// The 20 GB and five stores of `prompts/04` are an acceptance run. This is the same shape at a
/// size CI can pay for: what a real bug breaks here is what it would break there.
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one cluster, built and then checked from every angle the balance path has"
)]
async fn regions_reach_a_store_that_joins_and_none_is_left_without_a_leader() {
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

    // Store 1 bootstraps a region of one voter so it can commit alone; store 2 joins later, which
    // is what `AddPeer` is for.
    let first = open(first_address_listener, 1, &pd, &peers, Some(vec![1])).await;
    wait_for("a leader on the first store", 10, || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    // Enough to split several times at the low threshold.
    let value = vec![b'v'; 512];
    for n in 0..400 {
        put(&first.store, key(n), &value).await;
    }
    wait_for("the first store to split several times", 30, || {
        first.store.regions().len() >= 6
    })
    .await;
    let grown = first.store.regions().regions();
    assert_contiguous(&grown);

    // A second store, told the cluster already exists, hosts nothing of its own.
    let second = open(second_address_listener, 2, &pd, &peers, Some(vec![2])).await;
    assert!(second.store.regions().is_empty());

    // Ask for a replica of every region on the new store. A real placement driver issues these
    // from what the heartbeats tell it, **every heartbeat, from the region as it is now** — it is
    // level-triggered, not edge-triggered. So is this, and it has to be: an `AddPeer` carries the
    // epoch it was issued against, the store rejects one whose epoch has moved, and a region that
    // splits after the operator is issued never gains its learner. `grown` is a snapshot taken
    // while splitting is still in flight — the wait above stops at *six* regions and the store
    // keeps going — so under load one region's epoch really does move between the snapshot and
    // the apply. Issuing once left that region stuck at 26 of 27 for the whole timeout.
    let issue_missing = || {
        let live = first.store.regions().regions();
        for region in &live {
            if region.peers.iter().any(|peer| peer.store_id == 2) {
                continue;
            }
            pd.issue(Operator::AddPeer {
                region_id: region.id,
                epoch: region.epoch,
                store_id: 2,
                peer_id: 1_000 + region.id,
            });
        }
        live.len()
    };
    let wanted = issue_missing().max(grown.len());

    // First the membership: every region should gain a learner on store 2. Checked separately
    // from the transfer so that a failure says which half of the path is broken.
    wait_for("every region to gain a learner", 60, || {
        // Re-issued on every poll, against the epoch each region has now. A real placement driver
        // does the same thing on its next heartbeat; issuing once and hoping is what made this
        // test load-sensitive.
        issue_missing();
        first
            .store
            .regions()
            .regions()
            .iter()
            .filter(|region| region.peers.iter().any(|peer| peer.store_id == 2))
            .count()
            >= wanted
    })
    .await;

    wait_for("the regions to reach the second store", 60, || {
        second.store.regions().len() >= grown.len()
    })
    .await;

    // --- what must be true once everything has moved -------------------------------------

    let nodes = [&first, &second];
    let regions = cluster_regions(&nodes);
    assert_contiguous(&regions);

    // No region is orphaned: every region a store hosts names that store among its peers, and
    // every region in the cluster is led by somebody.
    for node in nodes {
        let store_id = node.store.store_id();
        for status in node.store.region_statuses() {
            assert!(
                status
                    .region
                    .peers
                    .iter()
                    .any(|peer| peer.store_id == store_id),
                "store {store_id} hosts region {} without being one of its peers",
                status.region.id
            );
        }
    }
    // **Placement is not leadership.** The two waits above stop when every region has a learner on
    // store 2 and when store 2 hosts them all — neither of which says anybody has *elected*. A
    // region that has just been split or just gained a peer still has to hold an election, and an
    // election is counted in ticks, so under load it lands later than the placement it followed.
    // Waiting here rather than asserting straight away is the difference between "no leader yet"
    // and "no leader ever", and only the second is a bug.
    let leaders = || -> std::collections::BTreeSet<u64> {
        nodes
            .iter()
            .flat_map(|node| node.store.region_statuses())
            .filter(|status: &RegionStatus| status.is_leader)
            .map(|status| status.region.id)
            .collect()
    };
    let elected_by = Instant::now();
    let mut led = leaders();
    while !regions.iter().all(|region| led.contains(&region.id)) {
        if elected_by.elapsed() >= AWAIT_DEADLINE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        led = leaders();
    }
    for region in &regions {
        if led.contains(&region.id) {
            continue;
        }
        // **What a region without a leader looks like from each store that hosts it.** A region
        // that cannot elect and one that has not yet elected are different failures: the first
        // has a membership that makes a quorum impossible — no voters, or peers on stores that
        // are not there — and the second has a sound one and no election yet. Only the state says
        // which, so it is printed rather than guessed at.
        let seen: Vec<String> = nodes
            .iter()
            .flat_map(|node| {
                let store_id = node.store.store_id();
                node.store
                    .region_statuses()
                    .into_iter()
                    .filter(|status| status.region.id == region.id)
                    .map(move |status| {
                        format!(
                            "store {store_id}: believes leader is peer {}, applied {}, peers {:?}",
                            status.leader_peer_id,
                            status.applied_index,
                            status
                                .region
                                .peers
                                .iter()
                                .map(|peer| (peer.store_id, peer.peer_id, peer.role))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        panic!(
            "region {} has no leader anywhere after {:?}; its data is unreadable\n  {}\n  \
             regions: {:?}\n  led:     {led:?}",
            region.id,
            elected_by.elapsed(),
            seen.join("\n  "),
            regions.iter().map(|r| r.id).collect::<Vec<_>>(),
        );
    }

    // And the data is still all there, through whichever region owns each key.
    for n in 0..400 {
        let k = key(n);
        // **The epoch is sampled and then used, and the cluster is still splitting between those
        // two lines.** This read loop asked the map which region owns the key, built a header from
        // the epoch it found, and unwrapped the answer — so a split landing in between came back
        // as `EpochNotMatch` and failed the test on a gate that was otherwise green at 4283 of
        // 4285: `region 28 [b"k000200", b"k000215")`, fifteen keys wide, still being cut.
        //
        // `put` above already knows this — *"through whichever region currently owns it, retrying
        // while the routing moves"* — and this loop is the same problem read instead of written.
        // Nothing is relaxed: every key must still read back the value it was written with, and a
        // refusal that is not the routing moving still fails.
        let found = loop {
            let state = first
                .store
                .regions()
                .find(&k)
                .expect("every key is covered");
            let header = RequestHeader::new(state.id(), state.region().epoch, 0);
            match first.store.handle(header, RawKvReq::get(k.clone())) {
                Ok(esker_proto::RawKvResp::Get { value }) => break value,
                Ok(other) => panic!("not a get: {other:?}"),
                Err(ProtoError::EpochNotMatch { .. } | ProtoError::KeyNotInRegion { .. }) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("{k:?} could not be read: {error}"),
            }
        };
        assert_eq!(found, Some(Bytes::from(value.clone())), "{k:?} was lost");
    }

    first.stop().await;
    second.stop().await;
}

/// A store hosting many regions on a **two-worker pool** keeps every one of them making progress.
/// This is the pool's promise at cluster scale rather than in a unit test: a dozen regions, two
/// threads, and every region still applying.
#[tokio::test(flavor = "multi_thread")]
async fn a_dozen_regions_on_two_workers_all_make_progress() {
    let pd = Arc::new(FakePd::new());
    let address_listener = reserve();
    let address = address_listener.local_addr().unwrap();
    let peers = vec![PeerAddress::new(1, 1, address)];
    let node = open(address_listener, 1, &pd, &peers, Some(vec![1])).await;
    wait_for("a leader", 10, || {
        node.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let value = vec![b'v'; 512];
    for n in 0..400 {
        put(&node.store, key(n), &value).await;
    }
    wait_for("a dozen regions", 60, || node.store.regions().len() >= 8).await;

    let regions = node.store.regions().regions();
    assert_contiguous(&regions);

    // Every region has a leader and has applied something: a region pinned to a worker that was
    // never scheduled would sit at applied zero for ever.
    wait_for("every region to elect and apply", 30, || {
        node.store
            .region_statuses()
            .iter()
            .all(|status| status.is_leader && status.applied_index > 0)
    })
    .await;

    // And one write into each region lands, which is the concurrency claim end to end: two
    // workers driving a dozen regions, each awaited before the next is sent.
    for region in &node.store.regions().regions() {
        if region.start_key.is_empty() {
            continue;
        }
        put(&node.store, region.start_key.clone(), b"pool").await;
    }

    node.stop().await;
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = within("the server to shut down", self.handle.shutdown()).await;
    }
}
