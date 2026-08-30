//! One operator's life: issued, observed, and finished — or given up on.
//!
//! `docs/DESIGN.md` §7: *every operator is a small state machine with a timeout; PD never sends
//! a second operator for a region while one is in flight.* This module is the state machine.
//! The "never a second" is [`crate::schedule`]'s and [`crate::pd`]'s, because it is a property
//! of the set rather than of one entry.
//!
//! # Progress is observed, never assumed
//!
//! PD does not know whether a store received an operator, and it must not pretend to. Every
//! transition here is driven by a **later region heartbeat** — the peer list changing, the
//! epoch moving — and by nothing else. An operator that was applied but whose heartbeat has
//! not arrived is indistinguishable from one that was lost, and both are handled the same way:
//! keep asking.
//!
//! That is why an operator is idempotent by construction. Re-sending `AddPeer` for a peer that
//! already exists is refused by the store, not applied twice, so PD can afford to repeat
//! itself until it sees the answer in the data.
//!
//! # Why the effect is checked before the epoch
//!
//! Completing an operator *is* an epoch change: adding a peer bumps `conf_ver`. So "the epoch
//! moved" cannot mean "something else happened" until the operator's own effect has been ruled
//! out. [`InFlight::observe`] checks the peer list first for exactly this reason, and gets the
//! opposite answer — [`Observed::Cancelled`] — only when the epoch moved and the effect is
//! *not* there.
//!
//! # The timeout is on being stuck, not on taking long
//!
//! A new replica is caught up by a snapshot, which for a large region is slow, and cancelling
//! a transfer that is working would throw away the work and start again. So the clock runs
//! from the last **observed progress**, not from the issue: an operator that reached
//! [`Progress::Started`] gets the full allowance again.

use esker_proto::{Operator, PeerRole};

use crate::record::RegionRecord;
use crate::schedule::LoadDelta;

/// How far along an operator is, as far as heartbeats have shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Sent, and nothing has been seen yet. PD keeps re-sending it.
    Issued,
    /// A heartbeat has shown the store acting on it: for `AddPeer`, the new replica exists but
    /// is not yet a voter — it is being caught up.
    ///
    /// PD stops re-sending here. The store has the work; asking again would only earn a
    /// refusal, because the epoch PD is addressing is already behind.
    Started,
}

/// What one heartbeat did to an in-flight operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// Still in flight. `Progress` says whether to re-send it or wait.
    Pending(Progress),
    /// A heartbeat showed the operator's effect. It is finished.
    Done,
    /// The region changed underneath it, so the plan it came from no longer describes the
    /// region. Dropped; the scheduling rule re-derives from what is true now.
    Cancelled(Cancelled),
    /// Nothing has moved for the whole timeout. Dropped, and free to be issued again.
    TimedOut,
}

/// Why an operator was cancelled. A `&'static str` in a log line, and a reason a test can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancelled {
    /// The region's epoch moved without the operator's effect appearing: some other membership
    /// change or a split got there first.
    EpochMoved,
    /// The region the operator names is not the region the heartbeat is about. A caller bug
    /// rather than a race, and refused rather than acted on.
    WrongRegion,
}

impl Cancelled {
    /// The name a log line uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::EpochMoved => "the region's epoch moved",
            Self::WrongRegion => "the heartbeat is for another region",
        }
    }
}

/// One operator PD is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// What was asked for.
    pub operator: Operator,
    /// How far it has been seen to get.
    pub progress: Progress,
    /// When it was issued, on PD's clock.
    pub issued_ms: u64,
    /// When progress was last observed, on PD's clock. The timeout runs from here.
    pub since_ms: u64,
    /// How many times it has been sent. Only for the logs and the tests — a high count means
    /// heartbeats are arriving and nothing is happening.
    pub sends: u32,
    /// What this operator will have done to the stores' load once it lands.
    ///
    /// Recorded here, on the entry, so that it is withdrawn exactly when the operator retires
    /// — a separate tally kept beside the in-flight set could drift from it, and a balancer
    /// that double-counts a move it has already forgotten sends the next one to the wrong
    /// place ([`crate::schedule::LoadDelta`]).
    pub load: LoadDelta,
}

