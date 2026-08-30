//! When a store talks to the placement driver, and what it says.
//!
//! `docs/DESIGN.md` §6, §14: a **store** heartbeat every 10 s, and a **region** heartbeat from
//! each leader every 60 s *or on change*. Both cadences live here, and neither reads a clock.
//!
//! # Why there is no clock in this file
//!
//! `CLAUDE.md` invariant 6 forbids a wall clock in an *ordering* decision, and a heartbeat
//! schedule is not one — but the same argument that keeps `esker-raft` pure applies for a
//! different reason: a rule written against `Instant::now()` can only be tested by waiting for it.
//! [`Heartbeats`] counts ticks and nothing else, so "every 60 s or on change" is asserted by
//! driving a counter, and the sixty seconds are the edge's business
//! ([`Heartbeats::interval_ticks`]).
//!
//! # Only a leader reports its region
//!
//! A follower's view of its own region is the leader's view of one round trip ago. Reporting from
//! every peer would triple the traffic to say the same thing and leave PD deciding which copy to
//! believe. A peer that *stops* leading stops reporting, and the one that takes over reports
//! immediately rather than waiting out the interval — which is the "or on change" half, and is
//! what makes a leader change visible to PD in one tick instead of sixty seconds.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use esker_proto::{Epoch, Region};

use crate::pd::{PdClient, RegionHeartbeat, StoreHeartbeat};
use crate::{REGION_HEARTBEAT_MS, STORE_HEARTBEAT_MS};

/// One region, as the store sees it at the moment a heartbeat round runs.
///
/// Everything here is read without waiting on the region's driver thread: the region metadata
/// comes from the region map and the three Raft numbers from what the driver publishes
/// ([`crate::peer::RaftPeer::term`]). A round that asked each driver in turn would queue behind
/// whatever `fsync` each was in the middle of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionReport {
    /// The range, the peers and the epoch.
    pub region: Region,
    /// The peer this store believes leads it, or `0` for "nobody, as far as this store knows".
    pub leader_peer_id: u64,
    /// Whether the leader is *this store's* peer. Only then is a region heartbeat sent.
    pub is_leader: bool,
    /// The leader's Raft term.
    pub term: u64,
    /// How far the state machine has applied.
    pub applied_index: u64,
    /// Approximate bytes of user data. Zero in 4a; `TODO(phase-4b)` measures it.
    pub approximate_size: u64,
}

/// What the store looks like at the moment a heartbeat round runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreReport {
    /// Bytes of storage. Zero in 4a — see [`StoreHeartbeat::capacity`].
    pub capacity: u64,
    /// Bytes free. Zero in 4a.
    pub available: u64,
    /// Bytes of user data applied. Zero in 4a.
    pub applied_bytes: u64,
    /// Every region this store hosts.
    pub regions: Vec<RegionReport>,
}

/// What was last reported for one region, so a change can be noticed without asking PD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reported {
    tick: u64,
    epoch: Epoch,
    leader: u64,
}

/// The heartbeat schedule: a tick counter, and what it has already said.
#[derive(Debug)]
pub struct Heartbeats {
    pd: Arc<dyn PdClient>,
    store_id: u64,
    store_every: u64,
    region_every: u64,
    tick: u64,
    last_store: Option<u64>,
    last_region: BTreeMap<u64, Reported>,
}

impl Heartbeats {
    /// The schedule of `docs/DESIGN.md` §14, in ticks of `tick`.
    #[must_use]
    pub fn new(pd: Arc<dyn PdClient>, store_id: u64, tick: Duration) -> Self {
        Self::with_intervals(
            pd,
            store_id,
            Self::interval_ticks(STORE_HEARTBEAT_MS, tick),
            Self::interval_ticks(REGION_HEARTBEAT_MS, tick),
        )
    }

    /// The schedule with both intervals given in ticks, for the tests that drive the counter.
    #[must_use]
    pub fn with_intervals(
        pd: Arc<dyn PdClient>,
        store_id: u64,
        store_every: u64,
        region_every: u64,
    ) -> Self {
        Self {
            pd,
            store_id,
            store_every: store_every.max(1),
            region_every: region_every.max(1),
            tick: 0,
            last_store: None,
            last_region: BTreeMap::new(),
        }
    }

