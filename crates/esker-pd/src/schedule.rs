//! The replica-repair rule: what a cluster's state says should happen, and nothing else.
//!
//! `prompts/04-multiraft-pd.md` 4c: *PD schedules replica repair when a store is down longer
//! than `max_store_down_time`.* This module is that rule, written as a **pure function** of
//! (routing table, store liveness, in-flight set) so that it is unit-testable against a clock a
//! test sets by hand — and, more importantly, so that it can be **re-derived after a PD
//! restart**. PD does not persist its in-flight operators; it recomputes what is needed from
//! the heartbeats that arrive after it comes back (`docs/plans/phase-4.md` §6, race 5). A rule
//! that depended on remembered state could not do that.
//!
//! # Liveness is an age on PD's clock
//!
//! A store is down when *PD* has not heard from it for `max_store_down_time`.
//! [`crate::record::StoreRecord::last_heartbeat_ms`] is stamped by PD when the beat arrives,
//! never taken from the store's own report — comparing wall clocks across nodes is exactly what
//! `CLAUDE.md` invariant 6 exists to prevent, and it would let a store with a fast clock
//! declare itself alive for ever.
//!
//! # A store PD does not know is not a store PD knows is down
//!
//! A peer may name a store with no record here — a PD whose state was replaced, a store that
//! never registered. Unknown is treated as **not down**, so nothing is repaired on the strength
//! of PD's own ignorance. The opposite reading is catastrophic and worth naming: a restarted PD
//! that treated every unrecorded store as dead would try to repair *every region in the
//! cluster* on its first heartbeat.
//!
//! # The rule is scoped to what a down store broke
//!
//! Only a region with a peer on a down store is repaired. A region that is simply
//! under-replicated — the single-peer region every cluster bootstraps with, for instance — is
//! left alone, because growing a healthy cluster to its replica target is *balance*, and
//! balance is 4d. Doing it here would make "repair" mean two things.

use std::collections::BTreeSet;

use esker_proto::Epoch;

use crate::record::{RegionRecord, StoreRecord};

/// Replicas a region should have (`prompts/04-multiraft-pd.md`: "PD repairs every region to 3
/// replicas").
pub const TARGET_REPLICAS: usize = 3;

/// What PD wants done, before the peer id that would make it an
/// [`Operator`](esker_proto::Operator) exists.
///
/// The id is deliberately absent: minting one is a write to the persisted allocator, which is
/// not something a pure function can do. The caller mints it and builds the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Put a new replica of `region_id` on `store_id`.
    AddPeer {
        /// The region to grow.
        region_id: u64,
        /// The epoch PD believes it is at, which the operator will carry.
        epoch: Epoch,
        /// Where the new replica goes.
        store_id: u64,
    },
    /// Drop `peer_id`, whose store is down.
    RemovePeer {
        /// The region to shrink.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// The replica on the down store.
        peer_id: u64,
    },
}

impl Repair {
    /// The region this repair is about.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        match self {
            Self::AddPeer { region_id, .. } | Self::RemovePeer { region_id, .. } => *region_id,
        }
    }
}

/// Everything the rule is allowed to look at.
#[derive(Debug, Clone, Copy)]
pub struct Cluster<'a> {
    /// Every store PD knows about.
    pub stores: &'a [StoreRecord],
    /// PD's clock, now.
    pub now_ms: u64,
    /// Silence after which a store is down.
    pub max_store_down_time_ms: u64,
    /// Replicas a region should have.
    pub target_replicas: usize,
}

/// Whether PD has not heard from `store` for longer than `max_down_ms`.
///
/// The one place a heartbeat age is turned into a verdict. See the module docs for why the age
/// is on PD's clock.
#[must_use]
pub fn is_down(store: &StoreRecord, now_ms: u64, max_down_ms: u64) -> bool {
    now_ms.saturating_sub(store.last_heartbeat_ms) > max_down_ms
}

