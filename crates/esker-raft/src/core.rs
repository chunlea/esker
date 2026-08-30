//! The state machine itself: roles, terms, and the rules that apply to *every* message before its
//! content is looked at.
//!
//! Those universal rules are Figure 3.1's "Rules for Servers", and they are short enough to state
//! in full:
//!
//! * A message from a **higher term** means this node is behind. It adopts the term and becomes a
//!   follower before doing anything else — including before deciding whether it likes the message.
//! * A message from a **lower term** is stale. It is ignored, except that a stale *leader* is
//!   answered, so it learns it has been deposed instead of retrying forever.
//! * **Pre-vote messages are exempt from the first rule.** A pre-vote carries a term its sender has
//!   not adopted; treating it as real is precisely the disruption pre-vote exists to prevent
//!   (§9.6).
//!
//! Everything past those rules is dispatched to the module that owns it: `election`,
//! `replication`, `snapshot`, `conf`, `readonly`.

use esker_base::rng::Pcg32;

use crate::conf::ConfTracker;
use crate::config::Config;
use crate::election::CampaignKind;
use crate::error::Result;
use crate::log::RaftLog;
use crate::message::Message;
use crate::progress::{Progress, ProgressMap};
use crate::readonly::ReadOnly;
use crate::storage::LogStorage;
use crate::types::{ConfState, HardState, Index, NodeId, ReadState, Term};

/// What a node currently believes it is.
///
/// [`Role::PreCandidate`] is not in the original paper. It is the state a node occupies during a
/// pre-vote round: campaigning in a *hypothetical* next term without having adopted it, so that
/// losing costs the cluster nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Replicating from a leader, or waiting for one.
    Follower,
    /// Asking whether it could win, without bumping the term.
    PreCandidate,
    /// Standing for election in the current term.
    Candidate,
    /// Ordering proposals for the group.
    Leader,
}

/// A read-only view of a node, for tests, the simulator's safety checkers and the store's metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// This node's id.
    pub id: NodeId,
    /// The term it is in.
    pub term: Term,
    /// Who it voted for in that term.
    pub voted_for: Option<NodeId>,
    /// What it believes it is.
    pub role: Role,
    /// Who it believes leads, if anyone.
    pub leader: Option<NodeId>,
    /// Its commit index.
    pub commit: Index,
    /// How far the state machine has been driven.
    pub applied: Index,
    /// The last index in its log.
    pub last_index: Index,
    /// The membership in force — the latest in the log, committed or not.
    pub conf: ConfState,
}

/// The Raft state machine for one group.
#[derive(Debug)]
pub(crate) struct Raft<S: LogStorage> {
    pub(crate) id: NodeId,
    pub(crate) term: Term,
    pub(crate) vote: Option<NodeId>,
    pub(crate) role: Role,
    pub(crate) leader: Option<NodeId>,
    pub(crate) log: RaftLog<S>,
    pub(crate) progress: ProgressMap,
    pub(crate) conf: ConfTracker,
    /// Votes received this election, sorted by voter. A `Vec` rather than a map, for the same
    /// determinism reason as [`ProgressMap`].
    pub(crate) votes: Vec<(NodeId, bool)>,
    /// Messages produced since the last [`Ready`](crate::Ready).
    pub(crate) messages: Vec<Message>,
    /// Reads whose index has been established since the last `Ready`.
    pub(crate) read_states: Vec<ReadState>,
    pub(crate) read_only: ReadOnly,
    /// Ticks since this node last heard from a leader (as a follower) or from a quorum (as a
    /// leader).
    pub(crate) election_elapsed: u64,
    /// Ticks since the leader last sent heartbeats.
    pub(crate) heartbeat_elapsed: u64,
    /// This election's timeout, redrawn every time the node resets its term.
    pub(crate) randomized_election_timeout: u64,
    pub(crate) election_tick: (u64, u64),
    pub(crate) heartbeat_tick: u64,
    pub(crate) max_inflight_msgs: usize,
    pub(crate) max_size_per_msg: u64,
    pub(crate) pre_vote: bool,
    pub(crate) check_quorum: bool,
    pub(crate) rng: Pcg32,
    /// The target of an in-flight leadership transfer. While it is set the leader refuses
    /// proposals, so that none is left owned by a leader that is stepping down.
    pub(crate) lead_transferee: Option<NodeId>,
}

