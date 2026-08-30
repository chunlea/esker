//! How a node is configured, and where its randomness comes from.
//!
//! The RNG is the part to read carefully. Raft's liveness depends on election timeouts that do not
//! collide: if two followers always time out on the same tick they campaign together, split the
//! vote, and repeat. Real implementations reach for a thread-local generator; this one cannot,
//! because a simulator that cannot replay a failing schedule is worth very little
//! (`docs/DESIGN.md` §5, "determinism rules").
//!
//! So the generator is injected, and two separate mistakes are ruled out:
//!
//! * **Two nodes on one seed drawing one stream.** [`Config::new`] passes the node id as PCG's
//!   *sequence* parameter, which selects a distinct stream from the same seed. A simulator can
//!   hand every node the same seed — which is what makes a run reproducible from one number — and
//!   still get different timeouts on each.
//! * **A timeout fixed at boot.** The timeout is redrawn at the start of *every* election, not
//!   once at construction. Two nodes that happened to draw the same number this term draw
//!   independently next term, so a tie is a delay rather than a livelock.

use esker_base::rng::Pcg32;

use crate::error::{RaftError, Result};
use crate::types::{ConfState, Index, NodeId};
use crate::{
    ELECTION_TIMEOUT_MAX_TICKS, ELECTION_TIMEOUT_MIN_TICKS, HEARTBEAT_TICKS, MAX_INFLIGHT_MSGS,
};

/// Default append batching budget, in bytes: one megabyte per `AppendEntries`.
///
/// Not a format, just a bound on how much a single message may carry. Large enough that a
/// catching-up follower makes real progress per round trip, small enough that one message does not
/// monopolise a connection the heartbeats also need.
pub const MAX_SIZE_PER_MSG: u64 = 1024 * 1024;

/// Everything a [`RawNode`](crate::RawNode) needs that is not in its log.
#[derive(Debug, Clone)]
pub struct Config {
    /// This node's id.
    pub id: NodeId,
    /// The voters to bootstrap with. Ignored when the storage's
    /// [`InitialState`](crate::InitialState) already has a configuration — a restarting node
    /// takes its membership from its log, never from a command line that may be out of date.
    pub voters: Vec<NodeId>,
    /// The learners to bootstrap with, under the same rule.
    pub learners: Vec<NodeId>,
    /// Inclusive range of ticks a follower waits before campaigning. Redrawn every election.
    pub election_tick: (u64, u64),
    /// Ticks between a leader's heartbeats.
    pub heartbeat_tick: u64,
    /// How many append messages may be in flight to one follower at once.
    pub max_inflight_msgs: usize,
    /// Byte budget for one `AppendEntries`.
    pub max_size_per_msg: u64,
    /// Run a pre-vote round before campaigning (§9.6). A node returning from a partition asks
    /// whether it *could* win before bumping the cluster's term, so its return does not depose a
    /// healthy leader.
    pub pre_vote: bool,
    /// A leader steps down when it has not heard from a quorum within an election timeout (§6.2),
    /// and a follower that has heard from its leader recently refuses votes. Together these stop a
    /// partitioned leader from serving stale reads and stop a disruptive node from taking over.
    pub check_quorum: bool,
    /// The index the state machine has already applied. Committed entries at or below it are not
    /// handed out again after a restart.
    pub applied: Index,
    /// The node's random number generator. See this module's documentation.
    pub rng: Pcg32,
}

impl Config {
    /// A configuration with the project defaults (`docs/DESIGN.md` §14) and both pre-vote and
    /// check-quorum on, which is the production pairing.
    ///
    /// `seed` may be shared across a whole cluster: the node id selects the RNG stream, so nodes
    /// still differ while one number still reproduces the run.
    pub fn new(id: NodeId, voters: Vec<NodeId>, seed: u64) -> Self {
        Self {
            id,
            voters,
            learners: Vec::new(),
            election_tick: (ELECTION_TIMEOUT_MIN_TICKS, ELECTION_TIMEOUT_MAX_TICKS),
            heartbeat_tick: HEARTBEAT_TICKS,
            max_inflight_msgs: MAX_INFLIGHT_MSGS,
            max_size_per_msg: MAX_SIZE_PER_MSG,
            pre_vote: true,
            check_quorum: true,
            applied: 0,
            rng: Pcg32::new(seed, id),
        }
    }

