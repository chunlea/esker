//! The driver: one region's `RawNode`, the thread that turns its `Ready` into durable bytes, and
//! the handle the request path talks to.
//!
//! # Where the ordering rule lives
//!
//! `esker-raft` cannot enforce [`Ready`](esker_raft::Ready)'s contract, because enforcing it means
//! writing to a disk and the core may not (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
//! [`PeerCore::drive`] is where it is enforced instead, and the order of its five steps is not
//! stylistic:
//!
//! 1. the hard state and the new entries go into **one** `WriteBatch` with `sync = true`;
//! 2. only then are the messages handed to the transport;
//! 3. committed entries are applied, in order, each with its `apply_index` in the same batch;
//! 4. reads whose index has now *applied* are answered;
//! 5. `advance`.
//!
//! Step 1 before step 2 is what makes a quorum of acknowledgements mean a quorum of durable
//! copies — including this peer's own, since a leader counts itself the moment it appends. A
//! transport that sees an entry it cannot read back from the log is the bug this ordering exists
//! to prevent, and `a_message_is_never_sent_before_its_entries_are_durable` asserts exactly that.
//!
//! # Why a thread and not a task
//!
//! The engine is synchronous: an `fsync` here would stall every connection the reactor serves. So
//! the Raft core, the log writes and the apply loop all run on one dedicated thread, which also
//! means they need no locks between them. Async stays at the edge, where it belongs — the network
//! feeds this thread through a bounded channel, and **time enters through a `tokio` interval that
//! sends [`PeerMsg::Tick`]**, so the core still never reads a clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use esker_engine::{WriteBatch, WriteOptions};
use esker_proto::{ProtoError, Region};
use esker_raft::{
    ConfState, Config as RaftConfig, Entry, EntryKind, Index, Message, NodeId, RawNode, ReadState,
    Role, Status, Term,
};
use tokio::sync::{mpsc, oneshot};

use crate::apply::Command;
use crate::error::{Result, StoreError};
use crate::raft_log::RaftLogStorage;

/// How many messages may be queued for the driver thread before a caller waits.
///
/// Bounded, like everything else that crosses a thread here: an unbounded queue in front of an
/// `fsync` is a memory leak with extra steps (`docs/DESIGN.md` §9).
pub const PEER_QUEUE_DEPTH: usize = 4096;

/// Where a peer's outbound Raft messages go.
///
/// Deliberately fire-and-forget and infallible. Raft already retries everything it sends — a lost
/// message is indistinguishable from a slow one — so a transport that reported failures would
/// give the driver a decision it has no better answer to than "send it again next tick".
pub trait RaftTransport: Send + Sync + std::fmt::Debug {
    /// Delivers `messages` towards their recipients, batched as they came out of one `Ready`.
    fn send(&self, messages: Vec<Message>);
}

/// A transport that drops everything, for a single-node store and for tests that do not care.
#[derive(Debug, Default)]
pub struct DiscardTransport;

impl RaftTransport for DiscardTransport {
    fn send(&self, _messages: Vec<Message>) {}
}

/// The store, as one region's driver needs it: the two things a split cannot do for itself.
///
/// A driver owns its `RawNode` exclusively and knows nothing about the store around it, which is
/// what makes it lock-free. A split has to reach outside that: the region map has to gain the
/// child and lose half of the parent, and the child needs a peer of its own with a transport and a
/// ticker. This is that reach, and it is a trait so the peer tests can drive a split without a
/// store.
///
/// Called from the parent's driver thread **after** the batch carrying both halves' metadata is
/// durable, so an implementation may assume the split is already a fact on disk.
pub trait RegionHost: Send + Sync + std::fmt::Debug {
    /// Brings a split into effect: narrows the parent in the region map, and starts the child.
    ///
    /// Failing here is a failure of the store, not of the split: the metadata is already durable,
    /// so the driver stops and the restart rebuilds the map from the records — which is the same
    /// state this call was trying to produce.
    fn split_applied(&self, parent: &Region, child: &Region) -> Result<()>;
}

/// A host that refuses to split, for a peer that has no store around it.
#[derive(Debug, Default)]
pub struct NoHost;

impl RegionHost for NoHost {
    fn split_applied(&self, parent: &Region, _child: &Region) -> Result<()> {
        Err(StoreError::RegionConflict(format!(
            "region {} split, but this peer has no store to put the halves in",
            parent.id
        )))
    }
}

/// What applying a command produced, for whoever proposed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// It applied and has nothing to report.
    Done,
    /// A `CompareAndSwap`'s answer, decided at apply time on every peer alike.
    Swapped {
        /// Whether the value matched and was replaced.
        swapped: bool,
        /// What was there before.
        previous: Option<Bytes>,
    },
    /// How many keys a `DeleteRange` removed.
    Deleted {
        /// The count.
        keys: u64,
    },
}

/// What the driver thread accepts.
#[derive(Debug)]
pub enum PeerMsg {
    /// One logical tick, from the timer at the edge.
    Tick,
    /// A Raft message from another peer.
    Raft(Message),
    /// A command to replicate. The answer comes back once it has *applied*.
    Propose {
        /// The encoded command.
        command: Bytes,
        /// Where the outcome goes.
        notify: oneshot::Sender<std::result::Result<Applied, ProtoError>>,
    },
    /// A linearizable read. The answer is the index the caller must read at — and the driver does
    /// not send it until the state machine has applied that far.
    ReadIndex {
        /// Where the index goes.
        notify: oneshot::Sender<std::result::Result<Index, ProtoError>>,
    },
    /// A snapshot of what this peer believes, for the request path and for tests.
    Status(oneshot::Sender<Status>),
    /// Stop the thread, failing everything outstanding.
    Stop,
}