impl<S: LogStorage> Raft<S> {
    /// Builds a node over `storage`.
    ///
    /// The membership comes from storage when storage has one: a restarting node's log is the
    /// authority on who is in the group, never the configuration it was started with, which may
    /// predate a membership change it already appended.
    pub(crate) fn new(config: Config, storage: S) -> Result<Self> {
        config.validate()?;
        let initial = storage.initial_state()?;
        let log = RaftLog::new(storage, config.applied)?;
        let conf_state =
            if initial.conf_state.voters.is_empty() && initial.conf_state.learners.is_empty() {
                config.conf_state()
            } else {
                initial.conf_state
            };
        let mut raft = Self {
            id: config.id,
            term: initial.hard_state.term,
            vote: initial.hard_state.voted_for,
            role: Role::Follower,
            leader: None,
            log,
            progress: ProgressMap::new(),
            conf: ConfTracker::new(conf_state, 0),
            votes: Vec::new(),
            messages: Vec::new(),
            read_states: Vec::new(),
            read_only: ReadOnly::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            randomized_election_timeout: config.election_tick.0,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            max_inflight_msgs: config.max_inflight_msgs,
            max_size_per_msg: config.max_size_per_msg,
            pre_vote: config.pre_vote,
            check_quorum: config.check_quorum,
            rng: config.rng,
            lead_transferee: None,
        };
        raft.rebuild_progress()?;
        raft.reset_election_timeout();
        Ok(raft)
    }

    /// The durable state as it stands.
    pub(crate) fn hard_state(&self) -> HardState {
        HardState {
            term: self.term,
            voted_for: self.vote,
            commit: self.log.committed,
        }
    }

    /// A snapshot of what this node believes, for tests and checkers.
    pub(crate) fn status(&self) -> Status {
        Status {
            id: self.id,
            term: self.term,
            voted_for: self.vote,
            role: self.role,
            leader: self.leader,
            commit: self.log.committed,
            applied: self.log.applied,
            last_index: self.log.last_index().unwrap_or(0),
            conf: self.conf.current().clone(),
        }
    }

    /// Draws this election's timeout from the injected RNG.
    ///
    /// Called on **every** reset, not once at construction. A timeout fixed at boot means two
    /// nodes that happened to draw the same number tie in every term thereafter; redrawing makes a
    /// tie a delay rather than a livelock (`docs/plans/phase-3.md` §6, and the trap the phase
    /// brief names).
    pub(crate) fn reset_election_timeout(&mut self) {
        let (low, high) = self.election_tick;
        self.randomized_election_timeout = self.rng.range_inclusive(low, high);
    }

    /// Rebuilds the progress map from the current configuration, keeping what is still known about
    /// peers that survived the change.
    pub(crate) fn rebuild_progress(&mut self) -> Result<()> {
        let next = self.log.last_index()? + 1;
        let conf = self.conf.current().clone();
        for id in conf.members() {
            let is_learner = conf.is_learner(id);
            match self.progress.get_mut(id) {
                Some(progress) => progress.is_learner = is_learner,
                None => {
                    self.progress
                        .insert(id, Progress::new(next, self.max_inflight_msgs, is_learner));
                }
            }
        }
        for id in self.progress.ids() {
            if !conf.contains(id) {
                self.progress.remove(id);
            }
        }
        Ok(())
    }

    /// Whether `id` counts toward a quorum in the configuration in force.
    pub(crate) fn is_voter(&self, id: NodeId) -> bool {
        self.conf.current().is_voter(id)
    }

