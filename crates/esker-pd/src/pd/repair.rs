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
use crate::balance::{self, Balance};
use crate::error::Result;
use crate::operator::{InFlight, Observed};
use crate::record::{EventKind, EventOutcome, OperatorEvent, RegionRecord, StoreRecord};
use crate::routing;
use crate::schedule::{self, Cluster, LoadDelta, Repair};

/// One line of the history, from an operator and what became of it.
fn event_of(operator: &Operator, outcome: EventOutcome, at_ms: u64) -> OperatorEvent {
    let (kind, store_id, peer_id) = match operator {
        Operator::AddPeer {
            store_id, peer_id, ..
        } => (EventKind::AddPeer, *store_id, *peer_id),
        // `RemovePeer` names a peer and not a store, because the store it is sent to needs only
        // the peer id. The history says nothing PD does not have on the wire.
        Operator::RemovePeer { peer_id, .. } => (EventKind::RemovePeer, 0, *peer_id),
        Operator::TransferLeader { to_peer_id, .. } => (EventKind::TransferLeader, 0, *to_peer_id),
    };
    OperatorEvent {
        at_ms,
        region_id: operator.region_id(),
        kind,
        outcome,
        store_id,
        peer_id,
    }
}

/// What the operator already in flight for a region means for this heartbeat.
#[derive(Debug)]
enum Step {
    /// The operator lives on. Answer with this and decide nothing new.
    Waiting(Option<Operator>),
    /// The region has no operator; the rules may choose one.
    Free,
}

use Step::{Free, Waiting};