/// A proposal waiting for its entry to apply.
#[derive(Debug)]
struct Pending {
    index: Index,
    term: Term,
    notify: oneshot::Sender<std::result::Result<Applied, ProtoError>>,
}

/// A read waiting for the state machine to reach its index.
#[derive(Debug)]
struct PendingRead {
    index: Index,
    notify: oneshot::Sender<std::result::Result<Index, ProtoError>>,
}

/// How a peer is built.
#[derive(Debug, Clone)]
pub struct PeerOptions {
    /// The region this peer serves, as it stands when the peer starts.
    ///
    /// The driver keeps its own copy and narrows it when a split applies, so the range every entry
    /// is checked against is the one the log has produced — never one read from a map another
    /// thread is also writing.
    pub region: Region,
    /// This peer's Raft id.
    pub peer_id: NodeId,
    /// The group's voters, used only when the log has no configuration of its own.
    pub voters: Vec<NodeId>,
    /// Seed for the election-timeout RNG. The peer id selects the stream, so a whole cluster may
    /// share one seed and still not campaign in lockstep
    /// (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
    pub seed: u64,
}

/// The Raft core plus everything the driver thread owns.
#[derive(Debug)]
pub struct PeerCore {
    node: RawNode<RaftLogStorage>,
    transport: Arc<dyn RaftTransport>,
    host: Arc<dyn RegionHost>,
    /// This region as the **log** has made it: narrowed by every split this peer has applied.
    ///
    /// The driver thread is its only writer and its only reader, so it needs no lock and cannot go
    /// stale behind anyone's back — the same argument that lets the log storage cache its bounds.
    /// The region map holds a copy for the request path, updated just after this one.
    region: Region,
    region_id: u64,
    peer_id: NodeId,
    pending: Vec<Pending>,
    reads: Vec<PendingRead>,
    /// Published for the request path, which must not wait on the driver just to learn who leads.
    leader: Arc<AtomicU64>,
    /// Published beside it, for the same reason and one more: a region heartbeat carries the
    /// leader's term and apply index, and a heartbeat round that asked the driver for them would
    /// queue behind whatever `fsync` it is in the middle of.
    published: Arc<Published>,
    /// How far the state machine has been driven. Reads wait for this, not for the commit index.
    applied_index: Index,
}

/// What the driver publishes for readers that must not wait on it.
///
/// Three independent atomics rather than one lock: nothing reads two of them and needs them to
/// agree. A heartbeat that reports last tick's term beside this tick's apply index reports a
/// state the peer really passed through, and the next heartbeat corrects it either way.
#[derive(Debug, Default)]
pub struct Published {
    term: AtomicU64,
    applied: AtomicU64,
    /// Bytes this peer's apply has staged, ever. See [`RaftPeer::approximate_size`].
    size: AtomicU64,
}

impl PeerCore {
    /// The five steps, in the order the contract requires.
    pub fn drive(&mut self) -> Result<()> {
        while self.node.has_ready() {
            let mut ready = self.node.ready();

            // 1. Persist. One batch, fsynced, before a single message leaves.
            if ready.hard_state.is_some() || !ready.entries.is_empty() {
                let mut batch = WriteBatch::new();
                self.node
                    .storage_mut()
                    .stage_ready(&mut batch, ready.hard_state, &ready.entries);
                self.node
                    .storage()
                    .db()
                    .write(batch, &WriteOptions { sync: true })?;
            }
            if ready.snapshot.is_some() {
                // TODO(phase-4): apply a received snapshot's bytes. A single region that never
                // compacts its log never sends one, so reaching here in 3e is a bug worth saying
                // out loud rather than a case to handle quietly.
                tracing::error!(
                    region_id = self.region_id,
                    "a Raft snapshot arrived; streaming is phase 4"
                );
            }

            // 2. Send. Taken rather than cloned: a `Ready`'s messages are moved out by design.
            let messages = std::mem::take(&mut ready.messages);
            if !messages.is_empty() {
                self.transport.send(messages);
            }

            // 3. Apply, in order.
            for entry in &ready.committed_entries {
                self.apply(entry)?;
            }

            // 4. Reads whose index has been established. Answering waits for apply, below.
            for state in &ready.read_states {
                self.record_read(state);
            }

            // 5. Advance.
            self.node.advance(&ready);
        }

        self.publish_leader();
        self.answer_ready_reads();
        Ok(())
    }