    /// The bootstrap membership, normalised.
    pub fn conf_state(&self) -> ConfState {
        let mut conf = ConfState {
            voters: self.voters.clone(),
            learners: self.learners.clone(),
        };
        conf.normalize();
        conf
    }

    /// Rejects a configuration that cannot work.
    ///
    /// The heartbeat check is the one that matters in practice: an election timeout that is not a
    /// comfortable multiple of the heartbeat interval produces a cluster that elects leaders
    /// instead of replicating, and it fails as a performance mystery rather than as an error.
    pub fn validate(&self) -> Result<()> {
        if self.id == 0 {
            return Err(RaftError::InvalidConfig(
                "node id 0 is reserved for 'no node'".into(),
            ));
        }
        let (low, high) = self.election_tick;
        if low == 0 || high < low {
            return Err(RaftError::InvalidConfig(format!(
                "election_tick {low}..={high} is empty or inverted"
            )));
        }
        if self.heartbeat_tick == 0 {
            return Err(RaftError::InvalidConfig(
                "heartbeat_tick must be at least 1".into(),
            ));
        }
        if low <= self.heartbeat_tick {
            return Err(RaftError::InvalidConfig(format!(
                "election_tick low bound {low} must exceed heartbeat_tick {}",
                self.heartbeat_tick
            )));
        }
        if self.max_inflight_msgs == 0 {
            return Err(RaftError::InvalidConfig(
                "max_inflight_msgs must be at least 1".into(),
            ));
        }
        if self.max_size_per_msg == 0 {
            return Err(RaftError::InvalidConfig(
                "max_size_per_msg must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::error::RaftError;

    #[test]
    fn the_defaults_are_valid_and_match_the_design_document() {
        let config = Config::new(1, vec![1, 2, 3], 7);
        assert!(config.validate().is_ok());
        assert_eq!(config.election_tick, (10, 20));
        assert_eq!(config.heartbeat_tick, 2);
        assert_eq!(config.max_inflight_msgs, 256);
        // Both on together is the production pairing, and the pair is what gets tested.
        assert!(config.pre_vote && config.check_quorum);
    }

    #[test]
    fn a_configuration_that_cannot_work_is_refused() {
        let base = Config::new(1, vec![1], 1);

        let mut zero_id = base.clone();
        zero_id.id = 0;
        assert!(matches!(
            zero_id.validate(),
            Err(RaftError::InvalidConfig(_))
        ));

        let mut inverted = base.clone();
        inverted.election_tick = (20, 10);
        assert!(matches!(
            inverted.validate(),
            Err(RaftError::InvalidConfig(_))
        ));

        // An election timeout that does not dominate the heartbeat interval produces a cluster
        // that elects leaders instead of replicating, and it fails as a mystery rather than an
        // error. Better to refuse it.
        let mut too_eager = base.clone();
        too_eager.election_tick = (2, 4);
        too_eager.heartbeat_tick = 2;
        assert!(matches!(
            too_eager.validate(),
            Err(RaftError::InvalidConfig(_))
        ));

        let mut no_window = base;
        no_window.max_inflight_msgs = 0;
        assert!(matches!(
            no_window.validate(),
            Err(RaftError::InvalidConfig(_))
        ));
    }

    /// The trap this whole design exists to avoid: nodes started from one seed — which is how a
    /// simulator makes a run reproducible from a single number — must not draw the same election
    /// timeouts and tie forever. The node id selects the PCG stream, so they do not.
    #[test]
    fn nodes_sharing_a_seed_still_draw_different_election_timeouts() {
        let mut first = Config::new(1, vec![1, 2, 3], 42);
        let mut second = Config::new(2, vec![1, 2, 3], 42);
        let draw = |config: &mut Config| {
            (0..16)
                .map(|_| config.rng.range_inclusive(10, 20))
                .collect::<Vec<_>>()
        };
        assert_ne!(draw(&mut first), draw(&mut second));
    }

    /// And the same seed with the same id must replay exactly, or no failing schedule can be
    /// reproduced from its seed.
    #[test]
    fn one_seed_and_one_id_replay_identically() {
        let mut first = Config::new(3, vec![1, 2, 3], 42);
        let mut second = Config::new(3, vec![1, 2, 3], 42);
        let draw = |config: &mut Config| {
            (0..16)
                .map(|_| config.rng.range_inclusive(10, 20))
                .collect::<Vec<_>>()
        };
        assert_eq!(draw(&mut first), draw(&mut second));
    }
}
