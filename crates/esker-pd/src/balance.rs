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
        /// Whether this transfer is a step of a replica move already under way, rather than
        /// leader balance in its own right.
        ///
        /// It matters because a move must run to completion: while it is half done the region
        /// sits on **two** stores and is counted on both, so a move left hanging inflates the
        /// numbers every other decision is taken from. See [`Balance::finishes_a_move`].
        finishing: bool,
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
        matches!(
            self,
            Self::RemovePeer { .. }
                | Self::TransferLeader {
                    finishing: true,
                    ..
                }
        )
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

    // A region can be over its replica target for two different reasons, and only one of them
    // is balance's. Either a move balance started has landed — and the copy on the busiest
    // store is the one to shed — or **repair** put a replacement beside a peer on a store that
    // is down, in which case the peer that should go is the dead one and repair is the only
    // rule entitled to say when the region can afford to lose it.
    //
    // Told apart by the state and not by memory: a peer on a down store means the second. The
    // phase-4 retest is what this is written from — balance, asked about a region mid-repair,
    // shed the *healthy* replica on the store that happened to tie for busiest, and repair then
    // had to put one back on that same store. Five membership changes for a repair that needed
    // two.
    let mid_repair = is_mid_repair(region, cluster);

    // The second half first: a region over its replica target is one whose move has landed and
    // needs finishing. Doing this before considering a new move is what stops PD starting a
    // second move while the first is half done.
    //
    // **Voters, not peers**, on both counts — the third instance of the family after
    // `schedule::urgency_for` and `schedule::repair_for`, and the one a real cluster found.
    // `target_replicas` is a number of voters, so a healthy three-voter region that gains a
    // columnar learner counted four against three and balance shed a **voter**: PD's own
    // operator history from `docs/bench/columnar-learner.md` shows the `AddLearner` and the
    // `RemovePeer` in the same millisecond with no store down, and the region at two voters for
    // the five minutes its replacement took to time out.
    //
    // A columnar learner is not a replica this rule may give back. Removing one is
    // `schedule::columnar_for`'s decision, taken from what the SQL layer asked for, and nobody
    // else's. Counting voters also stops the shed firing while an ordinary learner is still
    // catching up, which would undo the very move that added it.
    let voters = || {
        region
            .region
            .peers
            .iter()
            .filter(|peer| peer.role == PeerRole::Voter)
    };
    if voters().count() > cluster.target_replicas && !mid_repair {
        // The replica that goes is the one on the busiest store — that is the whole point of
        // the move, so nothing may override it. Picking any other replica would undo the move
        // that was just made, and the two halves would chase each other for ever.
        let heaviest = voters().max_by_key(|peer| {
            (
                cluster.effective_regions(peer.store_id),
                // Highest count wins; the *lowest* store id breaks the tie, so `max_by_key`
                // is given the negated id.
                -i64::try_from(peer.store_id).unwrap_or(i64::MAX),
            )
        })?;

        if heaviest.peer_id == region.leader_peer_id {
            // The replica that has to go is the leader's, so the office moves first
            // (`prompts/04-multiraft-pd.md` 4d: "move a follower replica, then transfer
            // leadership only if needed"). Removing a leader outright costs an election that
            // the cluster takes at PD's convenience rather than its own.
            let successor = region
                .region
                .peers
                .iter()
                .filter(|peer| peer.peer_id != heaviest.peer_id)
                .filter(|peer| peer.role == PeerRole::Voter)
                .filter(|peer| !cluster.is_store_down(peer.store_id))
                .min_by_key(|peer| (cluster.effective_leaders(peer.store_id), peer.store_id))?;
            return Some(Balance::TransferLeader {
                region_id,
                epoch,
                to_peer_id: successor.peer_id,
                from_store: heaviest.store_id,
                to_store: successor.store_id,
                finishing: true,
            });
        }

        return Some(Balance::RemovePeer {
            region_id,
            epoch,
            peer_id: heaviest.peer_id,
            from_store: heaviest.store_id,
        });
    }

    if mid_repair {
        // Over target or under it, a region with a peer on a down store belongs to repair until
        // repair is finished with it. Balance moving a replica of it now would be optimising a
        // region that is a failure away from losing quorum — the module's own first rule.
        return None;
    }

    // The first half: is one of this region's stores meaningfully busier than somewhere this
    // region could go?
    //
    // Every voter counts here, the leader's included. Adding a replica elsewhere does not move
    // the office — only the `RemovePeer` above can do that, and it handles the case. Excluding
    // the leader here instead would mean a region with a *single* replica could never move at
    // all, because that replica is always the leader: a cluster of one store would never spread
    // onto a store that joined it.
    //
    // **Voters again**, and for a reason the shed above does not have: this rule can only move a
    // voter. Measuring the imbalance over a columnar learner it may not touch would start a move
    // that does not relieve the store it was triggered by — add a voter on the quietest store,
    // shed the heaviest *voter* somewhere else entirely, and leave the busy store exactly as
    // busy, ready to trigger again. The fourth instance of the family, found by reading the
    // second half of this function while fixing the first.
    let busiest = voters()
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

