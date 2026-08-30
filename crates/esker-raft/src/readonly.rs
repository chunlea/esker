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

// TODO(step-4): step-4 (ReadIndex) is the first caller of every item here.
#![allow(dead_code)]

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

    /// Whether any round is outstanding.
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drops every round. Called when leadership changes: an unfinished round belongs to a term
    /// this node no longer owns, and answering it would be answering as a leader that is gone.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
    }
}
