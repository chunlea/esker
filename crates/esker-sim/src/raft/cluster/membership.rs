//! Servers joining and leaving, and the bookkeeping that goes with it.
//!
//! Membership is the fault that is not a fault: the cluster is supposed to survive it, and it
//! moves the one thing every other property is stated against — who a quorum is. A change takes
//! effect when its entry is *appended*, not when it commits (dissertation §4.1), and only one
//! server moves at a time, which is what makes every pair of consecutive configurations share a
//! quorum without joint consensus.

use esker_raft::NodeId as RaftId;

use super::Cluster;
use crate::raft::driver::{ConfFault, NodeSlot};
use crate::raft::report::{Event, Failure};

impl Cluster {
    /// Starts any node a live configuration now names and that has never run.
    ///
    /// A server being added does not exist until something says it does; this is the moment it
    /// starts. It comes up with an empty log and the configuration that added it, and catches
    /// up from the leader like any other follower that is behind.
    pub(super) fn start_named_spares(&mut self) -> Result<(), Failure> {
        let named: Vec<(RaftId, esker_raft::ConfState)> = self
            .nodes
            .values()
            .filter(|slot| !slot.started() && !slot.online())
            .filter_map(|slot| {
                let config = self
                    .nodes
                    .values()
                    .filter(|other| other.online())
                    .map(NodeSlot::config_of)
                    .find(|config| config.contains(slot.id))?;
                Some((slot.id, config))
            })
            .collect();

        for (id, config) in named {
            let node = self.build_node(id, None, 0, &config)?;
            if let Some(slot) = self.nodes.get_mut(&id) {
                slot.bootstrap = config;
                slot.revive(node);
            }
            self.stats.joins += 1;
            self.record(Event::Joined { node: id });
        }
        Ok(())
    }

    /// Asks the leader to add or remove one server.
    ///
    /// One at a time: the core refuses a second while one is pending, which is the
    /// single-server rule (dissertation §4.1) and is what makes every pair of consecutive
    /// configurations share a quorum.
    pub fn propose_conf_change(&mut self, change: esker_raft::ConfChange) -> bool {
        let Some(leader) = self.leader() else {
            return false;
        };
        let node = change.node;
        let kind = change.kind;
        let accepted = self
            .nodes
            .get_mut(&leader)
            .and_then(|slot| slot.node.as_mut())
            .is_some_and(|raw| raw.propose_conf_change(change).is_ok());
        if accepted {
            self.stats.conf_changes += 1;
        }
        if let Some(slot) = self.nodes.get_mut(&leader) {
            slot.refresh();
        }
        self.record(Event::ConfChange {
            leader,
            node,
            kind: match kind {
                esker_raft::ConfChangeKind::AddVoter => "add voter",
                esker_raft::ConfChangeKind::AddLearner => "add learner",
                esker_raft::ConfChangeKind::Remove => "remove",
            },
            accepted,
        });
        accepted
    }

    /// Draws a membership change that makes sense right now: add a server that is not a member,
    /// or remove one that is — never below three voters, because a group that shrinks to two
    /// cannot lose a node and still make progress, and a sweep that wedges itself proves
    /// nothing.
    pub(super) fn draw_conf_change(&mut self, pick: u64) -> Option<esker_raft::ConfChange> {
        let config = self
            .nodes
            .values()
            .find(|slot| slot.is_leader())
            .map(NodeSlot::config_of)?;
        let outside: Vec<RaftId> = self
            .population
            .iter()
            .copied()
            .filter(|id| !config.contains(*id))
            .collect();
        let inside: Vec<RaftId> = config.voters.clone();

        let add = pick % 2 == 0 || inside.len() <= 3;
        if add && !outside.is_empty() {
            let at = usize::try_from(pick % outside.len() as u64).unwrap_or(0);
            return Some(esker_raft::ConfChange::new(
                esker_raft::ConfChangeKind::AddVoter,
                *outside.get(at)?,
            ));
        }
        if inside.len() > 3 {
            let at = usize::try_from(pick % inside.len() as u64).unwrap_or(0);
            return Some(esker_raft::ConfChange::new(
                esker_raft::ConfChangeKind::Remove,
                *inside.get(at)?,
            ));
        }
        None
    }

    /// Makes every node derive its configuration wrongly, in the named way. One test uses each,
    /// to show that the membership checks go red.
    pub fn break_conf_changes(&mut self, fault: ConfFault) {
        for slot in self.nodes.values_mut() {
            slot.conf_fault = fault;
        }
    }
}
