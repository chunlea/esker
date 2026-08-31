//! A learner on a fresh store becomes a voter, under load, driven by a **real** placement driver.
//!
//! This is the phase-4 acceptance stall, reduced to a test. Repair never finished and balance
//! never converged, both for the same reason and neither visible to either side's own fake: the
//! store's contract said an `AddPeer` for a peer that is already a learner *is* the promotion, so
//! it needed the operator re-sent; PD's contract said an operator whose learner it can already see
//! has demonstrably started, so it stopped sending. Nobody asked for the second step, and every
//! region sat at two voters.
//!
//! Behind that was a plain factual error, written into `server.rs` in 4c: that PD "sees every
//! store's region heartbeats, including the learner's own `applied_index`". A region heartbeat
//! comes from a region's leader and only from its leader, so a learner is invisible to PD — the
//! criterion PD was supposed to apply had no input and never could have.
//!
//! So this test uses `esker-pd` itself rather than a script. A fake driver of mine would have
//! passed throughout: what was broken was the seam between two correct-looking halves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{PeerRole, RawKvReq, Region, RequestHeader, Server, Service, TransportConfig};
use esker_store::pd_remote::RemotePd;
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

/// How long a learner may stay one before it counts as stranded rather than merely new.
const PROMOTION_DEADLINE: Duration = Duration::from_secs(30);

/// Small enough that the load below makes several regions — so the assertion is about a cluster
/// rather than one lucky group — and large enough that it stops making them. A threshold that
/// keeps splitting through the settle window means there is always a learner that is merely new,
/// and "no learner anywhere" would never be true however well promotion worked.
const SPLIT_SIZE: u64 = 64 * 1024;

struct Node {
    store: Arc<Store>,
    handle: esker_proto::ServerHandle,
    _dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = self.handle.shutdown().await;
    }
}

fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn open_store(
    address: std::net::SocketAddr,
    store_id: u64,
    pd_address: std::net::SocketAddr,
    peers: &[PeerAddress],
) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let mut raft = RaftOptions::new(peers.to_vec(), 20_260_830);
    raft.tick = Duration::from_millis(5);
    raft.compaction = LogCompaction {
        threshold: 32,
        keep: 8,
        ..LogCompaction::new()
    };
    raft.driver_workers = 2;

    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id,
            peer_id: store_id,
            region_id: store_id,
            raft: Some(raft),
            pd: Some(Arc::new(RemotePd::connect(pd_address).unwrap())),
            address: address.to_string(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            split: SplitOptions {
                region_split_size: SPLIT_SIZE,
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

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:06}"))
}

