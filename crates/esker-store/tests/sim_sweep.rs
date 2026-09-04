//! The real sweep, put in front of every answer the placement driver can give.
//!
//! `esker_sim::mech::sweep` owns the case table and the checker; this file replays each case
//! against **a real two-store cluster** and reports what the store did. `tests/retire.rs` drives
//! the one ordering where the sweep is supposed to fire; this drives the six where it must not,
//! and each of those is a way to lose acknowledged writes rather than a way to waste disk.
//!
//! Copy this file and `crates/esker-sim/` into a detached worktree at `c31a8a8` — `92a5add`'s
//! parent — and the evidenced case fails: nothing runs on the operator path there, so the range
//! is reclaimed zero times. It compiles at that revision because it counts keys itself rather
//! than through `snapshot::key_counts`, which `92a5add` added.
//!
//! `docs/plans/phase-11-engine.md` §10 holds the recorded red.
//!
//! # The shape of one case
//!
//! Store 1 bootstraps region 1 and seeds all three shipped column families. Store 2 receives a
//! voting replica. Store 1 is then **stopped**, which leaves store 2's peer leaderless against a
//! group it cannot reach — which is exactly the state a removed peer is in permanently, and what
//! the probe counts rounds of. The placement driver's record for the range is then set to the
//! case's answer, and the store is watched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_engine::{ReadOptions, cf};
use esker_proto::{
    Operator, Peer, PeerRole, RawKvReq, Region, RequestHeader, Server, ServerHandle, Service,
    TransportConfig, TxnKvReq, TxnMutation,
};
use esker_sim::mech::sweep::{Case, Expected, Observed, PdAnswer, cases, check};
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions, StoreService};

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

fn raft_options(peers: Vec<PeerAddress>, bootstrap_voters: Option<Vec<u64>>) -> RaftOptions {
    let mut raft = RaftOptions::new(peers, 20_261_101);
    raft.tick = Duration::from_millis(25);
    raft.compaction = LogCompaction::new();
    raft.bootstrap_voters = bootstrap_voters;
    raft
}

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_: tokio::time::error::Elapsed| panic!("timed out waiting for {what}"))
}

/// Waits for a condition, with a deadline that is deliberately enormous next to the work.
///
/// Measured under forty-eight spinning threads, worst case across five runs: 429 ms for the
/// first election, 190 ms for the promotion, 276 ms for a peer to notice it has no leader, and
/// microseconds for the two that are already true when asked. Against thirty seconds that is
/// about seventy times over, so these waits are not what makes this file load-sensitive — which
/// is worth knowing, because the recorded sighting was labelled "time-based" and they are the
/// only clocks left in it once the watch window is accounted for (`docs/plans/debt-c6.md` §9).
async fn wait_for<F: FnMut() -> bool>(what: &str, mut ready: F) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
            // The sweep runs on this schedule, and its throttle is 50 consecutive leaderless
            // rounds — so 5 ms makes a case cost a quarter of a second rather than five seconds.
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

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:05}"))
}

/// Keys the store holds inside the range this test writes into, summed over the three shipped
/// column families.
///
/// Counted here rather than through `esker_store::snapshot::key_counts` for one reason: that
/// function arrived with `92a5add`, and this file has to compile at its parent to be worth
/// anything. The keys are this test's own, so the bounds are known without asking the store how
/// it maps a range onto the engine.
fn keys_held(store: &Arc<Store>) -> usize {
    let mut held = 0;
    for name in [cf::DEFAULT, cf::LOCK, cf::WRITE] {
        let mut iter = store.db().iter(name, &ReadOptions::default()).unwrap();
        iter.seek_to_first();
        while iter.valid() {
            // Every key this test writes carries `k` and five digits somewhere in it — a raw key
            // is namespaced and a transactional one is namespaced and suffixed with a timestamp,
            // and neither transformation removes the middle.
            if iter
                .key()
                .windows(2)
                .any(|window| window == b"k0" || window == b"k2")
            {
                held += 1;
            }
            iter.next();
        }
        iter.status().unwrap();
    }
    held
}

/// What the orphan probe waits for, mirrored from the private
/// `esker_store::server::ORPHAN_PROBE_ROUNDS`. A copy, because the constant is not public and the
/// window below is only meaningful next to it; if the two ever disagree the assertion fires,
/// which is the failure mode this pair is here to produce rather than to hide.
const PROBE_ROUNDS: usize = 50;

/// Heartbeat rounds per store beat: `store_heartbeat` 20 ms over `heartbeat_tick` 5 ms, both set
/// by [`open`]. This is what turns the beats PD received into a count of the rounds that ran.
const ROUNDS_PER_BEAT: usize = 4;

