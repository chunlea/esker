//! The placement driver, as a store sees it: one trait, and an in-memory implementation of it.
//!
//! `docs/DESIGN.md` §7 gives PD six methods; a store uses five of them (`Tso` belongs to
//! `esker-txn`). They are behind a trait rather than called directly for the reason every seam in
//! this project exists: the thing on the other side is a network, and a network in a unit test is
//! a test of the network. [`FakePd`] answers from a `BTreeMap`, which is what lets the bootstrap
//! and heartbeat rules be tested by driving a tick counter rather than by waiting ten seconds.
//!
//! # What is deliberately *not* asked of PD
//!
//! Which regions this store hosts. That is read from its own `'m'` records ([`crate::meta`]),
//! and PD is asked exactly one bootstrap question: *is this cluster new, and am I the store that
//! creates region 1?* Everything after that is the store telling PD what it has, not the other
//! way round. A store that took its region set from PD's reply would serve whatever PD currently
//! believes, which is not the same question as what its own log and data say — and PD's belief is
//! built out of heartbeats that this store sends, so the cycle would have no ground.
//!
//! # The types here are provisional
//!
//! The payloads below are the *field sets* pinned in `docs/plans/phase-4.md` §3.2, which the
//! placement-driver lane builds its wire methods and its records against. When that lane lands the
//! `Pd` service section of `esker-proto`, these move there and this module keeps only the trait
//! and the fake. Nothing above the trait changes when they do — which is the point of the trait.

use std::fmt;

#[cfg(any(test, feature = "testing"))]
use std::{collections::BTreeMap, sync::Mutex};

use esker_proto::{ProtoError, Region};

/// What a store tells the placement driver about itself when it registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    /// This store's id.
    pub store_id: u64,
    /// Where other stores and clients reach it. A client addresses a store by id, and turning
    /// one into a socket is PD's job (`docs/DESIGN.md` §10).
    pub address: String,
}

/// What a `Bootstrap` call answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrapped {
    /// The cluster's id, minted by whichever store bootstrapped it.
    pub cluster_id: u64,
    /// The region this store is to create, when this call is the one that bootstrapped the
    /// cluster; `None` when the cluster already existed.
    ///
    /// The distinction *is* the answer. Exactly one store in the life of a cluster is told to
    /// create region 1; every other call — including this store's own next restart — gets `None`
    /// and looks to its own disk.
    pub region: Option<Region>,
}

/// What a store heartbeat reports, every 10 s (`docs/DESIGN.md` §6, §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreHeartbeat {
    /// The store this is about.
    pub store_id: u64,
    /// Bytes of storage the store has.
    ///
    /// **Zero in 4a.** Reading a filesystem's size needs `statvfs`, which `std` does not expose
    /// and which no crate on the allowlist provides without compiling C. `TODO(phase-4d)`: the
    /// balance operators are the first thing that needs a real number, and getting one within the
    /// dependency policy needs an ADR of its own.
    pub capacity: u64,
    /// Bytes still free. Zero in 4a, for the reason [`StoreHeartbeat::capacity`] gives.
    pub available: u64,
    /// Regions with a peer on this store.
    pub region_count: u64,
    /// Regions this store leads.
    pub leader_count: u64,
    /// Bytes of user data this store holds — the sum of what each of its regions reports, with
    /// the same limits ([`crate::split::approximate_size`]).
    pub applied_bytes: u64,
}

/// What one region's **leader** reports, every 60 s or on a change (`docs/DESIGN.md` §6).
///
/// Only a leader sends one. A follower's view of its own region is exactly the leader's view one
/// round trip ago, so a report from every peer would triple the traffic to say the same thing —
/// and PD would have to decide which copy to believe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionHeartbeat {
    /// The range, the peers and the epoch: everything a client needs to address the region.
    pub region: Region,
    /// The peer sending this, which is the leader.
    pub leader_peer_id: u64,
    /// Its Raft term.
    pub term: u64,
    /// Approximate bytes of user data, with the limits [`crate::split::approximate_size`] spells
    /// out. It is what a split triggers on and what PD compares across stores.
    pub approximate_size: u64,
    /// Its apply index, so 4c can rebuild its in-flight operator view from heartbeats alone
    /// after a PD restart (`docs/plans/phase-4.md` §6, race 5).
    pub applied_index: u64,
}

