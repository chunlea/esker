//! Elections: campaigning, voting, and the four ways a node changes what it thinks it is.
//!
//! Two rules carry almost all of the safety, and both are easy to write plausibly and wrongly.
//!
//! **A server grants at most one vote per term** (§5.2). Combined with quorum intersection, that
//! is the whole of Election Safety: two candidates cannot both collect a majority in one term,
//! because some voter would have had to vote twice. The rule is only as good as its durability,
//! which is why the vote leaves in the same [`Ready`](crate::Ready) as the response that depends
//! on it — see [`Ready`](crate::Ready)'s contract, rule 1.
//!
//! **A vote is granted only to a candidate whose log is at least as up to date as the voter's**
//! (§5.4.1). This is what lets a leader assume its own log is authoritative and never overwrite a
//! committed entry. "At least as up to date" compares the last entry's *term* first and only then
//! its index: a longer log from an older term loses to a shorter log from a newer one.
//!
//! On top of Figure 3.1 sit two mechanisms from §6.2 and §9.6 that only make sense together:
//!
//! * **Check-quorum** makes a follower that has heard from a healthy leader recently *refuse* to
//!   vote, so a single disconnected node cannot depose a working leader by campaigning.
//! * **Pre-vote** makes a node ask whether it *could* win before it bumps the term, so a node
//!   returning from a partition — whose term has run ahead — does not force the cluster to step
//!   down just by coming back.
//!
//! The exception that makes them work is leadership transfer: a `RequestVote` carrying `force` was
//! ordered by the leader itself, and check-quorum's lease must not veto it
//! (`docs/plans/phase-3.md` §6 races 6 and 7).

use crate::core::{Raft, Role};
use crate::error::Result;
use crate::message::Message;
use crate::progress::Progress;
use crate::storage::LogStorage;
use crate::types::{Entry, NodeId, Term};

/// Why a node is campaigning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CampaignKind {
    /// A pre-vote round: ask whether the election is winnable without adopting the term.
    PreElection,
    /// A real election in `term + 1`.
    Election,
    /// A real election ordered by the outgoing leader, which ignores voters' leases.
    Transfer,
}

/// The outcome of counting the votes received so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoteResult {
    /// Neither a majority for nor a majority against yet.
    Pending,
    /// A majority granted.
    Won,
    /// A majority refused; no further vote can change that.
    Lost,
}

impl<S: LogStorage> Raft<S> {
    /// Stands for election.
    ///
    /// With pre-vote on, an ordinary campaign starts as a probe: the node does not adopt the new
    /// term until a majority has said it would vote for it. A transfer skips the probe, because
    /// the outgoing leader has already established that the cluster is healthy and that this node
    /// should have it.
    pub(crate) fn campaign(&mut self, kind: CampaignKind) -> Result<()> {
        // A learner, or a node a configuration change removed, has no business campaigning: it
        // cannot win, and the attempt would disturb a cluster that is fine without it.
        if !self.is_voter(self.id) {
            tracing::debug!(
                id = self.id,
                "declined to campaign: not a voter in the current configuration"
            );
            return Ok(());
        }

        let pre_vote = kind == CampaignKind::PreElection;
        // A pre-vote asks about the *next* term without adopting it; a real campaign adopts it.
        let vote_term = self.term + 1;
        if pre_vote {
            self.become_pre_candidate();
        } else {
            self.become_candidate();
        }

        // A node always votes for itself; in a single-voter group that is already a majority.
        if self.poll(self.id, true) == VoteResult::Won {
            return if pre_vote {
                self.campaign(CampaignKind::Election)
            } else {
                self.become_leader()
            };
        }

        let last_log_index = self.log.last_index()?;
        let last_log_term = self.log.last_term();
        let force = kind == CampaignKind::Transfer;
        for peer in self.conf.current().voters.clone() {
            if peer == self.id {
                continue;
            }
            self.send(Message::RequestVote {
                from: self.id,
                to: peer,
                term: vote_term,
                last_log_index,
                last_log_term,
                pre_vote,
                force,
            });
        }
        Ok(())
    }

    /// Enters a pre-vote round.
    ///
    /// Deliberately does **not** call [`Raft::reset`]: the term and the vote are untouched, which
    /// is the whole point — a lost pre-vote costs the cluster nothing. The election timeout is
    /// still redrawn, so two nodes that pre-campaigned in lockstep do not do it again.
    pub(crate) fn become_pre_candidate(&mut self) {
        debug_assert_ne!(self.role, Role::Leader, "a leader does not pre-campaign");
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes.clear();
        self.reset_election_timeout();
        tracing::debug!(id = self.id, term = self.term, "became pre-candidate");
    }