/// Writes one key, retrying while the answer is one the caller is told to retry.
///
/// **One store, deliberately.** See `retire.rs`'s note: a hint-ignoring retry livelocks against a
/// two-voter region, and `seed_all_three_families` runs before the second store is opened, so
/// every write here happens under a sole voter that cannot lose office.
async fn put(store: &Arc<Store>, region: &Region, k: Bytes, value: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        let request = RawKvReq::put(k.clone(), Bytes::copy_from_slice(value));
        match within("a put to be applied", store.serve(header, request)).await {
            Ok(_) => return,
            Err(error) => {
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "writing {k:?}: {error}");
                assert!(Instant::now() < deadline, "writing {k:?} never succeeded");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

async fn txn(store: &Arc<Store>, region: &Region, request: TxnKvReq) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let epoch = store
            .regions()
            .get(region.id)
            .map_or(region.epoch, |state| state.region().epoch);
        let header = RequestHeader::new(region.id, epoch, 0);
        match within(
            "a transactional write",
            store.serve_txn(header, request.clone()),
        )
        .await
        {
            Ok(_) => return,
            Err(error) => {
                if error.is_ambiguous() {
                    continue;
                }
                assert!(error.is_retryable(), "{request:?}: {error}");
                assert!(Instant::now() < deadline, "{request:?} never succeeded");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

/// Four committed rows, one uncommitted prewrite and one raw put, so `default`, `write` **and**
/// `lock` all hold keys when the sweep runs.
///
/// The prewrite is what fills the third family, and it is not decoration: a reclamation that
/// missed `lock` entirely would pass a test built only from commits, which is the shape ADR 0032
/// found in the version-1 snapshot stream.
async fn seed_all_three_families(store: &Arc<Store>, region: &Region) {
    for n in 0..4 {
        let k = key(n);
        let start_ts = 10 + u64::from(n) * 2;
        txn(
            store,
            region,
            TxnKvReq::Prewrite {
                start_ts,
                primary: k.clone(),
                ttl_ms: 60_000,
                mutations: vec![TxnMutation::Put {
                    key: k.clone(),
                    value: Bytes::from_static(b"committed"),
                    read_ts: None,
                }],
            },
        )
        .await;
        txn(
            store,
            region,
            TxnKvReq::Commit {
                start_ts,
                commit_ts: start_ts + 1,
                keys: vec![k],
            },
        )
        .await;
    }
    txn(
        store,
        region,
        TxnKvReq::Prewrite {
            start_ts: 100,
            primary: key(50),
            ttl_ms: 600_000,
            mutations: vec![TxnMutation::Put {
                key: key(50),
                value: Bytes::from_static(b"uncommitted"),
                read_ts: None,
            }],
        },
    )
    .await;
    put(store, region, key(200), b"raw").await;
}

/// Puts the case's record in front of the sweep, and checks that it is the record the sweep will
/// actually be answered with.
///
/// **Not belt and braces.** `FakePd::place` is keyed by start key and `get_region` walks back to
/// the last record containing the probe key, so a case that believes it has changed the answer and
/// has not would sit there being refused by the *bootstrap* record and pass for a reason the case
/// is not about. That is the exact shape this lane exists to avoid, and it is cheaper to assert
/// than to reason about — it is what turned the "PD has never heard of the range" case from a
/// green test into a documented skip.
fn place_and_verify(pd: &Arc<FakePd>, case: &Case, hosted: &Region) {
    let PdAnswer::Holds(record) = &case.answer else {
        unreachable!("the run loop skips the silent case; see the comment there")
    };
    pd.place(Region {
        id: record.region_id,
        start_key: hosted.start_key.clone(),
        end_key: hosted.end_key.clone(),
        peers: record
            .peers_on
            .iter()
            .map(|store_id| Peer::voter(*store_id, *store_id))
            .collect(),
        epoch: esker_proto::Epoch::new(record.conf_ver, hosted.epoch.version),
    });

    let answered = pd
        .get_region(&hosted.start_key)
        .expect("the fake placement driver answers")
        .map_or_else(
            || panic!("case {:?}: PD answered nothing", case.name),
            |route| route.region,
        );
    assert_eq!(
        (answered.id, answered.epoch.conf_ver),
        (record.region_id, record.conf_ver),
        "case {:?} did not put the record it meant to in front of the sweep",
        case.name
    );
    assert_eq!(
        answered
            .peers
            .iter()
            .any(|peer| peer.store_id == case.store_id),
        record.names(case.store_id),
        "case {:?} disagrees with PD about whether the record names this store",
        case.name
    );
}

/// Runs one case end to end and reports what the store did.
///
/// The stores are real, the transport is real, and everything the case varies is the placement
/// driver's answer. That is the axis the model enumerates; the timings are `tests/retire.rs`'s
/// subject and are not re-tested here.
async fn observe(case: &Case) -> Observed {
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
        raft_options(peers.clone(), Some(vec![1])),
        1,
    )
    .await;
    wait_for("a leader on the first store", || {
        first.store.peer_of(1).is_some_and(|peer| peer.is_leader())
    })
    .await;

    let region = first.store.regions().regions()[0].clone();
    seed_all_three_families(&first.store, &region).await;

    let second = open(
        second_address_listener,
        2,
        &pd,
        raft_options(peers.clone(), Some(vec![2])),
        2,
    )
    .await;
    pd.issue(Operator::AddPeer {
        region_id: 1,
        epoch: first.store.regions().regions()[0].epoch,
        store_id: 2,
        peer_id: 2,
    });
    wait_for("the region to arrive on the second store", || {
        second.store.regions().get(1).is_some()
    })
    .await;
    wait_for("the second store's peer to become a voter", || {
        first.store.regions().get(1).is_some_and(|state| {
            state
                .region()
                .peers
                .iter()
                .any(|peer| peer.peer_id == 2 && peer.role == PeerRole::Voter)
        })
    })
    .await;
    wait_for("every column family to arrive on the second store", || {
        keys_held(&second.store) > 0
    })
    .await;

    let hosted = second.store.regions().get(1).unwrap().region().clone();
    let keys_before = keys_held(&second.store);

    // **The removed peer's state, without the removal.** Stopping the first store leaves the
    // second's peer leaderless against a group of two it cannot reach, which is what a peer the
    // cluster has replaced is permanently — and what the probe counts rounds of. Doing it this
    // way rather than by issuing `RemovePeer` is what lets the case decide what PD says next,
    // including the answers a real removal would never produce.
    first.stop().await;
    wait_for("the second store's peer to lose its leader", || {
        second
            .store
            .peer_of(1)
            .is_some_and(|peer| peer.leader().is_none())
    })
    .await;

    place_and_verify(&pd, case, &hosted);

    // Long enough for the throttle (50 leaderless rounds at a 5 ms tick) several times over, so a
    // "nothing happened" answer is a decision and not a race.
    let beats_before = pd.store_beats().len();
    let watch_until = Instant::now() + Duration::from_secs(3);
    let mut reclaimed_inside_the_window = false;
    while Instant::now() < watch_until {
        if second.store.regions().get(1).is_none() && keys_held(&second.store) == 0 {
            // The reclamation has finished. Every other case runs the full window, because
            // "nothing happened" is only a decision once the throttle has had time to fire.
            reclaimed_inside_the_window = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // **The window's own precondition, asserted rather than asserted in prose.**
    //
    // Every "nothing happened" answer below rests on the throttle having had room to fire, and
    // the comment above says so — but the budget is spent in *wall clock* while the throttle
    // counts *rounds*, and the heartbeat interval is `MissedTickBehavior::Skip`, so a skipped
    // tick is a round that never happens rather than one that happens late. Nothing connected
    // the two, so a tighter tick, a larger `ORPHAN_PROBE_ROUNDS` or a slow enough box would
    // shorten the window in rounds while it still looked like three seconds — and the failure
    // would arrive as `still_hosted: true`, which this test reads as a *decision*.
    //
    // A store beat is emitted every `store_heartbeat / tick` rounds (20 ms / 5 ms = 4), so the
    // beats PD received are a count of the rounds that really ran. Measured at 520-600 rounds
    // against the 50 the throttle needs — quiet and under forty-eight spinning threads alike, so
    // the margin is real and this assertion is not a flake waiting to happen. It exists to make
    // the erosion loud if it ever starts.
    if !reclaimed_inside_the_window {
        let rounds = (pd.store_beats().len() - beats_before) * ROUNDS_PER_BEAT;
        assert!(
            rounds >= PROBE_ROUNDS * 2,
            "case {:?}: the window ran {rounds} heartbeat rounds, and the orphan probe needs \
             {PROBE_ROUNDS} before it asks PD anything — so \"nothing happened\" is this test \
             running out of clock, not the store deciding",
            case.name
        );
    }
    let observed = Observed {
        still_hosted: second.store.regions().get(1).is_some(),
        keys_left: keys_held(&second.store),
        keys_before,
    };
    second.stop().await;
    observed
}

/// Every answer the placement driver can give about a range this store hosts.
///
/// One test rather than one per case, because each case costs a two-store cluster and they share
/// nothing else. A failure names the case.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_reclaims_on_evidence_and_never_otherwise() {
    let mut ran = 0;
    let mut reclaimed = 0;
    let mut skipped: Vec<&'static str> = Vec::new();
    for case in cases(2, 1, 0) {
        // The hosted conf_ver is whatever the cluster reached by the time the case runs, so the
        // table's relative values are re-expressed against it inside `observe`. `AddPeer` plus the
        // promotion move it to 3.
        let case = Case {
            hosted: esker_sim::mech::sweep::RegionSpan {
                conf_ver: 3,
                ..case.hosted
            },
            answer: match case.answer {
                PdAnswer::Silent => PdAnswer::Silent,
                PdAnswer::Holds(record) => PdAnswer::Holds(esker_sim::mech::sweep::RegionSpan {
                    conf_ver: record.conf_ver + 3,
                    ..record
                }),
            },
            ..case
        };
        if case.overlapping_hosted {
            // Two overlapping regions cannot both be in one store's `RegionMap`, so this state
            // only arises from a *stale* record — a parent narrowed by a split, retiring against
            // the range it used to have. A leaderless store cannot split, so the cluster cannot
            // be driven into it here. The gate itself is asserted directly below.
            skipped.push(case.name);
            continue;
        }
        if case.answer == PdAnswer::Silent {
            // Not constructible through `FakePd`'s public surface, and worth writing down rather
            // than faking. `get_region` walks back to the last record containing the probe key;
            // the region under test starts at the empty key, so the only record that can answer
            // is the one at the empty key, and the bootstrap wrote one there. Overwriting it with
            // any range still leaves a range containing the empty key.
            //
            // The first version of this file "covered" the case by placing a record somewhere
            // else and watching nothing happen — which it did, because PD went on answering with
            // the bootstrap record that names this store. It passed, for the wrong reason. The
            // `answered` assertion in `observe` is what turns that into a failure now.
            //
            // The branch is one line in the sweep (`let Ok(Ok(Some(route))) = .. else { continue }`),
            // it is the same branch an unreachable PD takes, and it is fail-closed by
            // construction: no answer, no retirement.
            skipped.push(case.name);
            continue;
        }
        let observed = observe(&case).await;
        if let Err(violation) = check(&case, observed) {
            panic!(
                "{violation}\n\nThe sweep is fail-closed: every answer that is not positive \
                 evidence of a removal leaves the region alone, because keeping a region this \
                 store was removed from costs disk while dropping one it still holds loses \
                 acknowledged writes."
            );
        }
        ran += 1;
        if case.expected() == Expected::Reclaim {
            reclaimed += 1;
        }
    }
    assert_eq!(
        skipped.len(),
        2,
        "two cases are not constructible against a real cluster and each says why: {skipped:?}"
    );
    assert_eq!(ran, 5, "the table shrank without the checker noticing");
    assert_eq!(
        reclaimed, 1,
        "exactly one answer is evidence of a removal; a run where none was is a run that never \
         exercised the reclamation at all"
    );
}

/// The second gate, asserted directly against the map that decides it.
///
/// It is not driven through a cluster for the reason the skip above gives: a store cannot hold
/// two overlapping regions, so the state only arises from a stale record, and a leaderless store
/// cannot produce one. `RegionMap::overlapping` is the authority the reclamation consults, and
/// this is that call.
#[test]
fn a_hosted_region_covering_the_range_is_what_the_second_gate_reads() {
    let map = esker_store::RegionMap::new();
    let child = Region {
        id: 2,
        start_key: Bytes::from_static(b"m"),
        end_key: Bytes::new(),
        peers: vec![Peer::voter(2, 2)],
        epoch: esker_proto::Epoch::new(1, 2),
    };
    map.insert(esker_store::RegionState::unreplicated(
        esker_store::RegionMeta::new(child),
    ))
    .unwrap();

    // The range a parent narrowed by that split would retire against: the whole key space, which
    // it had before the split and which its stale record still says.
    let stale_parent = Region {
        id: 1,
        start_key: Bytes::new(),
        end_key: Bytes::new(),
        peers: vec![Peer::voter(1, 1)],
        epoch: esker_proto::Epoch::new(1, 1),
    };
    let overlapping = map.overlapping(&stale_parent.start_key, &stale_parent.end_key);
    assert_eq!(
        overlapping
            .iter()
            .map(|region| region.id)
            .collect::<Vec<_>>(),
        vec![2],
        "the gate has to see the child, or a retirement against the parent's old range would \
         empty the child's keys under its owner"
    );
}
