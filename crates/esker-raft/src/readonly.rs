//! `ReadIndex`: how a leader answers a linearizable read without writing to the log.
//!
//! A read served from a leader's own state is only linearizable if the leader is still the leader
//! *at the moment it answers*. Leadership is not a fact a node can check locally — a partitioned
//! leader believes it is one — so the leader has to ask: it records its commit index, broadcasts a
//! heartbeat tagged with the read's context, and once a quorum has answered that heartbeat it
//! knows no other leader could have been elected in the meantime. The recorded index is then a
//! valid linearization point (`docs/DESIGN.md` §2).
//!
//! Two rules fall out of that, and both are testable:
//!
//! * The round only counts in the **current term**. A quorum of responses from an old term proves
//!   nothing about who leads now.
//! * The read may be answered only once the state machine has applied through the recorded index
//!   (`docs/plans/phase-3.md` §4 rule 4). The core hands out the index; the driver waits.

use bytes::Bytes;

use crate::types::{Index, NodeId};

/// One outstanding `ReadIndex` round.
#[derive(Debug, Clone)]
pub(crate) struct ReadIndexRound {
    /// The commit index recorded when the round started.
    pub(crate) index: Index,
    /// The caller's tag, which also identifies the round in heartbeat responses.
    pub(crate) ctx: Bytes,
    /// Which peers have confirmed the leader within this round, sorted; the leader itself counts.
    acks: Vec<NodeId>,
    /// The follower that forwarded this read, if it was not raised locally.
    pub(crate) from: Option<NodeId>,
}

impl ReadIndexRound {
    /// Records `node` as having confirmed the leader. Returns the number of distinct confirmations.
    fn ack(&mut self, node: NodeId) -> usize {
        if let Err(at) = self.acks.binary_search(&node) {
            self.acks.insert(at, node);
        }
        self.acks.len()
    }
}

/// The leader's outstanding read rounds, oldest first.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadOnly {
    pending: Vec<ReadIndexRound>,
    /// Reads received before this leader had committed anything in its own term.
    ///
    /// A leader's commit index is only trustworthy once an entry of *its* term has committed: until
    /// then it has inherited a commit index it cannot vouch for, and answering a read at that index
    /// could return a state older than a write that is already committed elsewhere (§6.4). A new
    /// leader appends a no-op immediately, so the wait is short — but it is not zero, and a read
    /// that arrives inside it has to wait rather than be answered wrongly.
    postponed: Vec<(Bytes, Option<NodeId>)>,
}

impl ReadOnly {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Starts a round at `index`. A context already in flight is not started twice — the caller
    /// gets the earlier round's answer, which is at least as recent as it asked for.
    pub(crate) fn add_request(
        &mut self,
        index: Index,
        ctx: Bytes,
        from: Option<NodeId>,
        leader: NodeId,
    ) {
        if self.pending.iter().any(|round| round.ctx == ctx) {
            return;
        }
        let mut round = ReadIndexRound {
            index,
            ctx,
            acks: Vec::new(),
            from,
        };
        round.ack(leader);
        self.pending.push(round);
    }

    /// Records a heartbeat response tagged with `ctx`, returning the confirmations that round now
    /// has. `None` if the context does not name a round — a response from a round already
    /// completed, or from before this leader's term.
    pub(crate) fn record_ack(&mut self, ctx: &Bytes, node: NodeId) -> Option<usize> {
        self.pending
            .iter_mut()
            .find(|round| round.ctx == *ctx)
            .map(|round| round.ack(node))
    }

    /// Completes every round up to and including the one named by `ctx`, and returns them.
    ///
    /// Older rounds complete with it because heartbeats are ordered: a quorum that has answered
    /// this round has necessarily answered the ones the leader sent before it.
    pub(crate) fn advance(&mut self, ctx: &Bytes) -> Vec<ReadIndexRound> {
        let Some(at) = self.pending.iter().position(|round| round.ctx == *ctx) else {
            return Vec::new();
        };
        self.pending.drain(..=at).collect()
    }

    /// The context of the most recent round, which is what a heartbeat should carry.
    pub(crate) fn last_pending_ctx(&self) -> Option<Bytes> {
        self.pending.last().map(|round| round.ctx.clone())
    }

    /// Holds a read until this leader has committed an entry of its own term.
    pub(crate) fn postpone(&mut self, ctx: Bytes, from: Option<NodeId>) {
        if self.postponed.iter().any(|(pending, _)| *pending == ctx) {
            return;
        }
        self.postponed.push((ctx, from));
    }

    /// Takes the postponed reads, for a leader that can now answer them.
    pub(crate) fn take_postponed(&mut self) -> Vec<(Bytes, Option<NodeId>)> {
        core::mem::take(&mut self.postponed)
    }

    /// Drops every round. Called when leadership changes: an unfinished round belongs to a term
    /// this node no longer owns, and answering it would be answering as a leader that is gone.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.postponed.clear();
    }
}

