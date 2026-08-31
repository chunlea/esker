//! Leadership transfer (§3.10), and the leader's half of check-quorum (§6.2).
//!
//! Both are about a leader giving up office, and they are here together because they are the two
//! ways that happens on purpose.
//!
//! **Transfer** exists so a node can be taken out of service without the cluster paying for an
//! election timeout. The leader stops accepting proposals the moment it starts one — a proposal
//! accepted in the gap would belong to a leader that is deliberately standing down, and nobody
//! would own it (`docs/plans/phase-3.md` §6 race 7). It then makes sure the target's log is
//! current, because ordering a lagging node to campaign either fails (§5.4.1 refuses it) or
//! succeeds and loses entries. Only then does it send `TimeoutNow`, which tells the target to skip
//! its timeout; the resulting `RequestVote` carries `force`, so the voters' own leases — which
//! this leader granted — do not veto the election it asked for.
//!
//! **Check-quorum** is the involuntary half. A leader that has not heard from a majority within an
//! election timeout has probably been partitioned away, and it steps down rather than continuing
//! to believe it leads. Without this a partitioned leader keeps answering `ReadIndex` rounds from
//! its own stale state, and keeps a lease alive that stops the healthy side from replacing it.

use crate::core::{Raft, Role};
use crate::election::CampaignKind;
use crate::error::Result;
use crate::message::Message;
use crate::storage::LogStorage;
use crate::types::NodeId;

impl<S: LogStorage> Raft<S> {
    /// Asks the leader to hand office to `target`.
    pub(crate) fn transfer_leader(&mut self, target: NodeId) -> Result<()> {
        if self.role != Role::Leader {
            return Ok(());
        }
        if target == self.id {
            tracing::debug!(
                id = self.id,
                "declined a transfer to this node: it already leads"
            );
            return Ok(());
        }
        if !self.is_voter(target) {
            // A learner cannot win an election, so ordering it to campaign would only cost the
            // cluster this leader's silence until the transfer times out.
            tracing::warn!(
                id = self.id,
                target,
                "declined a transfer to a node that cannot vote"
            );
            return Ok(());
        }
        if self.lead_transferee == Some(target) {
            return Ok(());
        }

        self.lead_transferee = Some(target);
        // The transfer gets one election timeout to complete, measured from here.
        self.election_elapsed = 0;
        tracing::info!(id = self.id, target, "beginning a leadership transfer");
        self.nudge_transferee(target)
    }

    /// Sends `TimeoutNow` if the target is current, and otherwise sends it what it is missing.
    fn nudge_transferee(&mut self, target: NodeId) -> Result<()> {
        let last = self.log.last_index()?;
        let caught_up = self
            .progress
            .get(target)
            .is_some_and(|progress| progress.matched == last);
        if caught_up {
            self.send(Message::TimeoutNow {
                from: self.id,
                to: target,
                term: self.term,
            });
        } else {
            // §3.10: bring it up to date first. Ordering a lagging node to campaign either fails
            // the up-to-date check or, worse, succeeds — and a leader elected on a short log is
            // how committed entries disappear.
            self.send_append(target)?;
        }
        Ok(())
    }

    /// Called when a follower's progress moves, in case it was the transfer target catching up.
    pub(crate) fn maybe_finish_transfer(&mut self, from: NodeId) -> Result<()> {
        if self.lead_transferee == Some(from) {
            self.nudge_transferee(from)?;
        }
        Ok(())
    }

    /// Gives up on a transfer that has not completed within an election timeout.
    ///
    /// Abandoning it matters as much as starting it: while it is in flight the leader refuses
    /// proposals, so a transfer to a node that has died would otherwise stop the cluster
    /// accepting writes until something else deposed the leader.
    pub(crate) fn abort_transfer(&mut self) {
        if let Some(target) = self.lead_transferee.take() {
            tracing::info!(
                id = self.id,
                target,
                "abandoned a leadership transfer that timed out"
            );
        }
    }