    /// Applies one committed entry: its effect and its `apply_index`, in one batch.
    ///
    /// Two failures live here and only one is fatal. A **deterministic refusal** — a key outside
    /// the region as this entry finds it — completes one proposal with an error and moves on; the
    /// entry still applied, as a no-op, so `apply_index` still advances. A payload that cannot be
    /// **decoded** cannot be applied at all, and skipping it would leave this peer's state machine
    /// differing from every other's, so it stops the driver.
    fn apply(&mut self, entry: &Entry) -> Result<()> {
        let mut batch = WriteBatch::new();
        let mut split_halves = None;

        let outcome: std::result::Result<Applied, ProtoError> = match entry.kind {
            // A leader's no-op carries no payload; it exists so §5.4.2 lets the backlog commit.
            EntryKind::Normal if entry.data.is_empty() => Ok(Applied::Done),
            EntryKind::Normal => {
                let command = Command::decode(&entry.data).map_err(|error| {
                    StoreError::Bootstrap(format!("could not apply entry {}: {error}", entry.index))
                })?;
                match &command {
                    Command::Split {
                        split_key,
                        new_region_id,
                        new_peer_ids,
                    } => self.stage_split(
                        &mut batch,
                        &mut split_halves,
                        split_key,
                        *new_region_id,
                        new_peer_ids,
                    ),
                    _ => match crate::apply::check_scope(&command, &self.region) {
                        // Refused, deterministically, on every peer alike. Nothing is staged.
                        Err(refusal) => Err(refusal),
                        Ok(()) => Ok(crate::apply::stage(
                            self.node.storage().db(),
                            &mut batch,
                            &command,
                            &self.region,
                        )
                        .map_err(|error| {
                            StoreError::Bootstrap(format!(
                                "could not apply entry {}: {error}",
                                entry.index
                            ))
                        })?),
                    },
                }
            }
            EntryKind::ConfChange => {
                // TODO(phase-4c): membership over the wire. There is still no operator to have
                // proposed one, so reaching here is worth saying out loud.
                tracing::warn!(
                    region_id = self.region_id,
                    index = entry.index,
                    "a configuration change was committed; no operator proposes one yet"
                );
                Ok(Applied::Done)
            }
        };

        // What this entry actually staged, counted before `apply_index` is added to the batch so
        // that the bookkeeping does not count itself. It is a hint and nothing deterministic reads
        // it (`docs/plans/phase-4.md` §12.3), which is what lets it be per-peer and approximate.
        let staged = batch.byte_size() as u64;

        // `apply_index` travels with the data it applied. A crash therefore has both or neither,
        // which is what makes replaying from `apply_index + 1` correct and exactly-once — and, for
        // a split, what makes both halves' records land together or not at all.
        self.node
            .storage_mut()
            .stage_applied(&mut batch, entry.index);
        // Deliberately not synced: losing this batch loses nothing, because the entry is still in
        // the Raft log, which was. The restart re-applies it, and a replayed split is a no-op.
        self.node
            .storage()
            .db()
            .write(batch, &WriteOptions { sync: false })?;
        self.applied_index = entry.index;
        self.published.size.fetch_add(staged, Ordering::Relaxed);

        // Only now, with both records durable, does the split become visible to anything else.
        if let Some((parent, child)) = split_halves {
            self.region = parent.clone();
            // The parent has given away roughly half its keys, so it is holding roughly half the
            // bytes. Without this the hint would stay over the threshold for ever and the parent
            // would try to split on every tick until it ran out of boundaries — the counter counts
            // what was *applied*, and a split applies nothing it can subtract exactly.
            let _ =
                self.published
                    .size
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |size| Some(size / 2));
            self.host.split_applied(&parent, &child)?;
            tracing::info!(
                region_id = parent.id,
                child_id = child.id,
                index = entry.index,
                "a region split"
            );
        }