    /// Queues a message for the next [`Ready`](crate::Ready).
    pub(crate) fn send(&mut self, message: Message) {
        self.messages.push(message);
    }

    // ----- role transitions -------------------------------------------------------------------

    /// Clears everything that belonged to the previous term and redraws the election timeout.
    pub(crate) fn reset(&mut self, term: Term) {
        if self.term != term {
            self.term = term;
            self.vote = None;
        }
        self.leader = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.reset_election_timeout();
        self.votes.clear();
        self.read_only.reset();
        self.lead_transferee = None;
    }

    /// Becomes a follower of `leader` in `term`.
    pub(crate) fn become_follower(&mut self, term: Term, leader: Option<NodeId>) {
        self.reset(term);
        self.role = Role::Follower;
        self.leader = leader;
        tracing::debug!(id = self.id, term, ?leader, "became follower");
    }

    // ----- driving ----------------------------------------------------------------------------

    /// One logical tick. The only way time enters this crate (`CLAUDE.md` invariant 4).
    pub(crate) fn tick(&mut self) {
        match self.role {
            Role::Leader => self.tick_heartbeat(),
            Role::Follower | Role::PreCandidate | Role::Candidate => self.tick_election(),
        }
    }

    fn tick_election(&mut self) {
        self.election_elapsed += 1;
        if self.election_elapsed >= self.randomized_election_timeout {
            self.election_elapsed = 0;
            let kind = if self.pre_vote {
                CampaignKind::PreElection
            } else {
                CampaignKind::Election
            };
            if let Err(error) = self.campaign(kind) {
                // Campaigning reads the log; a storage failure here means this node cannot stand
                // for election, which is survivable — another node will. It is not a reason to
                // stop ticking.
                tracing::warn!(id = self.id, %error, "could not campaign");
            }
        }
    }