/// Writes one key through whichever store currently **leads** the region that owns it.
///
/// Following the leader is the whole of it, and it is what the two-store version of this test did
/// not have to do: while every leader stayed on store 1 — which was the bug — writing to store 1
/// always worked. A cluster whose leadership actually spreads refuses those writes with
/// `NotLeader`, correctly, so a load generator that only knows one store measures the bug rather
/// than the fix.
async fn put(stores: &[&Arc<Store>], key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last: Option<String> = None;
    loop {
        for store in stores {
            let Some(state) = store.regions().find(&key) else {
                continue;
            };
            let Some(peer) = store.peer_of(state.id()) else {
                continue;
            };
            if !peer.is_leader() {
                continue;
            }
            let header = RequestHeader::new(state.id(), state.region().epoch, 0);
            let request = RawKvReq::put(key.clone(), Bytes::copy_from_slice(value));
            match store.serve(header, request).await {
                Ok(_) => return,
                Err(error) => {
                    assert!(error.is_retryable(), "writing {key:?}: {error}");
                    last = Some(error.to_string());
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "writing {key:?} never succeeded; the last refusal was: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Every region as the placement driver holds it.
fn pd_regions(pd: &Arc<Pd>) -> Vec<Region> {
    pd.regions()
        .unwrap()
        .into_iter()
        .map(|record| record.region)
        .collect()
}

/// A second store joins a cluster under load, and every replica the placement driver places on it
/// becomes a **voter** — not a learner that stays one.
///
/// The bound is what the finding is about. A learner appearing proves only that `AddPeer`'s first
/// step works, which was never in doubt; what stalled acceptance is that the second step never
/// came, and a test that waits without a deadline would have reported that as success.
/// # Why this is `#[ignore]`d
///
/// It **fails**, and it is meant to be here failing: it is a faithful reduction of the phase-4
/// acceptance stall, and a repro belongs in the tree rather than in a scratchpad that dies with the
/// session. Run it with `cargo test -p esker-store --test promotion -- --ignored`.
///
/// What it caught, and what is fixed:
///
/// * the promotion never happened at all, because PD stops re-sending an `AddPeer` once it can see
///   the learner while the store was waiting to be asked again. The leader promotes on its own
///   now, on the learner's `matched`. **0 of 63 learners promoted before, 62 of 63 after.**
/// * a follower below the compaction boundary cannot be heartbeated, so `send_heartbeat` fell
///   through to a paused `send_append` and one lost probe stranded it. Fixed in `esker-raft`.
/// * an `InstallSnapshot` reaching a region this store already hosts was **stepped into the core**,
///   which restored from the metadata alone while the store wrote no data; the `Ready` carrying it
///   hit a phase-3 stub that logged and dropped it. That is the `a Raft snapshot arrived;
///   streaming is phase 4` line seen on a stalled region. The announcement is never stepped now.
///
/// What still fails, with the evidence gathered so far. Counting messages in both directions for a
/// stalled region gives 333 dispatched leader→learner, 238 stepped by the learner, and 238 back —
/// so the learner is **alive, replicating and answering**, and its own store reports it applying.
/// The leader's `matched` for it stays 0 regardless. The loop is: the learner adopts a snapshot at
/// some index, the leader writes on and compacts past it, every probe is then rejected, the
/// re-offered snapshot is declined because catching up a peer that already holds data is a
/// documented v1 limitation (`docs/plans/phase-4.md` §13.2), and that repeats for ever.
///
/// Two things ruled out rather than assumed. `recent_active = false` is **not** evidence the leader
/// never heard from the peer: check-quorum clears it every election timeout, which at this test's
/// 5 ms tick is about ten times a second. And the stale-epoch drops that correlate exactly with the
/// stalled regions — 14 regions, identical sets — are a symptom, not the cause: checking `version`
/// alone was tried and did not fix the stall, so that change was **reverted** rather than shipped.
///
/// Holding the leader's log for a peer still catching up is implemented and unit-tested
/// (`a_compaction_keeps_what_a_lagging_peer_still_needs`), and did **not** clear this test either —
/// recorded as tested behaviour that is right on its own merits, not as the fix.
#[ignore = "reproduces the phase-4 acceptance promotion stall; see the doc comment for what is \
            fixed and what remains"]
#[tokio::test(flavor = "multi_thread")]
async fn a_learner_on_a_fresh_store_becomes_a_voter_under_load() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let pd_address = reserve();
    let addresses: Vec<std::net::SocketAddr> = (0..3).map(|_| reserve()).collect();
    let peers: Vec<PeerAddress> = addresses
        .iter()
        .enumerate()
        .map(|(at, address)| {
            let id = at as u64 + 1;
            PeerAddress::new(id, id, *address)
        })
        .collect();

    let pd_dir = tempfile::tempdir().unwrap();
    let pd = Pd::open(
        pd_dir.path(),
        PdOptions {
            // **Three, as every acceptance scenario uses.** Two is the tempting size for a test
            // and it is a trap: a two-voter group has a quorum of two, so every write needs both
            // and a replica that falls a little behind stops the region committing. That stalls
            // the load rather than the promotion, which is a fault of the configuration and not of
            // the code under test.
            target_replicas: 3,
            // Short, so a stall shows up as a stall rather than as a slow success.
            operator_timeout_ms: 5_000,
            balance_cooldown_ms: 500,
            max_store_down_time_ms: 5_000,
            ..PdOptions::new()
        },
    )
    .unwrap();
    let pd_server = Server::bind(
        pd_address,
        PdService::new(Arc::clone(&pd)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap();
    let pd_handle = pd_server.spawn().unwrap();

    let first = open_store(addresses[0], 1, pd_address, &peers).await;
    wait_for("a leader on the first store", 20, || {
        first
            .store
            .regions()
            .regions()
            .first()
            .and_then(|region| first.store.peer_of(region.id))
            .is_some_and(|peer| peer.is_leader())
    })
    .await;

    // Enough to split a few times, so the cluster has regions to place rather than one.
    let value = vec![b'v'; 512];
    for n in 0..300 {
        put(&[&first.store], key(n), &value).await;
    }
    wait_for("the first store to split", 60, || {
        first.store.regions().len() >= 2
    })
    .await;

    let second = open_store(addresses[1], 2, pd_address, &peers).await;
    let third = open_store(addresses[2], 3, pd_address, &peers).await;

    // Load keeps running while the cluster grows, which is the case the lag criterion has to work
    // under: a learner is never exactly level with a leader that is still taking writes.
    let writer = {
        let store = Arc::clone(&first.store);
        let store2 = Arc::clone(&second.store);
        let store3 = Arc::clone(&third.store);
        let value = value.clone();
        tokio::spawn(async move {
            for n in 300..900 {
                put(&[&store, &store2, &store3], key(n), &value).await;
            }
        })
    };

    // **Watched, not sampled.** A learner that has only just been placed is not a bug, and an
    // instantaneous "no learner anywhere" calls it one — the placement driver keeps placing while
    // the load keeps splitting, so there is nearly always one that is merely new. A learner that
    // is *still* a learner after `PROMOTION_DEADLINE` is exactly the acceptance finding, where
    // every region held two of them for four minutes and for ever after.
    watch_until_every_learner_votes(&pd, &[&second, &third], &writer).await;
    writer.await.expect("the load completed");

    first.stop().await;
    second.stop().await;
    third.stop().await;
    let _ = pd_handle.shutdown().await;
}

/// Polls until every learner that appears has been promoted, failing the moment one outlives
/// [`PROMOTION_DEADLINE`]. Each is timed from when it was first seen, so a learner that is merely
/// new is not mistaken for one that is stranded.
async fn watch_until_every_learner_votes(
    pd: &Arc<Pd>,
    joined: &[&Node],
    writer: &tokio::task::JoinHandle<()>,
) {
    let mut first_seen: BTreeMap<(u64, u64), Instant> = BTreeMap::new();
    let mut promoted: BTreeSet<(u64, u64)> = BTreeSet::new();
    let mut placed_on_second = false;
    let deadline = Instant::now() + Duration::from_secs(180);

    loop {
        for region in pd_regions(pd) {
            for peer in &region.peers {
                let id = (region.id, peer.peer_id);
                if peer.store_id != 1 {
                    placed_on_second = true;
                }
                match peer.role {
                    PeerRole::Learner => {
                        let since = *first_seen.entry(id).or_insert_with(Instant::now);
                        if since.elapsed() >= PROMOTION_DEADLINE {
                            // What the *learner's own store* thinks, which is the half the
                            // leader's progress cannot show — and the half that proved it was
                            // alive and applying all along.
                            let theirs: Vec<String> = joined
                                .iter()
                                .flat_map(|node| node.store.region_statuses())
                                .filter(|status| status.region.id == region.id)
                                .map(|status| {
                                    format!(
                                        "applied={} leader={} epoch={:?}",
                                        status.applied_index, status.is_leader, status.region.epoch
                                    )
                                })
                                .collect();
                            panic!(
                                "peer {} of region {} has been a learner for {:?} — the phase-4 \
                                 acceptance stall. the learner's own store says: {theirs:?}",
                                peer.peer_id,
                                region.id,
                                since.elapsed()
                            );
                        }
                    }
                    // Only counts if it was seen as a learner first: a peer born a voter proves
                    // nothing about promotion.
                    PeerRole::Voter => {
                        if first_seen.contains_key(&id) {
                            promoted.insert(id);
                        }
                    }
                }
            }
        }
        // Every learner that appeared has been promoted, and there was at least one to promote.
        // Not a bigger number: with two stores at `target_replicas` two, two is all the placement
        // driver ever has reason to create.
        if writer.is_finished() && !promoted.is_empty() && first_seen.len() == promoted.len() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cluster never settled: {} learners seen, {} promoted",
            first_seen.len(),
            promoted.len()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        placed_on_second,
        "no replica was ever placed on a store that joined, so nothing was tested"
    );
}
