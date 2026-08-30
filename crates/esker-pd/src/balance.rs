//! Leader balance and region-count balance: the rules that move work off a busy store.
//!
//! `docs/DESIGN.md` §7 names both as PD's scheduling, and `prompts/04-multiraft-pd.md` 4d asks
//! for them "with in-flight limits and timeouts". Like [`crate::schedule`]'s repair rule these
//! are **pure functions** of what the cluster looks like, decided one region at a time on that
//! region's heartbeat.
//!
//! # Balance is what happens when nothing is broken
//!
//! Repair is urgent: a store is down and a region is a failure away from losing quorum. Balance
//! is an optimisation, and it is subordinate to repair everywhere — a region with a repair to do
//! is never balanced, and the caller asks in that order.
//!
//! # Why a gap of one is not an imbalance
//!
//! Moving one leader takes one off the busy store and puts one on the quiet store, so it changes
//! the **spread by two**. Acting on a gap of one would therefore turn 5 vs 4 into 4 vs 5 and then
//! back again, for ever: the classic balancer that never settles. Acting only at a gap of
//! [`LEADER_SPREAD_THRESHOLD`] (or [`REGION_SPREAD_THRESHOLD`]) guarantees the opposite —
//! every move strictly reduces the spread and none can overshoot, so the cluster converges to a
//! gap of at most one and then stops on its own.
//!
//! That is the hysteresis, and it is structural rather than a timer. The per-region cooldown the
//! caller applies ([`crate::pd`]) is a second, weaker guard for the case the arithmetic cannot
//! see: a store whose reported counts are lagging several heartbeats behind reality.
//!
//! # Counts are effective counts
//!
//! Every rule here reads [`Cluster::effective_leaders`] and [`Cluster::effective_regions`],
//! which include the operators already in flight. Deciding from the reported numbers alone would
//! send every region of an unbalanced cluster to the same store in one round.

use esker_proto::{Epoch, PeerRole};

use crate::record::RegionRecord;
use crate::schedule::Cluster;

/// The leader-count gap at which a transfer is worth making.
///
/// Two, because a transfer moves the spread by two. See the module docs.
pub const LEADER_SPREAD_THRESHOLD: i64 = 2;

/// The region-count gap at which a replica is worth moving. Two, for the same reason.
pub const REGION_SPREAD_THRESHOLD: i64 = 2;

/// A move PD wants for balance, before the peer id that would make it an operator exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Balance {
    /// Hand leadership of `region_id` to a peer on a quieter store.
    TransferLeader {
        /// The region whose office moves.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// The peer that should take office.
        to_peer_id: u64,
        /// The store losing a leader.
        from_store: u64,
        /// The store gaining one.
        to_store: u64,
    },
    /// Put a new replica of `region_id` on a quieter store — the first half of a move.
    AddPeer {
        /// The region gaining a replica.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// Where it goes.
        store_id: u64,
    },
    /// Drop the replica on the busiest store — the second half, once the region is over its
    /// replica target.
    RemovePeer {
        /// The region losing a replica.
        region_id: u64,
        /// The epoch PD believes it is at.
        epoch: Epoch,
        /// Which replica.
        peer_id: u64,
        /// The store it leaves.
        from_store: u64,
    },
}

impl Balance {
    /// Whether this is the *second* half of a replica move rather than the start of one.
    ///
    /// A region over its replica target is one whose move has landed and needs finishing, and
    /// finishing it is not the same act as choosing to move it — so the caller's cooldown does
    /// not apply. Leaving it over-replicated for a cooldown would waste a replica's worth of
    /// space and traffic for no gain.
    #[must_use]
    pub fn finishes_a_move(&self) -> bool {
        matches!(self, Self::RemovePeer { .. })
    }

    /// The region this move is about.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        match self {
            Self::TransferLeader { region_id, .. }
            | Self::AddPeer { region_id, .. }
            | Self::RemovePeer { region_id, .. } => *region_id,
        }
    }
}