        self.complete_proposal(entry, outcome);
        Ok(())
    }

    /// Stages both halves of a split, and hands the caller what has to happen once they are
    /// durable.
    ///
    /// **Idempotent by the range, not by a flag.** After a split the parent ends at `split_key`,
    /// so `split_key` is no longer strictly inside it — and that one question is the whole replay
    /// check. A split entry re-applied after a restart answers "no" and does nothing, with no
    /// marker to write and nothing to keep in step.
    fn stage_split(
        &mut self,
        batch: &mut WriteBatch,
        halves: &mut Option<(Region, Region)>,
        split_key: &Bytes,
        new_region_id: u64,
        new_peer_ids: &[u64],
    ) -> std::result::Result<Applied, ProtoError> {
        if !crate::split::is_legal_boundary(split_key, &self.region) {
            tracing::debug!(
                region_id = self.region_id,
                "a split entry whose boundary is no longer inside this region applied as a no-op"
            );
            return Ok(Applied::Done);
        }
        let (parent, child) =
            crate::apply::split_region(&self.region, split_key, new_region_id, new_peer_ids)?;
        let cf = self.node.storage().cf();
        crate::meta::stage_region(batch, cf, &parent);
        crate::meta::stage_region(batch, cf, &child);
        *halves = Some((parent, child));
        Ok(Applied::Done)
    }

    /// Notifies whoever proposed the entry at this index — or tells them it was replaced.
    fn complete_proposal(
        &mut self,
        entry: &Entry,
        outcome: std::result::Result<Applied, ProtoError>,
    ) {
        let Some(at) = self
            .pending
            .iter()
            .position(|pending| pending.index == entry.index)
        else {
            return;
        };
        let pending = self.pending.remove(at);
        if pending.term == entry.term {
            let _ = pending.notify.send(outcome);
        } else {
            // A different entry took this index, so the proposal was truncated. It provably did
            // not apply, which is what makes `NotLeader` the honest answer: retryable, and
            // `NotApplied` rather than ambiguous.
            let _ = pending.notify.send(Err(self.not_leader()));
        }
    }

    fn record_read(&mut self, state: &ReadState) {
        let Ok(index) = read_token(&state.ctx) else {
            tracing::warn!(
                region_id = self.region_id,
                "a read state came back with a tag this peer did not issue"
            );
            return;
        };
        if let Some(at) = self.reads.iter().position(|read| read.index == index) {
            let read = self.reads.remove(at);
            self.reads.push(PendingRead {
                index: state.index,
                notify: read.notify,
            });
        }
    }

    /// Answers every read the state machine has now caught up with.
    ///
    /// The wait is for **apply**, not for commit: the index the core hands back is a commit index,
    /// and answering before the state machine has run through it returns a state older than the
    /// read's own position in the order (`docs/DESIGN.md` §2).
    fn answer_ready_reads(&mut self) {
        let applied = self.applied_index;
        let mut still_waiting = Vec::new();
        for read in self.reads.drain(..) {
            if read.index <= applied {
                let _ = read.notify.send(Ok(read.index));
            } else {
                still_waiting.push(read);
            }
        }
        self.reads = still_waiting;
    }

    fn publish_leader(&self) {
        let leader = self.node.leader().unwrap_or(0);
        self.leader.store(leader, Ordering::Release);
        self.published
            .term
            .store(self.node.status().term, Ordering::Release);
        self.published
            .applied
            .store(self.applied_index, Ordering::Release);
    }

    fn not_leader(&self) -> ProtoError {
        ProtoError::NotLeader {
            region_id: self.region_id,
            leader_hint: self.node.leader().filter(|id| *id != self.peer_id),
        }
    }

    /// Fails everything outstanding — on shutdown, or when this peer stops leading and can no
    /// longer promise anything about what it accepted.
    fn fail_outstanding(&mut self, error: &ProtoError) {
        for pending in self.pending.drain(..) {
            let _ = pending.notify.send(Err(error.clone()));
        }
        for read in self.reads.drain(..) {
            let _ = read.notify.send(Err(error.clone()));
        }
    }

    fn handle(&mut self, message: PeerMsg) -> bool {
        match message {
            PeerMsg::Tick => self.node.tick(),
            PeerMsg::Raft(raft) => {
                if let Err(error) = self.node.step(raft) {
                    tracing::warn!(region_id = self.region_id, %error, "a Raft message was refused");
                }
            }
            PeerMsg::Propose { command, notify } => self.propose(command, notify),
            PeerMsg::ReadIndex { notify } => self.read_index(notify),
            PeerMsg::Status(notify) => {
                let _ = notify.send(self.node.status());
            }
            PeerMsg::Stop => return false,
        }
        // Published here as well as after driving, because a batch can carry the tick that
        // elects this peer and the status query that asks about it — and the two must not
        // disagree about who leads.
        self.publish_leader();
        true
    }

    fn propose(
        &mut self,
        command: Bytes,
        notify: oneshot::Sender<std::result::Result<Applied, ProtoError>>,
    ) {
        if self.node.role() != Role::Leader {
            let _ = notify.send(Err(self.not_leader()));
            return;
        }
        let before = self.node.status().last_index;
        if let Err(error) = self.node.propose(command) {
            let _ = notify.send(Err(propose_error(&error, self.region_id)));
            return;
        }
        let status = self.node.status();
        if status.last_index == before {
            // `propose` reported success but appended nothing, which no path should reach.
            // Failing the caller is better than leaving it waiting for an entry that will never
            // arrive.
            let _ = notify.send(Err(ProtoError::internal("the proposal appended no entry")));
            return;
        }
        self.pending.push(Pending {
            index: status.last_index,
            term: status.term,
            notify,
        });
    }

    fn read_index(&mut self, notify: oneshot::Sender<std::result::Result<Index, ProtoError>>) {
        if self.node.role() != Role::Leader {
            let _ = notify.send(Err(self.not_leader()));
            return;
        }
        // The tag has to be unique per outstanding round, and it has to come back recognisable.
        // A counter is enough: the driver is the only issuer.
        let token = self.next_read_token();
        self.reads.push(PendingRead {
            index: token,
            notify,
        });
        self.node.read_index(read_ctx(token));
    }

    fn next_read_token(&mut self) -> u64 {
        // Tokens are only ever compared against the ones this peer issued, so any strictly
        // increasing sequence does; starting above every real index keeps a token from colliding
        // with the commit index a completed round replaces it with.
        const TOKEN_BASE: u64 = 1 << 62;
        let highest = self.reads.iter().map(|read| read.index).max().unwrap_or(0);
        highest.max(TOKEN_BASE) + 1
    }
}

/// A read round's tag: the token, big-endian.
fn read_ctx(token: u64) -> Bytes {
    Bytes::copy_from_slice(&token.to_be_bytes())
}

fn read_token(ctx: &Bytes) -> std::result::Result<u64, ()> {
    let bytes: [u8; 8] = ctx.as_ref().try_into().map_err(|_| ())?;
    Ok(u64::from_be_bytes(bytes))
}

fn propose_error(error: &esker_raft::RaftError, region_id: u64) -> ProtoError {
    match error {
        esker_raft::RaftError::NotLeader => ProtoError::NotLeader {
            region_id,
            leader_hint: None,
        },
        esker_raft::RaftError::LeadershipTransferInProgress(_)
        | esker_raft::RaftError::ConfChangePending(_) => ProtoError::ServerIsBusy {
            reason: error.to_string(),
        },
        other => ProtoError::internal(other.to_string()),
    }
}

/// The handle the request path holds: a channel to the driver thread, and the one fact it needs
/// often enough to be worth publishing without asking.
#[derive(Debug)]
pub struct RaftPeer {
    commands: mpsc::Sender<PeerMsg>,
    region_id: u64,
    peer_id: NodeId,
    leader: Arc<AtomicU64>,
    published: Arc<Published>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RaftPeer {
    /// Builds the peer and starts its driver thread.
    pub fn start(
        options: PeerOptions,
        storage: RaftLogStorage,
        transport: Arc<dyn RaftTransport>,
        host: Arc<dyn RegionHost>,
    ) -> Result<Arc<Self>> {
        let region_id = options.region.id;
        let mut config = RaftConfig::new(options.peer_id, options.voters, options.seed);
        config.applied = storage.applied_index();
        let applied_index = storage.applied_index();

        let node = RawNode::new(config, storage).map_err(|error| {
            StoreError::Bootstrap(format!("could not start the Raft peer: {error}"))
        })?;

        let leader = Arc::new(AtomicU64::new(0));
        let published = Arc::new(Published::default());
        published.applied.store(applied_index, Ordering::Release);
        let core = PeerCore {
            node,
            transport,
            host,
            region: options.region,
            region_id,
            peer_id: options.peer_id,
            pending: Vec::new(),
            reads: Vec::new(),
            leader: Arc::clone(&leader),
            published: Arc::clone(&published),
            applied_index,
        };

        let (commands, receiver) = mpsc::channel(PEER_QUEUE_DEPTH);
        let thread = std::thread::Builder::new()
            .name(format!("raft-{region_id}"))
            .spawn(move || run(core, receiver))
            .map_err(|error| {
                StoreError::Bootstrap(format!("could not start the Raft thread: {error}"))
            })?;

        Ok(Arc::new(Self {
            commands,
            region_id,
            peer_id: options.peer_id,
            leader,
            published,
            thread: std::sync::Mutex::new(Some(thread)),
        }))
    }

    /// The region this peer serves.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        self.region_id
    }