    /// How many ticks of `tick` an interval of `millis` is, rounded up and never zero.
    ///
    /// Rounded **up** because beating early is the cheap mistake: a store heartbeat at 9.5 s
    /// costs PD one extra message, while one at 10.5 s eats into the 30 s `max_store_down_time`
    /// that decides whether this store is alive.
    #[must_use]
    pub fn interval_ticks(millis: u64, tick: Duration) -> u64 {
        let tick_ms = u64::try_from(tick.as_millis()).unwrap_or(u64::MAX).max(1);
        millis.div_ceil(tick_ms).max(1)
    }

    /// How many ticks this schedule has seen.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.tick
    }

    /// Advances one tick and sends whatever has become due.
    ///
    /// Failures are logged and dropped rather than returned. PD is advisory to a store — nothing
    /// a store does is blocked on it being reachable — and a heartbeat that could not be sent is
    /// re-sent by the next round, which is exactly what a retry would have done with more code.
    pub fn tick(&mut self, report: &StoreReport) {
        self.tick += 1;

        if self.store_due() {
            let beat = self.store_beat(report);
            if let Err(error) = self.pd.store_heartbeat(&beat) {
                tracing::debug!(store_id = self.store_id, %error, "a store heartbeat did not land");
            }
            self.last_store = Some(self.tick);
        }

        for region in &report.regions {
            if !region.is_leader {
                // A peer that has stopped leading forgets what it reported, so the peer that
                // takes over — here or on another store — reports immediately rather than
                // inheriting this one's interval.
                self.last_region.remove(&region.region.id);
                continue;
            }
            if !self.region_due(region) {
                continue;
            }
            let beat = RegionHeartbeat {
                region: region.region.clone(),
                leader_peer_id: region.leader_peer_id,
                term: region.term,
                approximate_size: region.approximate_size,
                applied_index: region.applied_index,
            };
            if let Err(error) = self.pd.region_heartbeat(&beat) {
                tracing::debug!(
                    region_id = region.region.id,
                    %error,
                    "a region heartbeat did not land"
                );
            }
            self.last_region.insert(
                region.region.id,
                Reported {
                    tick: self.tick,
                    epoch: region.region.epoch,
                    leader: region.leader_peer_id,
                },
            );
        }

        // A region this store no longer hosts must not hold a slot for ever.
        self.last_region
            .retain(|id, _| report.regions.iter().any(|region| region.region.id == *id));
    }

    fn store_due(&self) -> bool {
        self.last_store
            .is_none_or(|last| self.tick - last >= self.store_every)
    }

    /// Whether one region is due: the interval has passed, or something a client routes by has
    /// changed since the last report.
    ///
    /// The change half is the one that matters. An epoch bump is a split or a membership change,
    /// and every client cache in the cluster is wrong until PD knows; waiting out the sixty
    /// seconds would make `EpochNotMatch`'s hint the only repair, which is exactly the load the
    /// hint exists to avoid.
    fn region_due(&self, region: &RegionReport) -> bool {
        let Some(last) = self.last_region.get(&region.region.id) else {
            return true;
        };
        last.epoch != region.region.epoch
            || last.leader != region.leader_peer_id
            || self.tick - last.tick >= self.region_every
    }

    fn store_beat(&self, report: &StoreReport) -> StoreHeartbeat {
        StoreHeartbeat {
            store_id: self.store_id,
            capacity: report.capacity,
            available: report.available,
            region_count: report.regions.len() as u64,
            leader_count: report.regions.iter().filter(|r| r.is_leader).count() as u64,
            applied_bytes: report.applied_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Heartbeats, RegionReport, StoreReport};
    use crate::pd::{FakePd, PdClient, StoreInfo};
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, Region};
    use std::sync::Arc;
    use std::time::Duration;

    fn region(id: u64, epoch: Epoch) -> Region {
        Region {
            id,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, id)],
            epoch,
        }
    }

    fn led(id: u64, epoch: Epoch, leader: u64, is_leader: bool) -> RegionReport {
        RegionReport {
            region: region(id, epoch),
            leader_peer_id: leader,
            is_leader,
            term: 3,
            applied_index: 42,
            approximate_size: 0,
        }
    }

    fn pd() -> (Arc<FakePd>, Heartbeats) {
        let pd = Arc::new(FakePd::new());
        pd.bootstrap(&StoreInfo {
            store_id: 1,
            address: "127.0.0.1:7001".to_owned(),
        })
        .unwrap();
        let beats = Heartbeats::with_intervals(Arc::clone(&pd) as Arc<dyn PdClient>, 1, 10, 60);
        (pd, beats)
    }

    /// `docs/DESIGN.md` §14: 10 s and 60 s. The tick is 100 ms, so the intervals are 100 and 600
    /// ticks — and an interval shorter than one tick is one tick, never zero, which would beat on
    /// every tick for ever.
    #[test]
    fn the_documented_intervals_come_out_in_ticks() {
        let tick = Duration::from_millis(esker_raft::TICK_MS);
        assert_eq!(
            Heartbeats::interval_ticks(crate::STORE_HEARTBEAT_MS, tick),
            100
        );
        assert_eq!(
            Heartbeats::interval_ticks(crate::REGION_HEARTBEAT_MS, tick),
            600
        );
        // Rounded up: beating early costs PD a message, beating late eats the down-time budget.
        assert_eq!(
            Heartbeats::interval_ticks(101, Duration::from_millis(100)),
            2
        );
        assert_eq!(Heartbeats::interval_ticks(1, Duration::from_secs(10)), 1);
        assert_eq!(Heartbeats::interval_ticks(0, Duration::from_millis(100)), 1);
    }

    /// The first round reports, and then only every tenth. A store that reported on every tick
    /// would put its cadence in PD's inbox rather than in this schedule.
    #[test]
    fn a_store_reports_at_once_and_then_on_its_interval() {
        let (pd, mut beats) = pd();
        let report = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        };

        for _ in 0..25 {
            beats.tick(&report);
        }
        assert_eq!(pd.store_beats().len(), 3, "ticks 1, 11 and 21");

        let beat = pd.store_beats()[0];
        assert_eq!(beat.store_id, 1);
        assert_eq!(beat.region_count, 1);
        assert_eq!(beat.leader_count, 1);
    }

    #[test]
    fn a_store_counts_the_regions_it_holds_and_the_ones_it_leads() {
        let (pd, mut beats) = pd();
        beats.tick(&StoreReport {
            regions: vec![
                led(1, Epoch::INITIAL, 1, true),
                led(2, Epoch::INITIAL, 22, false),
                led(3, Epoch::INITIAL, 3, true),
            ],
            ..StoreReport::default()
        });
        let beat = pd.store_beats()[0];
        assert_eq!(beat.region_count, 3);
        assert_eq!(beat.leader_count, 2, "a follower is hosted, not led");
    }

    /// Only a leader reports its region. A follower's view is the leader's view of one round trip
    /// ago, so reporting from every peer would triple the traffic and leave PD choosing.
    #[test]
    fn a_follower_never_reports_its_region() {
        let (pd, mut beats) = pd();
        let report = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 99, false)],
            ..StoreReport::default()
        };
        for _ in 0..200 {
            beats.tick(&report);
        }
        assert!(pd.region_beats().is_empty());
    }

    #[test]
    fn a_leader_reports_at_once_and_then_on_its_interval() {
        let (pd, mut beats) = pd();
        let report = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        };
        for _ in 0..121 {
            beats.tick(&report);
        }
        assert_eq!(pd.region_beats().len(), 3, "ticks 1, 61 and 121");

        let beat = &pd.region_beats()[0];
        assert_eq!(beat.region.id, 1);
        assert_eq!(beat.leader_peer_id, 1);
        assert_eq!(beat.term, 3);
        assert_eq!(beat.applied_index, 42);
        assert_eq!(beat.region.epoch, Epoch::INITIAL);
    }

    /// The "or on change" half, and the reason it exists: an epoch bump is a split or a
    /// membership change, and every client cache in the cluster is wrong until PD knows. Waiting
    /// out sixty seconds would leave `EpochNotMatch`'s hint as the only repair — which is the
    /// load that hint exists to avoid.
    #[test]
    fn an_epoch_bump_reports_without_waiting_for_the_interval() {
        let (pd, mut beats) = pd();
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        });
        assert_eq!(pd.region_beats().len(), 1);

        // Two ticks later, a split.
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        });
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::new(1, 2), 1, true)],
            ..StoreReport::default()
        });
        assert_eq!(pd.region_beats().len(), 2, "the bump reported at once");
        assert_eq!(pd.region_beats()[1].region.epoch, Epoch::new(1, 2));

        // And the interval restarts from the change rather than from the last scheduled beat.
        for _ in 0..59 {
            beats.tick(&StoreReport {
                regions: vec![led(1, Epoch::new(1, 2), 1, true)],
                ..StoreReport::default()
            });
        }
        assert_eq!(pd.region_beats().len(), 2);
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::new(1, 2), 1, true)],
            ..StoreReport::default()
        });
        assert_eq!(pd.region_beats().len(), 3);
    }

    /// A leader change is the other thing a client routes by, and PD learns it the same way.
    #[test]
    fn a_leader_change_reports_without_waiting_for_the_interval() {
        let (pd, mut beats) = pd();
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        });
        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 7, true)],
            ..StoreReport::default()
        });
        assert_eq!(pd.region_beats().len(), 2);
        assert_eq!(pd.region_beats()[1].leader_peer_id, 7);
    }

    /// A peer that loses office and regains it reports immediately, rather than inheriting the
    /// interval it was on before. PD's view of a region that changed hands twice must not be
    /// sixty seconds stale because the same peer happens to lead again.
    #[test]
    fn a_peer_that_stops_leading_and_starts_again_reports_at_once() {
        let (pd, mut beats) = pd();
        let leading = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        };
        let following = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 9, false)],
            ..StoreReport::default()
        };

        beats.tick(&leading);
        assert_eq!(pd.region_beats().len(), 1);
        beats.tick(&following);
        beats.tick(&following);
        assert_eq!(pd.region_beats().len(), 1, "a follower says nothing");
        beats.tick(&leading);
        assert_eq!(
            pd.region_beats().len(),
            2,
            "back in office, reported at once"
        );
    }

    /// A region this store no longer hosts must not keep a slot in the schedule for ever. This is
    /// the bookkeeping half of `RemovePeer`, whose operator arrives in 4c.
    #[test]
    fn a_region_that_goes_away_is_forgotten() {
        let (_pd, mut beats) = pd();
        beats.tick(&StoreReport {
            regions: vec![
                led(1, Epoch::INITIAL, 1, true),
                led(2, Epoch::INITIAL, 2, true),
            ],
            ..StoreReport::default()
        });
        assert_eq!(beats.last_region.len(), 2);

        beats.tick(&StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        });
        assert_eq!(beats.last_region.len(), 1);
        assert!(beats.last_region.contains_key(&1));
    }

    /// PD is advisory to a store: nothing a store does is blocked on it being reachable, so a
    /// heartbeat that fails is logged and the next round sends the next one.
    #[test]
    fn a_placement_driver_that_refuses_does_not_stop_the_schedule() {
        #[derive(Debug)]
        struct Refuses;
        impl PdClient for Refuses {
            fn bootstrap(
                &self,
                _: &StoreInfo,
            ) -> Result<crate::pd::Bootstrapped, esker_proto::ProtoError> {
                Err(esker_proto::ProtoError::internal("no"))
            }
            fn alloc_id(&self, _: u64) -> Result<u64, esker_proto::ProtoError> {
                Err(esker_proto::ProtoError::internal("no"))
            }
            fn get_region(
                &self,
                _: &[u8],
            ) -> Result<Option<crate::pd::RegionRoute>, esker_proto::ProtoError> {
                Err(esker_proto::ProtoError::internal("no"))
            }
            fn store_heartbeat(
                &self,
                _: &crate::pd::StoreHeartbeat,
            ) -> Result<(), esker_proto::ProtoError> {
                Err(esker_proto::ProtoError::internal("no"))
            }
            fn region_heartbeat(
                &self,
                _: &crate::pd::RegionHeartbeat,
            ) -> Result<(), esker_proto::ProtoError> {
                Err(esker_proto::ProtoError::internal("no"))
            }
        }

        let mut beats = Heartbeats::with_intervals(Arc::new(Refuses), 1, 10, 60);
        let report = StoreReport {
            regions: vec![led(1, Epoch::INITIAL, 1, true)],
            ..StoreReport::default()
        };
        for _ in 0..25 {
            beats.tick(&report);
        }
        assert_eq!(beats.ticks(), 25, "the schedule kept counting");
    }
}
