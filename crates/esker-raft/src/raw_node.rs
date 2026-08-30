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
use crate::types::{
    ConfChange, ConfState, Entry, EntryKind, HardState, Index, NodeId, ReadState, Snapshot, Term,
};

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
///    Rule 1 is also what makes the leader's own bookkeeping safe. A leader counts *itself* as
///    holding an entry the moment it appends one — before any fsync. That is only sound because no
///    follower can acknowledge the entry until the leader has sent it, and the leader may not send
///    until it has persisted. Persist-before-send is therefore not a nicety about message
///    ordering: it is the reason a quorum of acknowledgements means a quorum of durable copies.
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
/// 5. **Then call [`RawNode::advance`].** Nothing already returned is returned again.
///
///    A `Ready` that is dropped without being advanced re-offers its *state* — `hard_state`,
///    `entries`, `snapshot`, `committed_entries` — so a driver that fails mid-discharge resumes
///    rather than skips. Its `messages` are **taken**, and a dropped `Ready` loses them. That is
///    deliberate and safe: the network may lose any message anyway, so Raft already retries
///    everything it sends. It is only worth knowing because inspecting a `Ready` and discarding it
///    is not free — the messages go with it.
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
    pub fn propose(&mut self, data: Bytes) -> Result<()> {
        if self.raft.role != Role::Leader {
            return Err(RaftError::NotLeader);
        }
        if let Some(target) = self.raft.lead_transferee {
            return Err(RaftError::LeadershipTransferInProgress(target));
        }
        self.raft.propose_entry(EntryKind::Normal, data).map(|_| ())
    }

    /// Proposes a single-server membership change.
    ///
    /// Refused while another change is appended but not committed: overlapping single-server
    /// changes can produce two disjoint majorities (dissertation §4.1).
    // The by-value signature is the pinned one (`docs/DESIGN.md` §5); the core reads the change
    // rather than taking it apart, so nothing here consumes it.
    #[allow(clippy::needless_pass_by_value)]
    pub fn propose_conf_change(&mut self, change: ConfChange) -> Result<()> {
        if self.raft.role != Role::Leader {
            return Err(RaftError::NotLeader);
        }
        self.raft.propose_conf_change(&change).map(|_| ())
    }

    /// Requests a linearizable read.
    ///
    /// The answer arrives later, as a [`ReadState`] in [`Ready::read_states`], carrying `ctx` back
    /// so the caller can match it. The driver must apply through that index before answering the
    /// read — [`Ready`]'s contract, rule 4.
    ///
    /// On a follower this forwards to the leader; if this node does not know a leader, the request
    /// is dropped and the caller retries, which is what it would have to do anyway.
    pub fn read_index(&mut self, ctx: Bytes) {
        if let Err(error) = self.raft.read_index(ctx, None) {
            // Establishing a read index reads the log. A failure means this node cannot answer,
            // not that the read is unanswerable: the caller's retry may reach a node that can.
            tracing::warn!(%error, "could not start a read");
        }
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
    ///
    /// A no-op anywhere but the leader, and on a leader asked to transfer to itself or to a node
    /// that cannot vote. While a transfer is in flight the leader refuses proposals, and it
    /// abandons the attempt after one election timeout.
    pub fn transfer_leader(&mut self, target: NodeId) {
        if let Err(error) = self.raft.transfer_leader(target) {
            tracing::warn!(%error, target, "could not begin a leadership transfer");
        }
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

    /// Entries in `[low, high)`, **including the tail that is not yet durable**.
    ///
    /// A reader that went to [`storage`](RawNode::storage) instead would see only what the driver
    /// has already written, and would have to wait for a `Ready` to be discharged before it could
    /// observe an entry the node has already decided on. This returns the log as the *core* sees
    /// it: the durable prefix and the unstable tail as one sequence.
    ///
    /// Ranges are half-open, as everywhere in [`LogStorage`]. [`RaftError::Compacted`] means the
    /// entries are only in a snapshot now; [`RaftError::Unavailable`] means `high` is past the end.
    pub fn log_entries(&self, low: Index, high: Index) -> Result<Vec<Entry>> {
        self.raft.log.slice(low, high, u64::MAX)
    }

    /// Where the core's log begins: the index a snapshot has replaced everything up to, and the
    /// term of the entry that was there — `(0, 0)` for a log that has never been compacted.
    ///
    /// Like [`log_entries`](RawNode::log_entries), this is the *core's* view rather than the
    /// driver's, and the two differ for exactly as long as a snapshot the core has accepted has
    /// not been written yet. In that window the core's commit index has already moved to the
    /// snapshot's index and its log below that index is gone, while storage still holds the
    /// entries the snapshot replaced. An observer that took the boundary from storage and the
    /// commit index from here would be reading two different logs, and would see committed
    /// entries that this node no longer has any opinion about.
    ///
    /// # Errors
    ///
    /// Only if storage cannot answer for the boundary it reported.
    pub fn snapshot_boundary(&self) -> Result<(Index, Term)> {
        let index = self.raft.log.first_index()?.saturating_sub(1);
        if index == 0 {
            return Ok((0, 0));
        }
        Ok((index, self.raft.log.term(index)?))
    }

    /// The configuration in force: the latest in the log, committed or not.
    ///
    /// "Committed or not" is the whole subtlety. A membership change takes effect when its entry
    /// is *appended* (dissertation §4.1), so this can name a configuration that a truncation may
    /// still take away — which is exactly what an observer watching membership needs to see, and
    /// what [`Status::conf`](crate::Status::conf) reports as part of a larger snapshot.
    pub fn conf_state(&self) -> ConfState {
        self.raft.conf.current().clone()
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
    use crate::storage::LogStorage;
    use crate::storage::MemStorage;
    use crate::testkit::Harness;
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
        let _ = node.ready();

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
        assert!(
            !first.messages.is_empty() && second.messages.is_empty(),
            "state is re-offered; messages are taken once, which the contract says explicitly"
        );

        node.advance(&second);
        assert!(node.ready().hard_state.is_none(), "advance settles it");
    }

    /// Rule 5, the other half: what a `Ready` returned is never returned again.
    #[test]
    fn nothing_is_offered_twice_after_advance() {
        let mut group = Harness::new(&[1, 2, 3], 91);
        group.campaign(1);
        group.settle();
        group.propose(1, b"once");

        // `settle` has already discharged and advanced everything.
        for id in [1, 2, 3] {
            assert!(
                !group.node(id).has_ready(),
                "node {id} still has work after advance"
            );
        }
    }

    /// Rule 1 is testable from the core's side as one claim: the entries a message depends on are
    /// in the *same* `Ready` as the message, so a driver that persists first is never forced to
    /// send something it has not written.
    #[test]
    fn a_message_never_precedes_the_entries_it_depends_on() {
        let mut leader = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1, 2, 3], 12)
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
        let ready = leader.ready();
        leader.storage_mut().append(&ready.entries).unwrap();
        leader.advance(&ready);

        leader
            .propose(bytes::Bytes::from_static(b"payload"))
            .unwrap();
        let ready = leader.ready();
        let carried: Vec<u64> = ready.entries.iter().map(|entry| entry.index).collect();
        let durable = leader.storage().last_index().unwrap();
        for message in &ready.messages {
            if let Message::AppendEntries { entries, .. } = message {
                for entry in entries {
                    assert!(
                        entry.index <= durable || carried.contains(&entry.index),
                        "an entry at {} was sent without being offered for persistence first",
                        entry.index
                    );
                }
            }
        }
        assert!(
            !ready.entries.is_empty(),
            "the proposal must be offered for persistence"
        );
    }

    /// Rule 3: committed entries are either already durable or are in the same `Ready`'s entries,
    /// which the driver has just written. A committed entry that appears in neither could be
    /// applied and then lost.
    #[test]
    fn committed_entries_are_durable_or_carried_alongside() {
        let mut group = Harness::new(&[1, 2, 3], 92);
        group.campaign(1);
        group.settle();

        // Drive by hand so the Ready can be inspected before it is discharged.
        group
            .node_mut(1)
            .propose(bytes::Bytes::from_static(b"x"))
            .unwrap();
        let node = group.node_mut(1);
        let durable = node.storage().last_index().unwrap();
        let ready = node.ready();
        let carried: Vec<u64> = ready.entries.iter().map(|entry| entry.index).collect();
        for entry in &ready.committed_entries {
            assert!(
                entry.index <= durable || carried.contains(&entry.index),
                "committed entry {} is neither durable nor being persisted now",
                entry.index
            );
        }
    }

    /// L1's timing: heartbeats go out on the heartbeat tick, not on the election tick.
    #[test]
    fn a_leader_sends_heartbeats_on_the_heartbeat_tick() {
        let mut group = Harness::new(&[1, 2, 3], 93);
        group.campaign(1);
        group.settle();

        let heartbeat_tick = 2;
        for tick in 1..=heartbeat_tick {
            group.node_mut(1).tick();
            let ready = group.node_mut(1).ready();
            let appends = ready
                .messages
                .iter()
                .filter(|message| matches!(message, Message::AppendEntries { .. }))
                .count();
            if tick < heartbeat_tick {
                assert_eq!(appends, 0, "a heartbeat went out early, on tick {tick}");
            } else {
                assert_eq!(appends, 2, "no heartbeat on the heartbeat tick");
            }
        }
    }

    /// The sim lane's checkers read the log as the *core* sees it, not as storage does: an entry
    /// the node has decided on is visible here before the driver has written it, which is what
    /// removes the need to wait for a `Ready` to settle before observing one.
    #[test]
    fn log_entries_include_the_tail_that_is_not_yet_durable() {
        let mut leader = RawNode::new(
            Config {
                pre_vote: false,
                ..Config::new(1, vec![1], 77)
            },
            MemStorage::with_conf_state(ConfState::from_voters(vec![1])),
        )
        .unwrap();
        leader.campaign().unwrap();
        leader
            .propose(bytes::Bytes::from_static(b"payload"))
            .unwrap();

        // Nothing has been persisted yet: the driver has not taken a `Ready`.
        assert_eq!(leader.storage().last_index().unwrap(), 0);

        let last = leader.status().last_index;
        let entries = leader.log_entries(1, last + 1).unwrap();
        assert_eq!(entries.len(), 2, "the leader's no-op and the proposal");
        assert_eq!(entries[1].data.as_ref(), b"payload");

        // The bounds behave as they do everywhere: half-open, and past the end is an error.
        assert!(leader.log_entries(1, 1).unwrap().is_empty());
        assert!(leader.log_entries(1, last + 2).is_err());
    }

    /// A membership observation has to show the configuration *in force*, which §4.1 makes the
    /// latest in the log rather than the latest committed — so a change is visible the moment its
    /// entry is appended, and disappears again if that entry is truncated.
    #[test]
    fn conf_state_reports_the_configuration_in_force_before_it_commits() {
        let mut leader = RawNode::new(
            Config {
                pre_vote: false,
                check_quorum: false,
                ..Config::new(1, vec![1, 2, 3], 78)
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
        assert_eq!(leader.conf_state().voters, vec![1, 2, 3]);

        // Its own empty entry of term 1 has to commit before it may move a server: until then
        // there could be a configuration change on a branch it cannot see (§4.1, and
        // `conf::tests::a_new_leader_refuses_a_conf_change_until_it_has_committed_its_own_term`).
        for follower in [2, 3] {
            leader
                .step(Message::AppendEntriesResponse {
                    from: follower,
                    to: 1,
                    term: 1,
                    reject: false,
                    index: 1,
                    hint_term: 0,
                    context: bytes::Bytes::new(),
                })
                .unwrap();
        }
        leader
            .propose_conf_change(crate::types::ConfChange::new(
                crate::types::ConfChangeKind::AddLearner,
                4,
            ))
            .unwrap();
        let conf = leader.conf_state();
        assert_eq!(conf.voters, vec![1, 2, 3]);
        assert_eq!(
            conf.learners,
            vec![4],
            "in force at append, before it commits"
        );
        assert!(
            leader.commit_index() < leader.status().last_index,
            "and the entry that carries it is not committed yet"
        );
        assert_eq!(
            conf,
            leader.status().conf,
            "status agrees with the accessor"
        );
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
