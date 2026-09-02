//! A stand-in for `esker_pd::balance`, so the model can be tested without the placement driver.
//!
//! **This proves nothing about `esker-pd`.** It is a transcription of the rules in the shape
//! [`super::placement`] speaks, and a transcription passes at every revision, including the one
//! with the bug. It exists for two jobs the real binding cannot do:
//!
//! * showing that the model *reaches* the states it claims to — a checker that never sees a
//!   learner on a live store cannot catch a rule that only mishandles one;
//! * showing that the checker can be made to go **red**, by running it against
//!   [`ReferenceBalance::narrow`], which is the definition as it stood before `548dd62`.
//!
//! The proof about the placement driver is `crates/esker-pd/tests/sim_balance.rs`.

use super::placement::{BalancePolicy, ClusterView, Move, RegionView, Role};

/// A reference policy: `esker_pd::balance`'s rules, in the shape this model speaks.
///
/// **Not a proof of anything about the placement driver.** It exists so that `esker-sim`'s own
/// tests can show the model reaches the states it claims to reach, and so that a deliberately
/// broken variant can show the checker fires. The proof is
/// `crates/esker-pd/tests/sim_balance.rs`, which calls the real function.
#[derive(Debug, Clone, Copy)]
pub struct ReferenceBalance {
    /// When false, a region is mid-repair only while it holds a peer on a down store — the
    /// definition `548dd62` widened, kept here so a test can watch the checker go red.
    pub learner_is_mid_repair: bool,
}

impl ReferenceBalance {
    /// The rule as it stands.
    #[must_use]
    pub fn current() -> Self {
        Self {
            learner_is_mid_repair: true,
        }
    }

    /// The rule as it was before `548dd62`.
    #[must_use]
    pub fn narrow() -> Self {
        Self {
            learner_is_mid_repair: false,
        }
    }

    fn is_mid_repair(self, region: &RegionView, cluster: &ClusterView) -> bool {
        region.peers.iter().any(|peer| {
            is_down(cluster, peer.store_id)
                || (self.learner_is_mid_repair && peer.role == Role::Learner)
        })
    }
}

fn is_down(cluster: &ClusterView, store_id: u64) -> bool {
    cluster
        .stores
        .iter()
        .find(|store| store.store_id == store_id)
        .is_some_and(|store| {
            cluster.now_ms.saturating_sub(store.last_heartbeat_ms) > cluster.max_store_down_time_ms
        })
}

fn regions_on(cluster: &ClusterView, store_id: u64) -> i64 {
    cluster
        .stores
        .iter()
        .find(|store| store.store_id == store_id)
        .map_or(0, |store| i64::try_from(store.region_count).unwrap_or(0))
}

impl BalancePolicy for ReferenceBalance {
    fn plan(&self, region: &RegionView, cluster: &ClusterView) -> Option<Move> {
        let mid_repair = self.is_mid_repair(region, cluster);
        let voters = || region.peers.iter().filter(|peer| peer.role == Role::Voter);

        if voters().count() > cluster.target_replicas && !mid_repair {
            let heaviest = voters().max_by_key(|peer| {
                (
                    regions_on(cluster, peer.store_id),
                    -i64::try_from(peer.store_id).unwrap_or(i64::MAX),
                )
            })?;
            return Some(Move::RemovePeer {
                region_id: region.region_id,
                peer_id: heaviest.peer_id,
            });
        }
        if mid_repair {
            return None;
        }
        let busiest = voters()
            .filter(|peer| !is_down(cluster, peer.store_id))
            .max_by_key(|peer| {
                (
                    regions_on(cluster, peer.store_id),
                    -i64::try_from(peer.store_id).unwrap_or(i64::MAX),
                )
            })?;
        let taken: Vec<u64> = region.peers.iter().map(|peer| peer.store_id).collect();
        let quietest = cluster
            .stores
            .iter()
            .filter(|store| !is_down(cluster, store.store_id))
            .filter(|store| !taken.contains(&store.store_id))
            .min_by_key(|store| (regions_on(cluster, store.store_id), store.store_id))?;
        if regions_on(cluster, busiest.store_id) - regions_on(cluster, quietest.store_id) < 2 {
            return None;
        }
        Some(Move::AddPeer {
            region_id: region.region_id,
            store_id: quietest.store_id,
        })
    }
}