    /// Adopts the next term and votes for itself (Figure 3.1, C1).
    pub(crate) fn become_candidate(&mut self) {
        debug_assert_ne!(self.role, Role::Leader, "a leader does not campaign");
        let next = self.term + 1;
        self.reset(next);
        self.vote = Some(self.id);
        self.role = Role::Candidate;
        tracing::debug!(id = self.id, term = self.term, "became candidate");
    }

    /// Takes office.
    ///
    /// The empty entry appended here is not ceremonial. §5.4.2 forbids committing an entry from an
    /// earlier term by counting replicas; a new leader therefore has no way to commit anything it
    /// inherited until it commits something of its own. Appending a no-op immediately means the
    /// backlog becomes committable as soon as the no-op does, rather than waiting for a client.
    pub(crate) fn become_leader(&mut self) -> Result<()> {
        debug_assert_ne!(
            self.role,
            Role::Follower,
            "a follower does not become leader directly"
        );
        self.reset(self.term);
        self.vote = Some(self.id);
        self.role = Role::Leader;
        self.leader = Some(self.id);

        let last = self.log.last_index()?;
        let conf = self.conf.current().clone();
        for peer in conf.members() {
            self.progress.insert(
                peer,
                Progress::new(last + 1, self.max_inflight_msgs, conf.is_learner(peer)),
            );
        }

        let index = last + 1;
        self.log.append(vec![Entry::empty(self.term, index)])?;
        if let Some(own) = self.progress.get_mut(self.id) {
            own.matched = index;
            own.become_replicate();
            own.recent_active = true;
        }
        tracing::debug!(id = self.id, term = self.term, index, "became leader");

        // A single-voter group has already committed the entry; a larger one starts replicating
        // it now rather than at the next heartbeat, because until it commits nothing this leader
        // inherited can commit either.
        self.maybe_commit()?;
        self.bcast_append()
    }

    /// Records a vote and says whether the election is decided.
    ///
    /// Votes from nodes that are not voters in the configuration **in force** do not count. A
    /// vote from a peer that a configuration change removed can still be in flight, and counting
    /// it would let a candidate win with a majority of a group that no longer exists
    /// (`docs/plans/phase-3.md` §6 race 9).
    pub(crate) fn poll(&mut self, from: NodeId, granted: bool) -> VoteResult {
        if let Err(at) = self.votes.binary_search_by_key(&from, |(id, _)| *id) {
            self.votes.insert(at, (from, granted));
        }
        let conf = self.conf.current();
        let quorum = conf.quorum();
        let mut for_count = 0;
        let mut against_count = 0;
        for (id, granted) in &self.votes {
            if !conf.is_voter(*id) {
                continue;
            }
            if *granted {
                for_count += 1;
            } else {
                against_count += 1;
            }
        }
        if for_count >= quorum {
            VoteResult::Won
        } else if against_count >= quorum {
            VoteResult::Lost
        } else {
            VoteResult::Pending
        }
    }

    /// Figure 3.1, V1 and V2, plus §6.2's lease and §9.6's pre-vote.
    pub(crate) fn handle_vote_request(
        &mut self,
        from: NodeId,
        term: Term,
        last_log_index: crate::types::Index,
        last_log_term: Term,
        pre_vote: bool,
    ) {
        // A node may vote for a candidate it has already voted for — the request was probably
        // retransmitted — and for anyone at all only if it has neither voted nor accepted a
        // leader this term. A pre-vote for a *future* term is answered on its merits without
        // recording anything, because nothing is being promised.
        let can_vote = self.vote == Some(from)
            || (self.vote.is_none() && self.leader.is_none())
            || (pre_vote && term > self.term);
        let granted = can_vote && self.log.is_up_to_date(last_log_index, last_log_term);

        if granted && !pre_vote {
            // The vote is recorded *before* the response is queued, so both leave in the same
            // `Ready` and the driver persists the vote before the candidate can count it.
            // Answering first is how a node votes twice in one term across a crash.
            self.vote = Some(from);
            self.election_elapsed = 0;
        }
        self.send(Message::RequestVoteResponse {
            from: self.id,
            to: from,
            // A pre-vote is answered in the term it asked about; a real vote in this node's own.
            term: if pre_vote { term } else { self.term },
            granted,
            pre_vote,
        });
        tracing::debug!(
            id = self.id,
            candidate = from,
            term,
            pre_vote,
            granted,
            "answered a vote request"
        );
    }