    /// This peer's Raft id.
    #[must_use]
    pub fn peer_id(&self) -> NodeId {
        self.peer_id
    }

    /// Who this peer believes leads, without asking the driver thread.
    #[must_use]
    pub fn leader(&self) -> Option<NodeId> {
        match self.leader.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// Whether this peer believes it is the leader. A hint, checked again by the driver — a peer
    /// that has just been deposed will still say yes for one round trip, and the proposal it
    /// accepts on the strength of it is failed rather than applied.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.leader() == Some(self.peer_id)
    }

    /// The term this peer was last in, without asking the driver thread.
    ///
    /// A heartbeat's worth of freshness, not a decision's: it is what the driver published at the
    /// end of its last round, so a peer that changed term microseconds ago still reports the old
    /// one. Nothing decides anything on it — the region heartbeat carries it, and the next one
    /// corrects it.
    #[must_use]
    pub fn term(&self) -> Term {
        self.published.term.load(Ordering::Acquire)
    }

    /// How far the state machine has applied, without asking the driver thread. Same freshness
    /// rule as [`RaftPeer::term`].
    #[must_use]
    pub fn applied_index(&self) -> Index {
        self.published.applied.load(Ordering::Acquire)
    }

    /// Roughly how many bytes this region holds — the trigger for a split, and what a region
    /// heartbeat reports (`docs/DESIGN.md` §6).
    ///
    /// It is the bytes this peer's *apply* has staged since the process started, and it is wrong
    /// in three known directions: it does not shrink when a key is deleted or a compaction drops
    /// an overwrite, it does not count what was on disk before this process opened the database,
    /// and two peers of one region will not agree on it.
    ///
    /// All three are acceptable because **nothing deterministic reads it**. A number that fed into
    /// apply would have to be identical on every peer or the peers would be two state machines;
    /// this one only decides when a leader *looks* at its region, and the split key that follows
    /// comes from the data itself. The honest number needs a per-range size from the engine, which
    /// it does not expose — `docs/plans/phase-4.md` §12.3 names the accessor and asks for it.
    #[must_use]
    pub fn approximate_size(&self) -> u64 {
        self.published.size.load(Ordering::Relaxed)
    }

    /// The error a non-leader answers with.
    #[must_use]
    pub fn not_leader(&self) -> ProtoError {
        ProtoError::NotLeader {
            region_id: self.region_id,
            leader_hint: self.leader().filter(|id| *id != self.peer_id),
        }
    }

    /// Feeds one Raft message in.
    pub async fn step(&self, message: Message) -> std::result::Result<(), ProtoError> {
        self.send(PeerMsg::Raft(message)).await
    }

    /// One logical tick.
    pub async fn tick(&self) -> std::result::Result<(), ProtoError> {
        self.send(PeerMsg::Tick).await
    }

    /// Replicates `command` and waits for it to *apply*.
    ///
    /// Takes a [`Command`] rather than bytes on purpose. An entry whose payload cannot be decoded
    /// cannot be applied, and skipping it would leave this peer's state machine differing from
    /// every other's — so the driver treats one as a hard failure. Accepting only commands here
    /// means the log can never contain a payload the apply loop will refuse.
    pub async fn propose(&self, command: &Command) -> std::result::Result<Applied, ProtoError> {
        let (notify, answer) = oneshot::channel();
        self.send(PeerMsg::Propose {
            command: command.encode(),
            notify,
        })
        .await?;
        answer
            .await
            .map_err(|_| ProtoError::internal("the Raft peer stopped"))?
    }

    /// Establishes a linearizable read, returning once the state machine has applied through it.
    pub async fn read_index(&self) -> std::result::Result<Index, ProtoError> {
        let (notify, answer) = oneshot::channel();
        self.send(PeerMsg::ReadIndex { notify }).await?;
        answer
            .await
            .map_err(|_| ProtoError::internal("the Raft peer stopped"))?
    }

    /// What this peer believes.
    pub async fn status(&self) -> std::result::Result<Status, ProtoError> {
        let (notify, answer) = oneshot::channel();
        self.send(PeerMsg::Status(notify)).await?;
        answer
            .await
            .map_err(|_| ProtoError::internal("the Raft peer stopped"))
    }

    /// Drives the clock. **This is the only place a wall clock touches consensus**: the core
    /// counts ticks and never reads one (`CLAUDE.md` invariant 4).
    pub fn spawn_ticker(
        self: &Arc<Self>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        self.spawn_ticker_on(&tokio::runtime::Handle::current(), interval)
    }

