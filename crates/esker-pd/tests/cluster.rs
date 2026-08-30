//! The placement driver's 4a surface, end to end against a real database.
//!
//! Bootstrap, ids, the oracle, the routing table and store liveness, driven through the public
//! API with a clock a test sets by hand. These live here rather than beside the code because
//! they need nothing private — and because `pd.rs` is where the *reasoning* is, and a file that
//! is two thirds test is a file nobody reads the reasoning in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_pd::clock::TestClock;
use esker_pd::pd::MAX_STORE_DOWN_TIME_MS;
use esker_pd::{Clock, Pd, PdOptions, RegionBeat, StoreBeat, StoreStats, Upsert};
use esker_proto::{Epoch, Peer, Region, StoreInfo};
use std::sync::Arc;

fn open() -> (tempfile::TempDir, Arc<TestClock>, Arc<Pd>) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let pd = Pd::open(
        dir.path(),
        PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
    )
    .unwrap();
    (dir, clock, pd)
}

#[test]
fn a_fresh_pd_is_not_bootstrapped() {
    let (_dir, _clock, pd) = open();
    assert!(pd.cluster().unwrap().is_none());
    assert!(matches!(
        pd.cluster_id().unwrap_err(),
        esker_pd::PdError::NotBootstrapped
    ));
    assert!(matches!(
        pd.get_region(b"anything").unwrap_err(),
        esker_pd::PdError::NotBootstrapped
    ));
}

/// `docs/DESIGN.md` §7: the first store to register receives region 1 covering everything.
#[test]
fn the_first_store_receives_the_region_that_covers_everything() {
    let (_dir, _clock, pd) = open();
    let first = pd.bootstrap(1, "127.0.0.1:20160").unwrap();
    let region = first
        .region
        .expect("the first store bootstraps the cluster");

    assert_eq!(region.id, 1, "the first region is region 1");
    assert!(region.start_key.is_empty() && region.end_key.is_empty());
    assert_eq!(region.peers.len(), 1);
    assert_eq!(region.peers[0].store_id, 1);
    assert_ne!(region.peers[0].peer_id, region.id, "a peer is not a region");
    assert_eq!(region.epoch, Epoch::INITIAL);
    assert_ne!(first.cluster_id, 0);

    // And it is routable immediately, at both ends of the key space.
    for key in [&b""[..], b"m", b"\xff\xff\xff\xff"] {
        let route = pd
            .get_region(key)
            .unwrap()
            .expect("a region covers {key:?}");
        assert_eq!(route.region.id, region.id);
        assert_eq!(route.stores, vec![StoreInfo::new(1, "127.0.0.1:20160")]);
        assert_eq!(route.leader_peer_id, None, "no heartbeat has arrived yet");
    }
}

/// A second call is a registration, not a second cluster: exactly one store in the life of
/// a cluster is told to create region 1.
#[test]
fn bootstrap_is_idempotent_and_only_one_store_gets_a_region() {
    let (_dir, _clock, pd) = open();
    let first = pd.bootstrap(1, "127.0.0.1:20160").unwrap();

    let again = pd.bootstrap(1, "127.0.0.1:20160").unwrap();
    assert_eq!(again.cluster_id, first.cluster_id);
    assert_eq!(again.region, None);

    let second_store = pd.bootstrap(2, "127.0.0.1:20161").unwrap();
    assert_eq!(second_store.cluster_id, first.cluster_id);
    assert_eq!(second_store.region, None);

    assert_eq!(pd.regions().unwrap().len(), 1, "one region, one bootstrap");
    assert_eq!(pd.stores().unwrap().len(), 2, "both stores are registered");
}

/// A store that restarts at a new address is reachable at the new one. This is why
/// `Bootstrap` is meant to be called on every start.
#[test]
fn re_registering_refreshes_the_address() {
    let (_dir, _clock, pd) = open();
    pd.bootstrap(1, "127.0.0.1:20160").unwrap();
    pd.bootstrap(1, "127.0.0.1:29999").unwrap();
    let route = pd.get_region(b"k").unwrap().unwrap();
    assert_eq!(route.stores, vec![StoreInfo::new(1, "127.0.0.1:29999")]);
}

