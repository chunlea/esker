//! A store that hosts more than one region: routing, refusals, restart and heartbeats.
//!
//! Phase 3e's store had one region covering everything, so every ownership check it ran always
//! passed. These tests are the ones that could not exist before, and each is a way the store could
//! be wrong now that the checks can fail.
//!
//! The two-region state is **hand-built**: the records are written straight into the `raft` column
//! family, exactly as they would look the instant after a split applied. Real splits are 4b
//! (`prompts/04-multiraft-pd.md`), and a test that waited for one would be testing the split
//! rather than the routing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::{WriteBatch, WriteOptions, cf};
use esker_proto::{Epoch, Peer, ProtoError, RawKvReq, RawKvResp, Region, RequestHeader};
use esker_store::pd::{FakePd, PdClient, StoreInfo};
use esker_store::{Store, StoreOptions, meta};

/// A store hosting one region covering everything, as a fresh database bootstraps it.
fn open(dir: &tempfile::TempDir) -> Arc<Store> {
    Store::open(dir.path(), StoreOptions::new()).unwrap()
}

/// Writes region records into a store's `raft` column family, replacing what is there.
///
/// The batch is one write, because the two halves of a split have to appear together: a state in
/// which region 1 has been narrowed and region 2 does not exist yet is a hole in the key space.
fn seed(store: &Store, regions: &[Region]) {
    let cf_id = store.db().cf_id(cf::RAFT).unwrap();
    let mut batch = WriteBatch::new();
    for region in regions {
        meta::stage_region(&mut batch, cf_id, region);
    }
    store
        .db()
        .write(batch, &WriteOptions { sync: true })
        .unwrap();
}

fn region(id: u64, start: &[u8], end: &[u8], peer_id: u64, epoch: Epoch) -> Region {
    Region {
        id,
        start_key: Bytes::copy_from_slice(start),
        end_key: Bytes::copy_from_slice(end),
        peers: vec![Peer::voter(1, peer_id)],
        epoch,
    }
}

/// The state one split leaves behind: region 1 narrowed to `["", "m")`, region 2 taking the rest,
/// both at `version` 2 (`docs/DESIGN.md` §6 — a split bumps `version` on both halves).
fn split_state() -> [Region; 2] {
    let after = Epoch::new(1, 2);
    [
        region(1, b"", b"m", 1, after),
        region(2, b"m", b"", 2, after),
    ]
}

/// A store hosting the two halves of a split.
fn two_regions(dir: &tempfile::TempDir) -> Arc<Store> {
    let store = open(dir);
    seed(&store, &split_state());
    drop(store);
    open(dir)
}

fn header(region_id: u64, epoch: Epoch) -> RequestHeader {
    RequestHeader::new(region_id, epoch, 0)
}

fn ids(regions: &[Region]) -> Vec<u64> {
    regions.iter().map(|region| region.id).collect()
}

// -- routing ---------------------------------------------------------------------------

#[test]
fn each_region_serves_its_own_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    assert_eq!(store.regions().len(), 2);
    let after = Epoch::new(1, 2);

    // A write to each half, through the region that owns it.
    store
        .handle(header(1, after), RawKvReq::put(&b"apple"[..], &b"low"[..]))
        .unwrap();
    store
        .handle(header(2, after), RawKvReq::put(&b"zebra"[..], &b"high"[..]))
        .unwrap();

    assert_eq!(
        store
            .handle(header(1, after), RawKvReq::get(&b"apple"[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"low"))
        }
    );
    assert_eq!(
        store
            .handle(header(2, after), RawKvReq::get(&b"zebra"[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"high"))
        }
    );
}

/// A request that names the right region for the wrong key is refused with the range that refused
/// it — the client's routing is wrong, and waiting cannot fix it.
#[test]
fn a_key_outside_the_region_named_is_refused_with_its_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    let after = Epoch::new(1, 2);

    let error = store
        .handle(header(1, after), RawKvReq::get(&b"zebra"[..]))
        .unwrap_err();
    match error {
        ProtoError::KeyNotInRegion {
            key,
            region_id,
            start_key,
            end_key,
        } => {
            assert_eq!(key, Bytes::from_static(b"zebra"));
            assert_eq!(region_id, 1);
            assert_eq!(start_key, Bytes::new());
            assert_eq!(end_key, Bytes::from_static(b"m"));
        }
        other => panic!("{other:?}"),
    }

    // And the mirror: the high region does not serve a low key.
    assert!(matches!(
        store
            .handle(header(2, after), RawKvReq::get(&b"apple"[..]))
            .unwrap_err(),
        ProtoError::KeyNotInRegion { .. }
    ));
}

