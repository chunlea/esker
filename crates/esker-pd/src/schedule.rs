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
//! # Under-replication is the trigger; a death is only one way to reach it
//!
//! The rule used to be scoped to what a down store broke: a region with no peer on a down store
//! was left alone however few replicas it had, on the reasoning that growing a healthy cluster
//! to its target was *balance*. Phase 4's acceptance run priced that reasoning
//! (`docs/bench/phase-4.md` Run 4). Region 27 came out of its pre-kill wait with **two voters
//! and every store alive** — an add that had not been promoted in time — and no rule in this
//! file would look at it, because nothing was down. When one of those two voters was then
//! killed, the survivor held one vote of two: it could not elect, could not commit, and could
//! not commit the very membership change that would have saved it. 1,410 keys stopped
//! answering and no amount of waiting was going to bring them back
//! (`docs/adr/0026-the-quorum-loss-boundary.md`).
//!
//! So the trigger is the **state**, not the event: any region with fewer live replicas than
//! the target is repaired, whether a store died or an earlier add never finished.
//! Under-replication is not a tidier sort of imbalance — it is a region closer to the boundary
//! past which repair stops being possible at all.
//!
//! # Priority is urgency, then id
//!
//! [`Urgency`] orders the regions PD is behind on: a region at exactly a quorum of live voters
//! comes first, because it is the one a single further failure ends. PD decides one region at a
//! time on that region's own heartbeat, so the *live* order is the order beats arrive and there
//! is no queue to sort — the order is what every caller that sees many regions at once
//! ([`repairs`], an operator view, any future budget on repairs in flight) has to apply, which
//! is why the urgency is a public answer about a region rather than a private step of the rule.

use std::collections::BTreeSet;

use esker_proto::{Epoch, PeerRole};

use crate::record::{RegionRecord, StoreRecord};

/// Replicas a region should have (`prompts/04-multiraft-pd.md`: "PD repairs every region to 3
/// replicas").
pub const TARGET_REPLICAS: usize = 3;

/// Live voters a region needs before it can agree on anything: `target_replicas / 2 + 1`.
///
/// The number is load-bearing twice over. It is the size repair has to keep a region above, and
/// it is the boundary past which repair stops being possible at all — a membership change is
/// itself a Raft proposal, so a region that cannot commit cannot commit its own repair
/// (`docs/adr/0026-the-quorum-loss-boundary.md`).
#[must_use]
pub const fn quorum(target_replicas: usize) -> usize {
    target_replicas / 2 + 1
}

/// How badly a region needs the repair it is owed — declared most urgent first, so the derived
/// [`Ord`] *is* the priority order.
///
/// This is a statement about the region and not about what PD can do for it: a region at quorum
/// risk with nowhere to put a replica is still at quorum risk, and an answer that called it
/// healthy because PD happened to have no spare store would be a lie in the one direction that
/// matters. [`repair_for`] answers the other question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// Exactly a quorum of live voters, and short of the target: one more loss and the region
    /// can agree on nothing at all, including the repair that would have saved it. Region 27
    /// sat here for a whole acceptance run with every store alive
    /// (`docs/bench/phase-4.md` Run 4), which is why this state is repaired at all and why it
    /// is repaired first.
    QuorumRisk,
    /// Fewer live voters than a quorum. Worse, and second — deliberately. PD still asks, because
    /// silence *to PD* is not unreachability *to the region's own peers* and the ask may yet
    /// commit; but if the votes are really gone, nothing PD says can be applied and the only
    /// exit is the operator's (`docs/adr/0026-the-quorum-loss-boundary.md`). A region PD can
    /// certainly still save comes before one it probably cannot.
    BelowQuorum,
    /// Below the replica target with a voter to spare above a quorum. Reachable only from
    /// `target_replicas >= 5`; with three, two voters is already the quorum.
    UnderTarget,
    /// At the target, carrying a dead replica that will never answer. Nothing is at risk; there
    /// is tidying to do.
    Cleanup,
}