#[test]
fn a_request_for_another_cluster_is_refused() {
    let (_dir, _clock, pd) = open();
    let cluster_id = pd.bootstrap(1, "a").unwrap().cluster_id;
    assert!(pd.check_cluster(cluster_id).is_ok());
    assert!(matches!(
        pd.check_cluster(cluster_id ^ 1).unwrap_err(),
        esker_pd::PdError::ClusterMismatch { .. }
    ));
}

#[test]
fn ids_are_monotone_and_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    let mut handed_out = Vec::new();
    {
        let pd = Pd::open(dir.path(), options()).unwrap();
        pd.bootstrap(1, "a").unwrap();
        for _ in 0..5 {
            handed_out.push(pd.alloc_id(1).unwrap());
        }
        handed_out.push(pd.alloc_id(10).unwrap());
    }
    {
        let pd = Pd::open(dir.path(), options()).unwrap();
        let after = pd.alloc_id(1).unwrap();
        assert!(
            after > *handed_out.last().unwrap() + 9,
            "{after} is inside a batch the previous process had reserved"
        );
        handed_out.push(after);
    }

    let unique: std::collections::BTreeSet<u64> = handed_out.iter().copied().collect();
    assert_eq!(unique.len(), handed_out.len(), "an id was handed out twice");
    assert!(handed_out.windows(2).all(|pair| pair[0] < pair[1]));
}

/// The cluster id is minted once and never again: a restart must not look like a new
/// cluster, or every store would be told it is talking to the wrong one.
#[test]
fn the_cluster_id_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    let minted = {
        let pd = Pd::open(dir.path(), options()).unwrap();
        pd.bootstrap(1, "a").unwrap().cluster_id
    };
    clock.advance(60_000);
    let pd = Pd::open(dir.path(), options()).unwrap();
    assert_eq!(pd.cluster_id().unwrap(), minted);
    assert_eq!(pd.bootstrap(1, "a").unwrap().cluster_id, minted);
    assert_eq!(pd.regions().unwrap().len(), 1);
}

/// The oracle's rule, end to end over a real database and a clock that goes backwards
/// across the reopen: nothing repeats, and nothing goes down.
#[test]
fn timestamps_never_repeat_across_a_reopen_with_a_backwards_clock() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    let mut seen = Vec::new();
    {
        let pd = Pd::open(dir.path(), options()).unwrap();
        pd.bootstrap(1, "a").unwrap();
        for _ in 0..4 {
            seen.push(pd.tso(16).unwrap());
            clock.advance(1);
        }
    }

    // A day backwards, which is what an NTP correction on a badly-set machine looks like.
    clock.set(1_700_000_000_000 - 86_400_000);
    let pd = Pd::open(dir.path(), options()).unwrap();
    let after = pd.tso(1).unwrap();

    let highest = seen.iter().copied().max().unwrap();
    assert!(
        after > highest,
        "after the restart {after} is not above {highest} from before it"
    );
    let unique: std::collections::BTreeSet<u64> = seen.iter().copied().collect();
    assert_eq!(unique.len(), seen.len());
}

