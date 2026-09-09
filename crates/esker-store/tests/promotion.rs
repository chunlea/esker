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

#[path = "load_arm/mod.rs"]
mod load_arm;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_pd::record::{EventKind, EventOutcome};
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

/// A port, **held** until the server that will serve on it adopts the socket.
///
/// Returning the address and dropping the listener leaves the port belonging to nobody until the
/// rebind, and under a parallel suite run something else takes it — `Address already in use`.
/// `Server::from_listener` takes the socket itself, so there is no window.
fn reserve() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

async fn wait_for<F: FnMut() -> bool>(what: &str, seconds: u64, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[allow(
    clippy::unused_async,
    reason = "the caller awaits it; adopting a listener is what stopped being async, not the helper"
)]
async fn open_store(
    address_listener: std::net::TcpListener,
    store_id: u64,
    pd_address: std::net::SocketAddr,
    peers: &[PeerAddress],
) -> Node {
    let address = address_listener.local_addr().unwrap();
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
/// How long one store may take to answer before the writer asks another.
///
/// The bound the arm was missing: its retry loop had a deadline and the call inside it had none, so
/// a single `serve` awaiting a commit that a leaderless region will never make ran the arm to two
/// hundred seconds against a ninety-second budget. Generous against a slow box, short against a
/// region that has stopped answering.
const ATTEMPT: Duration = Duration::from_secs(10);

async fn put(stores: &[&Arc<Store>], key: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(90);
    let started = Instant::now();
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
            // **One attempt cannot outlive the arm's own budget.** The deadline below is checked
            // between rounds, and that bounds the *loop* — it does not bound the call inside it.
            // `serve` awaits the proposal it made, and a peer that led when `is_leader()` was asked
            // and lost the office a moment later is awaiting a commit that will not come while the
            // region has no leader. That is how a ninety-second arm reported **two hundred
            // seconds** in g1's gate: one await, unbounded, inside a bounded loop.
            //
            // A round is short because the answer either comes from a leader or does not come at
            // all; giving up on it and asking the next store is exactly what the loop is for.
            let attempt = tokio::time::timeout(ATTEMPT, store.serve(header, request));
            let Ok(answered) = attempt.await else {
                last = Some(format!(
                    "store {} led region {} and did not answer within {ATTEMPT:?}",
                    store.store_id(),
                    state.id()
                ));
                continue;
            };
            match answered {
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
                    last = Some(error.to_string());
                }
            }
        }
        // **`last: None` means nobody was ever asked, and that is a different failure.** The
        // three `continue`s above are gates, not refusals: a store is skipped when it holds no
        // region containing the key, when it holds one but has no peer of it, or when its peer
        // does not lead. None of that reaches `store.serve`, so `last` stays `None` and the
        // message used to say only that -- which is the one thing already implied by the panic.
        //
        // Seen 2/4 runs at 14 busy threads on 2026-09-04 (`docs/plans/debt-c7.md` section 17):
        // `writing b"k000659" never succeeded; the last refusal was: None`. Ninety seconds in
        // which not one of three stores claimed the office for that key, and no way to tell a
        // leaderless region from a key no store admits to owning. So say which gate each store
        // stopped at, and who it believes holds the office.
        if Instant::now() >= deadline {
            let mut seen = Vec::new();
            for store in stores {
                let id = store.store_id();
                let Some(state) = store.regions().find(&key) else {
                    seen.push(format!("store {id}: holds no region containing this key"));
                    continue;
                };
                let region_id = state.id();
                let Some(peer) = store.peer_of(region_id) else {
                    seen.push(format!(
                        "store {id}: holds region {region_id} for this key but has no peer of it"
                    ));
                    continue;
                };
                // `leader=None` on every store is the interesting answer and it is ambiguous
                // on its own: a follower whose election timer keeps being reset and a candidate
                // that keeps losing look identical from outside. `role` separates them, and the
                // membership says whether a quorum was ever available -- a range that split into
                // peers that are still learners has no voters to elect anybody.
                let raft = peer.status().await.ok();
                let role = raft.as_ref().map_or_else(
                    || "unavailable".to_owned(),
                    |status| format!("{:?}", status.role),
                );
                let voted_for = raft.as_ref().and_then(|status| status.voted_for);
                let membership = state
                    .region()
                    .peers
                    .iter()
                    .map(|member| {
                        format!(
                            "{}@store{}:{:?}",
                            member.peer_id, member.store_id, member.role
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                // **The counters, on the same line as the state they explain.** A term ladder
                // with no ignored responses is a split vote; one with many is delivery arriving
                // after the round it answers. The state alone cannot tell those apart, and turning
                // tracing up stops the stall reproducing.
                let counters = peer.counters().await.ok();
                // **And what this peer's own core believes the membership is.** Every construction
                // path builds it correctly from the region record, so the next reproduction has to
                // say where the divergence comes in instead — and it cannot say that unless the
                // peer's own answer is on the line beside the driver's.
                let mine = peer.membership().await.ok();
                seen.push(format!(
                    "store {id}: region {region_id} term={} is_leader={} believes_leader={:?} \
                     raft_role={role} voted_for={voted_for:?}; membership [{membership}]; \
                     its own core says {mine:?}; elections {counters:?}",
                    peer.term(),
                    peer.is_leader(),
                    peer.leader()
                ));
            }
            panic!(
                "writing {key:?} never succeeded in {:?}; the last refusal was: {last:?}\n  {}",
                started.elapsed(),
                seen.join("\n  ")
            );
        }
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
/// # What it took to make this pass
///
/// It failed for the whole of the investigation and was kept in the tree failing, `#[ignore]`d, as
/// the reduction of the acceptance stall. **20 of 20 runs green** now. Six defects stood between
/// those two states, and every one of them was found by reading a trace and pinned by a unit test —
/// none was found by counting runs:
///
/// 1. **the promotion was never asked for.** PD stops re-sending an `AddPeer` once it can see the
///    learner, while the store waited to be asked again. The leader promotes on its own now, on the
///    learner's `matched`, because a region heartbeat comes only from a leader and PD can therefore
///    never see a learner's progress at all;
/// 2. **a follower below the compaction boundary cannot be heartbeated**, so `send_heartbeat` fell
///    through to a paused `send_append` and one lost probe stranded it for the term;
/// 3. **an `InstallSnapshot` was stepped into the core** for a region the store already hosted,
///    which restored from the metadata alone while no data was written;
/// 4. **an append below the follower's commit index was rejected** rather than answered, and since
///    `maybe_decr_to` never raises `next`, the leader probed an index the follower would refuse for
///    ever — 526 back-offs in one run against a follower answering every one;
/// 5. **the compaction hold was fail-open**: `RawNode::progress` is empty on a non-leader, so a
///    peer that was not leading that instant held nothing and compacted by the tail rule;
/// 6. **a peer started from a region record dropped that record's learners** on the floor, so the
///    core had no `Progress` for a peer its own region record listed — the leader sent it nothing
///    and it sat at `applied = 0` for the life of the cluster.
///
/// The shape they shared is worth naming: each turned a *transient* condition — a lost message, a
/// moment not leading, a configuration mid-flight — into a *permanent* one, because nothing in the
/// path could ever revisit the decision.
///
/// Two things this test learned about itself, both of which had it measuring the bug rather than
/// the fix. It wrote only to store 1, which worked precisely while every leader stayed there; and
/// it ran two stores at `target_replicas` two, where the quorum is two and one slightly slow
/// replica stops the region committing.
#[tokio::test(flavor = "multi_thread")]
async fn a_learner_on_a_fresh_store_becomes_a_voter_under_load() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let pd_address_listener = reserve();
    let pd_address = pd_address_listener.local_addr().unwrap();
    let listeners: Vec<std::net::TcpListener> = (0..3).map(|_| reserve()).collect();
    let addresses: Vec<std::net::SocketAddr> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap())
        .collect();
    let mut listeners = listeners.into_iter();
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
    let pd_server = Server::from_listener(
        pd_address_listener,
        PdService::new(Arc::clone(&pd)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .unwrap();
    let pd_handle = pd_server.spawn().unwrap();

    let first = open_store(listeners.next().unwrap(), 1, pd_address, &peers).await;
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

    let second = open_store(listeners.next().unwrap(), 2, pd_address, &peers).await;
    let third = open_store(listeners.next().unwrap(), 3, pd_address, &peers).await;

    // Load keeps running while the cluster grows, which is the case the lag criterion has to work
    // under: a learner is never exactly level with a leader that is still taking writes.
    //
    // **The writer counts what it has finished.** `JoinHandle::is_finished` is a bool, and the
    // difference between key 899 of 900 and key 350 is the whole diagnosis: the first is a clock
    // and the second is something blocking. A run that expires reporting only `writer_done=false`
    // sends the next reader to the deadline, which is the one place the answer is not.
    let written = Arc::new(AtomicU32::new(0));
    let writer = {
        let store = Arc::clone(&first.store);
        let store2 = Arc::clone(&second.store);
        let store3 = Arc::clone(&third.store);
        let value = value.clone();
        let written = Arc::clone(&written);
        tokio::spawn(async move {
            for n in 300..900 {
                put(&[&store, &store2, &store3], key(n), &value).await;
                written.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // **Watched, not sampled.** A learner that has only just been placed is not a bug, and an
    // instantaneous "no learner anywhere" calls it one — the placement driver keeps placing while
    // the load keeps splitting, so there is nearly always one that is merely new. A learner that
    // is *still* a learner after `PROMOTION_DEADLINE` is exactly the acceptance finding, where
    // every region held two of them for four minutes and for ever after.
    watch_until_every_learner_votes(&pd, &[&first, &second, &third], &writer, &written).await;
    writer.await.expect("the load completed");

    first.stop().await;
    second.stop().await;
    third.stop().await;
    let _ = pd_handle.shutdown().await;
}

/// **One peer per region per store**, checked before anything is timed.
///
/// A store keys its regions by region id — `RegionMap::insert` refuses a second outright, "a store
/// never holds two peers of one region" — and its transport drops a message it would have to
/// address to itself. So a second peer of one region on one store is a peer that can never be
/// created: every message to it is dropped, its `matched` stays 0, and it is a learner for ever,
/// while the placement driver counts it as a replica and stops repairing the region.
///
/// It used to arrive here as `PROMOTION_DEADLINE` expiring — thirty seconds later, naming the
/// clock instead of the cause. It is a state, so it is asserted as one
/// (`docs/plans/phase-14-flakes.md` U2).
fn one_peer_per_store(region: &Region) {
    let mut by_store: BTreeMap<u64, u64> = BTreeMap::new();
    for peer in &region.peers {
        if let Some(first) = by_store.insert(peer.store_id, peer.peer_id) {
            panic!(
                "region {} has peers {} and {} both on store {} — a store hosts one peer per \
                 region, so the second can never be created and never votes",
                region.id, first, peer.peer_id, peer.store_id
            );
        }
    }
}

/// How long a core and the driver may disagree about a role before it is a fault.
///
/// **Watched, not sampled**, which is the rule this file already lives by for learners. The driver
/// learns of a conf change from the *leader's* region heartbeat, so between a core applying a
/// promotion and PD hearing about it every core is ahead of the driver — a window, and a normal
/// one at 20 ms a heartbeat. Two hundred and fifty of them is not.
const DISAGREEMENT_ALLOWED: Duration = Duration::from_secs(5);

/// **No peer's own core calls itself a voter that `region` calls a learner — for long.**
///
/// The placement driver's record is the authority on a role; a core that disagrees with it about
/// *itself* is what lets a learner campaign at all, since `Raft::campaign` refuses a node that is
/// not a voter in its own configuration. ADR 0085 stops the campaign from doing damage. This is the
/// assertion that the state which produces it is not there.
///
/// **A window is expected and a state is not.** The driver learns of a conf change from the
/// leader's region heartbeat, so a core that has applied a promotion is ahead of the driver until
/// the next one — measured here the first time this was asserted instantly, which caught that
/// normal window on the second round. What is a fault is the disagreement *lasting*, which is what
/// `since` is for.
///
/// The other direction is not asserted separately: a core behind the driver is the same window seen
/// from the other end.
async fn no_core_disagrees_for_long(
    all: &[&Node],
    region: &Region,
    since: &mut BTreeMap<(u64, u64), Instant>,
) {
    for node in all {
        let Some(peer) = node.store.peer_of(region.id) else {
            continue;
        };
        let Ok(conf) = peer.membership().await else {
            continue;
        };
        for recorded in &region.peers {
            if recorded.store_id != node.store.store_id() {
                continue;
            }
            let a_learner_to_the_driver =
                matches!(recorded.role, PeerRole::Learner | PeerRole::ColumnarLearner);
            let key = (region.id, recorded.peer_id);
            if a_learner_to_the_driver && conf.voters.contains(&recorded.peer_id) {
                let first = *since.entry(key).or_insert_with(Instant::now);
                assert!(
                    first.elapsed() < DISAGREEMENT_ALLOWED,
                    "region {}: for {:?} the driver has held peer {} on store {} as {:?} while \
                     that peer's own core has had it among the voters {:?} — the state that lets \
                     a learner campaign",
                    region.id,
                    first.elapsed(),
                    recorded.peer_id,
                    node.store.store_id(),
                    recorded.role,
                    conf.voters
                );
            } else {
                since.remove(&key);
            }
        }
    }
}

/// What each store's core believes the membership of `region` is.
///
/// The placement driver's answer is already on the line above it. These two disagreeing is the root
/// of the stall ADR 0085 contains, and no construction path in `esker-store` explains it — so the
/// next reproduction has to be asked directly.
async fn memberships(all: &[&Node], region_id: u64) -> String {
    let mut out = Vec::new();
    for node in all {
        let Some(peer) = node.store.peer_of(region_id) else {
            continue;
        };
        let Ok(conf) = peer.membership().await else {
            continue;
        };
        out.push(format!(
            "store {}: voters {:?} learners {:?}",
            node.store.store_id(),
            conf.voters,
            conf.learners
        ));
    }
    out.join("; ")
}

/// Every store's election counters for `region`, as one line.
///
/// The four numbers that separate the hypotheses this stall has left: `campaigns_real` is the term
/// ladder's height, `ignored` counts vote responses that arrived after the round they answer had
/// ended — the signature of delivery that is late rather than lost — and `step_downs` counts a
/// leader standing down because `check_quorum` found no majority contact. A region with no leader
/// and no ignored responses and no step-downs is a region nobody is campaigning for, which is a
/// different problem again.
async fn election_counters(all: &[&Node], region_id: u64) -> String {
    let mut out = Vec::new();
    for node in all {
        let Some(peer) = node.store.peer_of(region_id) else {
            continue;
        };
        let Ok(counters) = peer.counters().await else {
            continue;
        };
        out.push(format!(
            "store {}: pre={} real={} sent={} answered={} granted={} ignored={} step_downs={}",
            node.store.store_id(),
            counters.campaigns_pre,
            counters.campaigns_real,
            counters.vote_requests_sent,
            counters.vote_responses_sent,
            counters.vote_responses_granted,
            counters.vote_responses_ignored,
            counters.check_quorum_step_downs,
        ));
    }
    out.join("; ")
}

/// What whichever store leads `region` believes about `peer`, or why nobody could say.
///
/// [`esker_store::RaftPeer::progress`] answers with the leader's `Progress` table and is **empty
/// unless it leads**, so asking every node and keeping the one that answers finds the leader
/// without having to know which it is — and without racing a transfer that moves it between the
/// question and the answer.
///
/// The three numbers are the ones the promotion decision reads: `matched` is how far the leader
/// thinks the peer has got, `pending_snapshot` non-zero means it is being caught up by state
/// rather than by log, and `recent_active` false means the leader has not heard from it inside an
/// election timeout. A learner that PD calls a learner, that says `applied=N` itself, and that the
/// leader records at `matched=0` is not a slow promotion — it is two parties describing different
/// peers.
async fn leader_progress(all: &[&Node], region_id: u64, peer_id: u64) -> String {
    for node in all {
        let Some(peer) = node.store.peer_of(region_id) else {
            continue;
        };
        let Ok(progress) = peer.progress().await else {
            continue;
        };
        let Some(entry) = progress.iter().find(|entry| entry.id == peer_id) else {
            continue;
        };
        return format!(
            "store {} leads region {region_id} and records peer {peer_id} at matched={} next={} \
             is_learner={} pending_snapshot={} recent_active={}",
            node.store.store_id(),
            entry.matched,
            entry.next,
            entry.is_learner,
            entry.pending_snapshot,
            entry.recent_active
        );
    }
    // Worth saying rather than printing an empty string: no store answering means no store led
    // this region at the moment it was asked, which is its own diagnosis.
    format!("no store led region {region_id} when asked, so nothing has a Progress for {peer_id}")
}

/// Polls until every learner that appears has been promoted, failing the moment one outlives
/// [`PROMOTION_DEADLINE`]. Each is timed from when it was first seen, so a learner that is merely
/// new is not mistaken for one that is stranded.
#[allow(
    clippy::too_many_lines,
    reason = "one loop, and the failure has to explain itself"
)]
async fn watch_until_every_learner_votes(
    pd: &Arc<Pd>,
    all: &[&Node],
    writer: &tokio::task::JoinHandle<()>,
    written: &Arc<AtomicU32>,
) {
    // Store 1 bootstrapped the cluster alone; the promotions under test are the peers placed on
    // the two that joined afterwards. `all` is taken instead of just those two because the
    // **leader** may by then be any of the three, and only a leader can say what it believes about
    // a follower.
    let joined: Vec<&Node> = all
        .iter()
        .copied()
        .filter(|node| node.store.store_id() != 1)
        .collect();
    let mut first_seen: BTreeMap<(u64, u64), Instant> = BTreeMap::new();
    let mut disagreeing: BTreeMap<(u64, u64), Instant> = BTreeMap::new();
    let mut promoted: BTreeSet<(u64, u64)> = BTreeSet::new();
    // Whether the load was still in flight when the cluster started to grow, so that "under
    // load" is checked rather than hoped for. Sampled at the first learner, and at the window's
    // start for the run where the roles were never caught in a sample — the `recorded_promoted`
    // path below exists for exactly that run.
    //
    // **It asks whether the writer had finished, not how far it had got.** The first version of
    // this check required the writer to *advance* between the first learner and the last
    // promotion, and that is rate-dependent in the wrong direction: on a quiet box the whole
    // promotion completes inside a single write, so it failed at zero load with
    // "16 writes when the first learner appeared, 16 when the last one voted" while passing at
    // 14, 40 and 80. A test that fails because the machine is fast is the mirror of the bug this
    // unit is fixing.
    let mut load_live_at_growth: Option<bool> = None;
    let mut placed_on_second = false;
    let deadline = Instant::now() + Duration::from_secs(180);

    loop {
        for region in pd_regions(pd) {
            one_peer_per_store(&region);
            // **What the driver says and what each core believes have to be the same thing.**
            //
            // A peer that has itself among the *voters* while the region record calls it a learner
            // is the root of the stall [ADR 0085](../../docs/adr/0085-a-vote-is-not-granted-to-a-learner.md)
            // contains: `Raft::campaign` refuses a non-voter, so a learner that campaigns is a
            // learner whose own configuration disagrees, and one such peer kept a region leaderless
            // for as long as the load lasted. The ADR made that harmless; it did not make it untrue,
            // and until this is asserted nothing here would notice it happening.
            //
            // Checked on every pass rather than at a deadline, because the divergence is a *state*
            // and not a delay — the fault this test was opened for arrives as a stall, and that is
            // exactly the thirty-second detour this avoids.
            no_core_disagrees_for_long(all, &region, &mut disagreeing).await;
            for peer in &region.peers {
                let id = (region.id, peer.peer_id);
                if peer.store_id != 1 {
                    placed_on_second = true;
                }
                match peer.role {
                    PeerRole::Learner => {
                        // **A role only ever moves forward.** Nothing in this system demotes a
                        // voter — `RemovePeer` takes a replica away and there is no `Demote` — so
                        // a peer PD has already reported as a voter and now reports as a learner
                        // is PD's *view* going backwards, not a promotion that failed. Told apart
                        // here because the two arrive at the deadline looking identical, thirty
                        // seconds after whichever of them happened.
                        assert!(
                            !promoted.contains(&id),
                            "region {} peer {} is a learner again after PD reported it a voter; \
                             PD holds {:?} at epoch {:?} from leader {}",
                            region.id,
                            peer.peer_id,
                            region
                                .peers
                                .iter()
                                .map(|peer| (peer.peer_id, peer.store_id, peer.role))
                                .collect::<Vec<_>>(),
                            region.epoch,
                            region.id
                        );
                        let since = *first_seen.entry(id).or_insert_with(|| {
                            load_live_at_growth.get_or_insert_with(|| !writer.is_finished());
                            Instant::now()
                        });
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
                            // **And what the leader believes about it**, which is the half neither
                            // PD nor the learner can show. PD reports a role and the learner
                            // reports its own applied index; a learner that is caught up by both
                            // and still not promoted is a leader that thinks otherwise, and
                            // `matched` / `pending_snapshot` / `recent_active` are the three
                            // numbers the promotion decision actually reads
                            // (`server.rs`, "not promoting: the learner has not caught up").
                            // `RaftPeer::progress` is empty unless the peer leads, so asking all
                            // three and keeping what answers finds the leader without naming it.
                            let believed = leader_progress(all, region.id, peer.peer_id).await;
                            // **Counted, not traced.** Four sightings of this stall share one
                            // state — a region with no leader for tens of seconds — and turning
                            // `esker_raft` up to `debug` made it stop reproducing, four runs of
                            // four. These are cheap enough to leave on while the race is on.
                            let elections = election_counters(all, region.id).await;
                            let beliefs = memberships(all, region.id).await;
                            panic!(
                                "peer {} of region {} has been a learner for {:?} — the phase-4 \
                                 acceptance stall. the placement driver holds {:?} at epoch {:?}, \
                                 led by peer {}. the learner's own store says: {theirs:?}. the \
                                 leader believes: {believed}. each core's own membership: \
                                 {beliefs}. elections: {elections}",
                                peer.peer_id,
                                region.id,
                                since.elapsed(),
                                region
                                    .peers
                                    .iter()
                                    .map(|peer| (peer.peer_id, peer.store_id, peer.role))
                                    .collect::<Vec<_>>(),
                                region.epoch,
                                pd.regions()
                                    .unwrap()
                                    .iter()
                                    .find(|record| record.region.id == region.id)
                                    .map_or(0, |record| record.leader_peer_id)
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
                    // A columnar replica is a learner that is **never** promoted (ADR 0022
                    // Decision 1), so it is not a promotion this test is waiting for — and if one
                    // ever appeared here it would mean the placement under test had changed
                    // shape, which is worth failing on rather than counting.
                    PeerRole::ColumnarLearner => {
                        panic!(
                            "region {} peer {} is a columnar learner; this test places row \
                             replicas and one arriving means something else placed it",
                            region.id, peer.peer_id
                        );
                    }
                }
            }
        }
        // **Neither observation is sufficient alone, so this takes either.**
        //
        // The roles above are sampled every 200 ms, and a promotion that completes between two
        // polls is invisible to them — on a fast cluster the loop could never break and always
        // ran to the 180-second deadline, reporting "0 learners seen" for a replica it had
        // already placed.
        //
        // PD's history is durable and unsampled, and an `AddPeer` is **done when the peer is a
        // voter** — the whole difference from `AddLearner`, which a columnar replica gets and
        // which finishes as soon as the peer exists. But the history is a **64-entry ring**, and
        // a run that transfers leadership often enough pushes the `AddPeer` out of it: a failing
        // run showed sixty-four consecutive `TransferLeader` events and nothing else.
        //
        // So each covers the other's blind spot, and a run has to miss both to hang.
        let seen_promoted = !promoted.is_empty() && first_seen.len() == promoted.len();
        let recorded_promoted = pd.history().unwrap_or_default().iter().any(|event| {
            event.kind == EventKind::AddPeer
                && event.outcome == EventOutcome::Done
                && joined
                    .iter()
                    .any(|node| node.store.store_id() == event.store_id)
        });
        if seen_promoted || recorded_promoted {
            // **"Under load" is asserted, not assumed.** Dropping `writer.is_finished()` from the
            // break would otherwise let this pass over an idle cluster, which is the easy case and
            // not the one the test is named for. What makes it load is that writes were *flowing
            // across the window*: the writer had got somewhere by the time the first learner
            // appeared, and got further before the last one voted.
            let live = *load_live_at_growth.get_or_insert_with(|| !writer.is_finished());
            assert!(
                live,
                "every learner voted, but the load generator had already finished all 600 writes \
                 before the cluster grew, so the promotions happened over a quiet store — which \
                 is the easy case and not the one this test is named for. It has written {} now.",
                written.load(Ordering::Relaxed)
            );
            // **The counters on the way out, not only on the way down.** `debts-v1.1.md` #9 is
            // about a term that climbs through pre-vote rounds, and a run that *passes* is where
            // the evidence for "it does not any more" has to come from: a failure prints these
            // already, and ten green runs that printed nothing would say only that nothing stalled.
            for id in pd_regions(pd).iter().map(|region| region.id) {
                eprintln!(
                    "promotion settled: region {id}: {}",
                    election_counters(all, id).await
                );
            }
            break;
        }
        // The history is what decides success, so it is what a failure has to show: "0 learners
        // seen" said nothing about why, and the answer was that nobody had been looking at the
        // right thing.
        assert!(
            Instant::now() < deadline,
            "the cluster never settled: {} learners seen, {} promoted, writer_done={} after {} \
             of 600 writes; PD's history is {:?}",
            first_seen.len(),
            promoted.len(),
            writer.is_finished(),
            written.load(Ordering::Relaxed),
            pd.history()
                .unwrap_or_default()
                .iter()
                .map(|event| (
                    event.region_id,
                    event.kind,
                    event.outcome,
                    event.store_id,
                    event.peer_id
                ))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        placed_on_second,
        "no replica was ever placed on a store that joined, so nothing was tested"
    );
}