/// Whether this region is still being repaired, and is therefore not balance's to touch.
///
/// **Two states, and the second was learned from a trace.**
///
/// * a peer on a **down store**: the region is a failure away from losing quorum, and a replica
///   moved now is one repair has to move back;
/// * a plain **`Learner`**: a replica that has been added and not yet promoted. Repair and balance
///   both add before they remove, and the peer they add starts as a learner — so a region holding
///   one is a move in progress whoever started it, and the region is not in its settled shape.
///
/// The second one costs more than an extra membership change, which is why it is a rule and not a
/// preference. The promotion is proposed by the **leader**, on the learner's `matched`, because a
/// region heartbeat comes only from a leader and PD can therefore never see a learner's progress
/// (`docs/plans/phase-4.md` §14.1). A leader with a leadership transfer in progress **refuses
/// proposals**. So a `TransferLeader` issued against a region with an un-promoted learner blocks
/// the very `AddVoter` that would finish the repair, the transfer times out waiting for a target
/// the promotion would have caught up, PD re-derives it, and the region can sit like that
/// indefinitely — which is what `esker-store`'s `promotion.rs` acceptance test caught.
///
/// **A `ColumnarLearner` is not a repair.** ADR 0022 Decision 1 says it is never promoted, so
/// counting it here would freeze every region holding one out of balance for ever. It is the same
/// distinction `region_balance` draws twice already when it counts voters rather than peers; this
/// is the third site.
fn is_mid_repair(region: &RegionRecord, cluster: &Cluster<'_>) -> bool {
    region.region.peers.iter().any(|peer| {
        cluster.is_store_down(peer.store_id)
            || match peer.role {
                PeerRole::Learner => true,
                // Exhaustive, like every other role match in this codebase: a role added later
                // must state its own answer rather than inherit one.
                PeerRole::Voter | PeerRole::ColumnarLearner => false,
            }
    })
}

