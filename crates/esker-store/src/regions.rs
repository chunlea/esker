//! Every region this store hosts: the map a request is routed through.
//!
//! Phase 3e had one region and could hold it in a field. A store now hosts many, and the two
//! questions the request path asks of them are different enough to need two indexes:
//!
//! * **"which region is this request for?"** — by id, because that is what a request header
//!   names, and answering by range instead would let a client with a stale cache be served by a
//!   region it did not mean;
//! * **"what should I tell a client whose routing is wrong?"** — by range, because the answer is
//!   *every local region overlapping what it asked for* ([`RegionMap::route`]).
//!
//! # The rule that makes the second index earn itself
//!
//! `EpochNotMatch` carries `current_regions`, and it must carry **all** of them. A client whose
//! cached region has just split is asking for a range that is now two regions; replying with only
//! the one whose id it named teaches it half of what it needs and costs a `GetRegion` round trip
//! for the other half. Under phase 4b's split storm that is a round trip per stale request against
//! a single placement driver, which is how the placement driver melts. The payload is asserted by
//! `a_stale_epoch_is_answered_with_every_overlapping_region` in this module's tests, not assumed.
//!
//! # What a store may not hold
//!
//! Two peers of one region on one store is the invariant `prompts/04-multiraft-pd.md` asks the
//! simulator to check after every event. Here it is structural — the map is keyed by region id, so
//! a second peer for a region is a typed failure at insert rather than a state to detect later.
//! Overlapping ranges are refused for the same reason: regions tile the key space, and a store
//! that holds two claims to one key cannot answer a routing question honestly.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use esker_proto::{ProtoError, RawKvReq, Region, RequestHeader};

use crate::error::{Result, StoreError};
use crate::peer::RaftPeer;
use crate::region::RegionMeta;

/// One region as this store hosts it: its metadata, and its Raft peer when it replicates.
///
/// Held behind an `Arc` so the request path can take one out of the map and drop the map's lock
/// before doing any engine work. In 4a a state is immutable once inserted — the epoch only moves
/// on a split or a membership change, and neither exists yet. `TODO(phase-4b)`: a split replaces
/// two entries under one lock, and the replacement is what the epoch bump *is*.
#[derive(Debug)]
pub struct RegionState {
    meta: RegionMeta,
    peer: Option<Arc<RaftPeer>>,
}

impl RegionState {
    /// A region with no replication: the phase-2 store, which writes straight to the engine.
    #[must_use]
    pub fn unreplicated(meta: RegionMeta) -> Self {
        Self { meta, peer: None }
    }

    /// A region replicated by `peer`.
    #[must_use]
    pub fn replicated(meta: RegionMeta, peer: Arc<RaftPeer>) -> Self {
        Self {
            meta,
            peer: Some(peer),
        }
    }

    /// Its metadata, and the ownership checks that hang off it.
    #[must_use]
    pub fn meta(&self) -> &RegionMeta {
        &self.meta
    }

    /// The region itself, as it goes on the wire.
    #[must_use]
    pub fn region(&self) -> &Region {
        self.meta.region()
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.meta.id()
    }

    /// Its Raft peer, when this store replicates it.
    #[must_use]
    pub fn peer(&self) -> Option<&Arc<RaftPeer>> {
        self.peer.as_ref()
    }
}

/// Every region this store hosts, indexed by id and by range.
#[derive(Debug, Default)]
pub struct RegionMap {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    by_id: BTreeMap<u64, Arc<RegionState>>,
    /// `start_key → region id`. Keyed by the **start**, not the end: an empty `end_key` means the
    /// end of the key space but sorts *below* every key, so a map keyed by `end_key` puts the last
    /// region of the cluster first and never finds it again.
    by_start: BTreeMap<Bytes, u64>,
}

impl RegionMap {
    /// An empty map: a store that hosts nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a region this store now hosts.
    ///
    /// Refuses a region id already present — *two peers of one region on one store* — and one
    /// whose range overlaps a region already held. Both are refusals rather than replacements,
    /// because either would mean this store had answered a routing question wrongly already and
    /// the honest move is to say so at bootstrap rather than to serve from a map that cannot be
    /// right.
    pub fn insert(&self, state: RegionState) -> Result<Arc<RegionState>> {
        let mut inner = self.write();
        let region = state.region().clone();
        if inner.by_id.contains_key(&region.id) {
            return Err(StoreError::RegionConflict(format!(
                "region {} is already on this store; a store never holds two peers of one region",
                region.id
            )));
        }
        // Cloned out so the immutable borrow of `inner` ends before the insert below.
        let conflict = inner
            .overlapping(&region.start_key, &region.end_key)
            .next()
            .map(|state| state.region().clone());
        if let Some(other) = conflict {
            return Err(StoreError::RegionConflict(format!(
                "region {} [{:?}, {:?}) overlaps region {} [{:?}, {:?}) already on this store",
                region.id,
                region.start_key,
                region.end_key,
                other.id,
                other.start_key,
                other.end_key,
            )));
        }
        let state = Arc::new(state);
        inner.by_start.insert(region.start_key.clone(), region.id);
        inner.by_id.insert(region.id, Arc::clone(&state));
        Ok(state)
    }