    /// The target's side: campaign immediately, skipping the timeout.
    pub(crate) fn handle_timeout_now(&mut self, from: NodeId) -> Result<()> {
        if self.role == Role::Leader {
            return Ok(());
        }
        if !self.is_voter(self.id) {
            tracing::debug!(id = self.id, "ignored TimeoutNow: this node cannot vote");
            return Ok(());
        }
        tracing::info!(
            id = self.id,
            leader = from,
            "campaigning at the leader's request"
        );
        // A transfer campaign skips the pre-vote probe. The outgoing leader has already
        // established that the cluster is healthy and that this node's log is current, which is
        // exactly what a pre-vote round would have gone and asked.
        self.campaign(CampaignKind::Transfer)
    }

    /// §6.2: steps down if a majority has not been heard from within an election timeout.
    ///
    /// Also clears every peer's activity flag, so the next interval measures the next interval and
    /// not the whole of history.
    pub(crate) fn check_quorum_active(&mut self) -> bool {
        let conf = self.conf.current().clone();
        let own_id = self.id;
        let mut active = 0;
        for (id, progress) in self.progress.iter_mut() {
            if !conf.is_voter(id) {
                progress.recent_active = false;
                continue;
            }
            if id == own_id || progress.recent_active {
                active += 1;
            }
            progress.recent_active = false;
        }
        active >= conf.quorum()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::config::Config;
    use crate::core::Role;
    use crate::error::RaftError;
    use crate::message::Message;
    use crate::raw_node::RawNode;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
    use crate::types::{ConfChange, ConfChangeKind, ConfState};

    /// §3.10: the target takes office without the cluster waiting out an election timeout.
    #[test]
    fn a_transfer_hands_office_to_a_current_follower() {
        let mut group = Harness::new(&[1, 2, 3], 401);
        group.campaign(1);
        group.settle();
        group.propose(1, b"x");

        group.node_mut(1).transfer_leader(3);
        group.settle();
        assert_eq!(group.node(3).role(), Role::Leader);
        assert_eq!(group.node(1).role(), Role::Follower);
        assert!(group.node(3).term() > 1);
    }

    /// **Race 7.** The outgoing leader stops accepting proposals the moment it starts a transfer.
    /// One accepted in the gap would belong to a leader that is deliberately standing down.
    #[test]
    fn a_transferring_leader_refuses_proposals() {
        let mut group = Harness::new(&[1, 2, 3], 402);
        group.campaign(1);
        group.settle();

        group.node_mut(1).transfer_leader(2);
        assert!(matches!(
            group.node_mut(1).propose(Bytes::from_static(b"late")),
            Err(RaftError::LeadershipTransferInProgress(2))
        ));
        assert!(matches!(
            group
                .node_mut(1)
                .propose_conf_change(ConfChange::new(ConfChangeKind::AddVoter, 4)),
            Err(RaftError::LeadershipTransferInProgress(2))
        ));
    }

    /// A learner promoted mid-window does not depose the leader that promoted it.
    ///
    /// `check_quorum` asks whether a majority answered within *this* election-timeout window. A
    /// peer that became a voter part-way through one has had no chance to answer as a voter in
    /// the part already elapsed — and a promoted learner arrives with its flag cleared *by
    /// construction*, because [`Raft::check_quorum_active`] clears `recent_active` on every
    /// non-voter each window, however busily it was replicating a millisecond earlier.
    ///
    /// Counting that silence deposed the leader at the exact moment its region grew a replica.
    /// Under CPU starvation, where the window is wide in ticks and narrow in wall clock, a dozen
    /// regions promoted their caught-up learners and a dozen leaders stepped down inside the
    /// following second — regions with no leader, and `AddPeer` operators arriving at peers that
    /// could no longer propose (`docs/plans/debt-c1.md`).
    ///
    /// Every offset into the window is tried: the bug is a race, and only the offsets near the
    /// boundary lose it.
    #[test]
    fn a_promoted_voter_does_not_depose_the_leader_that_promoted_it() {
        const WINDOW: u64 = 10;
        for late in 0..WINDOW {
            // Pinned to one value, so "this many ticks before the boundary" is a fact about the
            // test rather than a draw from the election RNG.
            let mut group = Harness::with_config(&[1], 404, |config| {
                config.election_tick = (WINDOW, WINDOW);
            });
            group.campaign(1);
            group.settle();
            group
                .node_mut(1)
                .propose_conf_change(ConfChange::new(ConfChangeKind::AddLearner, 2))
                .unwrap();
            group.settle();

            // `late` ticks into the window, promote. Node 2 is not in this harness and so never
            // answers: the claim is that its silence *within this window* is not evidence, not
            // that it is reachable.
            group.tick(1, late);
            group
                .node_mut(1)
                .propose_conf_change(ConfChange::new(ConfChangeKind::AddVoter, 2))
                .unwrap();
            assert_eq!(
                group.node(1).status().conf.quorum(),
                2,
                "the promotion did not take, so this run proves nothing"
            );

            // Cross the boundary the window was already heading for.
            group.tick(1, WINDOW - late);
            assert_eq!(
                group.node(1).role(),
                Role::Leader,
                "a learner promoted {late} ticks into the window deposed the leader"
            );
        }
    }

    /// And the safeguard is intact: a voter that has had a whole window of its own to answer and
    /// has not still deposes the leader. What the promotion buys is one window of patience, not
    /// an exemption from §6.2.
    #[test]
    fn a_voter_silent_for_a_full_window_still_deposes_the_leader() {
        const WINDOW: u64 = 10;
        let mut group = Harness::with_config(&[1], 405, |config| {
            config.election_tick = (WINDOW, WINDOW);
        });
        group.campaign(1);
        group.settle();
        group
            .node_mut(1)
            .propose_conf_change(ConfChange::new(ConfChangeKind::AddLearner, 2))
            .unwrap();
        group.settle();
        group
            .node_mut(1)
            .propose_conf_change(ConfChange::new(ConfChangeKind::AddVoter, 2))
            .unwrap();

        group.tick(1, WINDOW);
        assert_eq!(
            group.node(1).role(),
            Role::Leader,
            "the window the promotion landed in is the one that is forgiven"
        );

        group.tick(1, WINDOW);
        assert_eq!(
            group.node(1).role(),
            Role::Follower,
            "a voter that said nothing for a window of its own must cost the leader its office"
        );
    }

    /// A lagging target is brought up to date first. Ordering it to campaign immediately either
    /// fails §5.4.1's check or — worse — succeeds, and a leader elected on a short log is how
    /// committed entries disappear.
    #[test]
    fn a_lagging_target_is_caught_up_before_it_is_told_to_campaign() {
        let mut group = Harness::with_config(&[1, 2, 3], 403, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();

        group.isolate(3);
        for _ in 0..3 {
            group.propose(1, b"x");
        }
        group.heal();

        // Node 3 is behind; the transfer must not send TimeoutNow yet. Inspected through the
        // harness rather than by taking a `Ready`, which would discard those messages with it.
        group.node_mut(1).transfer_leader(3);
        group.drain_ready();
        assert!(
            !group
                .pending_messages()
                .iter()
                .any(|message| matches!(message, Message::TimeoutNow { .. })),
            "a lagging target was told to campaign before it had the log"
        );

        group.settle();
        assert_eq!(
            group.node(3).role(),
            Role::Leader,
            "and it takes office once it is current"
        );
        assert_eq!(group.log_of(3), group.log_of(1));
    }

    /// A transfer that never completes is abandoned after one election timeout — otherwise a
    /// transfer to a dead node stops the cluster accepting writes indefinitely.
    #[test]
    fn a_transfer_that_times_out_is_abandoned() {
        let mut group = Harness::new(&[1, 2, 3], 404);
        group.campaign(1);
        group.settle();

        group.isolate(3);
        group.node_mut(1).transfer_leader(3);
        assert!(group.node_mut(1).propose(Bytes::from_static(b"x")).is_err());

        group.tick_and_settle(40);
        assert_eq!(group.node(1).role(), Role::Leader, "it kept office");
        assert!(
            group.node_mut(1).propose(Bytes::from_static(b"x")).is_ok(),
            "and takes proposals again"
        );
    }

    /// A learner cannot win an election, so ordering one to campaign would only cost the cluster
    /// this leader's silence until the attempt timed out.
    #[test]
    fn a_transfer_to_a_node_that_cannot_vote_is_declined() {
        let mut group = Harness::with_config(&[1, 2, 3], 405, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();
        group
            .node_mut(1)
            .propose_conf_change(ConfChange::new(ConfChangeKind::AddLearner, 4))
            .unwrap();
        group.settle();

        group.node_mut(1).transfer_leader(4);
        assert!(
            group.node_mut(1).propose(Bytes::from_static(b"x")).is_ok(),
            "the transfer was declined, so proposals are still accepted"
        );
    }

    /// `TimeoutNow` makes the target campaign at once, and its request carries `force` so the
    /// voters' leases — granted by the very leader that asked — do not veto it.
    #[test]
    fn timeout_now_campaigns_immediately_and_forces_past_the_lease() {
        let mut node = RawNode::new(
            Config::new(2, vec![1, 2, 3], 406),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        node.step(Message::AppendEntries {
            from: 1,
            to: 2,
            term: 4,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            context: Bytes::new(),
        })
        .unwrap();
        let _ = node.ready();

        node.step(Message::TimeoutNow {
            from: 1,
            to: 2,
            term: 4,
        })
        .unwrap();
        assert_eq!(
            node.role(),
            Role::Candidate,
            "a transfer campaign skips the pre-vote round"
        );
        assert!(node.ready().messages.iter().all(|message| matches!(
            message,
            Message::RequestVote {
                pre_vote: false,
                force: true,
                ..
            }
        )));
    }

    /// A `TimeoutNow` to a node that cannot vote is ignored rather than starting an election it
    /// could never win.
    #[test]
    fn timeout_now_on_a_learner_is_ignored() {
        let mut node = RawNode::new(
            Config::new(3, vec![1, 2], 407),
            MemStorage::with_conf_state(ConfState {
                voters: vec![1, 2],
                learners: vec![3],
            }),
        )
        .unwrap();
        node.step(Message::TimeoutNow {
            from: 1,
            to: 3,
            term: 4,
        })
        .unwrap();
        assert_eq!(node.role(), Role::Follower);
    }

    /// §6.2, the leader's half. A partitioned leader that kept believing it led would go on
    /// answering reads from state the rest of the cluster has moved past.
    #[test]
    fn a_leader_without_quorum_contact_steps_down() {
        let mut group = Harness::new(&[1, 2, 3], 408);
        group.campaign(1);
        group.settle();
        assert_eq!(group.node(1).role(), Role::Leader);

        group.isolate(1);
        group.tick_and_settle(40);
        // It gives up office, and then keeps campaigning without ever winning, which is what a
        // node alone on the wrong side of a partition should do.
        assert_ne!(
            group.node(1).role(),
            Role::Leader,
            "a partitioned leader must step down"
        );
        assert_eq!(group.node(1).leader(), None);
    }

    /// And the control: a leader that *is* in contact with a majority keeps office indefinitely,
    /// so the previous test is measuring the partition and not merely the passage of time.
    #[test]
    fn a_leader_in_contact_with_a_majority_keeps_office() {
        let mut group = Harness::new(&[1, 2, 3], 409);
        group.campaign(1);
        group.settle();
        let term = group.node(1).term();

        group.tick_and_settle(400);
        assert_eq!(group.leaders(), vec![(1, term)]);
    }

    /// Check-quorum counts voters only. A group whose learners outnumber its voters must not keep
    /// a partitioned leader in office on their contact alone.
    #[test]
    fn learners_do_not_count_toward_quorum_contact() {
        let mut group = Harness::with_config(&[1, 2, 3], 410, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();
        for learner in [4, 5, 6] {
            group
                .node_mut(1)
                .propose_conf_change(ConfChange::new(ConfChangeKind::AddLearner, learner))
                .unwrap();
            group.settle();
        }
        // Turn check-quorum on for the leader now that the group is shaped.
        group.node_mut(1).raft_mut().check_quorum = true;

        group.isolate(1);
        group.tick_and_settle(40);
        assert_ne!(
            group.node(1).role(),
            Role::Leader,
            "learners kept a partitioned leader alive"
        );
    }
}