/// Where a key lives, as PD answers it (`docs/DESIGN.md` §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionRoute {
    /// The region covering the key.
    pub region: Region,
    /// The peer PD last heard was leading it; `0` for "no opinion", which is what the wire's
    /// peer hints already use.
    pub leader_peer_id: u64,
    /// The addresses of the stores hosting the region's peers. A peer whose store PD has never
    /// heard of is absent rather than present with an empty address.
    pub stores: Vec<(u64, String)>,
}

/// The placement driver's five store-facing methods (`docs/DESIGN.md` §7, §9).
///
/// Synchronous, because everything that calls it is: the bootstrap runs before the store serves
/// anything, and the heartbeats run on a blocking thread. The wire implementation the
/// placement-driver lane provides will put its own runtime behind this.
pub trait PdClient: Send + Sync + fmt::Debug {
    /// Registers this store, and asks whether it is the one that creates region 1.
    fn bootstrap(&self, store: &StoreInfo) -> Result<Bootstrapped, ProtoError>;

    /// Reserves a block of `count` cluster-unique ids, returning the first.
    ///
    /// A block rather than an id: new regions and their peers are allocated together, and a
    /// round trip per id would put PD on the split path.
    fn alloc_id(&self, count: u64) -> Result<u64, ProtoError>;

    /// The region covering `key`, or `None` if the cluster has none — which is a routing failure,
    /// not a missing key.
    fn get_region(&self, key: &[u8]) -> Result<Option<RegionRoute>, ProtoError>;

    /// Reports this store's capacity and load.
    fn store_heartbeat(&self, beat: &StoreHeartbeat) -> Result<(), ProtoError>;

    /// Reports one region, from its leader.
    fn region_heartbeat(&self, beat: &RegionHeartbeat) -> Result<(), ProtoError>;
}

/// A placement driver in a `BTreeMap`: the whole of [`PdClient`], in memory, in one process.
///
/// It exists so the store's bootstrap and heartbeat rules can be tested by driving a tick counter
/// instead of waiting ten seconds for a socket. It keeps every heartbeat it is sent, in order, so
/// a test asserts the *content* of what a store reported rather than that something was reported.
///
/// It is not a stub: bootstrap really is once-per-cluster, ids really are never reused, and
/// `get_region` really answers from the regions it has been told about. A fake that answered
/// anything would test nothing.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
pub struct FakePd {
    state: Mutex<FakeState>,
}

#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
struct FakeState {
    cluster_id: u64,
    next_id: u64,
    stores: BTreeMap<u64, String>,
    /// Regions PD knows about, by start key, so a lookup is the same walk a client's cache does.
    regions: BTreeMap<bytes::Bytes, Region>,
    leaders: BTreeMap<u64, u64>,
    store_beats: Vec<StoreHeartbeat>,
    region_beats: Vec<RegionHeartbeat>,
}

#[cfg(any(test, feature = "testing"))]
impl FakePd {
    /// A placement driver with no cluster: the first `bootstrap` mints one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every store heartbeat it has been sent, in order.
    #[must_use]
    pub fn store_beats(&self) -> Vec<StoreHeartbeat> {
        self.lock().store_beats.clone()
    }

    /// Every region heartbeat it has been sent, in order.
    #[must_use]
    pub fn region_beats(&self) -> Vec<RegionHeartbeat> {
        self.lock().region_beats.clone()
    }

    /// Forgets the heartbeats so far, so a test can assert about one window.
    pub fn clear_beats(&self) {
        let mut state = self.lock();
        state.store_beats.clear();
        state.region_beats.clear();
    }

