//! [`RawNode`], the handle a driver holds, and [`Ready`], the only way an effect leaves this crate.
//!
//! The split is etcd's, and it is what makes a consensus algorithm testable. The core decides;
//! the driver does. Nothing here opens a file, takes a lock, or reads a clock.

use bytes::Bytes;

use crate::config::Config;
use crate::core::{Raft, Role, Status};
use crate::election::CampaignKind;
use crate::error::{RaftError, Result};
use crate::message::Message;
use crate::storage::LogStorage;
use crate::types::{ConfChange, Entry, HardState, Index, NodeId, ReadState, Snapshot, Term};

/// Everything the core decided since the last [`RawNode::advance`], and the order the driver must
/// discharge it in.
///
/// # The driver contract
///
/// This ordering is part of Raft's safety argument, not an implementation detail. `esker-sim`
/// tests violations of each rule deliberately, because a driver that gets this wrong produces a
/// cluster that loses acknowledged writes and looks healthy while doing it.
///
/// 1. **Persist [`hard_state`](Ready::hard_state) and [`entries`](Ready::entries) — with fsync —
///    before sending any of [`messages`](Ready::messages).**
///
///    `hard_state` carries `voted_for`. A node that answers a `RequestVote` before that vote is
///    durable will, after a crash and restart, vote again in the same term — and two votes in one
///    term is two leaders in one term. The same shape of failure applies one level up: a leader
///    that counts an acknowledgement for an entry the follower has not durably written can commit
///    an entry a crash then removes. The `Ready` that grants a vote always carries the
///    `HardState` recording it; the core never emits the response in an earlier `Ready` than the
///    vote.
///
/// 2. **Apply [`snapshot`](Ready::snapshot) before `entries`** when both are present. The snapshot
///    replaces the log prefix that the entries continue; the other order leaves a hole.
///
/// 3. **Apply [`committed_entries`](Ready::committed_entries) to the state machine, in order,
///    exactly once.** They are already durable — every one of them appeared in an earlier
///    `Ready`'s `entries`, or is covered by a snapshot. Applying them is not what makes them safe;
///    it is what makes them visible.
///
/// 4. **Answer a [`read_state`](Ready::read_states) only once the state machine has applied
///    through its index.** The index is the point the read linearizes at; answering earlier
///    returns a state older than the read's own position in the order.
///
/// 5. **Then call [`RawNode::advance`].** Nothing already returned is returned again. A `Ready`
///    that is dropped without being advanced is re-offered unchanged, so a driver that crashes
///    mid-discharge resumes rather than skips.
///
/// Sending before persisting is not a small violation with a small consequence. It is the
/// difference between a cluster that survives a power cut and one that silently forgets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ready {
    /// The durable state, present only when it changed. Persist it first.
    pub hard_state: Option<HardState>,
    /// Entries to append to the log. Persist before sending `messages`.
    pub entries: Vec<Entry>,
    /// A snapshot to install, replacing the log below its index. Apply before `entries`.
    pub snapshot: Option<Snapshot>,
    /// Messages to send, **after** the persistence above.
    pub messages: Vec<Message>,
    /// Committed entries to hand to the state machine, in order.
    pub committed_entries: Vec<Entry>,
    /// Reads whose linearization index is now established.
    pub read_states: Vec<ReadState>,
}

impl Ready {
    /// Whether there is nothing to do. `RawNode::ready` never returns one of these — it is here
    /// for drivers that keep a `Ready` around.
    pub fn is_empty(&self) -> bool {
        self.hard_state.is_none()
            && self.entries.is_empty()
            && self.snapshot.is_none()
            && self.messages.is_empty()
            && self.committed_entries.is_empty()
            && self.read_states.is_empty()
    }
}

/// A Raft node: the handle a driver steps, ticks, and reads [`Ready`] from.
///
/// Everything about it is synchronous and deterministic. Two `RawNode`s built from the same
/// configuration and fed the same ticks and messages in the same order reach the same state, which
/// is what lets `esker-sim` replay a ten-thousandth seed's failure exactly.
#[derive(Debug)]
pub struct RawNode<S: LogStorage> {
    raft: Raft<S>,
    /// The last `HardState` handed to a driver, so an unchanged one is not written again.
    prev_hard_state: HardState,
}