    /// The region with this id, if this store hosts it.
    #[must_use]
    pub fn get(&self, region_id: u64) -> Option<Arc<RegionState>> {
        self.read().by_id.get(&region_id).cloned()
    }

    /// Replaces a region with its two halves, in one step.
    ///
    /// The parent keeps its id, its start key and its **peer** — its Raft group is unchanged by a
    /// split — and gives up everything from the child's start key up. The child arrives with its
    /// own peer already built.
    ///
    /// Both changes happen under one write lock, which is the whole reason this is not two calls.
    /// A reader between them would see the parent still claiming what the child now owns, and
    /// "regions tile the key space" would be false for as long as that took — an invariant that
    /// holds *except briefly* is not one a routing decision can be made against.
    pub fn apply_split(&self, parent: Region, child: RegionState) -> Result<()> {
        let mut inner = self.write();
        let Some(existing) = inner.by_id.get(&parent.id).cloned() else {
            return Err(StoreError::RegionConflict(format!(
                "region {} split, but this store does not host it",
                parent.id
            )));
        };
        if inner.by_id.contains_key(&child.id()) {
            return Err(StoreError::RegionConflict(format!(
                "region {} split into {}, which this store already hosts",
                parent.id,
                child.id()
            )));
        }
        if existing.region().start_key != parent.start_key {
            return Err(StoreError::RegionConflict(format!(
                "a split moved region {}'s start key, which a split never does",
                parent.id
            )));
        }

        let child_start = child.region().start_key.clone();
        let child_id = child.id();
        inner.by_id.insert(
            parent.id,
            Arc::new(RegionState {
                meta: RegionMeta::new(parent),
                peer: existing.peer.clone(),
            }),
        );
        inner.by_start.insert(child_start, child_id);
        inner.by_id.insert(child_id, Arc::new(child));
        Ok(())
    }

    /// Replaces a region's metadata, keeping its peer and its place in the range index.
    ///
    /// For a change that moves the *epoch and the membership* and not the range — a conf change.
    /// A range move is a split and goes through [`RegionMap::apply_split`], which has two entries
    /// to keep consistent rather than one.
    pub fn replace(&self, region: Region) -> Result<()> {
        let mut inner = self.write();
        let Some(existing) = inner.by_id.get(&region.id).cloned() else {
            return Err(StoreError::RegionConflict(format!(
                "region {} changed, but this store does not host it",
                region.id
            )));
        };
        if existing.region().start_key != region.start_key
            || existing.region().end_key != region.end_key
        {
            return Err(StoreError::RegionConflict(format!(
                "region {}'s range moved without a split, which nothing may do",
                region.id
            )));
        }
        inner.by_id.insert(
            region.id,
            Arc::new(RegionState {
                meta: RegionMeta::new(region),
                peer: existing.peer.clone(),
            }),
        );
        Ok(())
    }

    /// Drops a region this store no longer hosts, returning what it held.
    ///
    /// Nothing in 4a calls it; `TODO(phase-4c)` is the `RemovePeer` operator, and its hazard is
    /// already named in `docs/plans/phase-4.md` §6 race 3 — a store that crashes between the
    /// removal and the deletion of the data must not restart into serving the region again.
    pub fn remove(&self, region_id: u64) -> Option<Arc<RegionState>> {
        let mut inner = self.write();
        let state = inner.by_id.remove(&region_id)?;
        inner.by_start.remove(&state.region().start_key);
        Some(state)
    }

    /// The region covering `key`, if this store hosts one.
    #[must_use]
    pub fn find(&self, key: &[u8]) -> Option<Arc<RegionState>> {
        self.read().find(key)
    }

    /// Every region this store hosts that shares a key with `[start, end)`, in key order.
    ///
    /// An empty `end` is the end of the key space, as everywhere a region range is compared.
    #[must_use]
    pub fn overlapping(&self, start: &[u8], end: &[u8]) -> Vec<Region> {
        self.read()
            .overlapping(start, end)
            .map(|state| state.region().clone())
            .collect()
    }

