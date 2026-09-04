//! Where the client thinks each key lives.
//!
//! The cache is a **hint, never an authority** (`CLAUDE.md` invariant 5). Every request built
//! from it carries the epoch it believes, and the store checks that epoch; a stale entry
//! therefore costs a redirect and never a wrong answer. That is what lets the cache be
//! optimistic — it is repaired by the errors it causes.
//!
//! # Keyed by `start_key`, walked backwards
//!
//! An empty `end_key` means unbounded, so `b""` as an upper bound sorts *before* every key
//! rather than after it — a map keyed by `end_key`, which is the obvious choice and the one
//! `TiKV` uses with a sentinel maximum it does not have here, gets the last region of the cluster wrong
//! for ever. This one is keyed by `start_key` and walks back to the last region starting at or
//! before the key, then checks that the region actually reaches it. Same lookup, same cost, no
//! such case (`docs/plans/phase-4.md` §10).
//!
//! # The refresh hook
//!
//! A miss asks a [`RegionResolver`], which is `GetRegion(key) → Region + leader hint` from
//! `docs/DESIGN.md` §7 with the network taken out. [`StaticRegion`] answers for one region and
//! [`RegionTable`] for a routing table; the placement driver's client answers for a cluster, and
//! nothing above the trait changes when it does.
//!
//! **`Ok(None)` and `Err` are different answers and the difference matters.** `Ok(None)` is *no
//! region covers this key* — a routing failure the caller reports and does not retry. `Err` is
//! *the placement driver could not say*, which is retryable, and collapsing it into `Ok(None)`
//! would turn a momentary PD outage into a terminal error on every call in the process.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::RwLock;

use bytes::Bytes;

use crate::wire::{Epoch, Peer, ProtoError, Region};

/// A region and the peer the client currently believes leads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The region.
    pub region: Region,
    /// Where to send the next request; `None` means "no idea, try any peer".
    pub leader: Option<Peer>,
}

impl Route {
    /// The peer a request should go to: the believed leader, or any peer to ask.
    ///
    /// Asking a follower is not wasted — it answers `NotLeader` with a hint, which is how the
    /// cache learns. Returning `None` only happens for a region with no peers at all, which
    /// is a malformed routing answer rather than a state to route around.
    ///
    /// The leader is stored as a whole [`Peer`] rather than an id because that is what a
    /// request header needs; `NotLeader` hints arrive as peer ids and are resolved against
    /// this region's peer list by [`RegionCache::set_leader`].
    #[must_use]
    pub fn target(&self) -> Option<&Peer> {
        // `TODO(debt-c6 #3)`: prefer a peer that has not just failed rather than always the
        // first one, so a down store is not asked twice in one retry budget
        // (`docs/plans/debt-c6.md` §4).
        self.leader.as_ref().or_else(|| self.region.peers.first())
    }
}

/// `GetRegion(key)`, with the network taken out (`docs/DESIGN.md` §7).
pub trait RegionResolver: fmt::Debug + Send + Sync {
    /// Finds the region owning `key`, and the leader hint that came with it.
    ///
    /// `Ok(None)` means no region covers the key, which the caller turns into
    /// [`crate::Error::NoRegion`]. It is a routing failure, not a missing key, and not something
    /// waiting fixes.
    ///
    /// `Err` means the placement driver could not answer — it is unreachable, or busy, or this
    /// process is not bootstrapped yet. That is a different thing, it is often retryable, and the
    /// caller decides which by asking [`ProtoError::is_retryable`]. An implementation that
    /// reported it as `Ok(None)` would make every call in the process fail terminally for as long
    /// as PD was away.
    fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError>;
}

/// The whole key space, one region, one peer — phase 2's answer to every lookup.
#[derive(Debug, Clone)]
pub struct StaticRegion {
    route: Route,
}

impl StaticRegion {
    /// A region `["", "")` with id `region_id`, led by a single peer on `store_id`.
    #[must_use]
    pub fn whole_key_space(region_id: u64, store_id: u64, peer_id: u64) -> Self {
        Self {
            route: Route {
                region: Region::bootstrap(region_id, store_id, peer_id),
                leader: Some(Peer::voter(store_id, peer_id)),
            },
        }
    }