    /// [`RaftPeer::spawn_ticker`], on a runtime named explicitly.
    ///
    /// A split starts the child's peer from the **parent's driver thread**, which is a plain OS
    /// thread with no runtime of its own; `tokio::spawn` there is a panic. The store holds a handle
    /// from the runtime it was opened on and passes it here.
    pub fn spawn_ticker_on(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let peer = Arc::clone(self);
        runtime.spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if peer.tick().await.is_err() {
                    return;
                }
            }
        })
    }

    /// Stops the driver thread and waits for it, failing everything outstanding.
    pub fn stop(&self) {
        // A full queue on shutdown must not deadlock the caller: the thread is going away either
        // way, and dropping the sender ends its loop.
        let _ = self.commands.try_send(PeerMsg::Stop);
        let handle = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    async fn send(&self, message: PeerMsg) -> std::result::Result<(), ProtoError> {
        self.commands.send(message).await.map_err(|_| {
            // The thread is gone, so the request provably did not reach Raft.
            ProtoError::not_sent("the Raft peer is not running")
        })
    }
}

impl Drop for RaftPeer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The driver thread.
///
/// Every wake-up drains whatever else is queued before driving, so one `Ready` covers a batch of
/// messages rather than one apiece — which is where a leader's per-tick batching comes from.
fn run(mut core: PeerCore, mut receiver: mpsc::Receiver<PeerMsg>) {
    while let Some(message) = receiver.blocking_recv() {
        let mut running = core.handle(message);
        while running {
            match receiver.try_recv() {
                Ok(next) => running = core.handle(next),
                Err(_) => break,
            }
        }
        if let Err(error) = core.drive() {
            // A failed write is not something this layer can paper over: the log and the state
            // machine may now disagree. Say so loudly and stop, rather than continue on a log
            // whose durability is unknown.
            tracing::error!(region_id = core.region_id, %error, "the Raft driver failed");
            break;
        }
        if !running {
            break;
        }
    }
    let stopping = ProtoError::not_sent("the Raft peer stopped");
    core.fail_outstanding(&stopping);
}