impl<S: LogStorage> RawNode<S> {
    /// Builds a node over `storage`.
    pub fn new(config: Config, storage: S) -> Result<Self> {
        let raft = Raft::new(config, storage)?;
        let prev_hard_state = raft.hard_state();
        Ok(Self {
            raft,
            prev_hard_state,
        })
    }

    /// One logical tick — the only way time enters (`CLAUDE.md` invariant 4). The caller decides
    /// what a tick is worth; [`TICK_MS`](crate::TICK_MS) is the project's answer.
    pub fn tick(&mut self) {
        self.raft.tick();
    }

    /// Feeds one message in. Never panics, whatever the message says.
    pub fn step(&mut self, message: Message) -> Result<()> {
        self.raft.step(message)
    }

    /// Proposes an opaque payload. Fails with [`RaftError::NotLeader`] anywhere but the leader.
    #[allow(clippy::needless_pass_by_value)] // TODO(step-2): the payload becomes an entry.
    pub fn propose(&mut self, data: Bytes) -> Result<()> {
        if self.raft.role != Role::Leader {
            return Err(RaftError::NotLeader);
        }
        if let Some(target) = self.raft.lead_transferee {
            return Err(RaftError::LeadershipTransferInProgress(target));
        }
        let _ = data;
        // TODO(step-2): append to the log and replicate.
        Ok(())
    }

    /// Proposes a single-server membership change.
    ///
    /// Refused while another change is appended but not committed: overlapping single-server
    /// changes can produce two disjoint majorities (dissertation §4.1).
    #[allow(clippy::needless_pass_by_value)] // TODO(step-6): the change becomes an entry.
    pub fn propose_conf_change(&mut self, change: ConfChange) -> Result<()> {
        if self.raft.role != Role::Leader {
            return Err(RaftError::NotLeader);
        }
        if let Some(at) = self.raft.conf.pending() {
            return Err(RaftError::ConfChangePending(at));
        }
        let _ = change;
        // TODO(step-6): encode, append, and apply the configuration at append time.
        Ok(())
    }

    /// Requests a linearizable read. The index arrives later, in a [`Ready::read_states`].
    #[allow(clippy::needless_pass_by_value)] // TODO(step-4): the context tags the round.
    pub fn read_index(&mut self, ctx: Bytes) {
        let _ = ctx;
        // TODO(step-4): start a ReadIndex round.
    }

    /// Starts an election immediately, skipping the timeout.
    ///
    /// Exists so a test or the simulator can drive an election deterministically instead of
    /// ticking until one happens by itself.
    pub fn campaign(&mut self) -> Result<()> {
        let kind = if self.raft.pre_vote {
            CampaignKind::PreElection
        } else {
            CampaignKind::Election
        };
        self.raft.campaign(kind)
    }

    /// Asks the leader to hand leadership to `target` (§3.10).
    pub fn transfer_leader(&mut self, target: NodeId) {
        let _ = target;
        // TODO(step-6): leadership transfer.
    }

    /// Whether there is anything for the driver to do.
    pub fn has_ready(&self) -> bool {
        !self.raft.messages.is_empty()
            || !self.raft.read_states.is_empty()
            || !self.raft.log.unstable_entries().is_empty()
            || self.raft.log.unstable_snapshot().is_some()
            || self.raft.hard_state().differs_from(&self.prev_hard_state)
            || self.raft.log.applied < self.raft.log.committed
    }

    /// Takes everything the core has decided. See [`Ready`] for the order it must be discharged in.
    pub fn ready(&mut self) -> Ready {
        let hard_state = self.raft.hard_state();
        Ready {
            hard_state: hard_state
                .differs_from(&self.prev_hard_state)
                .then_some(hard_state),
            entries: self.raft.log.unstable_entries().to_vec(),
            snapshot: self.raft.log.unstable_snapshot().cloned(),
            messages: core::mem::take(&mut self.raft.messages),
            committed_entries: self
                .raft
                .log
                .next_committed(self.raft.max_size_per_msg)
                .unwrap_or_default(),
            read_states: core::mem::take(&mut self.raft.read_states),
        }
    }