    fn tick_heartbeat(&mut self) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;
        if self.heartbeat_elapsed >= self.heartbeat_tick {
            self.heartbeat_elapsed = 0;
            // TODO(step-2): broadcast heartbeats.
        }
        if self.check_quorum && self.election_elapsed >= self.randomized_election_timeout {
            self.election_elapsed = 0;
            // TODO(step-6): check quorum, step down without one.
        }
    }

    /// Feeds one message in.
    ///
    /// Never fails on the *content* of a message and never panics on one (`CLAUDE.md`
    /// invariant 9). A node does not control what the network hands it: a stale term, a sender it
    /// has never heard of, a vote from a peer a configuration change removed — each has a defined,
    /// quiet outcome.
    pub(crate) fn step(&mut self, message: Message) -> Result<()> {
        let term = message.term();
        if term > self.term {
            if self.vetoed_by_leader_lease(&message) {
                tracing::debug!(
                    id = self.id,
                    from = message.sender(),
                    "refused a vote request: the current leader's lease has not expired"
                );
                return Ok(());
            }
            self.step_higher_term(&message);
        } else if term < self.term {
            return self.step_lower_term(&message);
        }
        self.step_current_term(message)
    }

    /// §6.2: a node that has heard from a healthy leader within the election timeout ignores a
    /// vote request from a higher term entirely — no reply, and no term adopted.
    ///
    /// Without this, one disconnected node whose term has run ahead deposes a working leader
    /// simply by reappearing. With it, that node's request is dropped and it learns the truth from
    /// the next heartbeat instead.
    ///
    /// The exception is a request carrying `force`, which the leader itself ordered for a
    /// leadership transfer: vetoing that would make transfer fail exactly when the cluster is
    /// healthy, which is the only time it is ever used.
    fn vetoed_by_leader_lease(&self, message: &Message) -> bool {
        let Message::RequestVote { force, .. } = message else {
            return false;
        };
        !force
            && self.check_quorum
            && self.leader.is_some()
            && self.election_elapsed < self.randomized_election_timeout
    }

    /// A message from the future: adopt the term, unless it is a pre-vote.
    fn step_higher_term(&mut self, message: &Message) {
        match message {
            // §9.6: a pre-vote request names a term its sender has *not* adopted, and a granted
            // pre-vote response is equally hypothetical — the term is adopted when the real
            // campaign starts, not when the probe succeeds. Adopting either here would let a node
            // that cannot win an election still force every other node to abandon a healthy
            // leader, which is the disruption pre-vote exists to prevent.
            Message::RequestVote { pre_vote: true, .. }
            | Message::RequestVoteResponse {
                pre_vote: true,
                granted: true,
                ..
            } => {}
            // An append or a snapshot from a higher term identifies a leader; a vote request does
            // not, so the node becomes a follower of nobody and waits.
            Message::AppendEntries { from, .. } | Message::InstallSnapshot { from, .. } => {
                let leader = *from;
                self.become_follower(message.term(), Some(leader));
            }
            _ => self.become_follower(message.term(), None),
        }
    }

    /// A message from the past. Ignored, with two exceptions that exist so the *sender* learns.
    fn step_lower_term(&mut self, message: &Message) -> Result<()> {
        match message {
            // A deposed leader still sending appends must be told, or it retries forever and, with
            // check-quorum on, keeps believing it holds a lease it does not.
            Message::AppendEntries { from, .. } | Message::InstallSnapshot { from, .. } => {
                let to = *from;
                let index = self.log.last_index()?;
                self.send(Message::AppendEntriesResponse {
                    from: self.id,
                    to,
                    term: self.term,
                    reject: true,
                    index,
                    hint_term: self.log.last_term(),
                    context: bytes::Bytes::new(),
                });
            }
            // A pre-vote from an older term is refused *with this node's term*, which is how a
            // node returning from a partition discovers how far behind it is without disrupting
            // anyone.
            Message::RequestVote {
                from,
                pre_vote: true,
                ..
            } => {
                let to = *from;
                self.send(Message::RequestVoteResponse {
                    from: self.id,
                    to,
                    term: self.term,
                    granted: false,
                    pre_vote: true,
                });
            }
            _ => {
                tracing::trace!(
                    id = self.id,
                    term = self.term,
                    message = message.kind_name(),
                    from = message.sender(),
                    "ignored a message from an earlier term"
                );
            }
        }
        Ok(())
    }

    /// A message in this node's own term.
    ///
    /// A pre-vote request is the one message that can reach here with a term *above* this node's,
    /// because §9.6 forbids adopting it.
    // The handlers that land in the remaining steps consume the message; the signature is the one
    // they need, so it does not churn under the sibling lane.
    #[allow(clippy::needless_pass_by_value)] // TODO(step-2..6)
    fn step_current_term(&mut self, message: Message) -> Result<()> {
        match message {
            Message::RequestVote {
                from,
                term,
                last_log_index,
                last_log_term,
                pre_vote,
                ..
            } => {
                self.handle_vote_request(from, term, last_log_index, last_log_term, pre_vote);
                Ok(())
            }
            Message::RequestVoteResponse {
                from,
                granted,
                pre_vote,
                ..
            } => self.handle_vote_response(from, granted, pre_vote),
            Message::AppendEntries { from, term, .. } => {
                // Figure 3.1, C3: a candidate that hears from a leader of its own term concedes.
                // The leader is real — it could only have been elected by a majority — so
                // continuing to campaign would just cost the cluster another term.
                if matches!(self.role, Role::Candidate | Role::PreCandidate) {
                    self.become_follower(term, Some(from));
                } else if self.role == Role::Follower {
                    self.leader = Some(from);
                    self.election_elapsed = 0;
                }
                // TODO(step-2): the consistency check, the splice and the response.
                Ok(())
            }
            other => {
                // TODO(step-2..6): replication responses, snapshots, transfer and reads.
                tracing::trace!(
                    id = self.id,
                    term = self.term,
                    message = other.kind_name(),
                    from = other.sender(),
                    "message accepted; handler not implemented yet"
                );
                Ok(())
            }
        }
    }
}
