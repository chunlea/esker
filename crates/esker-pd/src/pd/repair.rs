//! Replica repair: what PD asks a region's leader to do about a store that has gone.
//!
//! The rule that decides *what* is [`crate::schedule`], a pure function of what the cluster
//! looks like. One operator's life is [`crate::operator`]. This module is the part that needs
//! PD itself: the in-flight set, the allocator that mints a peer id, and the heartbeat that
//! drives all three.
//!
//! It is a child of [`super`] rather than a sibling because it reads that module's private
//! state directly — a child module can see its ancestors' private items, which is exactly the
//! relationship these two have.

use std::collections::BTreeMap;
use std::sync::Arc;

use esker_proto::Operator;

use super::{Pd, State, persist_alloc};
use crate::error::Result;
use crate::operator::{InFlight, Observed};
use crate::record::RegionRecord;
use crate::routing;
use crate::schedule::{self, Cluster, LoadDelta, Repair};

impl Pd {
    /// Observes the operator in flight for `record`'s region, and issues one if none is.
    ///
    /// Called with the state lock held, because the in-flight set, the allocator and the
    /// decision all have to move together: two heartbeats for one region arriving at once must
    /// not each mint a peer id and each believe they are the only operator.
    pub(super) fn schedule(
        &self,
        state: &mut State,
        record: &RegionRecord,
        now_ms: u64,
    ) -> Result<Option<Operator>> {
        let region_id = record.region.id;

        if let Some(flight) = state.in_flight.get_mut(&region_id) {
            match flight.observe(record, now_ms, self.operator_timeout_ms) {
                Observed::Pending(progress) => {
                    // `advance` answers `None` for an operator the store has demonstrably
                    // started: it has the work, and asking again would only earn a refusal.
                    return Ok(flight.advance(progress, now_ms).cloned());
                }
                Observed::Done => {
                    tracing::info!(
                        region_id,
                        operator = flight.operator.name(),
                        "operator done"
                    );
                }
                Observed::Cancelled(why) => {
                    tracing::info!(
                        region_id,
                        operator = flight.operator.name(),
                        why = why.name(),
                        "operator cancelled"
                    );
                }
                Observed::TimedOut => {
                    tracing::warn!(
                        region_id,
                        operator = flight.operator.name(),
                        sends = flight.sends,
                        "operator timed out with nothing observed; it will be re-derived"
                    );
                }
            }
            // Every outcome but `Pending` finishes the operator. Dropping it here is what lets
            // the rule below issue a replacement on this same heartbeat rather than the next.
            state.in_flight.remove(&region_id);
        }

        let stores = routing::stores(&self.db)?;
        // Every operator still in flight has already committed to moving load; the rules see
        // the cluster as it will be, not as its last round of heartbeats described it.
        let pending: Vec<LoadDelta> = state.in_flight.values().map(|flight| flight.load).collect();
        let cluster = Cluster {
            stores: &stores,
            pending: &pending,
            now_ms,
            max_store_down_time_ms: self.max_store_down_time_ms,
            target_replicas: self.target_replicas,
        };
        let Some(repair) = schedule::repair_for(record, &cluster) else {
            return Ok(None);
        };

        let (operator, load) = match repair {
            Repair::AddPeer {
                region_id,
                epoch,
                store_id,
            } => {
                // A fresh peer id, from the persisted allocator, every time an `AddPeer` is
                // issued — including after a restart that re-derived the same repair. Reusing
                // the id of an operator PD has forgotten would risk two peers with one id;
                // burning one is free (`docs/adr/0010-pd-durable-state.md`).
                let db = Arc::clone(&self.db);
                let cf = self.cf;
                let peer_id = state.alloc.allocate(1, |end| persist_alloc(&db, cf, end))?;
                (
                    Operator::AddPeer {
                        region_id,
                        epoch,
                        store_id,
                        peer_id,
                    },
                    LoadDelta::add_peer(store_id),
                )
            }
            Repair::RemovePeer {
                region_id,
                epoch,
                peer_id,
            } => {
                // The store the replica is leaving, resolved here while the record is in hand.
                let from = record
                    .region
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == peer_id)
                    .map_or(0, |peer| peer.store_id);
                (
                    Operator::RemovePeer {
                        region_id,
                        epoch,
                        peer_id,
                    },
                    LoadDelta::remove_peer(from),
                )
            }
        };
        tracing::info!(region_id, operator = operator.name(), "operator issued");
        state
            .in_flight
            .insert(region_id, InFlight::new(operator.clone(), now_ms, load));
        Ok(Some(operator))
    }

    /// The operators PD is waiting on, by region. For the inspector and the tests.
    pub fn in_flight(&self) -> Result<BTreeMap<u64, InFlight>> {
        Ok(self.lock()?.in_flight.clone())
    }
}