/// Every timestamp handed out is strictly below the mark on disk. This is the property the
/// restart rule leans on; if it ever fails, a restart can repeat a timestamp.
#[test]
fn every_timestamp_is_below_the_persisted_mark() {
    let (_dir, clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    for step in 0..8 {
        let ts = pd.tso(4).unwrap();
        let (physical, _) = esker_pd::decompose_ts(ts);
        let mark = pd.tso_high_water_ms().unwrap();
        assert!(
            physical < mark,
            "step {step}: {physical} is not below {mark}"
        );
        clock.advance(500);
    }
    // And the mark on disk is the one in memory, not one still in a buffer somewhere.
    let stored = esker_pd::record::TsoRecord::decode(
        &pd.db()
            .get(
                esker_engine::cf::DEFAULT,
                &esker_pd::keys::tso_key(),
                &esker_engine::ReadOptions::default(),
            )
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.high_water_ms, pd.tso_high_water_ms().unwrap());
}

fn beat(region: Region, leader: u64, term: u64) -> RegionBeat {
    RegionBeat {
        region,
        leader_peer_id: leader,
        term,
        approximate_size: 0,
        applied_index: 0,
    }
}

fn ranged(id: u64, start: &'static [u8], end: &'static [u8], epoch: (u64, u64)) -> Region {
    Region {
        id,
        start_key: bytes::Bytes::from_static(start),
        end_key: bytes::Bytes::from_static(end),
        peers: vec![Peer::voter(1, id * 10)],
        epoch: Epoch::new(epoch.0, epoch.1),
    }
}

/// The heartbeat that arrives second is not necessarily the one that happened second.
/// Both orders are tested, because only one of them can be got right by accident.
#[test]
fn a_stale_heartbeat_never_overwrites_a_newer_epoch() {
    for reversed in [false, true] {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();

        let old = ranged(1, b"", b"", (1, 1));
        let new = ranged(1, b"", b"", (1, 2));
        let (first, second) = if reversed {
            (beat(new, 20, 5), beat(old, 10, 4))
        } else {
            (beat(old, 10, 4), beat(new, 20, 5))
        };

        assert_eq!(pd.region_heartbeat(&first).unwrap().upsert, Upsert::Applied);
        let outcome = pd.region_heartbeat(&second).unwrap().upsert;
        assert_eq!(
            outcome,
            if reversed {
                Upsert::Stale
            } else {
                Upsert::Applied
            },
            "arriving {}",
            if reversed { "newest first" } else { "in order" }
        );

        // Whichever order they arrived in, PD holds the newer epoch and its leader.
        let held = pd.regions().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].region.epoch, Epoch::new(1, 2));
        assert_eq!(held[0].leader_peer_id, 20);
    }
}

/// The counters move on different events, so a beat behind in *either* one is stale.
#[test]
fn a_heartbeat_behind_in_either_counter_is_stale() {
    let (_dir, _clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    pd.region_heartbeat(&beat(ranged(1, b"", b"", (3, 3)), 10, 1))
        .unwrap();

    for stale in [(2, 3), (3, 2), (2, 4)] {
        assert_eq!(
            pd.region_heartbeat(&beat(ranged(1, b"", b"", stale), 99, 9))
                .unwrap()
                .upsert,
            Upsert::Stale,
            "epoch {stale:?} was accepted over (3, 3)"
        );
    }
    assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 10);
}

/// Within one epoch a leader election is invisible, so the term is what says which of two
/// heartbeats is the newer one.
#[test]
fn within_one_epoch_the_newer_term_wins_and_the_older_is_dropped() {
    let (_dir, _clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    let region = || ranged(1, b"", b"", (1, 1));

    pd.region_heartbeat(&beat(region(), 10, 7)).unwrap();
    assert_eq!(
        pd.region_heartbeat(&beat(region(), 20, 6)).unwrap().upsert,
        Upsert::Stale,
        "a beat from a leader that has already lost office"
    );
    assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 10);

    // The same leader reporting again, at the same term, is fresher stats.
    assert_eq!(
        pd.region_heartbeat(&beat(region(), 30, 7)).unwrap().upsert,
        Upsert::Applied
    );
    assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 30);
}