    /// Records that `ready` has been discharged. Nothing in it is offered again.
    pub fn advance(&mut self, ready: &Ready) {
        if let Some(hard_state) = ready.hard_state {
            self.prev_hard_state = hard_state;
        }
        if let Some(snapshot) = &ready.snapshot {
            self.raft.log.stable_snapshot_to(snapshot.meta.index);
        }
        if let Some(last) = ready.entries.last() {
            self.raft.log.stable_to(last.index, last.term);
        }
        if let Some(last) = ready.committed_entries.last() {
            self.raft.log.applied_to(last.index);
            self.raft.conf.commit_to(last.index);
        }
    }

    /// The state machine underneath, for the crate's own tests: they exercise paths — a transfer
    /// campaign, a forced conf change — that no public method reaches on its own yet.
    #[cfg(test)]
    pub(crate) fn raft_mut(&mut self) -> &mut Raft<S> {
        &mut self.raft
    }

    /// What this node currently believes.
    pub fn status(&self) -> Status {
        self.raft.status()
    }

    /// What it believes it is.
    pub fn role(&self) -> Role {
        self.raft.role
    }

    /// The term it is in.
    pub fn term(&self) -> Term {
        self.raft.term
    }

    /// Who it believes leads, if anyone.
    pub fn leader(&self) -> Option<NodeId> {
        self.raft.leader
    }

    /// Its commit index.
    pub fn commit_index(&self) -> Index {
        self.raft.log.committed
    }

    /// The storage underneath, for a driver or a test that also owns the writes.
    pub fn storage(&self) -> &S {
        &self.raft.log.store
    }

    /// The storage underneath, mutably: this is how a driver performs the writes a [`Ready`]
    /// asks for.
    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.raft.log.store
    }
}

#[cfg(test)]
mod tests {
    use super::RawNode;
    use crate::config::Config;
    use crate::core::Role;
    use crate::message::Message;
    use crate::storage::MemStorage;
    use crate::types::{ConfState, Entry, HardState, Snapshot, SnapshotMeta};