impl InFlight {
    /// An operator just issued, with the load it commits to moving.
    #[must_use]
    pub fn new(operator: Operator, now_ms: u64, load: LoadDelta) -> Self {
        Self {
            operator,
            progress: Progress::Issued,
            issued_ms: now_ms,
            since_ms: now_ms,
            sends: 1,
            load,
        }
    }

    /// What `record` says about this operator.
    ///
    /// The order is load-bearing and the module docs say why: effect, then contradiction, then
    /// the clock.
    #[must_use]
    pub fn observe(&self, record: &RegionRecord, now_ms: u64, timeout_ms: u64) -> Observed {
        if record.region.id != self.operator.region_id() {
            return Observed::Cancelled(Cancelled::WrongRegion);
        }

        // Partial progress, if the heartbeat shows any. `Some` here means the epoch moving is
        // *explained* by this operator, so the contradiction test below must not run.
        let mut partial = None;
        match &self.operator {
            Operator::AddPeer { peer_id, .. } => {
                match record
                    .region
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == *peer_id)
                {
                    // A voter is the whole of what was asked for.
                    Some(peer) if peer.role == PeerRole::Voter => return Observed::Done,
                    // A learner is the store catching the replica up before promoting it
                    // (`prompts/04-multiraft-pd.md` 4c, learner first).
                    Some(_) => partial = Some(Progress::Started),
                    None => {}
                }
            }
            Operator::RemovePeer { peer_id, .. } => {
                if !record
                    .region
                    .peers
                    .iter()
                    .any(|peer| peer.peer_id == *peer_id)
                {
                    return Observed::Done;
                }
            }
            Operator::TransferLeader { to_peer_id, .. } => {
                if record.leader_peer_id == *to_peer_id {
                    return Observed::Done;
                }
            }
        }

        if let Some(progress) = partial {
            // Fresh news beats the clock: an operator whose progress this heartbeat is the
            // first to show must not time out on the same heartbeat that proves it is alive.
            if progress != self.progress {
                return Observed::Pending(progress);
            }
            // But a transfer that started and then stopped moving still times out. Without
            // this the region would be blocked for ever, which is the one thing the timeout
            // exists to prevent — "one in flight" means a stuck operator blocks every repair
            // of that region.
            if now_ms.saturating_sub(self.since_ms) > timeout_ms {
                return Observed::TimedOut;
            }
            return Observed::Pending(progress);
        }

        // The effect is not there at all. Now an epoch that has moved means something *else*
        // moved it, and the plan this operator came from described a shape that is gone.
        if record.region.epoch != self.operator.epoch() {
            return Observed::Cancelled(Cancelled::EpochMoved);
        }