    /// A region `["", "")` replicated by every store in `store_ids`, with no opinion about which
    /// leads.
    ///
    /// The peer list is what makes a redirect work: `NotLeader { leader_hint }` names a *peer*,
    /// and only a region that lists more than one can turn that into a different store to send to.
    /// A one-peer region learns who leads and has nowhere to go with it.
    ///
    /// The peer id is taken to equal the store id, which is what a single-region cluster
    /// bootstraps with. Phase 4's placement driver hands out the real mapping and this goes away.
    #[must_use]
    pub fn replicated(region_id: u64, store_ids: &[u64]) -> Self {
        Self {
            route: Route {
                region: Region {
                    id: region_id,
                    start_key: Bytes::new(),
                    end_key: Bytes::new(),
                    peers: store_ids
                        .iter()
                        .map(|store_id| Peer::voter(*store_id, *store_id))
                        .collect(),
                    epoch: Epoch::INITIAL,
                },
                // No opinion: the first request goes to whichever peer is listed first and, if it
                // is a follower, comes back with the hint that fixes the cache.
                leader: None,
            },
        }
    }

    /// A resolver that always answers with `route`.
    #[must_use]
    pub fn new(route: Route) -> Self {
        Self { route }
    }
}

impl RegionResolver for StaticRegion {
    fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
        Ok(self.route.region.contains(key).then(|| self.route.clone()))
    }
}

/// A routing table: many regions, answered by range — the shape a placement driver replies in.
///
/// A client given a static topology uses it directly; a test uses it to drive the cache's
/// multi-region paths without a placement driver. It is the same lookup [`RegionCache`] performs,
/// deliberately: a resolver that answered by a different rule than the cache it fills would make
/// the cache's correctness depend on which of the two was asked.
#[derive(Debug, Default)]
pub struct RegionTable {
    by_start: BTreeMap<Bytes, Route>,
}

impl RegionTable {
    /// A table covering nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A table over `routes`, whatever order they arrive in.
    #[must_use]
    pub fn from_routes<I: IntoIterator<Item = Route>>(routes: I) -> Self {
        let mut table = Self::new();
        for route in routes {
            table.insert(route);
        }
        table
    }

    /// Adds or replaces the region beginning where `route` begins.
    pub fn insert(&mut self, route: Route) {
        self.by_start.insert(route.region.start_key.clone(), route);
    }

    /// How many regions it covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    /// Whether it covers none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }
}

impl RegionResolver for RegionTable {
    fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
        Ok(self
            .by_start
            .range(..=Bytes::copy_from_slice(key))
            .next_back()
            .filter(|(_, route)| route.region.contains(key))
            .map(|(_, route)| route.clone()))
    }
}

/// Regions the client has learned about, keyed by range.
#[derive(Debug, Default)]
pub struct RegionCache {
    by_start: RwLock<BTreeMap<Bytes, Route>>,
}