/// Three regions, including the one that runs to the end of the key space: every key must
/// land in exactly the region that owns it, and the index must not leave a stale entry
/// behind when a range changes.
#[test]
fn a_lookup_finds_the_region_that_owns_the_key() {
    let (_dir, _clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();

    // Region 1 shrinks to ["", "m"), and two more cover the rest. This is the shape a
    // split leaves behind; 4a only ever gets here by heartbeat.
    for region in [
        ranged(1, b"", b"m", (1, 2)),
        ranged(2, b"m", b"t", (1, 2)),
        ranged(3, b"t", b"", (1, 2)),
    ] {
        pd.region_heartbeat(&beat(region, 0, 1)).unwrap();
    }

    for (key, expected) in [
        (&b""[..], 1),
        (b"a", 1),
        (b"l", 1),
        (b"m", 2),
        (b"s", 2),
        (b"t", 3),
        (b"z", 3),
        (b"\xff\xff\xff", 3),
    ] {
        let route = pd
            .get_region(key)
            .unwrap()
            .unwrap_or_else(|| panic!("no region owns {key:?}"));
        assert_eq!(route.region.id, expected, "key {key:?}");
    }

    // The index holds one entry per region and no orphan from region 1's old range.
    let index = esker_pd::routing::range_index(pd.db()).unwrap();
    assert_eq!(index.len(), 3, "the index kept a stale entry: {index:?}");
}

/// A store that never registered has no address, so a heartbeat from one is refused
/// rather than inventing a record a client could be routed to.
#[test]
fn a_heartbeat_from_an_unregistered_store_is_refused() {
    let (_dir, clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    let beat = StoreBeat {
        store_id: 9,
        stats: StoreStats::default(),
    };
    assert!(pd.store_heartbeat(&beat).is_err());

    // And a registered one is recorded, liveness included.
    clock.advance(1_000);
    let beat = StoreBeat {
        store_id: 1,
        stats: StoreStats {
            capacity: 100,
            available: 40,
            region_count: 3,
            leader_count: 1,
            applied_bytes: 7,
        },
    };
    pd.store_heartbeat(&beat).unwrap();
    let stored = pd.stores().unwrap();
    assert_eq!(stored[0].stats.available, 40);
    assert_eq!(stored[0].last_heartbeat_ms, clock.now_ms());
    assert_eq!(stored[0].address, "a", "the heartbeat lost the address");
}

#[test]
fn a_malformed_region_is_refused() {
    let (_dir, _clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    assert!(
        pd.region_heartbeat(&beat(ranged(0, b"", b"", (1, 1)), 1, 1))
            .is_err(),
        "region id zero"
    );
    assert!(
        pd.region_heartbeat(&beat(ranged(2, b"z", b"a", (1, 1)), 1, 1))
            .is_err(),
        "an inverted range"
    );
    assert!(
        pd.region_heartbeat(&beat(ranged(2, b"a", b"a", (1, 1)), 1, 1))
            .is_err(),
        "an empty range"
    );
}

/// The routing table survives a reopen: it is on disk, not in memory.
#[test]
fn the_routing_table_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    {
        let pd = Pd::open(dir.path(), options()).unwrap();
        pd.bootstrap(1, "127.0.0.1:1").unwrap();
        pd.region_heartbeat(&beat(ranged(1, b"", b"m", (1, 2)), 10, 3))
            .unwrap();
        pd.region_heartbeat(&beat(ranged(2, b"m", b"", (1, 2)), 20, 3))
            .unwrap();
    }
    let pd = Pd::open(dir.path(), options()).unwrap();
    assert_eq!(pd.regions().unwrap().len(), 2);
    let route = pd.get_region(b"zz").unwrap().unwrap();
    assert_eq!(route.region.id, 2);
    assert_eq!(route.leader_peer_id, Some(20));
}

#[test]
fn a_store_id_of_zero_is_refused() {
    let (_dir, _clock, pd) = open();
    assert!(pd.bootstrap(0, "a").is_err());
}

/// Liveness is derived from the last beat and nothing else — and in 4a it is only
/// reported.
#[test]
fn a_silent_store_is_reported_as_down() {
    let (_dir, clock, pd) = open();
    pd.bootstrap(1, "a").unwrap();
    assert!(pd.down_stores().unwrap().is_empty());
    clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
    assert_eq!(pd.down_stores().unwrap(), vec![1]);
}