        if now_ms.saturating_sub(self.since_ms) > timeout_ms {
            return Observed::TimedOut;
        }
        Observed::Pending(self.progress)
    }

    /// Applies what [`InFlight::observe`] saw, for the outcomes that keep the operator alive.
    ///
    /// Returns the operator to send again, or `None` when PD should wait instead — which is
    /// what [`Progress::Started`] means.
    pub fn advance(&mut self, progress: Progress, now_ms: u64) -> Option<&Operator> {
        if progress != self.progress {
            // Progress resets the clock: the timeout is on being stuck, not on taking long.
            self.progress = progress;
            self.since_ms = now_ms;
        }
        match self.progress {
            Progress::Issued => {
                self.sends += 1;
                Some(&self.operator)
            }
            Progress::Started => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cancelled, InFlight, Observed, Progress};
    use crate::record::RegionRecord;
    use crate::schedule::LoadDelta;
    use bytes::Bytes;
    use esker_proto::{Epoch, Operator, Peer, PeerRole, Region};

    const TIMEOUT: u64 = 300_000;

    fn region(epoch: Epoch, peers: Vec<Peer>) -> RegionRecord {
        RegionRecord {
            region: Region {
                id: 7,
                start_key: Bytes::new(),
                end_key: Bytes::new(),
                peers,
                epoch,
            },
            leader_peer_id: 10,
            term: 4,
            approximate_size: 0,
            applied_index: 0,
            last_heartbeat_ms: 0,
        }
    }

    fn add_peer(epoch: Epoch) -> Operator {
        Operator::AddPeer {
            region_id: 7,
            epoch,
            store_id: 4,
            peer_id: 41,
        }
    }

    /// The happy path, in the order heartbeats really arrive: nothing, then a learner catching
    /// up, then a voter.
    #[test]
    fn an_add_peer_is_pending_then_started_then_done() {
        let epoch = Epoch::new(1, 1);
        let mut flight = InFlight::new(add_peer(epoch), 1_000, LoadDelta::add_peer(4));

        let nothing_yet = region(epoch, vec![Peer::voter(1, 10)]);
        assert_eq!(
            flight.observe(&nothing_yet, 1_100, TIMEOUT),
            Observed::Pending(Progress::Issued)
        );
        assert!(
            flight.advance(Progress::Issued, 1_100).is_some(),
            "an operator nothing has acted on is sent again"
        );
        assert_eq!(flight.sends, 2);

        // The learner appears. Adding it bumped conf_ver — which must not read as "something
        // else happened", because it is this operator happening.
        let catching_up = region(
            Epoch::new(2, 1),
            vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 4,
                    peer_id: 41,
                    role: PeerRole::Learner,
                },
            ],
        );
        assert_eq!(
            flight.observe(&catching_up, 1_200, TIMEOUT),
            Observed::Pending(Progress::Started)
        );
        assert!(
            flight.advance(Progress::Started, 1_200).is_none(),
            "a store that has started needs nothing more from PD"
        );
        assert_eq!(flight.since_ms, 1_200, "progress reset the clock");

        let promoted = region(
            Epoch::new(3, 1),
            vec![Peer::voter(1, 10), Peer::voter(4, 41)],
        );
        assert_eq!(flight.observe(&promoted, 1_300, TIMEOUT), Observed::Done);
    }

    /// A store that adds the voter directly, with no learner step, skips `Started` and is
    /// finished. The middle state is an observation, not a requirement.
    #[test]
    fn an_add_peer_that_goes_straight_to_a_voter_is_done() {
        let epoch = Epoch::new(1, 1);
        let flight = InFlight::new(add_peer(epoch), 1_000, LoadDelta::add_peer(4));
        let done = region(
            Epoch::new(2, 1),
            vec![Peer::voter(1, 10), Peer::voter(4, 41)],
        );
        assert_eq!(flight.observe(&done, 1_100, TIMEOUT), Observed::Done);
    }

    #[test]
    fn a_remove_peer_is_done_when_the_peer_is_gone() {
        let epoch = Epoch::new(2, 1);
        let flight = InFlight::new(
            Operator::RemovePeer {
                region_id: 7,
                epoch,
                peer_id: 41,
            },
            1_000,
            LoadDelta::remove_peer(4),
        );
        let still_there = region(epoch, vec![Peer::voter(1, 10), Peer::voter(4, 41)]);
        assert_eq!(
            flight.observe(&still_there, 1_100, TIMEOUT),
            Observed::Pending(Progress::Issued)
        );
        let gone = region(Epoch::new(3, 1), vec![Peer::voter(1, 10)]);
        assert_eq!(flight.observe(&gone, 1_200, TIMEOUT), Observed::Done);
    }

    /// The contradiction case: the epoch moved and the effect is *not* there, so something
    /// else changed the region and the plan this came from is describing a shape that is gone.
    #[test]
    fn an_epoch_that_moved_without_the_effect_cancels() {
        let flight = InFlight::new(add_peer(Epoch::new(1, 1)), 1_000, LoadDelta::add_peer(4));

        // A split: same peers, `version` bumped.
        let split = region(Epoch::new(1, 2), vec![Peer::voter(1, 10)]);
        assert_eq!(
            flight.observe(&split, 1_100, TIMEOUT),
            Observed::Cancelled(Cancelled::EpochMoved)
        );

        // Somebody else's membership change: `conf_ver` bumped, our peer absent.
        let other = region(
            Epoch::new(2, 1),
            vec![Peer::voter(1, 10), Peer::voter(9, 99)],
        );
        assert_eq!(
            flight.observe(&other, 1_100, TIMEOUT),
            Observed::Cancelled(Cancelled::EpochMoved)
        );
    }

    #[test]
    fn a_heartbeat_for_another_region_cancels_rather_than_counting() {
        let flight = InFlight::new(add_peer(Epoch::new(1, 1)), 1_000, LoadDelta::add_peer(4));
        let mut elsewhere = region(Epoch::new(1, 1), vec![Peer::voter(1, 10)]);
        elsewhere.region.id = 8;
        assert_eq!(
            flight.observe(&elsewhere, 1_100, TIMEOUT),
            Observed::Cancelled(Cancelled::WrongRegion)
        );
    }

    /// The timeout is what stops a region being blocked for ever by an operator nobody is
    /// acting on: one in flight means no second one, so an operator that never finishes would
    /// wedge the region's repair permanently.
    #[test]
    fn an_operator_nothing_moves_times_out() {
        let epoch = Epoch::new(1, 1);
        let flight = InFlight::new(add_peer(epoch), 1_000, LoadDelta::add_peer(4));
        let unchanged = region(epoch, vec![Peer::voter(1, 10)]);

        assert_eq!(
            flight.observe(&unchanged, 1_000 + TIMEOUT, TIMEOUT),
            Observed::Pending(Progress::Issued),
            "the boundary itself is not yet over"
        );
        assert_eq!(
            flight.observe(&unchanged, 1_001 + TIMEOUT, TIMEOUT),
            Observed::TimedOut
        );
    }

    /// A transfer that started and then stopped moving must still time out. This is the case
    /// that ordering the checks wrongly hides: returning `Started` from the effect branch
    /// before consulting the clock makes a stuck learner block its region's repair for ever,
    /// because one operator in flight means no second one.
    #[test]
    fn a_started_operator_that_stops_moving_still_times_out() {
        let mut flight = InFlight::new(add_peer(Epoch::new(1, 1)), 1_000, LoadDelta::add_peer(4));
        let catching_up = region(
            Epoch::new(2, 1),
            vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 4,
                    peer_id: 41,
                    role: PeerRole::Learner,
                },
            ],
        );
        assert_eq!(
            flight.observe(&catching_up, 1_100, TIMEOUT),
            Observed::Pending(Progress::Started)
        );
        flight.advance(Progress::Started, 1_100);
        assert_eq!(
            flight.observe(&catching_up, 1_101 + TIMEOUT, TIMEOUT),
            Observed::TimedOut,
            "a learner that never catches up cannot hold the region for ever"
        );
    }

    /// A slow snapshot must not be cancelled halfway: reaching `Started` earns the whole
    /// allowance again, because the timeout is on being stuck rather than on taking long.
    #[test]
    fn progress_restarts_the_clock() {
        let mut flight = InFlight::new(add_peer(Epoch::new(1, 1)), 1_000, LoadDelta::add_peer(4));
        let catching_up = region(
            Epoch::new(2, 1),
            vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 4,
                    peer_id: 41,
                    role: PeerRole::Learner,
                },
            ],
        );
        // Almost out of time, and then it starts.
        let late = 1_000 + TIMEOUT;
        assert_eq!(
            flight.observe(&catching_up, late, TIMEOUT),
            Observed::Pending(Progress::Started)
        );
        flight.advance(Progress::Started, late);
        assert_eq!(
            flight.observe(&catching_up, late + TIMEOUT, TIMEOUT),
            Observed::Pending(Progress::Started),
            "the transfer was given the full allowance from when it started"
        );
        assert_eq!(
            flight.observe(&catching_up, late + TIMEOUT + 1, TIMEOUT),
            Observed::TimedOut,
            "and no more than that"
        );
    }
}