/// What balance wants for one region, if anything.
///
/// Region count is considered before leader count: a replica move changes both — the store it
/// leaves also loses any leadership of that region — while a leader transfer changes only one,
/// so doing the coarser move first avoids a transfer that a later replica move would undo.
#[must_use]
pub fn balance_for(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Balance> {
    region_balance(region, cluster).or_else(|| leader_balance(region, cluster))
}

/// Move a replica off the busiest of this region's stores, or finish a move already begun.
///
/// **Add before remove**, exactly as repair does it: a region briefly holds one replica more
/// than its target rather than one fewer, so a balance move never lowers the number of live
/// copies of anything (`docs/DESIGN.md` §7).
#[must_use]
pub fn region_balance(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Balance> {
    let epoch = region.region.epoch;
    let region_id = region.region.id;

    // The second half first: a region over its replica target is one whose move has landed and
    // needs finishing. Doing this before considering a new move is what stops PD starting a
    // second move while the first is half done.
    if region.region.peers.len() > cluster.target_replicas {
        let heaviest = region
            .region
            .peers
            .iter()
            // Never drop the leader to finish a move: transferring first would cost an election
            // that the next round would have to pay for anyway. A region whose only spare
            // replica is its leader waits for leader balance to move the office.
            .filter(|peer| peer.peer_id != region.leader_peer_id)
            .max_by_key(|peer| {
                (
                    cluster.effective_regions(peer.store_id),
                    // Highest count wins; the *lowest* store id breaks the tie, so `max_by_key`
                    // is given the negated id.
                    -i64::try_from(peer.store_id).unwrap_or(i64::MAX),
                )
            })?;
        return Some(Balance::RemovePeer {
            region_id,
            epoch,
            peer_id: heaviest.peer_id,
            from_store: heaviest.store_id,
        });
    }

    // The first half: is one of this region's stores meaningfully busier than somewhere this
    // region could go?
    let busiest = region
        .region
        .peers
        .iter()
        // Moving the leader's replica means moving the office too; leader balance is the
        // cheaper tool for that, so a follower is preferred and a region whose only busy peer
        // is its leader is left to it.
        .filter(|peer| peer.peer_id != region.leader_peer_id)
        .filter(|peer| !cluster.is_store_down(peer.store_id))
        .max_by_key(|peer| {
            (
                cluster.effective_regions(peer.store_id),
                -i64::try_from(peer.store_id).unwrap_or(i64::MAX),
            )
        })?;

    let taken: Vec<u64> = region
        .region
        .peers
        .iter()
        .map(|peer| peer.store_id)
        .collect();
    let quietest = cluster
        .live_stores()
        .into_iter()
        .filter(|store| !taken.contains(&store.store_id))
        .min_by_key(|store| (cluster.effective_regions(store.store_id), store.store_id))?;

    let gap =
        cluster.effective_regions(busiest.store_id) - cluster.effective_regions(quietest.store_id);
    if gap < REGION_SPREAD_THRESHOLD {
        return None;
    }
    Some(Balance::AddPeer {
        region_id,
        epoch,
        store_id: quietest.store_id,
    })
}

/// Hand this region's leadership to a peer on a quieter store, if that helps enough.
#[must_use]
pub fn leader_balance(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Balance> {
    if region.leader_peer_id == 0 {
        // PD has no opinion about who leads, so it has no business moving the office.
        return None;
    }
    let leader = region
        .region
        .peers
        .iter()
        .find(|peer| peer.peer_id == region.leader_peer_id)?;
    if cluster.is_store_down(leader.store_id) {
        // A leader on a down store is repair's problem, not balance's — and Raft will have
        // elected somebody else long before PD notices.
        return None;
    }

    let candidate = region
        .region
        .peers
        .iter()
        .filter(|peer| peer.peer_id != leader.peer_id)
        // A learner cannot take office; it is not in the configuration that votes
        // (`docs/DESIGN.md` §5).
        .filter(|peer| peer.role == PeerRole::Voter)
        .filter(|peer| !cluster.is_store_down(peer.store_id))
        .min_by_key(|peer| (cluster.effective_leaders(peer.store_id), peer.store_id))?;

    let gap =
        cluster.effective_leaders(leader.store_id) - cluster.effective_leaders(candidate.store_id);
    if gap < LEADER_SPREAD_THRESHOLD {
        return None;
    }
    Some(Balance::TransferLeader {
        region_id: region.region.id,
        epoch: region.region.epoch,
        to_peer_id: candidate.peer_id,
        from_store: leader.store_id,
        to_store: candidate.store_id,
    })
}

#[cfg(test)]
mod tests {
    use super::{Balance, LEADER_SPREAD_THRESHOLD, balance_for, leader_balance, region_balance};
    use crate::record::{RegionRecord, StoreRecord, StoreStats};
    use crate::schedule::{Cluster, LoadDelta, TARGET_REPLICAS};
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, PeerRole, Region};

    const NOW: u64 = 1_000_000;
    const DOWN_AFTER: u64 = 30_000;

    fn store(store_id: u64, regions: u64, leaders: u64) -> StoreRecord {
        StoreRecord {
            store_id,
            address: format!("127.0.0.1:2016{store_id}"),
            started_ms: 0,
            last_heartbeat_ms: NOW,
            stats: StoreStats {
                region_count: regions,
                leader_count: leaders,
                ..StoreStats::default()
            },
        }
    }

    fn region(peers: &[(u64, u64)], leader: u64) -> RegionRecord {
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
            leader_peer_id: leader,
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

    #[test]
    fn a_leader_moves_from_the_busiest_store_to_the_quietest() {
        let stores = [store(1, 10, 8), store(2, 10, 4), store(3, 10, 1)];
        let region = region(&[(1, 10), (2, 20), (3, 30)], 10);
        assert_eq!(
            leader_balance(&region, &cluster(&stores)),
            Some(Balance::TransferLeader {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                to_peer_id: 30,
                from_store: 1,
                to_store: 3,
            })
        );
    }

    /// The hysteresis, stated as the property it buys: a gap of one is left alone, because
    /// closing it would open the same gap the other way and the cluster would never settle.
    #[test]
    fn a_gap_of_one_is_a_rounding_and_is_left_alone() {
        let stores = [store(1, 10, 5), store(2, 10, 4)];
        let region = region(&[(1, 10), (2, 20)], 10);
        assert_eq!(leader_balance(&region, &cluster(&stores)), None);

        // And exactly at the threshold it acts.
        let stores = [store(1, 10, 5), store(2, 10, 3)];
        assert!(leader_balance(&region, &cluster(&stores)).is_some());
        assert_eq!(
            LEADER_SPREAD_THRESHOLD, 2,
            "the threshold is the spread a move closes"
        );
    }

    /// Every move strictly reduces the spread, which is what makes convergence an argument
    /// rather than a hope: a transfer takes one from the busy store and gives one to the quiet
    /// one, so a gap of `n` becomes `n - 2` and never `-n`.
    #[test]
    fn a_transfer_never_overshoots_into_the_opposite_imbalance() {
        let mut leaders = [6_i64, 0];
        let region = region(&[(1, 10), (2, 20)], 10);
        for _ in 0..10 {
            let stores = [
                store(1, 10, u64::try_from(leaders[0]).unwrap()),
                store(2, 10, u64::try_from(leaders[1]).unwrap()),
            ];
            let Some(Balance::TransferLeader { .. }) = leader_balance(&region, &cluster(&stores))
            else {
                break;
            };
            leaders[0] -= 1;
            leaders[1] += 1;
        }
        assert!(
            (leaders[0] - leaders[1]).abs() <= 1,
            "settled at {leaders:?}, which is not balanced"
        );
    }

    /// A learner is not in the voting configuration, so it cannot be handed the office.
    #[test]
    fn a_learner_is_never_asked_to_take_office() {
        let stores = [store(1, 10, 8), store(2, 10, 0)];
        let mut region = region(&[(1, 10), (2, 20)], 10);
        region.region.peers[1].role = PeerRole::Learner;
        assert_eq!(leader_balance(&region, &cluster(&stores)), None);
    }

    #[test]
    fn a_down_store_is_never_a_destination_and_its_leader_is_repairs_problem() {
        let mut stores = [store(1, 10, 8), store(2, 10, 0)];
        stores[1].last_heartbeat_ms = NOW - DOWN_AFTER - 1;
        let region = region(&[(1, 10), (2, 20)], 10);
        assert_eq!(
            leader_balance(&region, &cluster(&stores)),
            None,
            "the only candidate is on a dead store"
        );

        // And a leader on a dead store is left to repair.
        let mut stores = [store(1, 10, 8), store(2, 10, 0)];
        stores[0].last_heartbeat_ms = NOW - DOWN_AFTER - 1;
        assert_eq!(leader_balance(&region, &cluster(&stores)), None);
    }

    /// The counts a rule reads are effective counts, so a transfer already in flight is not
    /// proposed twice from two regions' heartbeats.
    #[test]
    fn a_transfer_in_flight_is_counted_before_the_next_is_decided() {
        let stores = [store(1, 10, 5), store(2, 10, 3)];
        let region = region(&[(1, 10), (2, 20)], 10);
        assert!(leader_balance(&region, &cluster(&stores)).is_some());

        let pending = [LoadDelta::transfer_leader(1, 2)];
        let moving = Cluster {
            pending: &pending,
            ..cluster(&stores)
        };
        assert_eq!(
            leader_balance(&region, &moving),
            None,
            "4 vs 4 once the transfer in flight lands"
        );
    }

    #[test]
    fn a_replica_moves_off_the_busiest_store_onto_the_quietest() {
        let stores = [store(1, 40, 0), store(2, 30, 0), store(3, 5, 0)];
        // A single-replica region on the busiest store, and store 3 is nearly empty.
        let region = region(&[(1, 10)], 0);
        let cluster = Cluster {
            target_replicas: 1,
            ..cluster(&stores)
        };
        assert_eq!(
            region_balance(&region, &cluster),
            Some(Balance::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 3,
            })
        );
    }

    /// The second half of a move: once the new replica is there, the region is over its target
    /// and the copy on the busiest store goes. Add before remove, so the count of live copies
    /// never dips.
    #[test]
    fn an_over_replicated_region_drops_the_copy_on_the_busiest_store() {
        let stores = [store(1, 40, 0), store(2, 30, 0), store(3, 5, 0)];
        let region = region(&[(1, 10), (3, 30)], 0);
        let cluster = Cluster {
            target_replicas: 1,
            ..cluster(&stores)
        };
        assert_eq!(
            region_balance(&region, &cluster),
            Some(Balance::RemovePeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                peer_id: 10,
                from_store: 1,
            })
        );
    }

    /// Moving a leader's replica costs an election as well as a transfer, so a follower is
    /// preferred and a region whose only busy peer leads is left to leader balance.
    #[test]
    fn the_leaders_replica_is_not_the_one_that_moves() {
        let stores = [store(1, 40, 0), store(2, 5, 0)];
        let region = region(&[(1, 10)], 10);
        let cluster = Cluster {
            target_replicas: 1,
            ..cluster(&stores)
        };
        assert_eq!(region_balance(&region, &cluster), None);
    }

    /// A balanced cluster asks for nothing — the property that makes "converge and stop" true.
    #[test]
    fn a_balanced_cluster_wants_nothing() {
        let stores = [store(1, 33, 11), store(2, 33, 11), store(3, 34, 11)];
        let region = region(&[(1, 10), (2, 20), (3, 30)], 10);
        assert_eq!(balance_for(&region, &cluster(&stores)), None);
    }

    /// Region count is considered first: a replica move takes any leadership of that region
    /// with it, so deciding the leader first would propose a transfer the replica move undoes.
    #[test]
    fn a_replica_move_is_preferred_to_a_leader_move() {
        let stores = [store(1, 40, 9), store(2, 30, 0), store(3, 5, 0)];
        // Leader on the store with nine leaders, a follower on a store with thirty regions,
        // and an empty store 3 to move something to: both rules have something to say.
        let region = region(&[(1, 10), (2, 20)], 10);
        let cluster = Cluster {
            target_replicas: 2,
            ..cluster(&stores)
        };
        assert!(
            leader_balance(&region, &cluster).is_some(),
            "the leader rule would fire on its own"
        );
        assert_eq!(
            balance_for(&region, &cluster),
            Some(Balance::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 3,
            }),
            "and the replica move wins, because it moves the office too"
        );
    }
}