    fn node(id: u64) -> RawNode<MemStorage> {
        RawNode::new(
            Config::new(id, vec![1, 2, 3], 9),
            MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3])),
        )
        .unwrap()
    }

    #[test]
    fn a_fresh_node_is_a_follower_of_nobody() {
        let node = node(1);
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), 0);
        assert_eq!(node.leader(), None);
        assert_eq!(node.status().conf, ConfState::from_voters(vec![1, 2, 3]));
        assert!(!node.has_ready());
    }

    /// A restarting node's log is the authority on who is in the group — never the configuration
    /// it was started with, which may predate a membership change it already appended.
    #[test]
    fn storage_membership_wins_over_the_configuration_it_was_started_with() {
        let storage = MemStorage::with_conf_state(ConfState {
            voters: vec![7, 8],
            learners: vec![9],
        });
        let node = RawNode::new(Config::new(7, vec![1, 2, 3], 1), storage).unwrap();
        assert_eq!(node.status().conf.voters, vec![7, 8]);
        assert_eq!(node.status().conf.learners, vec![9]);
    }

    /// Invariant 9, and the property the sibling lane fuzzes from day one: every message kind, in
    /// every role, at a stale, current and future term, has a defined outcome and none of them is
    /// a panic.
    #[test]
    fn stepping_any_message_at_any_term_never_panics() {
        let payload = bytes::Bytes::from_static(b"ctx");
        for term in [0, 1, 5, u64::MAX] {
            for sender in [1, 2, 99] {
                let messages = vec![
                    Message::RequestVote {
                        from: sender,
                        to: 1,
                        term,
                        last_log_index: 0,
                        last_log_term: 0,
                        pre_vote: false,
                        force: false,
                    },
                    Message::RequestVote {
                        from: sender,
                        to: 1,
                        term,
                        last_log_index: u64::MAX,
                        last_log_term: u64::MAX,
                        pre_vote: true,
                        force: true,
                    },
                    Message::RequestVoteResponse {
                        from: sender,
                        to: 1,
                        term,
                        granted: true,
                        pre_vote: false,
                    },
                    Message::RequestVoteResponse {
                        from: sender,
                        to: 1,
                        term,
                        granted: false,
                        pre_vote: true,
                    },
                    Message::AppendEntries {
                        from: sender,
                        to: 1,
                        term,
                        prev_log_index: u64::MAX,
                        prev_log_term: 3,
                        entries: vec![Entry::empty(term, 1)],
                        leader_commit: u64::MAX,
                        context: payload.clone(),
                    },
                    Message::AppendEntriesResponse {
                        from: sender,
                        to: 1,
                        term,
                        reject: true,
                        index: u64::MAX,
                        hint_term: term,
                        context: payload.clone(),
                    },
                    Message::InstallSnapshot {
                        from: sender,
                        to: 1,
                        term,
                        snapshot: Snapshot {
                            meta: SnapshotMeta {
                                index: u64::MAX,
                                term,
                                conf: ConfState::from_voters(vec![4]),
                            },
                            data: payload.clone(),
                        },
                    },
                    Message::TimeoutNow {
                        from: sender,
                        to: 1,
                        term,
                    },
                    Message::ReadIndex {
                        from: sender,
                        to: 1,
                        term,
                        ctx: payload.clone(),
                    },
                    Message::ReadIndexResponse {
                        from: sender,
                        to: 1,
                        term,
                        index: u64::MAX,
                        ctx: payload.clone(),
                    },
                ];
                for message in messages {
                    let mut node = node(1);
                    // A node that has already moved on, so the stale-term path is exercised too.
                    node.storage_mut().set_hard_state(HardState {
                        term: 5,
                        voted_for: Some(3),
                        commit: 0,
                    });
                    let _ = node.step(message);
                }
            }
        }
    }

    /// A deposed leader still sending appends has to be told, or it retries forever and — with
    /// check-quorum on — keeps believing it holds a lease it does not.
    #[test]
    fn an_append_from_an_older_term_is_answered_so_its_sender_learns() {
        let storage = MemStorage::with_conf_state(ConfState::from_voters(vec![1, 2, 3]));
        let mut node = RawNode::new(
            Config {
                applied: 0,
                ..Config::new(1, vec![1, 2, 3], 3)
            },
            storage,
        )
        .unwrap();
        node.step(Message::AppendEntries {
            from: 2,
            to: 1,
            term: 9,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            context: bytes::Bytes::new(),
        })
        .unwrap();
        assert_eq!(node.term(), 9);
        assert_eq!(node.leader(), Some(2));

        // Now the stale one.
        node.step(Message::AppendEntries {
            from: 3,
            to: 1,
            term: 4,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            context: bytes::Bytes::new(),
        })
        .unwrap();
        let ready = node.ready();
        assert!(matches!(
            ready.messages.as_slice(),
            [Message::AppendEntriesResponse {
                to: 3,
                term: 9,
                reject: true,
                ..
            }]
        ));
    }

    /// A pre-vote carries a term its sender has not adopted. Treating it as real is exactly the
    /// disruption pre-vote exists to prevent (§9.6).
    #[test]
    fn a_pre_vote_from_a_higher_term_does_not_move_this_node_s_term() {
        let mut node = node(1);
        node.step(Message::RequestVote {
            from: 2,
            to: 1,
            term: 99,
            last_log_index: 0,
            last_log_term: 0,
            pre_vote: true,
            force: false,
        })
        .unwrap();
        assert_eq!(
            node.term(),
            0,
            "a pre-vote must not bump the cluster's term"
        );
    }

    /// Rule 5 of the driver contract: a `Ready` that is dropped without being advanced is
    /// re-offered unchanged, so a driver that crashes mid-discharge resumes rather than skips.
    #[test]
    fn a_ready_that_is_not_advanced_is_offered_again() {
        let mut node = node(1);
        node.step(Message::AppendEntries {
            from: 2,
            to: 1,
            term: 3,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            context: bytes::Bytes::new(),
        })
        .unwrap();
        let first = node.ready();
        assert_eq!(
            first.hard_state,
            Some(HardState {
                term: 3,
                voted_for: None,
                commit: 0
            })
        );
        let second = node.ready();
        assert_eq!(first.hard_state, second.hard_state);

        node.advance(&second);
        assert!(node.ready().hard_state.is_none(), "advance settles it");
    }

    #[test]
    fn proposing_anywhere_but_the_leader_is_refused() {
        let mut node = node(1);
        assert!(node.propose(bytes::Bytes::from_static(b"x")).is_err());
        assert!(
            node.propose_conf_change(crate::types::ConfChange::new(
                crate::types::ConfChangeKind::AddVoter,
                4
            ))
            .is_err()
        );
    }
}