/// What one rule decided, before it becomes an operator.
///
/// The two rules answer different questions and are asked in order — repair is urgent, balance
/// is an optimisation — so the result is one type with two shapes rather than two calls whose
/// precedence a reader has to infer.
#[derive(Debug, Clone, Copy)]
enum Plan {
    /// Something is broken.
    Repair(Repair),
    /// Nothing is broken and something is uneven.
    Balance(Balance),
}

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
        if let Waiting(answer) = self.observe_in_flight(state, record, now_ms)? {
            return Ok(answer);
        }

        let stores = routing::stores(&self.db)?;
        // Every operator still in flight has already committed to moving load; the rules see
        // the cluster as it will be, not as its last round of heartbeats described it. And an
        // operator that has just *finished* is corrected for too, until the stores it moved have
        // reported since — see `State::settling`, and `settled` below for the rule.
        self.retire_settled(state, &stores, now_ms);
        let pending: Vec<LoadDelta> = state
            .in_flight
            .values()
            .map(|flight| flight.load)
            .chain(state.settling.iter().map(|(load, _)| *load))
            .collect();
        let cluster = Cluster {
            stores: &stores,
            pending: &pending,
            now_ms,
            max_store_down_time_ms: self.max_store_down_time_ms,
            target_replicas: self.target_replicas,
        };

        let Some(plan) = self.plan(state, record, &cluster) else {
            return Ok(None);
        };
        self.issue(state, record, plan, now_ms).map(Some)
    }

    /// Reads the heartbeat against the operator already in flight for this region.
    ///
    /// [`Waiting`] means the operator lives on and carries what to send — which is `None` when
    /// the store has demonstrably started, because it has the work and asking again would only
    /// earn a refusal. [`Free`] means the region has no operator and may be decided afresh.
    fn observe_in_flight(
        &self,
        state: &mut State,
        record: &RegionRecord,
        now_ms: u64,
    ) -> Result<Step> {
        let region_id = record.region.id;
        let Some(flight) = state.in_flight.get_mut(&region_id) else {
            return Ok(Free);
        };

        let outcome = match flight.observe(record, now_ms, self.operator_timeout_ms) {
            Observed::Pending(progress) => {
                return Ok(Waiting(flight.advance(progress, now_ms).cloned()));
            }
            Observed::Done => {
                tracing::info!(
                    region_id,
                    operator = flight.operator.name(),
                    "operator done"
                );
                EventOutcome::Done
            }
            Observed::Cancelled(why) => {
                tracing::info!(
                    region_id,
                    operator = flight.operator.name(),
                    why = why.name(),
                    "operator cancelled"
                );
                EventOutcome::Cancelled
            }
            Observed::TimedOut => {
                tracing::warn!(
                    region_id,
                    operator = flight.operator.name(),
                    sends = flight.sends,
                    "operator timed out with nothing observed; it will be re-derived"
                );
                EventOutcome::TimedOut
            }
        };
        let event = event_of(&flight.operator, outcome, now_ms);
        // Every outcome but `Pending` finishes the operator. Dropping it here is what lets the
        // rules issue a replacement on this same heartbeat rather than the next.
        //
        // Its *load* is not dropped with it. `Done` means the stores have moved the replica and
        // will say so in their own time; until they do, forgetting the move would show the
        // rules a cluster that has not moved at all. `Cancelled` and `TimedOut` mean nothing
        // happened — but PD cannot tell which of the two ends actually did, and holding a
        // correction that turns out to be unnecessary costs one deferred move, while dropping
        // one that was necessary costs a sweep.
        if let Some(flight) = state.in_flight.remove(&region_id) {
            state.settling.push((flight.load, now_ms));
        }
        // A region that has just been moved is not moved again for balance until it has
        // settled. Repair is not subject to this — see `BALANCE_COOLDOWN_MS`.
        state
            .cooling
            .insert(region_id, now_ms.saturating_add(self.balance_cooldown_ms));
        self.record_event(state, event)?;
        Ok(Free)
    }

    /// What this region needs, if anything.
    ///
    /// Repair first, always: a region a failure away from losing quorum is not a region to
    /// optimise the placement of. Balance is asked only when nothing is broken, the region is
    /// not cooling from its last move, and balancing is switched on at all.
    fn plan(
        &self,
        state: &mut State,
        record: &RegionRecord,
        cluster: &Cluster<'_>,
    ) -> Option<Plan> {
        // Prune as we pass: `cooling` holds only regions still cooling.
        state.cooling.retain(|_, until| *until > cluster.now_ms);

        if let Some(repair) = schedule::repair_for(record, cluster) {
            return Some(Plan::Repair(repair));
        }
        if !self.balance {
            return None;
        }
        let move_ = balance::balance_for(record, cluster)?;
        if move_.finishes_a_move() {
            // A move already begun always proceeds: neither the cooldown nor the in-flight cap
            // may strand a region on two stores. Both exist to stop moves being *started*.
            return Some(Plan::Balance(move_));
        }
        // A cooling region is not picked up again, and neither is any region while enough
        // moves are already under way — see `MAX_BALANCE_OPERATORS` for why the cap is about
        // arithmetic rather than throughput.
        if state.cooling.contains_key(&record.region.id)
            || state.in_flight.len() >= self.max_balance_operators
        {
            return None;
        }
        Some(Plan::Balance(move_))
    }

    /// Turns a plan into an operator, minting a peer id if it needs one, and records it as in
    /// flight together with the load it commits to moving.
    fn issue(
        &self,
        state: &mut State,
        record: &RegionRecord,
        plan: Plan,
        now_ms: u64,
    ) -> Result<Operator> {
        let (operator, load) = match plan {
            Plan::Balance(Balance::TransferLeader {
                region_id,
                epoch,
                to_peer_id,
                from_store,
                to_store,
                ..
            }) => (
                Operator::TransferLeader {
                    region_id,
                    epoch,
                    to_peer_id,
                },
                LoadDelta::transfer_leader(from_store, to_store),
            ),
            Plan::Balance(Balance::AddPeer {
                region_id,
                epoch,
                store_id,
            })
            | Plan::Repair(Repair::AddPeer {
                region_id,
                epoch,
                store_id,
            }) => {
                // A fresh peer id, from the persisted allocator, every time an `AddPeer` is
                // issued — including after a restart that re-derived the same plan. Reusing
                // the id of an operator PD has forgotten would risk two peers with one id;
                // burning one is free (`docs/adr/0010-pd-durable-state.md`).
                let peer_id = self.next_peer_id(state)?;
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
            Plan::Balance(Balance::RemovePeer {
                region_id,
                epoch,
                peer_id,
                from_store,
            }) => (
                Operator::RemovePeer {
                    region_id,
                    epoch,
                    peer_id,
                },
                LoadDelta::remove_peer(from_store),
            ),
            Plan::Repair(Repair::RemovePeer {
                region_id,
                epoch,
                peer_id,
            }) => {
                // The store the replica leaves, resolved while the record is in hand.
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

        let region_id = record.region.id;
        tracing::info!(region_id, operator = operator.name(), "operator issued");
        self.record_event(state, event_of(&operator, EventOutcome::Issued, now_ms))?;
        state
            .in_flight
            .insert(region_id, InFlight::new(operator.clone(), now_ms, load));
        Ok(operator)
    }

    /// Drops the corrections whose stores have caught up, and the ones too old to mean anything.
    ///
    /// "Caught up" is per store and on PD's clock: a delta is held until **every** store it
    /// names has sent a report stamped after the operator retired, because a delta corrects both
    /// ends of a move and half a correction is worse than none. A store PD has no record of is
    /// treated as caught up — there is nothing to correct.
    fn retire_settled(&self, state: &mut State, stores: &[StoreRecord], now_ms: u64) {
        let reported_since = |store_id: Option<u64>, retired_ms: u64| {
            let Some(store_id) = store_id else {
                return true;
            };
            stores
                .iter()
                .find(|store| store.store_id == store_id)
                .is_none_or(|store| store.last_heartbeat_ms >= retired_ms)
        };
        let too_old = now_ms.saturating_sub(self.max_store_down_time_ms);
        state.settling.retain(|(load, retired_ms)| {
            *retired_ms > too_old
                && !(reported_since(load.region_to, *retired_ms)
                    && reported_since(load.region_from, *retired_ms)
                    && reported_since(load.leader_to, *retired_ms)
                    && reported_since(load.leader_from, *retired_ms))
        });
    }

    /// One cluster-unique peer id, persisted before it is handed out ([`crate::alloc`]).
    fn next_peer_id(&self, state: &mut State) -> Result<u64> {
        let db = Arc::clone(&self.db);
        let cf = self.cf;
        state.alloc.allocate(1, |end| persist_alloc(&db, cf, end))
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

    // ------------------------------------------------------------------------------------
    // The phase-4 retest's repair, replayed
    // ------------------------------------------------------------------------------------

    /// Region 12 of the retest, from the state repair inherited, driven to the end.
    ///
    /// The trace: store 3 is killed and PD asks for a replacement on store 1; the store adds it
    /// as a learner and does not promote it; thirty seconds later the operator times out, PD
    /// re-derives — and **removed the dead voter anyway**, because [`crate::schedule::repair_for`]
    /// counted the learner as a live replica. The region spent the rest of the run at two voters
    /// with a learner beside them, and the acceptance poll gave up at 164 s having repaired
    /// nothing:
    ///
    /// ```text
    /// AddLearner node=31   (store 1)
    /// ...30 s, the learner is never promoted...
    /// Remove     node=15   (store 3, the dead voter)   <- two voters left
    /// ```
    ///
    /// What it has to be instead is two operators and no more: add the replacement, and remove
    /// the dead peer once — and only once — the replacement can vote.
    ///
    /// Mutation check: counting the learner as a replica again (`live_voters` back to
    /// `live_replicas` in `repair_for`) puts the `RemovePeer` back at the timeout and turns the
    /// sequence assertion red.
    #[test]
    fn a_repair_is_two_operators_and_never_drops_the_dead_voter_onto_a_learner() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions {
                // The retest's setting, and the point of the test: the operator does not
                // outlive it, so PD re-derives with the learner still a learner.
                operator_timeout_ms: 30_000,
                ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>)
            },
        )
        .unwrap();
        cluster_of_four(&pd);

        // The region as repair found it: peers 13 and 27 alive on stores 2 and 4, peer 15 on
        // store 3, which has just been killed. Store 1 is the only live store with no peer of
        // it, so it is where the replacement has to go.
        let region = |peers: Vec<Peer>, conf_ver: u64| Region {
            id: 12,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            peers,
            epoch: Epoch::new(conf_ver, 4),
        };
        let dying = vec![Peer::voter(2, 13), Peer::voter(3, 15), Peer::voter(4, 27)];

        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 5);
        }

        let mut issued = Vec::new();
        let ask = |pd: &Pd, peers: Vec<Peer>, conf_ver: u64| {
            pd.region_heartbeat(&beat(region(peers, conf_ver), 13, 4))
                .unwrap()
                .operator
        };

        // 1. The replacement, on the one live store that has no peer of this region.
        let first = ask(&pd, dying.clone(), 5).expect("a repair");
        issued.push(first.clone());
        let Operator::AddPeer {
            store_id, peer_id, ..
        } = first
        else {
            panic!("expected an AddPeer, got {first:?}");
        };
        assert_eq!(store_id, 1, "the only live store without a peer");

        // 2. The store adds it as a learner. PD has been shown progress and waits.
        let catching_up = || {
            let mut peers = dying.clone();
            peers.push(Peer {
                store_id: 1,
                peer_id,
                role: esker_proto::PeerRole::Learner,
            });
            peers
        };
        assert_eq!(ask(&pd, catching_up(), 6), None, "the store has the work");

        // 3. Thirty seconds later nothing has promoted it and the operator is abandoned. This
        //    is the heartbeat the retest failed on: the region is four peers, one of them dead,
        //    and it still has only two votes.
        clock.advance(30_001);
        for store_id in [1, 2, 4] {
            alive(&pd, store_id, 5);
        }
        if let Some(operator) = ask(&pd, catching_up(), 6) {
            issued.push(operator);
        }

        // 4. The learner is promoted at last. Now the region can afford to lose the dead voter.
        let promoted = || {
            let mut peers = dying.clone();
            peers.push(Peer::voter(1, peer_id));
            peers
        };
        let second = ask(&pd, promoted(), 7).expect("the dead peer may go now");
        issued.push(second);

        // 5. And once it is gone, nothing more.
        let repaired = vec![
            Peer::voter(2, 13),
            Peer::voter(4, 27),
            Peer::voter(1, peer_id),
        ];
        assert_eq!(ask(&pd, repaired, 8), None, "the repair is finished");

        assert_eq!(
            issued,
            vec![
                Operator::AddPeer {
                    region_id: 12,
                    epoch: Epoch::new(5, 4),
                    store_id: 1,
                    peer_id,
                },
                Operator::RemovePeer {
                    region_id: 12,
                    epoch: Epoch::new(7, 4),
                    peer_id: 15,
                },
            ],
            "a repair is add-then-remove-the-dead-peer and nothing else",
        );
        assert!(pd.in_flight().unwrap().is_empty());
    }

    /// The other half of the retest's churn, at the rule that caused it: while a region is
    /// mid-repair, balance must not shed anything. In the trace it shed the *healthy* replica on
    /// store 1 — `Remove node=14` — and repair then had to add one back on store 1 four seconds
    /// later, which is two of the five membership changes a two-change repair spent.
    ///
    /// Mutation check: dropping the `mid_repair` guard from `region_balance` makes this a
    /// `RemovePeer` for peer 14.
    #[test]
    fn balance_sheds_nothing_from_a_region_that_is_mid_repair() {
        let (_dir, clock, pd) = open();
        cluster_of_four(&pd);
        clock.advance(MAX_STORE_DOWN_TIME_MS + 1);
        // Stores 1 and 2 tie for busiest, which is how store 1 came to be chosen: the tie
        // breaks to the lowest id. Store 4 has just joined and is empty.
        alive(&pd, 1, 6);
        alive(&pd, 2, 6);
        alive(&pd, 4, 0);

        // Region 12 exactly as the trace has it: the replacement on store 4 has landed as a
        // learner and the dead peer is still there, so the region is four peers with only two
        // votes. Repair has nothing to ask for until the learner is promoted — which is what
        // leaves balance holding the decision.
        let region = |peers: Vec<Peer>, conf_ver: u64| Region {
            id: 12,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            peers,
            epoch: Epoch::new(conf_ver, 4),
        };
        let catching_up = vec![
            Peer::voter(2, 13),
            Peer::voter(1, 14),
            Peer::voter(3, 15),
            Peer {
                store_id: 4,
                peer_id: 27,
                role: esker_proto::PeerRole::Learner,
            },
        ];
        assert_eq!(
            pd.region_heartbeat(&beat(region(catching_up, 6), 13, 4))
                .unwrap()
                .operator,
            None,
            "balance shed peer 14, the healthy replica on store 1, from a region mid-repair",
        );

        // And once the replacement can vote, the peer that goes is the dead one.
        let promoted = vec![
            Peer::voter(2, 13),
            Peer::voter(1, 14),
            Peer::voter(3, 15),
            Peer::voter(4, 27),
        ];
        assert_eq!(
            pd.region_heartbeat(&beat(region(promoted, 7), 13, 4))
                .unwrap()
                .operator,
            Some(Operator::RemovePeer {
                region_id: 12,
                epoch: Epoch::new(7, 4),
                peer_id: 15,
            }),
        );
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
