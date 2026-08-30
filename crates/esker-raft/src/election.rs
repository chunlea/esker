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
        // A leader campaigning against itself would depose itself for nothing. `campaign` is
        // public, so this is a caller's mistake to absorb rather than a panic to inflict
        // (`CLAUDE.md` invariant 9).
        if self.role == Role::Leader {
            tracing::debug!(id = self.id, "declined to campaign: already the leader");
            return Ok(());
        }
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
        // Saturating because a message off the network can carry any term at all, including the
        // last one: a node that has been pushed to `Term::MAX` can never win another election, but
        // it must not panic trying (`CLAUDE.md` invariant 9).
        let vote_term = self.term.saturating_add(1);
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
        let next = self.term.saturating_add(1);
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
        // §4.1: nothing this leader inherited may be assumed committed until it has committed
        // something of its own term, so no configuration change may be proposed until then.
        self.pending_conf_index = last;
        let conf = self.conf.current().clone();
        for peer in conf.members() {
            self.progress.insert(
                peer,
                Progress::new(last + 1, self.max_inflight_msgs, conf.is_learner(peer)),
            );
        }

        let index = last.saturating_add(1);
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
mod tests;
