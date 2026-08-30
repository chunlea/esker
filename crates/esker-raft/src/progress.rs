//! What the leader believes about each follower.
//!
//! The container is a **sorted `Vec`, not a `HashMap`**. Every leader decision — who to send to,
//! whether a quorum has acknowledged an index, which peer to step down for — reads this, and a
//! `HashMap`'s iteration order would make those decisions depend on hash seeding rather than on
//! state. That breaks reproducibility silently and only under some seeds, which is the worst way
//! for a simulator to fail (`docs/plans/phase-3.md` §7 risk 2).
//!
//! The per-follower state machine is etcd's, and it earns its three states:
//!
//! * [`ProgressState::Probe`] — the leader does not know where this follower's log agrees with its
//!   own. It sends one message and waits, because sending more before the answer arrives would
//!   just be more guesses at the same unknown.
//! * [`ProgressState::Replicate`] — agreement is known, so the leader pipelines up to
//!   `max_inflight_msgs` batches without waiting.
//! * [`ProgressState::Snapshot`] — the follower needs entries the leader has compacted away.
//!   Nothing is sent until the snapshot is acknowledged.

// TODO(step-2): step-2 (replication) is the first caller of every item here.
#![allow(dead_code)]

use std::collections::VecDeque;

use crate::types::{Index, NodeId};

/// How the leader is currently talking to one follower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressState {
    /// Searching for the index where the logs agree; one message in flight at a time.
    Probe,
    /// Agreement found; pipelining.
    Replicate,
    /// A snapshot is in flight; nothing else is sent.
    Snapshot,
}

/// A bounded window of in-flight append messages, tracked by the last index each carried.
///
/// This is the flow control: without it, a leader with a fast log and a slow follower queues
/// unbounded work into the transport, and the first thing to suffer is the heartbeat that keeps
/// the leader in office.
#[derive(Debug, Clone)]
pub(crate) struct Inflights {
    window: VecDeque<Index>,
    capacity: usize,
}

impl Inflights {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Whether the leader must stop sending to this follower.
    pub(crate) fn is_full(&self) -> bool {
        self.window.len() >= self.capacity
    }

    /// Records a message carrying up to `last`.
    pub(crate) fn add(&mut self, last: Index) {
        if self.is_full() {
            return;
        }
        self.window.push_back(last);
    }

    /// Releases every message the follower has now acknowledged.
    pub(crate) fn free_to(&mut self, acked: Index) {
        while self.window.front().is_some_and(|last| *last <= acked) {
            self.window.pop_front();
        }
    }

    /// Releases one slot. A heartbeat response proves the follower is alive but says nothing about
    /// which append it processed, so it buys back exactly one slot — enough to keep a stalled
    /// pipeline from deadlocking, not enough to pretend the window drained.
    pub(crate) fn free_one(&mut self) {
        self.window.pop_front();
    }

    pub(crate) fn reset(&mut self) {
        self.window.clear();
    }
}

/// The leader's view of one peer.
#[derive(Debug, Clone)]
pub(crate) struct Progress {
    /// The highest index known to be replicated on this peer.
    pub(crate) matched: Index,
    /// The next index to send. A guess in [`ProgressState::Probe`], a fact in
    /// [`ProgressState::Replicate`].
    pub(crate) next: Index,
    /// How the leader is talking to this peer.
    pub(crate) state: ProgressState,
    /// Whether this peer has been heard from within the current election timeout. Check-quorum
    /// reads and then clears it (§6.2).
    pub(crate) recent_active: bool,
    /// In [`ProgressState::Probe`], whether the one allowed message is outstanding.
    pub(crate) probe_sent: bool,
    /// The index of the snapshot in flight, or 0.
    pub(crate) pending_snapshot: Index,
    /// Learners replicate but do not count toward a quorum.
    pub(crate) is_learner: bool,
    /// The in-flight window used in [`ProgressState::Replicate`].
    pub(crate) inflights: Inflights,
}

impl Progress {
    pub(crate) fn new(next: Index, max_inflight: usize, is_learner: bool) -> Self {
        Self {
            matched: 0,
            next,
            state: ProgressState::Probe,
            recent_active: false,
            probe_sent: false,
            pending_snapshot: 0,
            is_learner,
            inflights: Inflights::new(max_inflight),
        }
    }