#[test]
fn a_region_this_store_does_not_host_is_not_an_epoch_problem() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    assert_eq!(
        store
            .handle(header(7, Epoch::new(1, 2)), RawKvReq::get(&b"a"[..]))
            .unwrap_err(),
        ProtoError::RegionNotFound { region_id: 7 }
    );
}

// -- the epoch matrix ------------------------------------------------------------------

/// The trap the whole of 4a is shaped around. A client that still believes the pre-split region 1
/// is asking about a range that is now two regions; an answer naming only region 1 costs it a
/// `GetRegion` round trip for the other half, and under 4b's split storm that is a round trip per
/// stale request against a single placement driver.
#[test]
fn a_stale_epoch_is_answered_with_every_region_the_request_touches() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    let before = Epoch::INITIAL;

    // The pre-split client scans the whole key space, believing region 1 owns it.
    let error = store
        .handle(header(1, before), RawKvReq::scan(&b""[..], &b""[..], 0))
        .unwrap_err();
    match error {
        ProtoError::EpochNotMatch { current_regions } => {
            assert_eq!(
                ids(&current_regions),
                [1, 2],
                "both halves, not just the one named"
            );
            assert_eq!(current_regions[0].end_key, Bytes::from_static(b"m"));
            assert_eq!(
                current_regions[1].end_key,
                Bytes::new(),
                "the last region's empty end key survives the round trip"
            );
            for region in &current_regions {
                assert_eq!(region.epoch, Epoch::new(1, 2));
            }
        }
        other => panic!("{other:?}"),
    }

    // A point request needs one region, and sending both would be one the client did not ask
    // about — the hint is repair, not a routing table dump.
    let error = store
        .handle(header(1, before), RawKvReq::get(&b"apple"[..]))
        .unwrap_err();
    match error {
        ProtoError::EpochNotMatch { current_regions } => assert_eq!(ids(&current_regions), [1]),
        other => panic!("{other:?}"),
    }

    // A range that straddles the new boundary needs both, and the region below the range start is
    // the one a walk that began at the start key would have missed.
    let error = store
        .handle(
            header(1, before),
            RawKvReq::scan(&b"kiwi"[..], &b"zebra"[..], 0),
        )
        .unwrap_err();
    match error {
        ProtoError::EpochNotMatch { current_regions } => assert_eq!(ids(&current_regions), [1, 2]),
        other => panic!("{other:?}"),
    }
}

/// Both counters move on different events, so a client can be stale in one and current in the
/// other — and being *ahead* is as wrong as being behind, because it means the client is
/// describing a region this store has not become.
#[test]
fn every_epoch_mismatch_is_refused_in_either_direction() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    let current = Epoch::new(1, 2);

    for wrong in [
        Epoch::new(0, 2),
        Epoch::new(1, 1),
        Epoch::new(2, 2),
        Epoch::new(1, 3),
        Epoch::new(0, 1),
        Epoch::new(2, 3),
    ] {
        let error = store
            .handle(header(1, wrong), RawKvReq::get(&b"apple"[..]))
            .unwrap_err();
        assert!(
            matches!(error, ProtoError::EpochNotMatch { .. }),
            "{wrong:?} gave {error:?}"
        );
    }
    store
        .handle(header(1, current), RawKvReq::get(&b"apple"[..]))
        .unwrap();
}

// -- the `["", "")` boundary -----------------------------------------------------------