#[cfg(test)]
mod tests {
    use crate::clock::TestClock;
    use crate::pd::{MAX_STORE_DOWN_TIME_MS, Pd, PdOptions};
    use crate::routing::{RegionBeat, StoreBeat};
    use esker_proto::{Epoch, Operator, Peer, Region};
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Arc<TestClock>, Arc<Pd>) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>),
        )
        .unwrap();
        (dir, clock, pd)
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

    // ------------------------------------------------------------------------------------
    // 4c: replica repair
    // ------------------------------------------------------------------------------------

    /// Registers `store_id` and beats for it, which is what a live store does.
    fn alive(pd: &Pd, store_id: u64, region_count: u64) {
        pd.store_heartbeat(&StoreBeat {
            store_id,
            stats: crate::StoreStats {
                region_count,
                ..crate::StoreStats::default()
            },
        })
        .unwrap();
    }

    fn whole_space(peers: Vec<Peer>, epoch: Epoch) -> Region {
        Region {
            id: 1,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            peers,
            epoch,
        }
    }

    fn three_replicas() -> Region {
        whole_space(
            vec![Peer::voter(1, 10), Peer::voter(2, 20), Peer::voter(3, 30)],
            Epoch::new(1, 1),
        )
    }

    /// Four stores registered, three replicas, and store 3 about to go quiet.
    fn cluster_of_four(pd: &Pd) {
        for store_id in 1..=4 {
            pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
                .unwrap();
        }
    }

    /// Three stores, three replicas, one store dies: the surviving leader's next heartbeat
    /// comes back with an `AddPeer`, and PD keeps asking until a heartbeat shows it happened.
    #[test]
    fn a_dead_store_earns_an_add_peer_on_the_next_heartbeat() {
        let (_dir, clock, pd) = open();
        cluster_of_four(&pd);
        for store_id in 1..=4 {
            alive(&pd, store_id, u64::from(store_id == 4));
        }
        assert_eq!(
            pd.region_heartbeat(&beat(three_replicas(), 10, 4))
                .unwrap()
                .operator,
            None,
            "a healthy region is left alone"
        );

        // Store 3 goes quiet. The others keep beating, so they stay live.
        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, u64::from(store_id == 4));
        }

        let operator = pd
            .region_heartbeat(&beat(three_replicas(), 10, 4))
            .unwrap()
            .operator
            .expect("a repair");
        let (store_id, peer_id) = match operator {
            Operator::AddPeer {
                region_id,
                epoch,
                store_id,
                peer_id,
            } => {
                assert_eq!(region_id, 1);
                assert_eq!(epoch, Epoch::new(1, 1), "addressed to the epoch PD holds");
                (store_id, peer_id)
            }
            other => panic!("expected an AddPeer, got {other:?}"),
        };
        assert_eq!(store_id, 4, "the only live store without a peer");
        assert!(peer_id > 0);

        // Asked again, and again, until something changes: the *same* operator, not a second.
        for _ in 0..3 {
            let again = pd
                .region_heartbeat(&beat(three_replicas(), 10, 4))
                .unwrap()
                .operator
                .expect("still asking");
            assert_eq!(again, operator, "PD invented a second operator");
        }
        assert_eq!(pd.in_flight().unwrap().len(), 1);
        assert_eq!(pd.in_flight().unwrap()[&1].sends, 4);
    }

    /// The other half of the repair: once the new replica is a voter, PD stops asking for it
    /// and asks for the dead one to go — add first, remove second.
    #[test]
    fn the_dead_peer_is_removed_only_after_the_new_one_is_a_voter() {
        let (_dir, clock, pd) = open();
        cluster_of_four(&pd);
        pd.region_heartbeat(&beat(three_replicas(), 10, 4)).unwrap();

        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 0);
        }
        let Some(Operator::AddPeer { peer_id, .. }) = pd
            .region_heartbeat(&beat(three_replicas(), 10, 4))
            .unwrap()
            .operator
        else {
            panic!("expected an AddPeer");
        };

        // The store adds it as a learner first: PD sees progress and stops asking.
        let catching_up = whole_space(
            vec![
                Peer::voter(1, 10),
                Peer::voter(2, 20),
                Peer::voter(3, 30),
                Peer {
                    store_id: 4,
                    peer_id,
                    role: esker_proto::PeerRole::Learner,
                },
            ],
            Epoch::new(2, 1),
        );
        assert_eq!(
            pd.region_heartbeat(&beat(catching_up, 10, 4))
                .unwrap()
                .operator,
            None,
            "the store has the work; asking again would only earn a refusal"
        );

        // Promoted. Now there are three live replicas, so the dead peer may go.
        let promoted = whole_space(
            vec![
                Peer::voter(1, 10),
                Peer::voter(2, 20),
                Peer::voter(3, 30),
                Peer::voter(4, peer_id),
            ],
            Epoch::new(3, 1),
        );
        assert_eq!(
            pd.region_heartbeat(&beat(promoted, 10, 4))
                .unwrap()
                .operator,
            Some(Operator::RemovePeer {
                region_id: 1,
                epoch: Epoch::new(3, 1),
                peer_id: 30,
            })
        );

        // And once it is gone, nothing more.
        let repaired = whole_space(
            vec![
                Peer::voter(1, 10),
                Peer::voter(2, 20),
                Peer::voter(4, peer_id),
            ],
            Epoch::new(4, 1),
        );
        assert_eq!(
            pd.region_heartbeat(&beat(repaired, 10, 4))
                .unwrap()
                .operator,
            None
        );
        assert!(pd.in_flight().unwrap().is_empty(), "nothing left in flight");
    }

    /// An operator nothing acts on must not hold its region for ever: one in flight means no
    /// second one, so a stuck operator would block every later repair of that region.
    #[test]
    fn a_timed_out_operator_is_replaced_rather_than_left_in_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions {
                operator_timeout_ms: 1_000,
                ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>)
            },
        )
        .unwrap();
        cluster_of_four(&pd);

        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 0);
        }
        let first = pd
            .region_heartbeat(&beat(three_replicas(), 10, 4))
            .unwrap()
            .operator
            .expect("a repair");

        // Nothing happens for longer than the operator's patience.
        clock.advance(2_000);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 0);
        }
        let second = pd
            .region_heartbeat(&beat(three_replicas(), 10, 4))
            .unwrap()
            .operator
            .expect("a fresh repair");

        assert_ne!(first, second, "the abandoned operator was re-sent verbatim");
        match (first, second) {
            (Operator::AddPeer { peer_id: old, .. }, Operator::AddPeer { peer_id: new, .. }) => {
                assert!(new > old, "a re-issue mints a fresh peer id");
            }
            other => panic!("expected two AddPeers, got {other:?}"),
        }
        assert_eq!(pd.in_flight().unwrap().len(), 1, "still only one at a time");
    }

    /// The prompt's explicit test: PD is killed between issuing an operator and its
    /// completion. In-flight operators are not persisted, so the restarted PD must re-derive
    /// the need from heartbeats — and must not reuse the peer id it has forgotten, because the
    /// old one may be halfway through being added.
    #[test]
    fn a_pd_restarted_mid_operator_re_derives_and_never_reuses_a_peer_id() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);

        let issued = {
            let pd = Pd::open(dir.path(), options()).unwrap();
            cluster_of_four(&pd);
            clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
            for store_id in [1, 2, 4] {
                alive(&pd, store_id, 0);
            }
            pd.region_heartbeat(&beat(three_replicas(), 10, 4))
                .unwrap()
                .operator
                .expect("a repair")
        };

        // PD dies here, with the operator in flight and nothing applied.
        let pd = Pd::open(dir.path(), options()).unwrap();
        assert!(
            pd.in_flight().unwrap().is_empty(),
            "in-flight operators are memory, not state"
        );

        // The stores re-register and beat, as they do on any PD they find.
        for store_id in [1, 2, 4] {
            pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
                .unwrap();
        }
        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 0);
        }

        let after = pd
            .region_heartbeat(&beat(three_replicas(), 10, 4))
            .unwrap()
            .operator
            .expect("the need is re-derived from the heartbeats");

        match (issued, after) {
            (
                Operator::AddPeer {
                    store_id: before_store,
                    peer_id: before_peer,
                    ..
                },
                Operator::AddPeer {
                    store_id: after_store,
                    peer_id: after_peer,
                    ..
                },
            ) => {
                assert_eq!(
                    before_store, after_store,
                    "the same data re-derives the same placement"
                );
                assert!(
                    after_peer > before_peer,
                    "peer id {after_peer} was reused after the restart"
                );
            }
            other => panic!("expected two AddPeers, got {other:?}"),
        }
    }

    /// 4d's operator is on the wire and nothing in 4c issues one. If this ever fails, leader
    /// balance arrived early.
    #[test]
    fn nothing_in_this_phase_issues_a_transfer_leader() {
        let (_dir, clock, pd) = open();
        cluster_of_four(&pd);
        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 0);
        }
        for _ in 0..4 {
            let beat = pd.region_heartbeat(&beat(three_replicas(), 10, 4)).unwrap();
            assert!(
                !matches!(beat.operator, Some(Operator::TransferLeader { .. })),
                "leader balance is 4d"
            );
        }
    }
}