impl Cluster<'_> {
    /// Whether `store_id` is a store PD knows to be down. Unknown is not down — see the module
    /// docs.
    #[must_use]
    pub fn is_store_down(&self, store_id: u64) -> bool {
        self.stores
            .iter()
            .find(|store| store.store_id == store_id)
            .is_some_and(|store| is_down(store, self.now_ms, self.max_store_down_time_ms))
    }

    /// The stores PD knows to be down.
    #[must_use]
    pub fn down_stores(&self) -> Vec<u64> {
        self.stores
            .iter()
            .filter(|store| is_down(store, self.now_ms, self.max_store_down_time_ms))
            .map(|store| store.store_id)
            .collect()
    }
}

/// What one region needs, if anything.
///
/// `None` covers three different situations that are all "do nothing now": the region has no
/// peer on a down store, it already has enough live replicas and no dead peer left to drop, or
/// it needs a replica and there is nowhere live to put one.
///
/// **Add before remove.** A region below its replica target gets an `AddPeer`; only once it is
/// back at the target does the dead peer get a `RemovePeer`. Removing first would take a
/// three-replica region with one dead peer down to one live replica out of two — a single
/// further failure from losing quorum, and for no gain (`docs/DESIGN.md` §7).
#[must_use]
pub fn repair_for(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Repair> {
    let dead: Vec<u64> = region
        .region
        .peers
        .iter()
        .filter(|peer| cluster.is_store_down(peer.store_id))
        .map(|peer| peer.peer_id)
        .collect();
    if dead.is_empty() {
        // Nothing a down store broke. Under-replication on its own is 4d's business.
        return None;
    }

    let live_replicas = region.region.peers.len() - dead.len();
    let epoch = region.region.epoch;
    let region_id = region.region.id;

    if live_replicas < cluster.target_replicas {
        return healthiest_store_without_a_peer(region, cluster).map(|store_id| Repair::AddPeer {
            region_id,
            epoch,
            store_id,
        });
    }

    // Back at the target, so the dead replica can go. The lowest id, so that two PDs — or one
    // PD before and after a restart — make the same choice from the same data.
    dead.into_iter().min().map(|peer_id| Repair::RemovePeer {
        region_id,
        epoch,
        peer_id,
    })
}

/// Every repair the cluster wants, skipping the regions in `busy`.
///
/// `busy` is the set of regions with an operator already in flight: **never two operators for
/// one region** (`docs/DESIGN.md` §7). Regions are visited in id order, so the answer is a
/// function of the data and not of a hash seed.
#[must_use]
pub fn repairs(
    regions: &[RegionRecord],
    cluster: &Cluster<'_>,
    busy: &BTreeSet<u64>,
) -> Vec<Repair> {
    regions
        .iter()
        .filter(|region| !busy.contains(&region.region.id))
        .filter_map(|region| repair_for(region, cluster))
        .collect()
}

/// The live store best placed to take a new replica of `region`.
///
/// "Healthiest" is **fewest regions, then lowest store id**. The region count is the store's
/// own last report, which is the only load number PD has that means anything across stores;
/// free bytes would be the other candidate and is deliberately not used, because a store with
/// a big empty disk and a thousand regions is not the one to send a thousand-and-first to.
/// Capacity becomes an input in 4d, where balance has to weigh both.
///
/// The lowest-id tiebreak is not cosmetic: it makes the choice deterministic, so a PD that
/// restarts mid-repair re-derives the *same* placement from the same heartbeats instead of
/// scattering replicas differently each time.
///
/// A store already hosting a peer of this region is never a candidate — two replicas of one
/// region on one store is two copies in one failure domain, which is no replication at all
/// (`docs/plans/phase-4.md` §6, race 7).
fn healthiest_store_without_a_peer(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<u64> {
    let taken: BTreeSet<u64> = region
        .region
        .peers
        .iter()
        .map(|peer| peer.store_id)
        .collect();
    cluster
        .stores
        .iter()
        .filter(|store| !taken.contains(&store.store_id))
        .filter(|store| !is_down(store, cluster.now_ms, cluster.max_store_down_time_ms))
        .min_by_key(|store| (store.stats.region_count, store.store_id))
        .map(|store| store.store_id)
}

#[cfg(test)]
mod tests {
    use super::{Cluster, Repair, TARGET_REPLICAS, is_down, repair_for, repairs};
    use crate::record::{RegionRecord, StoreRecord, StoreStats};
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, Region};
    use std::collections::BTreeSet;

    const DOWN_AFTER: u64 = 30_000;
    const NOW: u64 = 1_000_000;

    fn store(store_id: u64, last_beat_ms: u64, region_count: u64) -> StoreRecord {
        StoreRecord {
            store_id,
            address: format!("127.0.0.1:2016{store_id}"),
            started_ms: 0,
            last_heartbeat_ms: last_beat_ms,
            stats: StoreStats {
                region_count,
                ..StoreStats::default()
            },
        }
    }

    fn region(peers: &[(u64, u64)]) -> RegionRecord {
        RegionRecord {
            region: Region {
                id: 7,
                start_key: Bytes::new(),
                end_key: Bytes::new(),
                peers: peers
                    .iter()
                    .map(|(store_id, peer_id)| Peer::voter(*store_id, *peer_id))
                    .collect(),
                epoch: Epoch::new(1, 1),
            },
            leader_peer_id: peers.first().map_or(0, |(_, peer)| *peer),
            term: 4,
            approximate_size: 0,
            applied_index: 0,
            last_heartbeat_ms: NOW,
        }
    }

    fn cluster(stores: &[StoreRecord]) -> Cluster<'_> {
        Cluster {
            stores,
            now_ms: NOW,
            max_store_down_time_ms: DOWN_AFTER,
            target_replicas: TARGET_REPLICAS,
        }
    }

    #[test]
    fn a_store_is_down_only_after_the_whole_interval_of_silence() {
        let fresh = store(1, NOW, 0);
        assert!(!is_down(&fresh, NOW, DOWN_AFTER));
        let exactly = store(1, NOW - DOWN_AFTER, 0);
        assert!(!is_down(&exactly, NOW, DOWN_AFTER), "the boundary is alive");
        let past = store(1, NOW - DOWN_AFTER - 1, 0);
        assert!(is_down(&past, NOW, DOWN_AFTER));
    }

    /// Liveness is an age on PD's clock. A store that reports enormous stats, or that has been
    /// running since before the epoch, is alive or dead purely by when PD last heard from it.
    #[test]
    fn nothing_a_store_reports_about_itself_affects_its_liveness() {
        let mut loud = store(1, NOW - DOWN_AFTER - 1, 9_999);
        loud.started_ms = NOW + 1_000_000;
        loud.stats.capacity = u64::MAX;
        loud.stats.available = u64::MAX;
        assert!(is_down(&loud, NOW, DOWN_AFTER), "only the last beat counts");
    }

    /// The headline: three replicas, one store dies, and the region gets a new replica
    /// somewhere live *before* the dead one is dropped.
    #[test]
    fn a_dead_replica_is_replaced_before_it_is_removed() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5), // dead
            store(4, NOW, 1),                  // the spare, and the emptiest
        ];
        let three_replicas = region(&[(1, 10), (2, 20), (3, 30)]);

        // Two live replicas of three: add first.
        let add = repair_for(&three_replicas, &cluster(&stores)).expect("a repair");
        assert_eq!(
            add,
            Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 4,
            }
        );

        // Once the new replica exists, the dead one goes — and not before.
        let repaired = region(&[(1, 10), (2, 20), (3, 30), (4, 41)]);
        assert_eq!(
            repair_for(&repaired, &cluster(&stores)),
            Some(Repair::RemovePeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                peer_id: 30,
            })
        );

        // And when it is gone there is nothing left to do.
        let healthy = region(&[(1, 10), (2, 20), (4, 41)]);
        assert_eq!(repair_for(&healthy, &cluster(&stores)), None);
    }

    /// The scope rule: a healthy region below the replica target is *not* repaired. Growing a
    /// cluster to its target is balance, and balance is 4d.
    #[test]
    fn an_under_replicated_region_with_no_dead_peer_is_left_alone() {
        let stores = [store(1, NOW, 1), store(2, NOW, 0), store(3, NOW, 0)];
        let lonely = region(&[(1, 10)]);
        assert_eq!(repair_for(&lonely, &cluster(&stores)), None);
    }

    /// Ignorance is not evidence of death. A PD that has just restarted knows no stores, and
    /// must not conclude that every region in the cluster needs repairing.
    #[test]
    fn a_store_pd_has_never_heard_of_is_not_treated_as_down() {
        let nothing_known: [StoreRecord; 0] = [];
        let region = region(&[(1, 10), (2, 20), (3, 30)]);
        assert_eq!(repair_for(&region, &cluster(&nothing_known)), None);

        // And with one store known and down, only that one counts as dead.
        let one_known = [store(3, NOW - DOWN_AFTER - 1, 5)];
        let repair = repair_for(&region, &cluster(&one_known));
        assert_eq!(
            repair, None,
            "the two unknown stores are live, so there is nowhere to add and nothing safe to \
             remove"
        );
    }

    /// Fewest regions first, and the lowest id breaks a tie — so the same data always places
    /// the replica in the same place, which is what makes a restarted PD re-derive the same
    /// plan instead of scattering replicas.
    #[test]
    fn the_emptiest_store_wins_and_the_lowest_id_breaks_the_tie() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5),
            store(5, NOW, 2),
            store(4, NOW, 2), // same count as store 5, lower id
            store(6, NOW, 9),
        ];
        let region = region(&[(1, 10), (2, 20), (3, 30)]);
        assert_eq!(
            repair_for(&region, &cluster(&stores)),
            Some(Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 4,
            })
        );
    }

    /// Two replicas of one region on one store is two copies in one failure domain.
    #[test]
    fn a_store_already_hosting_a_peer_is_never_chosen() {
        let stores = [
            store(1, NOW, 0), // emptiest, but already hosts a peer
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5),
        ];
        let region = region(&[(1, 10), (2, 20), (3, 30)]);
        assert_eq!(
            repair_for(&region, &cluster(&stores)),
            None,
            "store 1 is taken and store 2 is taken, so there is nowhere live to add"
        );
    }

    /// Nowhere to put a replica is "do nothing", not a panic and not a bad placement.
    #[test]
    fn a_cluster_with_nowhere_to_add_asks_for_nothing() {
        let stores = [
            store(1, NOW, 0),
            store(2, NOW, 0),
            store(3, NOW - DOWN_AFTER - 1, 0),
        ];
        let region = region(&[(1, 10), (2, 20), (3, 30)]);
        assert_eq!(repair_for(&region, &cluster(&stores)), None);
    }

    /// A region with an operator in flight is skipped entirely: never two for one region.
    #[test]
    fn a_region_already_being_repaired_is_skipped() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5),
            store(4, NOW, 1),
        ];
        let regions = [region(&[(1, 10), (2, 20), (3, 30)])];

        let idle = BTreeSet::new();
        assert_eq!(repairs(&regions, &cluster(&stores), &idle).len(), 1);

        let busy: BTreeSet<u64> = [7].into_iter().collect();
        assert!(repairs(&regions, &cluster(&stores), &busy).is_empty());
    }

    /// Every region touched by a dead store is repaired, and the sweep is in id order so two
    /// runs over the same data agree.
    #[test]
    fn every_region_the_dead_store_touched_is_repaired_in_id_order() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5),
            store(4, NOW, 1),
        ];
        let mut first = region(&[(1, 10), (2, 20), (3, 30)]);
        first.region.id = 9;
        let mut second = region(&[(1, 11), (2, 21), (3, 31)]);
        second.region.id = 3;
        let untouched = {
            let mut region = region(&[(1, 12), (2, 22), (4, 42)]);
            region.region.id = 5;
            region
        };

        let plans = repairs(
            &[first, second, untouched],
            &cluster(&stores),
            &BTreeSet::new(),
        );
        assert_eq!(plans.len(), 2, "only the regions the dead store touched");
        assert_eq!(plans[0].region_id(), 9);
        assert_eq!(plans[1].region_id(), 3, "the sweep follows the slice");
    }
}