/// An empty `end_key` means the end of the key space in region metadata, and nowhere else. Region
/// 1 of a fresh store owns every key including the empty one; a *bounded* region does not own an
/// unbounded scan, which is the case a plain comparison gets backwards.
#[test]
fn the_empty_end_key_means_the_end_of_the_key_space() {
    let dir = tempfile::tempdir().unwrap();
    let whole = open(&dir);
    for key in [&b""[..], b"a", b"r", b"\xff\xff\xff\xff"] {
        whole
            .handle(header(1, Epoch::INITIAL), RawKvReq::put(key, &b"v"[..]))
            .unwrap_or_else(|error| panic!("the whole-key-space region rejected {key:?}: {error}"));
    }
    whole
        .handle(
            header(1, Epoch::INITIAL),
            RawKvReq::scan(&b""[..], &b""[..], 0),
        )
        .unwrap();
    drop(whole);

    let store = two_regions(&dir);
    let after = Epoch::new(1, 2);

    // The empty key is the *first* key, so it belongs to the low region, not the unbounded one.
    assert_eq!(
        store
            .handle(header(1, after), RawKvReq::get(&b""[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"v"))
        }
    );
    assert!(matches!(
        store
            .handle(header(2, after), RawKvReq::get(&b""[..]))
            .unwrap_err(),
        ProtoError::KeyNotInRegion { .. }
    ));

    // An unbounded scan runs past a bounded region and is refused rather than clamped: a client
    // that asked for everything and was quietly given half would not know.
    assert!(
        store
            .handle(header(1, after), RawKvReq::scan(&b"a"[..], &b""[..], 0))
            .is_err()
    );
    // The unbounded region answers one, and the top of the key space is inside it.
    store
        .handle(header(2, after), RawKvReq::scan(&b"m"[..], &b""[..], 0))
        .unwrap();
    assert_eq!(
        store
            .handle(header(2, after), RawKvReq::get(&b"\xff\xff\xff\xff"[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"v"))
        }
    );
}

// -- restart ---------------------------------------------------------------------------

/// What a store hosts is on its own disk. A restart recovers the set it had — ranges, epochs and
/// peers — rather than re-deriving one from its configuration, which would silently undo a split.
#[test]
fn a_restart_recovers_exactly_the_regions_it_hosted() {
    let dir = tempfile::tempdir().unwrap();
    let store = two_regions(&dir);
    let after = Epoch::new(1, 2);
    store
        .handle(header(2, after), RawKvReq::put(&b"zebra"[..], &b"high"[..]))
        .unwrap();
    drop(store);

    let store = open(&dir);
    assert_eq!(store.regions().len(), 2, "a restart re-bootstrapped");
    assert_eq!(store.regions().regions(), split_state());
    assert!(
        store.region().is_none(),
        "a store hosting two has no `the` region"
    );

    // And the data is still addressed through the region that owns it.
    assert_eq!(
        store
            .handle(header(2, after), RawKvReq::get(&b"zebra"[..]))
            .unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from_static(b"high"))
        }
    );
    // The pre-split epoch is still refused after a restart: the epoch is on disk, not in memory.
    assert!(matches!(
        store
            .handle(header(1, Epoch::INITIAL), RawKvReq::get(&b"a"[..]))
            .unwrap_err(),
        ProtoError::EpochNotMatch { .. }
    ));
}

/// `docs/plans/phase-4.md` §6, race 3. A store that crashes between `RemovePeer` applying and its
/// own data being deleted restarts holding a record for a region it is no longer in. Starting a
/// peer for it would put a voter back into a group that has already removed it.
#[test]
fn a_record_that_does_not_name_this_store_is_not_started() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    seed(
        &store,
        &[
            region(1, b"", b"m", 1, Epoch::new(1, 2)),
            // Region 2's only peer is on store 9 — this store has been removed from it.
            Region {
                id: 2,
                start_key: Bytes::from_static(b"m"),
                end_key: Bytes::new(),
                peers: vec![Peer::voter(9, 99)],
                epoch: Epoch::new(1, 2),
            },
        ],
    );
    drop(store);

    let store = open(&dir);
    assert_eq!(ids(&store.regions().regions()), [1], "region 2 was started");
    assert_eq!(
        store
            .handle(header(2, Epoch::new(1, 2)), RawKvReq::get(&b"zebra"[..]))
            .unwrap_err(),
        ProtoError::RegionNotFound { region_id: 2 },
        "a region this store does not host is not served"
    );
}

// -- the placement driver --------------------------------------------------------------