use crate::core::{Raft, Role};
use crate::error::Result;
use crate::message::Message;
use crate::storage::LogStorage;
use crate::types::ReadState;

impl<S: LogStorage> Raft<S> {
    /// Starts a linearizable read.
    ///
    /// `from` is the follower that forwarded it, or `None` when it was raised on this node.
    pub(crate) fn read_index(&mut self, ctx: Bytes, from: Option<NodeId>) -> Result<()> {
        if self.role != Role::Leader {
            // Only the leader can establish a read's index. A follower that does not know one
            // drops the request; the caller retries, which is what it would have to do anyway
            // while the cluster has no leader.
            if let Some(leader) = self.leader {
                self.send(Message::ReadIndex {
                    from: self.id,
                    to: leader,
                    term: self.term,
                    ctx,
                });
            }
            return Ok(());
        }

        if !self.has_committed_in_current_term() {
            self.read_only.postpone(ctx, from);
            return Ok(());
        }

        let index = self.log.committed;
        // A single voter is its own quorum: there is no other node that could have been elected
        // without its vote, so it does not have to ask.
        if self.conf.current().quorum() == 1 && self.is_voter(self.id) {
            self.answer_read(index, ctx, from);
            return Ok(());
        }

        self.read_only
            .add_request(index, ctx.clone(), from, self.id);
        self.bcast_heartbeat(&ctx)
    }

    /// Whether this leader's commit index is one it can vouch for.
    ///
    /// A leader inherits a commit index from its predecessor and cannot prove anything about it
    /// until an entry of its own term commits. Until then, reads wait.
    pub(crate) fn has_committed_in_current_term(&self) -> bool {
        self.log
            .term(self.log.committed)
            .is_ok_and(|term| term == self.term)
    }

    /// Counts a heartbeat response toward the round its context names, and completes the round
    /// once a quorum has confirmed the leader.
    pub(crate) fn record_read_ack(&mut self, from: NodeId, context: &Bytes) {
        if context.is_empty() || self.role != Role::Leader || !self.is_voter(from) {
            return;
        }
        let Some(acks) = self.read_only.record_ack(context, from) else {
            return;
        };
        if acks < self.conf.current().quorum() {
            return;
        }
        // Heartbeats are ordered, so a quorum that answered this round necessarily answered every
        // round the leader started before it.
        for round in self.read_only.advance(context) {
            self.answer_read(round.index, round.ctx, round.from);
        }
    }

    /// Delivers a completed read: to the driver if it was raised here, to the follower otherwise.
    fn answer_read(&mut self, index: Index, ctx: Bytes, from: Option<NodeId>) {
        match from {
            None => self.read_states.push(ReadState { index, ctx }),
            Some(follower) => self.send(Message::ReadIndexResponse {
                from: self.id,
                to: follower,
                term: self.term,
                index,
                ctx,
            }),
        }
    }

    /// Records the leader's answer to a read this node forwarded.
    pub(crate) fn handle_read_index_response(&mut self, index: Index, ctx: Bytes) {
        self.read_states.push(ReadState { index, ctx });
    }

    /// Releases reads that were waiting for this leader to commit something of its own term.
    pub(crate) fn flush_postponed_reads(&mut self) -> Result<()> {
        if !self.has_committed_in_current_term() {
            return Ok(());
        }
        for (ctx, from) in self.read_only.take_postponed() {
            self.read_index(ctx, from)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::config::Config;
    use crate::message::Message;
    use crate::raw_node::RawNode;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
    use crate::types::{ConfState, ReadState};

    const CTX: Bytes = Bytes::from_static(b"read-1");

    /// The round confirms the leader is still the leader, and the index it hands back is the
    /// commit index it held when the round started.
    #[test]
    fn a_read_is_answered_at_the_commit_index_after_a_heartbeat_quorum() {
        let mut group = Harness::new(&[1, 2, 3], 101);
        group.campaign(1);
        group.settle();
        group.propose(1, b"a");
        group.propose(1, b"b");
        let committed = group.commit_of(1);

        group.node_mut(1).read_index(CTX);
        group.settle();
        let states = group.take_read_states(1);
        assert_eq!(
            states,
            vec![ReadState {
                index: committed,
                ctx: CTX
            }]
        );
    }

    /// A single voter cannot have been deposed without its own vote, so it answers without asking.
    #[test]
    fn a_lone_voter_answers_a_read_without_a_round_trip() {
        let mut group = Harness::new(&[1], 102);
        group.campaign(1);
        group.settle();

        group.node_mut(1).read_index(CTX);
        let ready = group.node_mut(1).ready();
        assert_eq!(ready.read_states.len(), 1);
        assert!(
            ready.messages.is_empty(),
            "a lone voter has nobody to ask and should not have tried"
        );
    }

    /// §6.4. Until an entry of the leader's own term commits, its commit index is inherited and
    /// unproven: answering a read there could return a state older than a write that is already
    /// committed elsewhere. The read waits, and is released the moment the leader's no-op commits.
    #[test]
    fn a_read_waits_until_the_leader_has_committed_in_its_own_term() {
        let mut leader = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1, 2, 3], 103)
            },
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        leader.campaign().unwrap();
        for voter in [2, 3] {
            leader
                .step(Message::RequestVoteResponse {
                    from: voter,
                    to: 1,
                    term: 1,
                    granted: true,
                    pre_vote: false,
                })
                .unwrap();
        }
        // The no-op is appended but nothing has acknowledged it yet.
        assert_eq!(leader.commit_index(), 0);
        leader.read_index(CTX);
        let ready = leader.ready();
        assert!(
            ready.read_states.is_empty(),
            "the read must not be answered on an unproven index"
        );
        leader.storage_mut().append(&ready.entries).unwrap();
        leader.advance(&ready);