    /// Tells this PD about a region without a heartbeat, so `get_region` can answer for it.
    ///
    /// Keyed by start key, so placing a region replaces whatever began where it begins — which is
    /// how a test narrows the bootstrap region and leaves a gap above it.
    pub fn place(&self, region: Region) {
        self.lock().regions.insert(region.start_key.clone(), region);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(any(test, feature = "testing"))]
impl PdClient for FakePd {
    fn bootstrap(&self, store: &StoreInfo) -> Result<Bootstrapped, ProtoError> {
        if store.store_id == 0 {
            return Err(ProtoError::invalid("store id zero is not a store"));
        }
        let mut state = self.lock();
        state.stores.insert(store.store_id, store.address.clone());
        if state.cluster_id != 0 {
            return Ok(Bootstrapped {
                cluster_id: state.cluster_id,
                region: None,
            });
        }
        // The first store to register mints the cluster and receives region 1, covering
        // everything (`docs/DESIGN.md` §7).
        state.cluster_id = 1;
        state.next_id = 1;
        let region = Region::bootstrap(1, store.store_id, 1);
        state.next_id = 2;
        state
            .regions
            .insert(region.start_key.clone(), region.clone());
        Ok(Bootstrapped {
            cluster_id: state.cluster_id,
            region: Some(region),
        })
    }

    fn alloc_id(&self, count: u64) -> Result<u64, ProtoError> {
        if count == 0 {
            return Err(ProtoError::invalid("a block of zero ids is not a block"));
        }
        let mut state = self.lock();
        if state.cluster_id == 0 {
            return Err(ProtoError::internal("the cluster is not bootstrapped"));
        }
        let first = state.next_id.max(1);
        state.next_id = first + count;
        Ok(first)
    }

    fn get_region(&self, key: &[u8]) -> Result<Option<RegionRoute>, ProtoError> {
        let state = self.lock();
        let Some((_, region)) = state
            .regions
            .range(..=bytes::Bytes::copy_from_slice(key))
            .next_back()
            .filter(|(_, region)| region.contains(key))
        else {
            return Ok(None);
        };
        let stores = region
            .peers
            .iter()
            .filter_map(|peer| {
                state
                    .stores
                    .get(&peer.store_id)
                    .map(|address| (peer.store_id, address.clone()))
            })
            .collect();
        Ok(Some(RegionRoute {
            region: region.clone(),
            leader_peer_id: state.leaders.get(&region.id).copied().unwrap_or(0),
            stores,
        }))
    }

    fn store_heartbeat(&self, beat: &StoreHeartbeat) -> Result<(), ProtoError> {
        self.lock().store_beats.push(*beat);
        Ok(())
    }

    fn region_heartbeat(&self, beat: &RegionHeartbeat) -> Result<(), ProtoError> {
        let mut state = self.lock();
        state
            .regions
            .insert(beat.region.start_key.clone(), beat.region.clone());
        state.leaders.insert(beat.region.id, beat.leader_peer_id);
        state.region_beats.push(beat.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FakePd, PdClient, RegionHeartbeat, StoreHeartbeat, StoreInfo};
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, Region};

    fn info(store_id: u64) -> StoreInfo {
        StoreInfo {
            store_id,
            address: format!("127.0.0.1:{}", 7000 + store_id),
        }
    }

    /// Exactly one store in the life of a cluster creates region 1. Every other call — a second
    /// store, or this one's own restart — is told the cluster exists and looks to its own disk.
    #[test]
    fn exactly_one_bootstrap_creates_a_region() {
        let pd = FakePd::new();
        let first = pd.bootstrap(&info(1)).unwrap();
        let region = first.region.expect("the first store creates region 1");
        assert_eq!(region.id, 1);
        assert_eq!(region.start_key, Bytes::new());
        assert_eq!(region.end_key, Bytes::new(), "it covers everything");
        assert_eq!(region.epoch, Epoch::INITIAL);

        for store in [2, 3, 1] {
            let later = pd.bootstrap(&info(store)).unwrap();
            assert_eq!(later.cluster_id, first.cluster_id);
            assert!(
                later.region.is_none(),
                "store {store} was told to bootstrap"
            );
        }
    }