/// How urgent `region`'s repair is, or `None` when it needs none.
///
/// Counted in **voters** wherever quorum is the question, because a learner is not in the
/// configuration that votes (`docs/DESIGN.md` §5): a region with two voters and a learner
/// catching up can still muster two votes and no more, and saying otherwise would report the
/// most dangerous state in the system as a healthy one.
#[must_use]
pub fn urgency_for(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Urgency> {
    let live = || {
        region
            .region
            .peers
            .iter()
            .filter(|peer| !cluster.is_store_down(peer.store_id))
    };
    let live_voters = live().filter(|peer| peer.role == PeerRole::Voter).count();
    let quorum = quorum(cluster.target_replicas);

    if live_voters < quorum {
        return Some(Urgency::BelowQuorum);
    }
    if live_voters == quorum && live_voters < cluster.target_replicas {
        return Some(Urgency::QuorumRisk);
    }
    if live().count() < cluster.target_replicas {
        return Some(Urgency::UnderTarget);
    }
    region
        .region
        .peers
        .iter()
        .any(|peer| cluster.is_store_down(peer.store_id))
        .then_some(Urgency::Cleanup)
}

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

/// The load one operator will have moved once it lands.
///
/// A store's own reported counts are the truth *as of its last heartbeat*, which is up to a
/// heartbeat interval behind — and PD issues operators much faster than that. Without this,
/// balancing a cluster would mean deciding every region's move from the same stale picture and
/// sending them all to the same emptiest store: the classic thundering herd, and the reason a
/// naive balancer oscillates instead of converging.
///
/// So an operator's effect on the counts is applied the moment it is issued, and withdrawn when
/// it retires — the entry that carries it *is* the in-flight record ([`crate::operator`]), so
/// the two can never disagree. It is a delta rather than a count because it describes a move:
/// something leaves one store and arrives at another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadDelta {
    /// The store gaining a replica.
    pub region_to: Option<u64>,
    /// The store losing one.
    pub region_from: Option<u64>,
    /// The store gaining leadership of a region.
    pub leader_to: Option<u64>,
    /// The store losing it.
    pub leader_from: Option<u64>,
}

impl LoadDelta {
    /// A replica arriving on `store_id`.
    #[must_use]
    pub fn add_peer(store_id: u64) -> Self {
        Self {
            region_to: Some(store_id),
            ..Self::default()
        }
    }

    /// A replica leaving `store_id`.
    ///
    /// The store is named here rather than on the wire: `RemovePeer` carries a peer id, which is
    /// all the receiving store needs, and PD knows which store that peer is on at the moment it
    /// decides — so recording it costs nothing and keeps the wire minimal.
    #[must_use]
    pub fn remove_peer(store_id: u64) -> Self {
        Self {
            region_from: Some(store_id),
            ..Self::default()
        }
    }

    /// Leadership moving from one store to another. A replica does not move, only its office.
    #[must_use]
    pub fn transfer_leader(from_store: u64, to_store: u64) -> Self {
        Self {
            leader_from: Some(from_store),
            leader_to: Some(to_store),
            ..Self::default()
        }
    }

    fn regions_for(self, store_id: u64) -> i64 {
        i64::from(self.region_to == Some(store_id)) - i64::from(self.region_from == Some(store_id))
    }

    fn leaders_for(self, store_id: u64) -> i64 {
        i64::from(self.leader_to == Some(store_id)) - i64::from(self.leader_from == Some(store_id))
    }
}