/// Re-exported so a caller can name the configuration a peer bootstraps with.
pub type PeerConfState = ConfState;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, ReadOptions, WalSyncMode, cf};
    use esker_keys::prefix;
    use esker_proto::{ProtoError, Region};
    use esker_raft::{ConfState, LogStorage, Message, Role};

    use super::{
        Applied, DiscardTransport, NoHost, PEER_QUEUE_DEPTH, PeerOptions, RaftPeer, RaftTransport,
    };
    use crate::apply::Command;
    use crate::raft_log::{PersistedState, RaftLogStorage, decode_entry, log_entry_key, state_key};

    const REGION: u64 = 1;

    /// Any valid command; several tests only need the proposal to be well-formed.
    fn put(key: &'static [u8]) -> Command {
        Command::Put {
            key: Bytes::from_static(key),
            value: Bytes::from_static(b"v"),
        }
    }

    fn open_db() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap();
        (dir, Arc::new(db))
    }

    fn start(
        db: &Arc<Db>,
        peer_id: u64,
        voters: Vec<u64>,
        transport: Arc<dyn RaftTransport>,
    ) -> Arc<RaftPeer> {
        let storage = RaftLogStorage::open(
            Arc::clone(db),
            REGION,
            ConfState::from_voters(voters.clone()),
        )
        .unwrap();
        RaftPeer::start(
            PeerOptions {
                region: Region::bootstrap(REGION, 1, peer_id),
                peer_id,
                voters,
                seed: 7,
            },
            storage,
            transport,
            Arc::new(NoHost),
        )
        .unwrap()
    }

    /// A transport that checks the driver contract at the only moment it can be checked: when a
    /// message is handed over. Every entry an `AppendEntries` carries must already be readable
    /// from the log, and every vote a response grants must already be in the state record.
    ///
    /// This is rule 1 of `Ready`'s contract, asserted from outside the core — which is the whole
    /// reason the core does no I/O.
    #[derive(Debug)]
    struct Auditor {
        db: Arc<Db>,
        sent: Mutex<Vec<Message>>,
        violations: Mutex<Vec<String>>,
        /// Entries the audit actually inspected. A count of what was checked is the only thing
        /// that makes "no violations" mean anything.
        audited: AtomicUsize,
    }

    impl Auditor {
        fn new(db: &Arc<Db>) -> Arc<Self> {
            Arc::new(Self {
                db: Arc::clone(db),
                sent: Mutex::new(Vec::new()),
                violations: Mutex::new(Vec::new()),
                audited: AtomicUsize::new(0),
            })
        }

        fn violation(&self, detail: String) {
            self.violations.lock().unwrap().push(detail);
        }

        fn audited(&self) -> usize {
            self.audited.load(Ordering::Relaxed)
        }

        fn state(&self) -> Option<PersistedState> {
            self.db
                .get(cf::RAFT, &state_key(REGION), &ReadOptions::default())
                .ok()
                .flatten()
                .and_then(|bytes| PersistedState::decode(&bytes).ok())
        }

        fn violations(&self) -> Vec<String> {
            self.violations.lock().unwrap().clone()
        }

        fn take_sent(&self) -> Vec<Message> {
            std::mem::take(&mut self.sent.lock().unwrap())
        }
    }

    impl RaftTransport for Auditor {
        fn send(&self, messages: Vec<Message>) {
            for message in &messages {
                match message {
                    Message::AppendEntries { entries, .. } => {
                        for entry in entries {
                            self.audited.fetch_add(1, Ordering::Relaxed);
                            let key = log_entry_key(REGION, entry.index);
                            let stored = self
                                .db
                                .get(cf::RAFT, &key, &ReadOptions::default())
                                .ok()
                                .flatten()
                                .and_then(|bytes| decode_entry(entry.index, &bytes).ok());
                            if stored.as_ref() != Some(entry) {
                                self.violation(format!(
                                    "entry {} was sent before it was durable",
                                    entry.index
                                ));
                            }
                        }
                    }
                    // The candidate voted for itself before asking anyone else.
                    Message::RequestVote {
                        from,
                        pre_vote: false,
                        ..
                    } => {
                        if self.state().and_then(|state| state.hard_state.voted_for) != Some(*from)
                        {
                            self.violation(
                                "a vote request went out before the self-vote was durable".into(),
                            );
                        }
                    }
                    // And a granted vote is on disk before the candidate can count it.
                    Message::RequestVoteResponse {
                        to,
                        granted: true,
                        pre_vote: false,
                        ..
                    } if self.state().and_then(|state| state.hard_state.voted_for) != Some(*to) => {
                        self.violation("a vote was granted before it was durable".into());
                    }
                    _ => {}
                }
            }
            self.sent.lock().unwrap().extend(messages);
        }
    }

    /// Ticks a lone voter until it elects itself, syncing through the driver each time so the
    /// test is not asking about work that has not happened yet.
    async fn elect_alone(peer: &Arc<RaftPeer>) {
        for _ in 0..400 {
            peer.tick().await.unwrap();
            if peer.status().await.unwrap().role == Role::Leader {
                return;
            }
        }
        panic!("a lone voter never elected itself");
    }

    /// Answers everything the peer sends — votes and appends alike — so a single running peer
    /// behaves like a healthy group of three. Without acknowledging the appends, a proposal would
    /// never reach a quorum and the test would wait for ever.
    async fn pump(peer: &Arc<RaftPeer>, auditor: &Arc<Auditor>, voters: &[u64], rounds: usize) {
        for _ in 0..rounds {
            for message in auditor.take_sent() {
                match message {
                    Message::RequestVote {
                        from,
                        term,
                        pre_vote,
                        ..
                    } => {
                        for voter in voters.iter().filter(|id| **id != from) {
                            peer.step(Message::RequestVoteResponse {
                                from: *voter,
                                to: from,
                                term,
                                granted: true,
                                pre_vote,
                            })
                            .await
                            .unwrap();
                        }
                    }
                    Message::AppendEntries {
                        from,
                        to,
                        term,
                        prev_log_index,
                        entries,
                        ..
                    } => {
                        let index = entries.last().map_or(prev_log_index, |entry| entry.index);
                        peer.step(Message::AppendEntriesResponse {
                            from: to,
                            to: from,
                            term,
                            reject: false,
                            index,
                            hint_term: 0,
                            context: Bytes::new(),
                        })
                        .await
                        .unwrap();
                    }
                    _ => {}
                }
            }
            peer.tick().await.unwrap();
            let _ = peer.status().await.unwrap();
        }
    }

    /// **The driver contract, unit-tested.** Nothing the transport was handed had left the log
    /// behind: every entry it carried was already readable from the `raft` column family, and
    /// every vote it depended on was already in the state record.
    ///
    /// This is the assertion that only exists because the core does no I/O. If `esker-raft`
    /// persisted its own state, "persist before send" would be an internal detail with no
    /// observer.
    #[tokio::test]
    async fn a_message_is_never_sent_before_its_entries_are_durable() {
        let (_dir, db) = open_db();
        let auditor = Auditor::new(&db);
        let peer = start(
            &db,
            1,
            vec![1, 2, 3],
            Arc::clone(&auditor) as Arc<dyn RaftTransport>,
        );

        // Elect first: a proposal before there is a leader is refused, correctly.
        pump(&peer, &auditor, &[1, 2, 3], 100).await;
        assert!(peer.is_leader(), "the peer never took office");

        // Then the proposals run alongside the pump, because each waits for its entry to apply
        // and that needs the acknowledgements the pump provides.
        let proposer = {
            let peer = Arc::clone(&peer);
            tokio::spawn(async move {
                for index in 0..8_u32 {
                    let command = Command::Put {
                        key: Bytes::from(index.to_be_bytes().to_vec()),
                        value: Bytes::from_static(b"v"),
                    };
                    peer.propose(&command)
                        .await
                        .expect("a proposal on the leader");
                }
            })
        };
        pump(&peer, &auditor, &[1, 2, 3], 400).await;
        proposer.await.expect("the proposals completed");

        let violations = auditor.violations();
        assert!(
            violations.is_empty(),
            "driver contract violated: {violations:?}"
        );
        assert!(
            auditor.audited() >= 8,
            "the audit inspected only {} entries, so it proved nothing",
            auditor.audited()
        );
        peer.stop();
    }

    /// A single voter is its own majority, so it takes office from ticks alone and its proposals
    /// commit and apply without a network at all.
    #[tokio::test]
    async fn a_lone_voter_applies_its_own_proposals() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1], Arc::new(DiscardTransport));

        for _ in 0..200 {
            peer.tick().await.unwrap();
            if peer.is_leader() {
                break;
            }
        }
        assert!(peer.is_leader(), "a lone voter should elect itself");
        assert_eq!(peer.leader(), Some(1));

        let put = |key: &'static [u8]| Command::Put {
            key: Bytes::from_static(key),
            value: Bytes::from_static(b"v"),
        };
        assert_eq!(peer.propose(&put(b"one")).await.unwrap(), Applied::Done);
        assert_eq!(peer.propose(&put(b"two")).await.unwrap(), Applied::Done);

        let status = peer.status().await.unwrap();
        assert_eq!(status.role, Role::Leader);
        // The no-op plus two proposals, all applied.
        assert_eq!(status.last_index, 3);
        peer.stop();

        // And the apply index is on disk, where a restart will resume from.
        let reopened = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
        assert_eq!(reopened.applied_index(), 3);
        assert_eq!(reopened.last_index().unwrap(), 3);
    }

    /// The split trigger's input. It has to grow with what was actually written — a counter that
    /// moved per *entry* rather than per byte would fire a split on a region of a million empty
    /// keys and never on one holding a single large value.
    #[tokio::test]
    async fn the_size_hint_grows_with_the_bytes_that_were_applied() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1], Arc::new(DiscardTransport));
        elect_alone(&peer).await;

        // The leader's own no-op has already applied, and it stages nothing but the state record.
        let empty = peer.approximate_size();

        peer.propose(&Command::Put {
            key: Bytes::from_static(b"k"),
            value: Bytes::from(vec![b'v'; 4096]),
        })
        .await
        .unwrap();
        let after_big = peer.approximate_size();
        assert!(
            after_big >= empty + 4096,
            "4 KiB of value did not reach the hint: {empty} then {after_big}"
        );

        peer.propose(&Command::Put {
            key: Bytes::from_static(b"j"),
            value: Bytes::from_static(b"v"),
        })
        .await
        .unwrap();
        let after_small = peer.approximate_size();
        assert!(after_small > after_big, "a small write moved it not at all");
        assert!(
            after_small - after_big < 4096,
            "a one-byte value cost as much as a 4 KiB one"
        );
        peer.stop();

        // It is a hint held in memory, and it says so by starting again from nothing. A restart
        // therefore delays a split rather than mis-sizing one, which is the safe direction.
        let restarted = start(&db, 1, vec![1], Arc::new(DiscardTransport));
        assert_eq!(restarted.approximate_size(), 0);
        restarted.stop();
    }

    /// End to end: a command proposed on the leader is replicated, applied, and visible in the
    /// data column family — with its `apply_index` recorded in the same batch that wrote it.
    #[tokio::test]
    async fn a_committed_command_lands_in_the_data_and_the_apply_index_with_it() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1], Arc::new(DiscardTransport));
        elect_alone(&peer).await;

        let put = Command::Put {
            key: Bytes::from_static(b"k"),
            value: Bytes::from_static(b"v"),
        };
        assert_eq!(peer.propose(&put).await.unwrap(), Applied::Done);

        let stored = db
            .get(cf::DEFAULT, &prefix::raw_key(b"k"), &ReadOptions::default())
            .unwrap();
        assert_eq!(stored.as_deref(), Some(&b"v"[..]));

        // The apply index moved with it. Both were in one batch, so a crash has both or neither.
        let state = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
        assert_eq!(state.applied_index(), state.last_index().unwrap());
        assert!(state.applied_index() >= 2, "the no-op and the command");
        peer.stop();
    }

    /// A `CompareAndSwap` is decided when the entry applies, not when it is proposed — so its
    /// answer comes back through the same path a write's does, from this peer's own apply.
    #[tokio::test]
    async fn a_compare_and_swap_answers_from_its_apply() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1], Arc::new(DiscardTransport));
        elect_alone(&peer).await;

        let swap = |expected: Option<&'static [u8]>, value: Option<&'static [u8]>| {
            Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: expected.map(Bytes::from_static),
                value: value.map(Bytes::from_static),
            }
        };
        assert_eq!(
            peer.propose(&swap(None, Some(b"one"))).await.unwrap(),
            Applied::Swapped {
                swapped: true,
                previous: None
            }
        );
        assert_eq!(
            peer.propose(&swap(None, Some(b"two"))).await.unwrap(),
            Applied::Swapped {
                swapped: false,
                previous: Some(Bytes::from_static(b"one"))
            }
        );
        peer.stop();
    }

    /// A peer that does not lead cannot order anything, and says so with the redirect a client
    /// acts on rather than an opaque failure.
    #[tokio::test]
    async fn a_follower_refuses_a_proposal_with_a_redirect() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1, 2, 3], Arc::new(DiscardTransport));

        let error = peer.propose(&put(b"x")).await.unwrap_err();
        assert!(matches!(
            error,
            ProtoError::NotLeader {
                region_id: REGION,
                ..
            }
        ));
        assert!(error.is_retryable());
        assert_eq!(error.outcome(), esker_proto::RequestOutcome::NotApplied);

        let error = peer.read_index().await.unwrap_err();
        assert!(matches!(error, ProtoError::NotLeader { .. }));
        peer.stop();
    }

    /// A read is answered with an index the state machine has *already* reached. Answering at the
    /// commit index before applying it would return a state older than the read's own position.
    #[tokio::test]
    async fn a_read_is_answered_only_once_its_index_has_applied() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1], Arc::new(DiscardTransport));
        for _ in 0..200 {
            peer.tick().await.unwrap();
            if peer.is_leader() {
                break;
            }
        }
        peer.propose(&put(b"one")).await.unwrap();

        let index = peer.read_index().await.unwrap();
        let reopened = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
        assert!(
            reopened.applied_index() >= index,
            "a read was answered at {index} with only {} applied",
            reopened.applied_index()
        );
        peer.stop();
    }

    /// Stopping fails everything outstanding rather than leaving a caller waiting for an answer
    /// that can never come.
    #[tokio::test]
    async fn stopping_fails_outstanding_work_rather_than_stranding_it() {
        let (_dir, db) = open_db();
        let peer = start(&db, 1, vec![1, 2, 3], Arc::new(DiscardTransport));
        peer.stop();

        let error = peer.propose(&put(b"x")).await.unwrap_err();
        assert!(matches!(error, ProtoError::NotSent { .. }));
        assert_eq!(error.outcome(), esker_proto::RequestOutcome::NotApplied);
    }

    /// Nothing that crosses a thread here is unbounded: an unbounded queue in front of an `fsync`
    /// is a memory leak with extra steps.
    #[test]
    fn the_driver_queue_is_bounded() {
        assert!(PEER_QUEUE_DEPTH > 0 && PEER_QUEUE_DEPTH <= 65_536);
    }
}