    /// Back to probing: the leader no longer knows where the logs agree.
    pub(crate) fn become_probe(&mut self) {
        // Coming back from a snapshot, the follower is known to have at least the snapshot's
        // index, so probing restarts from there rather than from scratch.
        if self.state == ProgressState::Snapshot {
            let pending = self.pending_snapshot;
            self.next = self.matched.max(pending) + 1;
        } else {
            self.next = self.matched + 1;
        }
        self.state = ProgressState::Probe;
        self.pending_snapshot = 0;
        self.probe_sent = false;
        self.inflights.reset();
    }

    /// Agreement is known; start pipelining.
    pub(crate) fn become_replicate(&mut self) {
        self.state = ProgressState::Replicate;
        self.next = self.matched + 1;
        self.pending_snapshot = 0;
        self.probe_sent = false;
        self.inflights.reset();
    }

    /// A snapshot at `index` is on its way.
    pub(crate) fn become_snapshot(&mut self, index: Index) {
        self.state = ProgressState::Snapshot;
        self.pending_snapshot = index;
        self.probe_sent = false;
        self.inflights.reset();
    }

    /// Records an acknowledgement. Returns whether it moved `matched` forward — a duplicate or
    /// reordered response must not be counted twice toward a quorum.
    pub(crate) fn maybe_update(&mut self, acked: Index) -> bool {
        let advanced = self.matched < acked;
        if advanced {
            self.matched = acked;
            self.probe_sent = false;
        }
        self.next = self.next.max(acked + 1);
        advanced
    }

    /// Applies a rejection, using the follower's hint to skip a whole term instead of walking back
    /// one index per round trip.
    ///
    /// Returns whether `next` moved. A rejection that does not move it is stale — the leader
    /// already backed off past it — and acting on it would undo real progress.
    pub(crate) fn maybe_decr_to(&mut self, rejected: Index, hint_index: Index) -> bool {
        if self.state == ProgressState::Replicate {
            // In replicate mode `next` is known, so only a rejection below `matched` is news, and
            // that cannot happen: `matched` entries are acknowledged. Anything else is stale.
            if rejected <= self.matched {
                return false;
            }
            self.next = self.matched + 1;
            return true;
        }
        if self.next == 0 || rejected != self.next - 1 {
            // Not the rejection for the message we are waiting on.
            return false;
        }
        self.next = hint_index
            .max(1)
            .min(self.next.saturating_sub(1))
            .max(self.matched + 1);
        self.probe_sent = false;
        true
    }

    /// Whether the leader must not send to this peer right now.
    pub(crate) fn is_paused(&self) -> bool {
        match self.state {
            ProgressState::Probe => self.probe_sent,
            ProgressState::Replicate => self.inflights.is_full(),
            ProgressState::Snapshot => true,
        }
    }
}

/// Every peer's [`Progress`], keyed by id and kept sorted.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProgressMap {
    peers: Vec<(NodeId, Progress)>,
}

impl ProgressMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, id: NodeId) -> Option<&Progress> {
        self.index_of(id).map(|at| &self.peers[at].1)
    }

    pub(crate) fn get_mut(&mut self, id: NodeId) -> Option<&mut Progress> {
        self.index_of(id).map(|at| &mut self.peers[at].1)
    }

    pub(crate) fn insert(&mut self, id: NodeId, progress: Progress) {
        if let Some(at) = self.index_of(id) {
            self.peers[at].1 = progress;
        } else {
            let at = self.peers.partition_point(|(existing, _)| *existing < id);
            self.peers.insert(at, (id, progress));
        }
    }

    pub(crate) fn remove(&mut self, id: NodeId) {
        if let Some(at) = self.index_of(id) {
            self.peers.remove(at);
        }
    }

    pub(crate) fn contains(&self, id: NodeId) -> bool {
        self.index_of(id).is_some()
    }

    /// Every peer in id order. The order is part of the algorithm's determinism, not incidental.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (NodeId, &Progress)> {
        self.peers.iter().map(|(id, progress)| (*id, progress))
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (NodeId, &mut Progress)> {
        self.peers.iter_mut().map(|(id, progress)| (*id, progress))
    }

    pub(crate) fn ids(&self) -> Vec<NodeId> {
        self.peers.iter().map(|(id, _)| *id).collect()
    }

    fn index_of(&self, id: NodeId) -> Option<usize> {
        self.peers
            .binary_search_by_key(&id, |(existing, _)| *existing)
            .ok()
    }
}