/// Everything the rules are allowed to look at.
#[derive(Debug, Clone, Copy)]
pub struct Cluster<'a> {
    /// Every store PD knows about.
    pub stores: &'a [StoreRecord],
    /// The load every operator already in flight will have moved once it lands.
    pub pending: &'a [LoadDelta],
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

    /// Regions `store_id` will hold once everything in flight has landed.
    ///
    /// Signed, because a delta can outrun a report: a store that has not beaten since PD asked
    /// for a replica to leave it reads as one below what it says it has. Balancing on the
    /// number *after* the moves in flight is what makes the rules converge instead of piling
    /// every region onto the same store.
    #[must_use]
    pub fn effective_regions(&self, store_id: u64) -> i64 {
        let reported = self
            .stores
            .iter()
            .find(|store| store.store_id == store_id)
            .map_or(0, |store| {
                i64::try_from(store.stats.region_count).unwrap_or(i64::MAX)
            });
        reported
            + self
                .pending
                .iter()
                .map(|delta| delta.regions_for(store_id))
                .sum::<i64>()
    }

    /// Regions `store_id` will lead once everything in flight has landed.
    #[must_use]
    pub fn effective_leaders(&self, store_id: u64) -> i64 {
        let reported = self
            .stores
            .iter()
            .find(|store| store.store_id == store_id)
            .map_or(0, |store| {
                i64::try_from(store.stats.leader_count).unwrap_or(i64::MAX)
            });
        reported
            + self
                .pending
                .iter()
                .map(|delta| delta.leaders_for(store_id))
                .sum::<i64>()
    }

    /// The stores that are live, in id order.
    #[must_use]
    pub fn live_stores(&self) -> Vec<&StoreRecord> {
        self.stores
            .iter()
            .filter(|store| !is_down(store, self.now_ms, self.max_store_down_time_ms))
            .collect()
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
/// `None` covers the situations that are all "do nothing now": the region has its replicas and
/// no dead peer left to drop, it needs one and there is nowhere live to put it, or the
/// replacement it needs is already beside it as a learner and wants only promoting.
///
/// # Fewer live replicas than the target is the whole trigger
///
/// A store that died and an add that never finished leave a region in the same place, and the
/// answer to both is the same replica — so the rule asks how many live replicas the region has
/// and not what happened to it. See the module docs for what the older, death-scoped rule cost.
///
/// **Add before remove.** A region below its replica target gets an `AddPeer`; only once it is
/// back at the target does the dead peer get a `RemovePeer`. Removing first would take a
/// three-replica region with one dead peer down to one live replica out of two — a single
/// further failure from losing quorum, and for no gain (`docs/DESIGN.md` §7).
///
/// # "Back at the target" is counted in voters
///
/// A learner is not in the configuration that votes (`docs/DESIGN.md` §5), so a region with two
/// live voters and a learner catching up is still a two-voter region: dropping its dead voter
/// there is the same mistake as removing first, made one step later. The distinction only shows
/// once the `AddPeer` is no longer in flight — while it is, [`crate::operator::Progress::Started`]
/// is what keeps PD waiting — so it surfaces exactly when the operator times out or PD restarts,
/// which is where the phase-4 retest found it: a repair that had already been re-derived once
/// dropped the dead voter onto a learner and left the region at two voters for the rest of the
/// run.
///
/// Adding is counted in *replicas*, though, and deliberately: a live learner is a replacement
/// already on its way, and asking for a second one would grow the region by a replica per
/// operator timeout without bringing the voter it is waiting for any closer.
#[must_use]
pub fn repair_for(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Repair> {
    let live = || {
        region
            .region
            .peers
            .iter()
            .filter(|peer| !cluster.is_store_down(peer.store_id))
    };
    let live_replicas = live().count();
    let live_voters = live().filter(|peer| peer.role == PeerRole::Voter).count();
    let epoch = region.region.epoch;
    let region_id = region.region.id;

    if live_replicas < cluster.target_replicas {
        return healthiest_store_without_a_peer(region, cluster).map(|store_id| Repair::AddPeer {
            region_id,
            epoch,
            store_id,
        });
    }

    let dead: Vec<u64> = region
        .region
        .peers
        .iter()
        .filter(|peer| cluster.is_store_down(peer.store_id))
        .map(|peer| peer.peer_id)
        .collect();
    if dead.is_empty() {
        // Enough live replicas and nothing dead to clean up. A region *over* the target is
        // balance's to shrink, not repair's.
        return None;
    }
    if live_voters < cluster.target_replicas {
        // The replacement exists and is catching up. Nothing to ask for and nothing to give up:
        // the dead peer stays until the region can afford to lose it.
        return None;
    }

    // Back at the target, so the dead replica can go. The lowest id, so that two PDs — or one
    // PD before and after a restart — make the same choice from the same data.
    dead.into_iter().min().map(|peer_id| Repair::RemovePeer {
        region_id,
        epoch,
        peer_id,
    })
}

/// Every repair the cluster wants, most urgent first, skipping the regions in `busy`.
///
/// `busy` is the set of regions with an operator already in flight: **never two operators for
/// one region** (`docs/DESIGN.md` §7).
///
/// The order is ([`Urgency`], region id) and not the order of the slice, so a caller that can
/// only act on some of these acts on the regions nearest the quorum boundary first — and two
/// runs over the same data agree whatever order the regions were read in. PD's own live path
/// never sees this list: it decides one region at a time as that region's beat arrives (see the
/// module docs), so this is the answer for anything that does see the cluster at once.
#[must_use]
pub fn repairs(
    regions: &[RegionRecord],
    cluster: &Cluster<'_>,
    busy: &BTreeSet<u64>,
) -> Vec<Repair> {
    let mut wanted: Vec<(Urgency, u64, Repair)> = regions
        .iter()
        .filter(|region| !busy.contains(&region.region.id))
        .filter_map(|region| {
            Some((
                urgency_for(region, cluster)?,
                region.region.id,
                repair_for(region, cluster)?,
            ))
        })
        .collect();
    wanted.sort_by_key(|(urgency, region_id, _)| (*urgency, *region_id));
    wanted.into_iter().map(|(_, _, repair)| repair).collect()
}

/// The live store best placed to take a new replica of `region`.
///
/// "Healthiest" is **fewest regions, then lowest store id**. The count is the store's
/// [`Cluster::effective_regions`] — its last report corrected by every operator already in
/// flight — which is the only load number PD has that means anything across stores; free bytes
/// would be the other candidate and is deliberately not used, because a store with a big empty
/// disk and a thousand regions is not the one to send a thousand-and-first to. Capacity becomes
/// an input in 4d, where balance has to weigh both.
///
/// **Effective**, not reported, because repair is now a sweep. A store's report is up to a
/// heartbeat interval old and PD issues operators far faster than that, so sixteen regions
/// deciding in one round from the same stale numbers would every one of them pick the same
/// emptiest store — the thundering herd [`LoadDelta`] exists to prevent, and one the old
/// death-scoped rule already risked whenever a store died with many regions on it.
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
        .min_by_key(|store| (cluster.effective_regions(store.store_id), store.store_id))
        .map(|store| store.store_id)
}

#[cfg(test)]
mod tests {
    use super::{
        Cluster, LoadDelta, Repair, TARGET_REPLICAS, Urgency, is_down, quorum, repair_for, repairs,
        urgency_for,
    };
    use crate::record::{RegionRecord, StoreRecord, StoreStats};
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, PeerRole, Region};
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
            pending: &[],
            now_ms: NOW,
            max_store_down_time_ms: DOWN_AFTER,
            target_replicas: TARGET_REPLICAS,
        }
    }

    /// The counts a rule sees are what the cluster will be, not what its last heartbeats said.
    /// Without this every region's decision is taken from the same stale picture and they all
    /// go to the same store.
    #[test]
    fn the_effective_counts_include_what_is_already_in_flight() {
        let stores = [store(1, NOW, 10), store(2, NOW, 4)];
        let mut with_leaders = stores;
        with_leaders[0].stats.leader_count = 6;
        with_leaders[1].stats.leader_count = 1;

        let idle = cluster(&with_leaders);
        assert_eq!(idle.effective_regions(1), 10);
        assert_eq!(idle.effective_leaders(1), 6);

        let pending = [
            LoadDelta::remove_peer(1),
            LoadDelta::add_peer(2),
            LoadDelta::transfer_leader(1, 2),
        ];
        let moving = Cluster {
            pending: &pending,
            ..cluster(&with_leaders)
        };
        assert_eq!(moving.effective_regions(1), 9, "one replica is leaving");
        assert_eq!(moving.effective_regions(2), 5, "and arriving here");
        assert_eq!(moving.effective_leaders(1), 5);
        assert_eq!(moving.effective_leaders(2), 2);

        // A store PD has no record of counts as zero plus whatever is in flight for it.
        assert_eq!(moving.effective_regions(9), 0);
    }

    /// A delta can outrun a report — a store that has not beaten since PD asked for its last
    /// replica to leave reads below what it says it has. Signed arithmetic, so the rule sees a
    /// number rather than a saturated zero that would look like an empty store.
    #[test]
    fn an_effective_count_may_go_below_what_a_store_reported() {
        let stores = [store(1, NOW, 1)];
        let pending = [LoadDelta::remove_peer(1), LoadDelta::remove_peer(1)];
        let moving = Cluster {
            pending: &pending,
            ..cluster(&stores)
        };
        assert_eq!(moving.effective_regions(1), -1);
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

    /// The retest's stuck shape, at the rule: the replacement is on a live store but is still a
    /// **learner**, so the region has two votes and cannot spare the dead one. Removing it here
    /// is "remove before add" with an extra step, and it is what left a region at two voters for
    /// the rest of an acceptance run.
    ///
    /// And nothing is added either: the replacement is already on its way, and a second one
    /// would grow the region by a replica every time the first operator timed out.
    #[test]
    fn a_dead_voter_outlives_a_replacement_that_cannot_vote_yet() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW - DOWN_AFTER - 1, 5), // dead
            store(4, NOW, 1),
        ];
        let mut catching_up = region(&[(1, 10), (2, 20), (3, 30), (4, 41)]);
        catching_up.region.peers[3].role = PeerRole::Learner;
        assert_eq!(repair_for(&catching_up, &cluster(&stores)), None);

        // Promoted, and now the dead voter may go.
        let promoted = region(&[(1, 10), (2, 20), (3, 30), (4, 41)]);
        assert_eq!(
            repair_for(&promoted, &cluster(&stores)),
            Some(Repair::RemovePeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                peer_id: 30,
            })
        );
    }

    /// Region 27, rebuilt: **two voters with every store alive**, which is how it came out of
    /// the phase-4 acceptance run's pre-kill wait (`docs/bench/phase-4.md` Run 4). The rule this
    /// file used to hold looked for a peer on a down store, found none, and left it exactly
    /// there — and when one of the two voters was killed the survivor held one vote of two and
    /// could commit nothing, its own repair included: 1,410 keys stopped answering for good.
    ///
    /// Mutation check: put the old early return back at the top of `repair_for` — `None` when
    /// no peer is on a down store — and this is the test that goes red.
    #[test]
    fn a_region_short_of_the_target_is_repaired_with_every_store_alive() {
        let stores = [store(1, NOW, 5), store(2, NOW, 5), store(3, NOW, 1)];
        let two_voters = region(&[(1, 28), (2, 29)]);

        assert_eq!(
            repair_for(&two_voters, &cluster(&stores)),
            Some(Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 3,
            }),
            "nothing is down and the region is still a replica short"
        );
        assert_eq!(
            urgency_for(&two_voters, &cluster(&stores)),
            Some(Urgency::QuorumRisk),
            "two voters of three is a quorum exactly: one more loss ends the region"
        );

        // And once the replica is there, PD stops: repair restores the target, it does not grow
        // past it.
        let grown = region(&[(1, 28), (2, 29), (3, 30)]);
        assert_eq!(repair_for(&grown, &cluster(&stores)), None);
        assert_eq!(urgency_for(&grown, &cluster(&stores)), None);
    }

    /// The other half of region 27's story. One of the two voters dies and the region is a
    /// single vote of a two-vote membership. PD asks anyway — silence *to PD* is not
    /// unreachability *to the region's peers*, so the ask may still commit — but if the vote is
    /// truly gone, the group cannot agree to be repaired
    /// (`docs/adr/0026-the-quorum-loss-boundary.md`).
    #[test]
    fn a_region_below_quorum_is_named_as_such_and_still_asked_for() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW - DOWN_AFTER - 1, 5), // the second of the two voters, killed
            store(3, NOW, 1),
        ];
        let halved = region(&[(1, 28), (2, 29)]);
        assert_eq!(
            urgency_for(&halved, &cluster(&stores)),
            Some(Urgency::BelowQuorum)
        );
        assert_eq!(
            repair_for(&halved, &cluster(&stores)),
            Some(Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 3,
            })
        );
    }

    /// The order itself, and the counting behind it: quorum is `target / 2 + 1`, a learner does
    /// not count towards it, and tidying up a dead peer comes after every region that is short.
    #[test]
    fn urgency_ranks_the_quorum_boundary_first_and_tidying_last() {
        assert!(Urgency::QuorumRisk < Urgency::BelowQuorum);
        assert!(Urgency::BelowQuorum < Urgency::UnderTarget);
        assert!(Urgency::UnderTarget < Urgency::Cleanup);
        assert_eq!((quorum(1), quorum(3), quorum(5)), (1, 2, 3));

        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW, 5),
            store(4, NOW, 5),
            store(5, NOW - DOWN_AFTER - 1, 5), // dead
        ];

        // At the target in live voters, with a dead peer still on the books: nothing is at risk.
        let tidy = region(&[(1, 10), (2, 20), (3, 30), (5, 50)]);
        assert_eq!(
            urgency_for(&tidy, &cluster(&stores)),
            Some(Urgency::Cleanup)
        );

        // A voter to spare above the quorum, and still short of the target — which only a target
        // of five or more can be, because with three, two voters is already the quorum.
        let short_of_five = region(&[(1, 10), (2, 20), (3, 30), (4, 40)]);
        let of_five = Cluster {
            target_replicas: 5,
            ..cluster(&stores)
        };
        assert_eq!(
            urgency_for(&short_of_five, &of_five),
            Some(Urgency::UnderTarget)
        );
        assert_eq!(
            urgency_for(&short_of_five, &cluster(&stores)),
            None,
            "the same region at a target of three is simply healthy"
        );

        // Two voters and a learner catching up is two votes, whatever the replica count says.
        let mut catching_up = region(&[(1, 10), (2, 20), (3, 30)]);
        catching_up.region.peers[2].role = PeerRole::Learner;
        assert_eq!(
            urgency_for(&catching_up, &cluster(&stores)),
            Some(Urgency::QuorumRisk),
            "a learner is not in the configuration that votes"
        );
        assert_eq!(
            repair_for(&catching_up, &cluster(&stores)),
            None,
            "and yet nothing is asked for: the replacement is already on its way"
        );
    }

    /// The sweep hands its regions over most urgent first, so a caller that can only act on some
    /// of them acts on the ones nearest the boundary. Equal urgency falls back to the id, so two
    /// runs over the same data agree however the regions were read.
    #[test]
    fn the_sweep_puts_the_quorum_boundary_first_and_breaks_ties_by_id() {
        let stores = [
            store(1, NOW, 5),
            store(2, NOW, 5),
            store(3, NOW, 5),
            store(4, NOW, 5),
            store(5, NOW, 0),
            store(6, NOW, 0),
            store(7, NOW - DOWN_AFTER - 1, 5), // dead
        ];
        let shaped = |id: u64, peers: &[(u64, u64)]| {
            let mut record = region(peers);
            record.region.id = id;
            record
        };
        // Three voters and a dead peer: tidying.
        let tidying = shaped(40, &[(1, 401), (2, 402), (3, 403), (7, 407)]);
        // Two live voters, nothing down: a quorum exactly.
        let at_risk = shaped(30, &[(1, 301), (2, 302)]);
        let also_at_risk = shaped(10, &[(3, 101), (4, 102)]);
        // One live voter of two: past the boundary.
        let past_it = shaped(20, &[(5, 201), (7, 207)]);

        let plans = repairs(
            &[tidying, at_risk, past_it, also_at_risk],
            &cluster(&stores),
            &BTreeSet::new(),
        );
        let order: Vec<u64> = plans.iter().map(Repair::region_id).collect();
        assert_eq!(
            order,
            vec![10, 30, 20, 40],
            "quorum risk (by id), then below quorum, then tidying — not the order of the slice"
        );
    }

    /// Repair is a sweep now, so its placement has to read the same effective counts balance
    /// does: a store's report is up to a heartbeat interval old, and sixteen regions deciding
    /// from one stale reading would every one of them pick the same emptiest store.
    ///
    /// Mutation check: read `store.stats.region_count` here instead of `effective_regions` and
    /// both halves of this test choose store 3.
    #[test]
    fn a_second_repair_in_the_same_round_does_not_pile_onto_the_first_one_s_store() {
        let stores = [
            store(1, NOW, 9),
            store(2, NOW, 9),
            store(3, NOW, 0),
            store(4, NOW, 0),
        ];
        let short = region(&[(1, 10), (2, 20)]);
        assert_eq!(
            repair_for(&short, &cluster(&stores)),
            Some(Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 3,
            }),
            "two empty stores, and the lower id breaks the tie"
        );

        let pending = [LoadDelta::add_peer(3)];
        let mid_round = Cluster {
            pending: &pending,
            ..cluster(&stores)
        };
        assert_eq!(
            repair_for(&short, &mid_round),
            Some(Repair::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 4,
            }),
            "store 3 has a replica arriving; the next region goes elsewhere"
        );
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

    /// Every region a dead store left short is repaired, and the sweep is in id order so two
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
        assert_eq!(plans.len(), 2, "only the regions the dead store left short");
        assert_eq!(
            plans[0].region_id(),
            3,
            "equal urgency, so the lower id goes first"
        );
        assert_eq!(
            plans[1].region_id(),
            9,
            "and the sweep does not follow the slice"
        );
    }
}