        // Node 2 acknowledges the no-op, which commits it — and releases the read.
        leader
            .step(Message::AppendEntriesResponse {
                from: 2,
                to: 1,
                term: 1,
                reject: false,
                index: 1,
                hint_term: 0,
                context: Bytes::new(),
            })
            .unwrap();
        assert_eq!(leader.commit_index(), 1);
        let ready = leader.ready();
        // The read is now in flight as a heartbeat round rather than answered outright.
        assert!(
            ready.messages.iter().any(|message| matches!(
                message,
                Message::AppendEntries { context, .. } if *context == CTX
            )),
            "the postponed read should have started its round once the term's entry committed"
        );
    }

    /// A follower cannot establish a read's index — only the leader knows whether it is still the
    /// leader — so it forwards, and reports the answer it gets back.
    #[test]
    fn a_follower_forwards_a_read_and_reports_the_answer() {
        let mut group = Harness::new(&[1, 2, 3], 104);
        group.campaign(1);
        group.settle();
        group.propose(1, b"a");
        let committed = group.commit_of(1);

        group.node_mut(3).read_index(CTX);
        group.settle();
        assert_eq!(
            group.take_read_states(3),
            vec![ReadState {
                index: committed,
                ctx: CTX
            }]
        );
        assert!(
            group.take_read_states(1).is_empty(),
            "the leader answered on the follower's behalf"
        );
    }

    /// A follower with no leader has nowhere to send the read. Dropping it is the honest outcome:
    /// the caller retries, which is what it would have to do anyway.
    #[test]
    fn a_read_on_a_node_that_knows_no_leader_is_dropped() {
        let mut node = RawNode::new(
            Config::new(2, vec![1, 2, 3], 105),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap();
        node.read_index(CTX);
        assert!(!node.has_ready());
    }

    /// A leader that has lost contact with a quorum cannot confirm it still leads, so it produces
    /// no read state at all. Answering anyway is precisely the stale read `ReadIndex` prevents.
    #[test]
    fn a_leader_without_a_quorum_answers_no_read() {
        let mut group = Harness::with_config(&[1, 2, 3], 106, |config| config.check_quorum = false);
        group.campaign(1);
        group.settle();
        group.propose(1, b"a");

        group.isolate(1);
        group.node_mut(1).read_index(CTX);
        group.settle();
        assert!(
            group.take_read_states(1).is_empty(),
            "a partitioned leader answered a read"
        );
    }

    /// Heartbeats are ordered, so a quorum that answered the newest round has answered every
    /// earlier one. All of them complete together rather than one per round trip.
    #[test]
    fn an_earlier_round_completes_with_a_later_one() {
        let mut group = Harness::new(&[1, 2, 3], 107);
        group.campaign(1);
        group.settle();
        group.propose(1, b"a");

        group.node_mut(1).read_index(Bytes::from_static(b"first"));
        group.node_mut(1).read_index(Bytes::from_static(b"second"));
        group.settle();
        let states = group.take_read_states(1);
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].ctx, Bytes::from_static(b"first"));
        assert_eq!(states[1].ctx, Bytes::from_static(b"second"));
    }

    /// A round belongs to the term it started in. When leadership changes, an unfinished one is
    /// dropped: answering it would be answering as a leader that is gone.
    #[test]
    fn a_read_does_not_survive_a_change_of_leadership() {
        let mut group = Harness::with_config(&[1, 2, 3], 108, |config| {
            config.pre_vote = false;
            config.check_quorum = false;
        });
        group.campaign(1);
        group.settle();
        group.propose(1, b"a");

        group.isolate(1);
        group.node_mut(1).read_index(CTX);
        group.heal();
        group.campaign(2);
        group.settle();
        assert!(
            group.take_read_states(1).is_empty(),
            "a deposed leader answered its own read"
        );
    }
}