    /// Counts a vote, and acts once the election is decided (Figure 3.1, C2).
    pub(crate) fn handle_vote_response(
        &mut self,
        from: NodeId,
        granted: bool,
        pre_vote: bool,
    ) -> Result<()> {
        // A response has to match the round this node is actually running. Both rounds can be in
        // flight at once — a pre-vote answered late, after the real campaign started — and taking
        // a pre-vote grant for a real one would elect a leader on votes nobody committed to.
        let expected = match self.role {
            Role::PreCandidate => true,
            Role::Candidate => false,
            Role::Follower | Role::Leader => return Ok(()),
        };
        if pre_vote != expected {
            return Ok(());
        }
        match self.poll(from, granted) {
            VoteResult::Pending => Ok(()),
            VoteResult::Won => {
                if self.role == Role::PreCandidate {
                    self.campaign(CampaignKind::Election)
                } else {
                    self.become_leader()
                }
            }
            VoteResult::Lost => {
                // A lost pre-vote leaves the term alone, which is the point of it.
                self.become_follower(self.term, None);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::CampaignKind;
    use crate::config::Config;
    use crate::core::Role;
    use crate::message::Message;
    use crate::raw_node::RawNode;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
    use crate::types::{ConfState, Entry, HardState, Index, NodeId, Term};

    fn lone(id: NodeId, voters: &[NodeId], seed: u64) -> RawNode<MemStorage> {
        RawNode::new(
            Config::new(id, voters.to_vec(), seed),
            MemStorage::with_conf_state(ConfState::from_voters(voters.to_vec())),
        )
        .unwrap()
    }

    fn ask(from: NodeId, to: NodeId, term: Term, last: (Index, Term), pre_vote: bool) -> Message {
        Message::RequestVote {
            from,
            to,
            term,
            last_log_index: last.0,
            last_log_term: last.1,
            pre_vote,
            force: false,
        }
    }

    /// An empty append from `leader`, which is how a follower learns who leads and starts its
    /// check-quorum lease.
    fn heartbeat(leader: NodeId, to: NodeId, term: Term) -> Message {
        Message::AppendEntries {
            from: leader,
            to,
            term,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            context: Bytes::new(),
        }
    }

    /// Figure 3.1, C1 and C2: a candidate that collects a majority takes office, in the term it
    /// campaigned in, and its voters recorded the vote that put it there.
    #[test]
    fn a_candidate_with_a_majority_becomes_leader() {
        let mut group = Harness::new(&[1, 2, 3], 11);
        group.campaign(1);
        group.settle();
        assert_eq!(group.leaders(), vec![(1, 1)]);
        assert_eq!(group.node(2).status().voted_for, Some(1));
        assert_eq!(group.node(3).status().voted_for, Some(1));
    }

    /// §5.4.2: a new leader appends an empty entry of its own term, because nothing it inherited
    /// can be committed by counting until something of its own is.
    #[test]
    fn a_new_leader_appends_an_empty_entry_of_its_own_term() {
        let mut group = Harness::new(&[1, 2, 3], 11);
        group.campaign(1);
        group.settle();
        let log = group.log_of(1);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].term, group.node(1).term());
        assert!(log[0].data.is_empty());
    }

    /// A single voter is its own majority and needs no round trip.
    #[test]
    fn a_lone_voter_elects_itself() {
        let mut group = Harness::new(&[1], 3);
        group.campaign(1);
        assert_eq!(group.node(1).role(), Role::Leader);
    }

    /// Figure 3.1, C3. The leader could only have been elected by a majority, so continuing to
    /// campaign against it would cost the cluster another term for nothing.
    #[test]
    fn a_candidate_concedes_to_a_leader_of_its_own_term() {
        let mut node = lone(3, &[1, 2, 3], 5);
        node.campaign().unwrap();
        node.step(Message::RequestVoteResponse {
            from: 1,
            to: 3,
            term: 1,
            granted: true,
            pre_vote: true,
        })
        .unwrap();
        assert_eq!(
            node.role(),
            Role::Candidate,
            "the pre-vote carried it into a real campaign"
        );

        node.step(heartbeat(2, 3, node.term())).unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.leader(), Some(2));
    }