    /// Every region this store hosts, in key order.
    #[must_use]
    pub fn regions(&self) -> Vec<Region> {
        self.overlapping(b"", b"")
    }

    /// Every region's state, in id order.
    #[must_use]
    pub fn states(&self) -> Vec<Arc<RegionState>> {
        self.read().by_id.values().cloned().collect()
    }

    /// How many regions this store hosts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().by_id.len()
    }

    /// Whether it hosts none — which is what a freshly created database looks like.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().by_id.is_empty()
    }

    /// Routes one request: the region it names, at the epoch it claims, on a peer this store has.
    ///
    /// The order of the three refusals is the interesting part.
    ///
    /// 1. **A region id this store does not host** is [`ProtoError::RegionNotFound`]. The client
    ///    is at the wrong store entirely, and this store has nothing useful to say about where
    ///    the right one is — it may never have hosted that region.
    /// 2. **The right region at the wrong epoch** is [`ProtoError::EpochNotMatch`] carrying
    ///    *every* local region overlapping what the request asked for, so one round trip repairs
    ///    the client's cache rather than starting a guessing game. See the module docs.
    /// 3. **A header naming a peer this region does not have** is [`ProtoError::NotLeader`] with a
    ///    peer that exists as the hint. `header.peer == 0` means "no opinion about the leader",
    ///    which is the truth for a client that has just connected, and is accepted.
    ///
    /// The key-range check is deliberately *not* here: it belongs to the region that was found, it
    /// runs after the epoch is agreed, and where it runs differs between the direct and the
    /// replicated path ([`crate::region::RegionMeta::check_scope`]).
    pub fn route(
        &self,
        header: &RequestHeader,
        request: Option<&RawKvReq>,
    ) -> std::result::Result<Arc<RegionState>, ProtoError> {
        let inner = self.read();
        let Some(state) = inner.by_id.get(&header.region_id).cloned() else {
            return Err(ProtoError::RegionNotFound {
                region_id: header.region_id,
            });
        };
        if header.epoch != state.region().epoch {
            // With no request to bound the answer, the region that was asked for is the answer:
            // a header alone says nothing about which keys the caller wanted.
            let (start, end) = request.map_or_else(
                || {
                    let region = state.region();
                    (region.start_key.clone(), region.end_key.clone())
                },
                crate::region::request_range,
            );
            let current_regions = inner
                .overlapping(&start, &end)
                .map(|state| state.region().clone())
                .collect();
            return Err(ProtoError::EpochNotMatch { current_regions });
        }
        drop(inner);
        state.meta.check_peer(header.peer)?;
        Ok(state)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Inner {
    /// The region covering `key`: the last one starting at or before it, if it actually reaches.
    ///
    /// The second half is not redundant. A store need not hold a contiguous slice of the key
    /// space — it holds whichever regions PD placed on it — so a key between two of them belongs
    /// to neither, and returning the one before it would serve a request for a range this store
    /// does not own (`CLAUDE.md` invariant 5).
    fn find(&self, key: &[u8]) -> Option<Arc<RegionState>> {
        let (_, id) = self
            .by_start
            .range(..=Bytes::copy_from_slice(key))
            .next_back()?;
        let state = self.by_id.get(id)?;
        state.region().contains(key).then(|| Arc::clone(state))
    }

    /// Every region sharing a key with `[start, end)`, in key order.
    ///
    /// Walks from the region that could contain `start` — the last one starting at or before it —
    /// and stops at the first region that begins at or after `end`. The scan cannot start at
    /// `start` itself: a region beginning below it may still reach past it, and skipping that one
    /// is exactly the region a client straddling a split boundary needs.
    fn overlapping<'a>(
        &'a self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> impl Iterator<Item = &'a Arc<RegionState>> + 'a {
        let from = self
            .by_start
            .range(..=Bytes::copy_from_slice(start))
            .next_back()
            .map_or_else(Bytes::new, |(at, _)| at.clone());
        self.by_start
            .range(from..)
            .filter_map(|(_, id)| self.by_id.get(id))
            .take_while(move |state| end.is_empty() || &state.region().start_key[..] < end)
            .filter(move |state| state.region().overlaps(start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::{RegionMap, RegionState};
    use crate::region::RegionMeta;
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, ProtoError, RawKvReq, Region, RequestHeader};

    fn region(id: u64, start: &[u8], end: &[u8]) -> Region {
        Region {
            id,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, id)],
            epoch: Epoch::INITIAL,
        }
    }

    fn map(regions: impl IntoIterator<Item = Region>) -> RegionMap {
        let map = RegionMap::new();
        for region in regions {
            map.insert(RegionState::unreplicated(RegionMeta::new(region)))
                .unwrap();
        }
        map
    }

    fn three() -> RegionMap {
        map([
            region(1, b"", b"g"),
            region(2, b"g", b"q"),
            region(3, b"q", b""),
        ])
    }

    fn ids(regions: &[Region]) -> Vec<u64> {
        regions.iter().map(|region| region.id).collect()
    }

    #[test]
    fn a_key_finds_the_region_that_covers_it() {
        let map = three();
        for (key, expected) in [
            (&b""[..], 1),
            (b"a", 1),
            (b"f\xff", 1),
            (b"g", 2),
            (b"p", 2),
            (b"q", 3),
            (b"\xff\xff\xff", 3),
        ] {
            assert_eq!(map.find(key).map(|s| s.id()), Some(expected), "key {key:?}");
        }
    }

    /// A store holds whichever regions the placement driver put on it, not a contiguous slice of
    /// the key space. A key between two of them belongs to neither, and answering with the region
    /// before it would serve a range this store does not own.
    #[test]
    fn a_key_in_a_gap_belongs_to_no_region() {
        let map = map([region(1, b"", b"g"), region(3, b"q", b"")]);
        assert!(map.find(b"j").is_none());
        assert!(map.find(b"g").is_none());
        assert_eq!(map.find(b"f").map(|s| s.id()), Some(1));
        assert_eq!(map.find(b"q").map(|s| s.id()), Some(3));
    }

    #[test]
    fn overlapping_finds_every_region_a_range_touches() {
        let map = three();
        assert_eq!(ids(&map.overlapping(b"", b"")), [1, 2, 3], "everything");
        assert_eq!(ids(&map.overlapping(b"a", b"b")), [1]);
        assert_eq!(ids(&map.overlapping(b"f", b"h")), [1, 2], "across a border");
        assert_eq!(ids(&map.overlapping(b"g", b"q")), [2], "exactly one region");
        assert_eq!(ids(&map.overlapping(b"p", b"")), [2, 3], "unbounded above");
        assert_eq!(ids(&map.overlapping(b"z", b"")), [3]);
        assert_eq!(ids(&map.overlapping(b"h", b"h")), [2], "a point");
        assert_eq!(ids(&map.regions()), [1, 2, 3]);
    }

    /// The walk must start *below* the range, not at it: the region a key straddling a split
    /// boundary needs is the one that begins before the key and reaches past it.
    #[test]
    fn a_range_starting_inside_a_region_still_finds_that_region() {
        let map = three();
        assert_eq!(
            ids(&map.overlapping(b"m", b"z")),
            [2, 3],
            "region 2 starts at `g`, below the range, and reaches into it"
        );
    }

    /// Two peers of one region on one store is the invariant the simulator checks after every
    /// event. Keying by region id makes it structural: the second insert is refused.
    #[test]
    fn a_store_never_holds_two_peers_of_one_region() {
        let map = map([region(1, b"", b"g")]);
        let error = map
            .insert(RegionState::unreplicated(RegionMeta::new(region(
                1, b"m", b"n",
            ))))
            .unwrap_err();
        assert!(
            error.to_string().contains("already on this store"),
            "{error}"
        );
        assert_eq!(map.len(), 1);
    }

    /// Regions tile the key space. A store holding two claims to one key cannot answer a routing
    /// question honestly, so the overlap is refused at insert rather than detected later.
    #[test]
    fn overlapping_ranges_are_refused() {
        let map = map([region(1, b"g", b"q")]);
        for (start, end) in [
            (&b""[..], &b""[..]),
            (b"", b"h"),
            (b"h", b"z"),
            (b"h", b"i"),
            (b"g", b"q"),
            (b"p", b""),
        ] {
            let error = map
                .insert(RegionState::unreplicated(RegionMeta::new(region(
                    9, start, end,
                ))))
                .unwrap_err();
            assert!(
                error.to_string().contains("overlaps"),
                "{start:?} {end:?}: {error}"
            );
        }
        // A range that merely touches a boundary shares no key and is fine.
        map.insert(RegionState::unreplicated(RegionMeta::new(region(
            2, b"", b"g",
        ))))
        .unwrap();
        map.insert(RegionState::unreplicated(RegionMeta::new(region(
            3, b"q", b"",
        ))))
        .unwrap();
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn a_region_id_this_store_does_not_host_is_not_an_epoch_problem() {
        let map = three();
        let error = map
            .route(&RequestHeader::new(7, Epoch::INITIAL, 0), None)
            .unwrap_err();
        assert_eq!(error, ProtoError::RegionNotFound { region_id: 7 });
    }

    /// The headline of 4a. A client whose cached region has split asks for a range that is now
    /// several regions; an answer naming only the region it asked about costs a `GetRegion` round
    /// trip per stale request, and phase 4b's split storm melts the placement driver with them.
    #[test]
    fn a_stale_epoch_is_answered_with_every_overlapping_region() {
        let map = three();
        // The client still believes region 1 is `["", "")` — the pre-split world — and scans all
        // of it. Every region that range now touches must come back.
        let header = RequestHeader::new(1, Epoch::new(1, 0), 0);
        let scan = RawKvReq::scan(&b""[..], &b""[..], 0);
        let error = map.route(&header, Some(&scan)).unwrap_err();
        match error {
            ProtoError::EpochNotMatch { current_regions } => {
                assert_eq!(ids(&current_regions), [1, 2, 3]);
            }
            other => panic!("{other:?}"),
        }

        // A point request only needs the region holding its key, and sending three would be
        // three regions the client did not ask about.
        let error = map
            .route(&header, Some(&RawKvReq::get(&b"a"[..])))
            .unwrap_err();
        match error {
            ProtoError::EpochNotMatch { current_regions } => {
                assert_eq!(ids(&current_regions), [1]);
            }
            other => panic!("{other:?}"),
        }

        // And with no request to bound it, the region that was asked for.
        let error = map.route(&header, None).unwrap_err();
        match error {
            ProtoError::EpochNotMatch { current_regions } => {
                assert_eq!(ids(&current_regions), [1]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Both counters move on different events, so a client can be stale in one and current in
    /// the other, and being *ahead* is as wrong as being behind.
    #[test]
    fn every_epoch_mismatch_is_refused_in_either_direction() {
        let map = map([region(1, b"", b"")]);
        let current = Epoch::INITIAL;
        for wrong in [
            Epoch::new(current.conf_ver - 1, current.version),
            Epoch::new(current.conf_ver, current.version - 1),
            Epoch::new(current.conf_ver + 1, current.version),
            Epoch::new(current.conf_ver, current.version + 1),
        ] {
            let error = map
                .route(&RequestHeader::new(1, wrong, 0), None)
                .unwrap_err();
            assert!(
                matches!(error, ProtoError::EpochNotMatch { .. }),
                "{wrong:?} gave {error:?}"
            );
        }
        map.route(&RequestHeader::new(1, current, 0), None).unwrap();
    }

    /// Peer 0 is "no opinion about the leader", which is the truth for a freshly connected
    /// client; a peer this region does not have gets one that exists as a hint.
    #[test]
    fn the_peer_in_a_header_is_checked_against_the_region_that_owns_it() {
        let map = three();
        // `region()` gives region N the peer id N.
        map.route(&RequestHeader::new(2, Epoch::INITIAL, 0), None)
            .unwrap();
        map.route(&RequestHeader::new(2, Epoch::INITIAL, 2), None)
            .unwrap();

        let error = map
            .route(&RequestHeader::new(2, Epoch::INITIAL, 99), None)
            .unwrap_err();
        assert_eq!(
            error,
            ProtoError::NotLeader {
                region_id: 2,
                leader_hint: Some(2),
            }
        );

        // A peer belonging to a *different* region on this same store is still not this
        // region's, which is the mistake a store-wide peer list would make.
        let error = map
            .route(&RequestHeader::new(2, Epoch::INITIAL, 1), None)
            .unwrap_err();
        assert!(matches!(error, ProtoError::NotLeader { .. }), "{error:?}");
    }

    #[test]
    fn a_removed_region_is_gone_from_both_indexes() {
        let map = three();
        let removed = map.remove(2).expect("region 2 was there");
        assert_eq!(removed.id(), 2);
        assert_eq!(map.len(), 2);
        assert!(map.find(b"h").is_none(), "its keys route nowhere now");
        assert_eq!(ids(&map.overlapping(b"", b"")), [1, 3]);
        assert!(map.remove(2).is_none(), "removing it twice is not a panic");

        // And its range is free again.
        map.insert(RegionState::unreplicated(RegionMeta::new(region(
            4, b"g", b"q",
        ))))
        .unwrap();
        assert_eq!(map.find(b"h").map(|s| s.id()), Some(4));
    }

    #[test]
    fn an_empty_map_is_what_a_fresh_database_looks_like() {
        let map = RegionMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert!(map.find(b"anything").is_none());
        assert!(map.regions().is_empty());
        assert_eq!(
            map.route(&RequestHeader::new(1, Epoch::INITIAL, 0), None)
                .unwrap_err(),
            ProtoError::RegionNotFound { region_id: 1 }
        );
    }
}