impl RegionCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached route for `key`, if one covers it.
    ///
    /// Walks back to the last region starting at or before `key`, then checks that the region
    /// actually reaches it. Without the second half, a key in a gap — which is what a
    /// half-applied split leaves behind — would be routed to the region before it.
    #[must_use]
    pub fn lookup(&self, key: &[u8]) -> Option<Route> {
        let map = self.read();
        let (_, route) = map.range(..=Bytes::copy_from_slice(key)).next_back()?;
        route.region.contains(key).then(|| route.clone())
    }

    /// Adds or replaces a route, dropping anything it overlaps.
    ///
    /// A split turns one cached region into two whose ranges are inside the old one; without
    /// the overlap sweep the parent would stay in the map, shadowing a child on every lookup
    /// that landed in the parent's start.
    pub fn insert(&self, route: Route) {
        let mut map = self.write();
        Self::remove_overlapping(&mut map, &route.region);
        map.insert(route.region.start_key.clone(), route);
    }

    /// Replaces everything overlapping `routes` with `routes`, in one pass.
    pub fn insert_all<I: IntoIterator<Item = Route>>(&self, routes: I) {
        let mut map = self.write();
        for route in routes {
            Self::remove_overlapping(&mut map, &route.region);
            map.insert(route.region.start_key.clone(), route);
        }
    }

    /// Forgets one region, whatever range it held.
    pub fn invalidate(&self, region_id: u64) {
        self.write().retain(|_, route| route.region.id != region_id);
    }

    /// Forgets whichever region covers `key`.
    pub fn invalidate_key(&self, key: &[u8]) {
        let mut map = self.write();
        let start = map
            .range(..=Bytes::copy_from_slice(key))
            .next_back()
            .filter(|(_, route)| route.region.contains(key))
            .map(|(start, _)| start.clone());
        if let Some(start) = start {
            map.remove(&start);
        }
    }

    /// Records where a region's leader is, or that the client no longer knows.
    ///
    /// `peer_id` is what a `NotLeader` hint carries, and it is resolved against the peers this
    /// cache already holds for the region. A hint naming a peer the region does not have is
    /// **dropped rather than trusted**: it means the cached region is out of date, and the
    /// repair for that is to forget the leader and let the next answer teach it, not to route
    /// to a peer whose address nobody knows.
    pub fn set_leader(&self, region_id: u64, peer_id: Option<u64>) {
        let mut map = self.write();
        for route in map.values_mut() {
            if route.region.id != region_id {
                continue;
            }
            route.leader = peer_id.and_then(|peer_id| {
                route
                    .region
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == peer_id)
                    .copied()
            });
        }
    }

    /// How many regions are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Drops everything.
    pub fn clear(&self) {
        self.write().clear();
    }

    fn remove_overlapping(map: &mut BTreeMap<Bytes, Route>, region: &Region) {
        map.retain(|_, cached| !overlaps(&cached.region, region));
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<Bytes, Route>> {
        self.by_start
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<Bytes, Route>> {
        self.by_start
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether two regions share any key. An empty `end_key` is the end of the key space, so
/// every comparison against it has to be written out rather than left to `Ord`.
fn overlaps(left: &Region, right: &Region) -> bool {
    let left_before_right = !left.end_key.is_empty() && left.end_key <= right.start_key;
    let right_before_left = !right.end_key.is_empty() && right.end_key <= left.start_key;
    !(left_before_right || right_before_left)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{RegionCache, RegionResolver, RegionTable, Route, StaticRegion};
    use crate::wire::{Epoch, Peer, ProtoError, Region};

    fn route(id: u64, start: &[u8], end: &[u8], stores: &[u64]) -> Route {
        let peers: Vec<Peer> = stores
            .iter()
            .map(|store| Peer::voter(*store, store * 10))
            .collect();
        Route {
            leader: peers.first().copied(),
            region: Region {
                id,
                start_key: Bytes::copy_from_slice(start),
                end_key: Bytes::copy_from_slice(end),
                peers,
                epoch: Epoch::INITIAL,
            },
        }
    }

    #[test]
    fn a_lookup_finds_the_region_that_covers_the_key() {
        let cache = RegionCache::new();
        cache.insert_all([
            route(1, b"", b"g", &[1]),
            route(2, b"g", b"q", &[2]),
            route(3, b"q", b"", &[3]),
        ]);
        assert_eq!(cache.len(), 3);

        for (key, expected) in [
            (&b""[..], 1),
            (b"a", 1),
            (b"f\xff", 1),
            (b"g", 2),
            (b"p", 2),
            (b"q", 3),
            (b"\xff\xff\xff", 3),
        ] {
            let found = cache.lookup(key).expect("every key is covered");
            assert_eq!(found.region.id, expected, "key {key:?}");
        }
    }

    /// The mistake this structure exists to avoid: the last region's `end_key` is empty, and
    /// an empty upper bound sorts *before* every key.
    #[test]
    fn the_unbounded_last_region_covers_the_top_of_the_key_space() {
        let cache = RegionCache::new();
        cache.insert(route(9, b"m", b"", &[1]));
        assert!(cache.lookup(b"a").is_none());
        assert_eq!(cache.lookup(b"m").map(|r| r.region.id), Some(9));
        assert_eq!(
            cache.lookup(b"\xff\xff\xff\xff").map(|r| r.region.id),
            Some(9)
        );
    }

    /// A key that no cached region reaches must miss, not fall back to the region before it.
    /// A gap is what a half-learned split looks like.
    #[test]
    fn a_key_in_a_gap_misses() {
        let cache = RegionCache::new();
        cache.insert_all([route(1, b"", b"g", &[1]), route(3, b"q", b"", &[3])]);
        assert!(cache.lookup(b"j").is_none(), "a gap must not route");
        assert!(cache.lookup(b"g").is_none());
    }

    /// A split replaces one cached region with two inside its range. If the parent survived,
    /// it would shadow a child on every lookup at the shared start key.
    #[test]
    fn inserting_children_evicts_the_parent_they_came_from() {
        let cache = RegionCache::new();
        cache.insert(route(1, b"", b"", &[1]));
        assert_eq!(cache.len(), 1);

        cache.insert_all([route(1, b"", b"m", &[1]), route(4, b"m", b"", &[1])]);
        assert_eq!(cache.len(), 2, "the parent was left behind");
        assert_eq!(cache.lookup(b"a").map(|r| r.region.id), Some(1));
        assert_eq!(cache.lookup(b"z").map(|r| r.region.id), Some(4));
    }

    #[test]
    fn invalidation_takes_a_region_or_a_key() {
        let cache = RegionCache::new();
        cache.insert_all([route(1, b"", b"g", &[1]), route(2, b"g", b"", &[2])]);

        cache.invalidate(1);
        assert!(cache.lookup(b"a").is_none());
        assert!(cache.lookup(b"z").is_some());

        cache.invalidate_key(b"z");
        assert!(cache.is_empty());

        // Invalidating a key nothing covers is a no-op, not a panic.
        cache.invalidate_key(b"z");
        cache.invalidate(404);
    }

    #[test]
    fn a_leader_hint_is_recorded_and_a_nonsense_one_is_not() {
        let cache = RegionCache::new();
        cache.insert(route(1, b"", b"", &[1, 2, 3]));

        // `route` gives store N the peer id N * 10.
        cache.set_leader(1, Some(30));
        assert_eq!(
            cache
                .lookup(b"k")
                .and_then(|r| r.leader)
                .map(|p| p.store_id),
            Some(3)
        );

        // A hint for a peer the cached region has never heard of means the cache is stale;
        // trusting it would send the next request to an address nobody knows.
        cache.set_leader(1, Some(990));
        assert_eq!(cache.lookup(b"k").and_then(|r| r.leader), None);

        // And an explicit "I no longer know" clears it.
        cache.set_leader(1, Some(30));
        cache.set_leader(1, None);
        assert_eq!(cache.lookup(b"k").and_then(|r| r.leader), None);

        // With no leader known, any peer is worth asking — it answers with a hint.
        let route = cache.lookup(b"k").expect("still cached");
        assert_eq!(route.target().map(|p| p.store_id), Some(1));
    }

    #[test]
    fn the_static_resolver_answers_for_the_whole_key_space() {
        let resolver = StaticRegion::whole_key_space(1, 7, 70);
        let route = resolver
            .locate(b"anything")
            .unwrap()
            .expect("everything is covered");
        assert_eq!(route.region.id, 1);
        assert_eq!(route.target().map(|p| p.store_id), Some(7));
        assert!(resolver.locate(b"").unwrap().is_some());

        // A resolver whose region does not cover the key says so rather than guessing.
        let narrow = StaticRegion::new(Route {
            region: Region {
                id: 2,
                start_key: Bytes::from_static(b"m"),
                end_key: Bytes::from_static(b"n"),
                peers: vec![Peer::voter(1, 1)],
                epoch: Epoch::INITIAL,
            },
            leader: None,
        });
        assert!(narrow.locate(b"m").unwrap().is_some());
        assert!(narrow.locate(b"z").unwrap().is_none());
    }

    /// A routing table answers by the same rule the cache looks up by, including the case the
    /// whole structure exists for: the last region's end key is empty, and a key above every
    /// region's start belongs to it.
    #[test]
    fn a_routing_table_answers_across_a_split_key_space() {
        let table = RegionTable::from_routes([
            route(1, b"", b"g", &[1]),
            route(2, b"g", b"q", &[2]),
            route(3, b"q", b"", &[3]),
        ]);
        assert_eq!(table.len(), 3);
        assert!(!table.is_empty());

        for (key, expected) in [
            (&b""[..], 1),
            (b"f\xff", 1),
            (b"g", 2),
            (b"q", 3),
            (b"\xff\xff\xff", 3),
        ] {
            assert_eq!(
                table.locate(key).unwrap().map(|r| r.region.id),
                Some(expected),
                "key {key:?}"
            );
        }
    }

    /// A gap is not a region. A table that answered with the region below the key would send a
    /// request to a store that would refuse it with `KeyNotInRegion` — one round trip to learn
    /// what the table already knew.
    #[test]
    fn a_routing_table_says_no_rather_than_guessing() {
        let table =
            RegionTable::from_routes([route(1, b"", b"g", &[1]), route(3, b"q", b"", &[3])]);
        assert!(table.locate(b"j").unwrap().is_none());
        assert!(table.locate(b"g").unwrap().is_none());
        assert!(RegionTable::new().locate(b"anything").unwrap().is_none());
    }

    /// `Ok(None)` and `Err` are different answers. A resolver that reported an unreachable
    /// placement driver as "no region covers this key" would turn a momentary outage into a
    /// terminal error on every call in the process, because the first is retryable and the
    /// second is not.
    #[test]
    fn an_unanswerable_lookup_is_not_the_same_as_an_uncovered_key() {
        #[derive(Debug)]
        struct Unreachable;
        impl RegionResolver for Unreachable {
            fn locate(&self, _: &[u8]) -> Result<Option<Route>, ProtoError> {
                Err(ProtoError::ServerIsBusy {
                    reason: "the placement driver is not answering".to_owned(),
                })
            }
        }

        let error = Unreachable.locate(b"k").unwrap_err();
        assert!(error.is_retryable(), "an outage is worth waiting out");
        assert!(
            RegionTable::new().locate(b"k").unwrap().is_none(),
            "an uncovered key is an answer, not a failure"
        );
    }
}