    /// **The trap.** Answering a vote before it is durable lets a node vote twice in one term
    /// across a crash, and two votes in one term is two leaders. The core cannot make the driver
    /// fsync, but it can guarantee the driver is *able* to: the `HardState` recording the vote and
    /// the response that depends on it leave in the same `Ready`, never in two.
    #[test]
    fn the_ready_that_grants_a_vote_carries_the_vote_it_recorded() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.step(ask(2, 1, 1, (0, 0), false)).unwrap();

        let ready = node.ready();
        assert_eq!(
            ready.hard_state,
            Some(HardState {
                term: 1,
                voted_for: Some(2),
                commit: 0
            }),
            "the vote must be in the same Ready as the response"
        );
        assert!(matches!(
            ready.messages.as_slice(),
            [Message::RequestVoteResponse {
                to: 2,
                granted: true,
                pre_vote: false,
                ..
            }]
        ));
    }

    /// A pre-vote promises nothing, so it records nothing. If it did, a probe that lost would
    /// still have burned this node's vote for a term it never entered.
    #[test]
    fn a_granted_pre_vote_records_no_vote() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.step(ask(2, 1, 1, (0, 0), true)).unwrap();
        let ready = node.ready();
        assert_eq!(
            ready.hard_state, None,
            "a pre-vote changes no durable state"
        );
        assert!(matches!(
            ready.messages.as_slice(),
            [Message::RequestVoteResponse {
                granted: true,
                pre_vote: true,
                term: 1,
                ..
            }]
        ));
        assert_eq!(node.term(), 0);
    }

    /// §5.4.1, from the voter's side. A candidate missing entries this node has must not win, or a
    /// leader would have to overwrite a committed entry to catch up.
    #[test]
    fn a_vote_is_refused_to_a_candidate_whose_log_is_behind() {
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        storage
            .append(&[Entry::empty(2, 1), Entry::empty(2, 2)])
            .unwrap();
        storage.set_hard_state(HardState {
            term: 2,
            voted_for: None,
            commit: 0,
        });
        let mut node = RawNode::new(Config::new(3, vec![1, 2, 3], 8), storage).unwrap();

        // Shorter log, same term: refused.
        node.step(ask(2, 3, 3, (1, 2), false)).unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [Message::RequestVoteResponse { granted: false, .. }]
        ));

        // Shorter log, but a *newer* last term: granted. The newer term's entries are the ones
        // that could already be committed, so length is the tie-breaker and never the test.
        let mut node = RawNode::new(Config::new(3, vec![1, 2, 3], 8), {
            let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
            storage
                .append(&[Entry::empty(2, 1), Entry::empty(2, 2)])
                .unwrap();
            storage.set_hard_state(HardState {
                term: 2,
                voted_for: None,
                commit: 0,
            });
            storage
        })
        .unwrap();
        node.step(ask(2, 3, 4, (1, 3), false)).unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [Message::RequestVoteResponse { granted: true, .. }]
        ));
    }

    /// A node votes once per term. The second candidate is refused even though its log is fine.
    #[test]
    fn only_one_vote_is_granted_per_term() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.step(ask(2, 1, 1, (0, 0), false)).unwrap();
        node.step(ask(3, 1, 1, (0, 0), false)).unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [
                Message::RequestVoteResponse {
                    to: 2,
                    granted: true,
                    ..
                },
                Message::RequestVoteResponse {
                    to: 3,
                    granted: false,
                    ..
                },
            ]
        ));
    }

    /// A retransmitted request from the same candidate is granted again: it is the same vote, not
    /// a second one.
    #[test]
    fn a_repeated_request_from_the_same_candidate_is_granted_again() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.step(ask(2, 1, 1, (0, 0), false)).unwrap();
        node.step(ask(2, 1, 1, (0, 0), false)).unwrap();
        assert!(
            node.ready()
                .messages
                .iter()
                .all(|m| matches!(m, Message::RequestVoteResponse { granted: true, .. }))
        );
    }

    /// A candidate refused by a majority gives up rather than waiting out its timeout — and, the
    /// part that matters, does not lower its term while doing so.
    #[test]
    fn a_candidate_refused_by_a_majority_reverts_to_follower() {
        let mut node = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1, 2, 3], 6)
            },
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        node.campaign().unwrap();
        assert_eq!(node.role(), Role::Candidate);
        let term = node.term();
        for voter in [2, 3] {
            node.step(Message::RequestVoteResponse {
                from: voter,
                to: 1,
                term,
                granted: false,
                pre_vote: false,
            })
            .unwrap();
        }
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(
            node.term(),
            term,
            "losing an election must not lower the term"
        );
    }

    /// §6.2. Without this, one disconnected node whose term has run ahead deposes a working leader
    /// simply by reappearing.
    #[test]
    fn check_quorum_makes_a_follower_refuse_a_vote_while_its_leader_is_healthy() {
        let mut node = lone(3, &[1, 2, 3], 13);
        node.step(heartbeat(1, 3, 5)).unwrap();
        let _ = node.ready();
        assert_eq!(node.leader(), Some(1));

        node.step(ask(2, 3, 99, (9, 9), false)).unwrap();
        let ready = node.ready();
        assert!(
            ready.messages.is_empty(),
            "a leased follower does not even reply"
        );
        assert_eq!(node.term(), 5, "and does not adopt the term");
    }

    /// The exception that makes leadership transfer possible: a request the leader itself ordered
    /// is not vetoed by the lease it granted.
    #[test]
    fn a_forced_vote_request_is_not_vetoed_by_the_lease() {
        let mut node = lone(3, &[1, 2, 3], 13);
        node.step(heartbeat(1, 3, 5)).unwrap();
        let _ = node.ready();

        node.step(Message::RequestVote {
            from: 2,
            to: 3,
            term: 6,
            last_log_index: 0,
            last_log_term: 0,
            pre_vote: false,
            force: true,
        })
        .unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [Message::RequestVoteResponse { granted: true, .. }]
        ));
    }

    /// §9.6: a node whose pre-vote cannot succeed learns so without having disturbed anyone. Its
    /// own term never moves, so neither does the cluster's.
    #[test]
    fn a_partitioned_node_running_pre_votes_never_raises_its_term() {
        let mut group = Harness::new(&[1, 2, 3], 21);
        group.campaign(1);
        group.settle();
        let leader_term = group.node(1).term();

        group.isolate(3);
        group.tick(3, 200);
        group.settle();
        assert_eq!(
            group.node(3).term(),
            leader_term,
            "a pre-vote does not bump the term"
        );
        assert_eq!(group.node(3).role(), Role::PreCandidate);

        // The other half of the claim — that the leader survives the node's *return* — needs
        // heartbeats to exist before it means anything: without them the remaining follower has
        // no leader and an empty log, so it votes for the returning node quite correctly. That is
        // `a_returning_node_does_not_depose_a_healthy_leader`, with the replication tests.
    }

    /// Without pre-vote the same return is disruptive: the returning node's term has run ahead, so
    /// the leader steps down and the cluster pays for an election it did not need. This is the
    /// test that makes the previous one mean something.
    #[test]
    fn without_pre_vote_a_returning_node_deposes_the_leader() {
        let mut group = Harness::with_config(&[1, 2, 3], 21, |config| {
            config.pre_vote = false;
            config.check_quorum = false;
        });
        group.campaign(1);
        group.settle();
        let leader_term = group.node(1).term();

        group.isolate(3);
        group.tick(3, 200);
        group.settle();
        assert!(
            group.node(3).term() > leader_term,
            "a bare campaign raises the term"
        );

        group.heal();
        group.tick(3, 25);
        group.settle();
        assert!(
            group.node(1).term() > leader_term,
            "and the leader had to step down"
        );
        assert_ne!(group.node(1).role(), Role::Leader);
    }

    /// The randomisation trap. Every node here shares one seed — which is how a simulator
    /// reproduces a run from a single number — and they must still not campaign in lockstep
    /// forever. The timeout is drawn per node *and* redrawn per election, so a tie is a delay.
    #[test]
    fn a_group_sharing_one_seed_still_elects_someone() {
        for seed in 0..32 {
            let mut group = Harness::new(&[1, 2, 3, 4, 5], seed);
            let mut elected = false;
            for _ in 0..200 {
                group.tick_all();
                group.settle();
                if !group.leaders().is_empty() {
                    elected = true;
                    break;
                }
            }
            assert!(elected, "seed {seed}: five nodes on one seed tied forever");
        }
    }

    /// A learner is not a candidate. It replicates so a new replica can catch up without making
    /// elections harder while it does.
    #[test]
    fn a_learner_does_not_campaign() {
        let conf = ConfState {
            voters: vec![1, 2],
            learners: vec![3],
        };
        let mut node = RawNode::new(
            Config::new(3, vec![1, 2], 2),
            MemStorage::with_conf_state(conf),
        )
        .unwrap();
        node.campaign().unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), 0);
        assert!(!node.has_ready());
    }

    /// A vote from a peer a configuration change removed is still in flight somewhere. Counting it
    /// would let a candidate win with a majority of a group that no longer exists.
    #[test]
    fn votes_from_nodes_outside_the_configuration_do_not_count() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.campaign().unwrap();
        assert_eq!(node.role(), Role::PreCandidate);
        for stranger in [7, 8, 9] {
            node.step(Message::RequestVoteResponse {
                from: stranger,
                to: 1,
                term: 1,
                granted: true,
                pre_vote: true,
            })
            .unwrap();
        }
        assert_eq!(
            node.role(),
            Role::PreCandidate,
            "strangers cannot elect a leader"
        );
    }

    /// A pre-vote grant arriving after the real campaign started must not be counted as a real
    /// vote. Both rounds can be in flight at once, and taking one for the other would elect a
    /// leader on votes nobody committed to.
    #[test]
    fn a_late_pre_vote_grant_does_not_count_toward_a_real_election() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.campaign().unwrap();
        node.step(Message::RequestVoteResponse {
            from: 2,
            to: 1,
            term: 1,
            granted: true,
            pre_vote: true,
        })
        .unwrap();
        assert_eq!(node.role(), Role::Candidate, "the pre-vote round succeeded");

        // The straggler from the pre-vote round arrives now.
        node.step(Message::RequestVoteResponse {
            from: 3,
            to: 1,
            term: 1,
            granted: true,
            pre_vote: true,
        })
        .unwrap();
        assert_eq!(
            node.role(),
            Role::Candidate,
            "a pre-vote grant is not a vote"
        );
    }

    /// A transfer campaign skips the probe: the outgoing leader has already established that the
    /// cluster is healthy, and the request carries `force` so voters' leases do not veto it.
    #[test]
    fn a_transfer_campaign_skips_the_pre_vote_round() {
        let mut node = lone(2, &[1, 2, 3], 17);
        node.raft_mut().campaign(CampaignKind::Transfer).unwrap();
        assert_eq!(node.role(), Role::Candidate);
        assert!(node.ready().messages.iter().all(|message| matches!(
            message,
            Message::RequestVote {
                pre_vote: false,
                force: true,
                ..
            }
        )));
    }

    /// Figure 3.1's V1 says to reply false to a stale request; we stay quiet instead. A candidate
    /// a term behind cannot win whatever this node says, and answering a node stuck in a campaign
    /// loop just keeps it company. A stale *pre-vote* is still answered, because that reply is how
    /// a node behind the cluster discovers it is behind.
    #[test]
    fn a_vote_request_from_an_older_term_is_ignored() {
        let mut node = lone(1, &[1, 2, 3], 4);
        node.step(heartbeat(2, 1, 7)).unwrap();
        let _ = node.ready();

        node.step(ask(3, 1, 2, (9, 9), false)).unwrap();
        assert!(
            node.ready().messages.is_empty(),
            "a stale vote request gets no answer"
        );

        node.step(ask(3, 1, 2, (9, 9), true)).unwrap();
        assert!(
            matches!(
                node.ready().messages.as_slice(),
                [Message::RequestVoteResponse {
                    granted: false,
                    pre_vote: true,
                    term: 7,
                    ..
                }]
            ),
            "a stale pre-vote is refused with this node's term, so its sender learns"
        );
    }

    /// A node that restarts with a vote already recorded must not vote again in that term. This is
    /// the crash half of the durability rule the `Ready` test covers.
    #[test]
    fn a_recorded_vote_survives_a_restart_and_is_not_cast_twice() {
        let mut storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        storage.set_hard_state(HardState {
            term: 4,
            voted_for: Some(2),
            commit: 0,
        });
        let mut node = RawNode::new(Config::new(1, vec![1, 2, 3], 4), storage).unwrap();
        assert_eq!(node.term(), 4);

        node.step(ask(3, 1, 4, (0, 0), false)).unwrap();
        assert!(matches!(
            node.ready().messages.as_slice(),
            [Message::RequestVoteResponse {
                to: 3,
                granted: false,
                ..
            }]
        ));
    }
}