/// Exactly one store in the life of a cluster creates region 1. A second store told the cluster
/// already exists hosts **nothing** — a store that bootstrapped its own `["", "")` because it was
/// second would be a second claim to every key in the cluster.
#[tokio::test(flavor = "multi_thread")]
async fn the_placement_driver_decides_who_bootstraps() {
    let pd: Arc<FakePd> = Arc::new(FakePd::new());
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];

    let first = Store::open(
        dirs[0].path(),
        StoreOptions {
            store_id: 1,
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:7001".to_owned(),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    assert_eq!(ids(&first.regions().regions()), [1]);
    assert_eq!(first.region().unwrap().end_key, Bytes::new());

    let second = Store::open(
        dirs[1].path(),
        StoreOptions {
            store_id: 2,
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:7002".to_owned(),
            ..StoreOptions::new()
        },
    )
    .unwrap();
    assert!(
        second.regions().is_empty(),
        "the second store claimed the key space too"
    );
    first.stop();
    second.stop();
}

/// A store that has already bootstrapped reads its regions off its own disk, and PD's answer that
/// the cluster exists does not take them away.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_with_a_placement_driver_keeps_what_it_hosts() {
    let pd: Arc<FakePd> = Arc::new(FakePd::new());
    let dir = tempfile::tempdir().unwrap();
    let options = || StoreOptions {
        store_id: 1,
        pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
        address: "127.0.0.1:7001".to_owned(),
        ..StoreOptions::new()
    };

    let store = Store::open(dir.path(), options()).unwrap();
    store.stop();
    drop(store);

    let store = Store::open(dir.path(), options()).unwrap();
    assert_eq!(ids(&store.regions().regions()), [1]);
    store.stop();
}

/// A placement driver that cannot be reached fails the open rather than falling back to
/// bootstrapping a region of this store's own. The fallback is the dangerous one: two stores each
/// claiming `["", "")` would not find out until a client asked one of them.
#[tokio::test(flavor = "multi_thread")]
async fn a_placement_driver_that_refuses_fails_the_open() {
    #[derive(Debug)]
    struct Unreachable;
    impl PdClient for Unreachable {
        fn bootstrap(&self, _: &StoreInfo) -> Result<esker_store::Bootstrapped, ProtoError> {
            Err(ProtoError::internal("the placement driver is unreachable"))
        }
        fn alloc_id(&self, _: u64) -> Result<u64, ProtoError> {
            Err(ProtoError::internal("unreachable"))
        }
        fn get_region(&self, _: &[u8]) -> Result<Option<esker_store::RegionRoute>, ProtoError> {
            Err(ProtoError::internal("unreachable"))
        }
        fn store_heartbeat(&self, _: &esker_store::StoreHeartbeat) -> Result<(), ProtoError> {
            Err(ProtoError::internal("unreachable"))
        }
        fn region_heartbeat(&self, _: &esker_store::RegionHeartbeat) -> Result<(), ProtoError> {
            Err(ProtoError::internal("unreachable"))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let error = Store::open(
        dir.path(),
        StoreOptions {
            pd: Some(Arc::new(Unreachable)),
            address: "127.0.0.1:7001".to_owned(),
            ..StoreOptions::new()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("unreachable"), "{error}");
}

// -- heartbeats ------------------------------------------------------------------------

/// The content of what a store reports, not merely that it reported. The cadence itself is
/// asserted by driving a tick counter in `heartbeat.rs`; this is the wiring — that the schedule
/// runs, and that what it puts on the wire is what the store actually holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_reports_itself_and_its_regions_to_the_placement_driver() {
    let pd: Arc<FakePd> = Arc::new(FakePd::new());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 4,
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:7004".to_owned(),
            heartbeat_tick: std::time::Duration::from_millis(2),
            ..StoreOptions::new()
        },
    )
    .unwrap();

    // The first tick reports both, so this waits on one tick rather than on an interval.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while pd.store_beats().is_empty() || pd.region_beats().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no heartbeat arrived");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let beat = pd.store_beats()[0];
    assert_eq!(beat.store_id, 4);
    assert_eq!(beat.region_count, 1);
    assert_eq!(
        beat.leader_count, 1,
        "an unreplicated region is this store's"
    );

    let beat = pd.region_beats()[0].clone();
    assert_eq!(beat.region.id, 1);
    assert_eq!(beat.region.start_key, Bytes::new());
    assert_eq!(beat.region.end_key, Bytes::new());
    assert_eq!(beat.region.epoch, Epoch::INITIAL);
    assert_eq!(beat.leader_peer_id, 1);

    // PD can now route with what the heartbeat told it, which is the point of sending it.
    let route = pd.get_region(b"anything").unwrap().expect("covered");
    assert_eq!(route.region.id, 1);
    assert_eq!(route.leader_peer_id, 1);

    store.stop();
}

/// A store with no placement driver sends nothing and starts anyway — phase 2's single node and
/// phase 3e's static cluster, both of which the CLI still starts.
#[test]
fn a_store_without_a_placement_driver_reports_to_nobody() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let report = store.report();
    assert_eq!(report.regions.len(), 1);
    assert!(report.regions[0].is_leader);
    assert_eq!(report.capacity, 0, "4a reports a placeholder, and says so");
}