    #[test]
    fn id_blocks_are_never_handed_out_twice() {
        let pd = FakePd::new();
        assert!(pd.alloc_id(1).is_err(), "before the cluster exists");
        pd.bootstrap(&info(1)).unwrap();

        let mut seen: Vec<u64> = Vec::new();
        for count in [1, 4, 1, 16] {
            let first = pd.alloc_id(count).unwrap();
            for id in first..first + count {
                assert!(!seen.contains(&id), "id {id} was handed out twice");
                seen.push(id);
            }
        }
        assert!(pd.alloc_id(0).is_err(), "a block of zero is not a block");
        assert!(
            seen.iter().all(|id| *id > 1),
            "region 1's id is taken by the bootstrap"
        );
    }

    #[test]
    fn a_lookup_answers_from_the_regions_it_has_been_told_about() {
        let pd = FakePd::new();
        pd.bootstrap(&info(1)).unwrap();
        assert_eq!(
            pd.get_region(b"anything").unwrap().map(|r| r.region.id),
            Some(1),
            "the bootstrap region covers everything"
        );

        // Two regions, as a split would leave them, and a leader learned from a heartbeat.
        let left = Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::from_static(b"m"),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::new(1, 2),
        };
        let right = Region {
            id: 2,
            start_key: Bytes::from_static(b"m"),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, 2)],
            epoch: Epoch::new(1, 2),
        };
        pd.place(left);
        pd.region_heartbeat(&RegionHeartbeat {
            region: right,
            leader_peer_id: 2,
            term: 4,
            approximate_size: 0,
            applied_index: 9,
        })
        .unwrap();

        let low = pd.get_region(b"a").unwrap().expect("covered");
        assert_eq!(low.region.id, 1);
        assert_eq!(low.leader_peer_id, 0, "nothing has reported leading it");
        assert_eq!(low.stores, vec![(1, "127.0.0.1:7001".to_owned())]);

        let high = pd.get_region(b"z").unwrap().expect("covered");
        assert_eq!(high.region.id, 2);
        assert_eq!(high.leader_peer_id, 2, "learned from the heartbeat");
    }

    /// A key no region covers is a routing failure with an answer — `Ok(None)` — rather than an
    /// error, and never the region before it.
    #[test]
    fn a_key_no_region_covers_is_none() {
        let pd = FakePd::new();
        pd.bootstrap(&info(1)).unwrap();
        // Narrow region 1 to `["", "m")`, leaving the top of the key space uncovered. That is
        // what a client sees between a split applying and PD hearing about the right half.
        pd.place(Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::from_static(b"m"),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::new(1, 2),
        });
        assert_eq!(pd.get_region(b"a").unwrap().map(|r| r.region.id), Some(1));
        assert!(
            pd.get_region(b"m").unwrap().is_none(),
            "the region below must not answer for a key it does not reach"
        );
        assert!(pd.get_region(b"\xff").unwrap().is_none());
    }

    #[test]
    fn every_heartbeat_is_kept_in_order() {
        let pd = FakePd::new();
        pd.bootstrap(&info(1)).unwrap();
        for regions in [1_u64, 2, 3] {
            pd.store_heartbeat(&StoreHeartbeat {
                store_id: 1,
                region_count: regions,
                ..StoreHeartbeat::default()
            })
            .unwrap();
        }
        assert_eq!(
            pd.store_beats()
                .iter()
                .map(|beat| beat.region_count)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );

        pd.clear_beats();
        assert!(pd.store_beats().is_empty());
        assert!(pd.region_beats().is_empty());
    }
}