/// Hand this region's leadership to a peer on a quieter store, if that helps enough.
#[must_use]
pub fn leader_balance(region: &RegionRecord, cluster: &Cluster<'_>) -> Option<Balance> {
    if region.leader_peer_id == 0 {
        // PD has no opinion about who leads, so it has no business moving the office.
        return None;
    }
    if is_mid_repair(region, cluster) {
        // The office does not move out from under an unfinished repair. See `is_mid_repair`:
        // a leader mid-transfer refuses the promotion that would finish it, and the two then
        // wait for each other.
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
        finishing: false,
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

    /// A region as `region` builds it, plus one peer in `role` — a replica that has been added
    /// and has not yet finished becoming what it will be.
    fn region_with(
        peers: &[(u64, u64)],
        leader: u64,
        extra: (u64, u64),
        role: PeerRole,
    ) -> RegionRecord {
        let mut record = region(peers, leader);
        record.region.peers.push(Peer {
            store_id: extra.0,
            peer_id: extra.1,
            role,
        });
        record
    }

    /// **A region with an un-promoted learner belongs to repair, and balance leaves it alone.**
    ///
    /// The bug this is written from, off a real trace (`docs/plans/debt-c4.md` §9): PD issued a
    /// `TransferLeader` for a region whose learner was still catching up. A leader with a transfer
    /// in progress **refuses proposals** — including the `AddVoter` the store proposes to finish
    /// the repair — so the transfer and the promotion each waited for the other, the transfer
    /// timed out, PD re-derived it, and the learner sat there past the acceptance deadline.
    ///
    /// `mid_repair` counted a peer on a *down store* and nothing else, so a replica added on a
    /// live store and not yet promoted looked like a settled region. It is the same
    /// voters-are-not-peers family this module names twice already; this is the third site.
    #[test]
    fn a_region_with_a_learner_is_mid_repair_and_balance_leaves_it_alone() {
        // The busiest store by a wide margin, so both rules would otherwise fire.
        let stores = [store(1, 40, 20), store(2, 10, 4), store(3, 10, 1)];
        let settled = region(&[(1, 10), (2, 20), (3, 30)], 10);
        assert!(
            leader_balance(&settled, &cluster(&stores)).is_some(),
            "the imbalance has to be real, or this test would pass for the wrong reason"
        );

        // The same region, one learner still catching up on store 3.
        let repairing = region_with(&[(1, 10), (2, 20), (3, 30)], 10, (3, 40), PeerRole::Learner);
        assert_eq!(
            leader_balance(&repairing, &cluster(&stores)),
            None,
            "leadership was moved out from under a promotion that had not landed"
        );
        assert_eq!(
            region_balance(&repairing, &cluster(&stores)),
            None,
            "a replica was moved while the region was still repairing"
        );
        assert_eq!(balance_for(&repairing, &cluster(&stores)), None);
    }

    /// A **columnar** learner is not a repair in progress: ADR 0022 says it is never promoted, so
    /// a region holding one would otherwise be frozen out of balance for ever.
    #[test]
    fn a_columnar_learner_does_not_freeze_a_region_out_of_balance() {
        let stores = [store(1, 40, 20), store(2, 10, 4), store(3, 10, 1)];
        let region = region_with(
            &[(1, 10), (2, 20), (3, 30)],
            10,
            (3, 40),
            PeerRole::ColumnarLearner,
        );
        assert!(
            balance_for(&region, &cluster(&stores)).is_some(),
            "a columnar learner was mistaken for an unfinished repair"
        );
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
                finishing: false,
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

    /// A region whose only replica is its leader must still be able to move, or a cluster of
    /// one store never spreads onto a store that joins it — every region there is led by its
    /// only peer. Adding a replica elsewhere does not move the office, so nothing about the
    /// leader stands in the way of the *first* half of a move.
    ///
    /// This is the case the store lane's integration test found: with the leader excluded here,
    /// it timed out waiting for regions to reach a second store.
    #[test]
    fn a_region_whose_only_replica_leads_still_moves() {
        let stores = [store(1, 40, 40), store(2, 0, 0)];
        let region = region(&[(1, 10)], 10);
        let cluster = Cluster {
            target_replicas: 1,
            ..cluster(&stores)
        };
        assert_eq!(
            region_balance(&region, &cluster),
            Some(Balance::AddPeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                store_id: 2,
            })
        );
    }

    /// And the second half, when the replica that has to go is the leader's: the office moves
    /// first. Removing a leader outright costs an election at PD's convenience rather than the
    /// cluster's, and picking a *different* replica to drop would undo the move just made.
    #[test]
    fn a_move_whose_replica_is_the_leaders_transfers_the_office_first() {
        let stores = [store(1, 40, 40), store(2, 1, 1)];
        // The move has landed: the region is on both stores and over its target of one.
        let landed = region(&[(1, 10), (2, 20)], 10);
        let cluster = Cluster {
            target_replicas: 1,
            ..cluster(&stores)
        };
        assert_eq!(
            region_balance(&landed, &cluster),
            Some(Balance::TransferLeader {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                to_peer_id: 20,
                from_store: 1,
                to_store: 2,
                finishing: true,
            })
        );

        // Once the office has moved, the replica on the busy store goes.
        let moved = region(&[(1, 10), (2, 20)], 20);
        assert_eq!(
            region_balance(&moved, &cluster),
            Some(Balance::RemovePeer {
                region_id: 7,
                epoch: Epoch::new(1, 1),
                peer_id: 10,
                from_store: 1,
            })
        );
    }

    /// A region with a peer on a down store belongs to repair, over its target or under it.
    ///
    /// Both halves matter. Over target, balance would shed the replica on the busiest *live*
    /// store while the dead one stayed — the retest's `Remove node=14`, a healthy replica on
    /// store 1 taken from a region whose store-3 peer was already gone. Under target, balance
    /// would start a fresh move on a region that is a failure away from losing quorum.
    #[test]
    fn a_region_with_a_dead_peer_is_left_to_repair() {
        let mut stores = [
            store(1, 6, 0),
            store(2, 6, 0),
            store(3, 6, 0),
            store(4, 0, 0),
        ];
        stores[2].last_heartbeat_ms = NOW - DOWN_AFTER - 1;
        let cluster = cluster(&stores);

        // Over target: the replacement on store 4 has landed, the dead peer is still there.
        let over = region(&[(2, 13), (1, 14), (3, 15), (4, 27)], 13);
        assert_eq!(region_balance(&over, &cluster), None);

        // At target, with one of the three on the down store: not a region to optimise.
        let under = region(&[(2, 13), (1, 14), (3, 15)], 13);
        assert_eq!(region_balance(&under, &cluster), None);

        // And with store 3 alive it is ordinary balance again, which is what makes the guard a
        // guard rather than a rule that never fires.
        let healthy = [
            store(1, 6, 0),
            store(2, 6, 0),
            store(3, 6, 0),
            store(4, 0, 0),
        ];
        assert!(region_balance(&under, &self::cluster(&healthy)).is_some());
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
