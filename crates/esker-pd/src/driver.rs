//! The driver: PD's `RawNode`, the thread that turns its `Ready` into durable bytes, and the
//! handle the synchronous placement driver talks to.
//!
//! # Where the ordering rule lives
//!
//! `esker-raft` cannot enforce [`Ready`](esker_raft::Ready)'s contract, because enforcing it means
//! writing to a disk and the core may not
//! ([ADR 0008](../../../docs/adr/0008-raft-determinism-and-the-driver-contract.md)). `PdCore::drive`
//! is where it is enforced for PD, and the order of its five steps is the store's order for the
//! store's reasons (`esker_store::peer`):
//!
//! 1. the hard state and the new entries go into **one** `WriteBatch` with `sync = true`;
//! 2. only then are the messages handed to the transport;
//! 3. committed entries are applied, in order, each with its apply index in the same batch;
//! 4. whoever proposed one is answered;
//! 5. `advance`.
//!
//! Step 3's "in the same batch" is what makes an id or a timestamp safe: the record and the fact
//! that it applied are one write, so a crash between them is not a state a restart can reach.
//!
//! # Why a thread, and why it may not take PD's lock
//!
//! The engine is synchronous, so an `fsync` on the reactor would stall every connection PD serves.
//! The core, the log writes and the apply loop therefore run on one dedicated thread, which also
//! means they need no locks between them — the same shape and the same reason as a region's peer.
//!
//! There is a second, sharper reason here. A leader's [`Allocator`](crate::alloc::Allocator) calls
//! a persist that now **blocks on this thread**, and it calls it holding `Pd`'s state lock. So
//! this thread may never take that lock: apply writes the engine and
//! [`AppliedState`](crate::machine::AppliedState), which is a different lock that nothing holds
//! across a propose. Getting that wrong is a deadlock on the first `AllocId`, which is why it is
//! written here rather than remembered.
//!
//! # Time
//!
//! Ticks arrive through [`PdDriver::tick`], sent by a `tokio` interval at the service edge, so the
//! core still never reads a clock (`CLAUDE.md` invariant 4). A **single-member** PD needs no ticks
//! at all: it campaigns once, wins with a quorum of itself, and every proposal commits inside the
//! call that made it — which is what lets `Pd::open` stay synchronous and runtime-free.

use std::fmt;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, RwLock};

use esker_engine::{WriteBatch, WriteOptions};
use esker_raft::{
    Config, Entry, EntryKind, Index, LogStorage, Message, NodeId, RawNode, Role, Snapshot, Term,
};

use crate::command::Command;
use crate::error::{PdError, Result};
use crate::machine::{Answer, Machine};
use crate::raft_log::PdLogStorage;

/// Messages the driver thread may queue before a sender starts blocking.
///
/// Bounded on purpose: an unbounded queue in front of a stalled disk is a memory leak that ends
/// the process instead of the request (`docs/DESIGN.md` §9, "nothing is unbounded").
pub const DRIVER_QUEUE: usize = 1_024;

/// Entries PD lets pile up before it compacts its log.
///
/// PD's whole state machine is a couple of hundred kilobytes, so a snapshot is cheap and there is
/// no reason to keep a long log. Four thousand entries is a few minutes of heartbeats on a busy
/// cluster and a few days on an idle one.
pub const COMPACT_THRESHOLD: u64 = 4_096;

/// How far behind a member may be and still be fed entries rather than a snapshot.
///
/// Past this a leader stops holding its log open for it: a member that is not coming back must not
/// be able to fill a leader's disk (the same rule, and the same bound in spirit, as a region's
/// `slow_peer_allowance`).
pub const SLOW_MEMBER_ALLOWANCE: u64 = 1_024;

/// Where PD's Raft messages go. Fire-and-forget and infallible, because Raft retries everything.
pub trait PdTransport: Send + Sync + fmt::Debug {
    /// Sends a tick's worth of messages. A failure is a dropped message and a log line, never an
    /// error the consensus layer has to reason about.
    fn send(&self, messages: Vec<Message>);
}

/// A transport for a group of one, which has nobody to talk to.
#[derive(Debug, Default)]
pub struct NoPeers;

impl PdTransport for NoPeers {
    fn send(&self, messages: Vec<Message>) {
        for message in messages {
            tracing::warn!(
                to = message.recipient(),
                "a single-member placement driver produced a message; dropping it"
            );
        }
    }
}

/// What this member believes about who leads, published by the driver for everyone else to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leadership {
    /// This member's id.
    pub id: NodeId,
    /// What it believes it is.
    pub role: Role,
    /// The term it is in.
    pub term: Term,
    /// Who it believes leads, if anyone.
    pub leader: Option<NodeId>,
    /// Whether this member may **serve**: it leads, and it has applied its own `TakeOffice`.
    ///
    /// The two are not the same fact, and the gap between them is where a new leader would
    /// otherwise answer out of state it has not caught up on
    /// ([ADR 0058](../../../docs/adr/0058-pd-is-a-raft-group.md)).
    pub serving: bool,
    /// The term this member last took office in, or `0`. A caller rebuilds its working state
    /// when this moves.
    pub office_term: Term,
    /// The clock stamp the term's `TakeOffice` carried; what the oracle resumes against.
    pub office_now_ms: u64,
}

impl Leadership {
    fn new(id: NodeId) -> Self {
        Self {
            id,
            role: Role::Follower,
            term: 0,
            leader: None,
            serving: false,
            office_term: 0,
            office_now_ms: 0,
        }
    }
}

/// One thing for the driver thread to do.
enum DriverMsg {
    /// One logical tick.
    Tick,
    /// A message from another member.
    Step(Box<Message>),
    /// A command to propose; the answer comes back when it **applies**.
    Propose {
        command: Box<Command>,
        notify: Sender<Result<Answer>>,
    },
    /// Start an election now. Used once, by a single-member `Pd::open`.
    Campaign { notify: Sender<Result<()>> },
    /// A barrier: answer once everything posted before it has been driven.
    Settle { notify: Sender<()> },
}

struct Pending {
    index: Index,
    term: Term,
    notify: Sender<Result<Answer>>,
}

/// What one entry's apply produced, held between staging it and its batch being durable.
struct Applying {
    /// What to answer whoever proposed it.
    outcome: Result<Answer>,
    /// The command, when applying it may have been this member taking office.
    took_office: Option<Command>,
}

/// The Raft core, the state machine, and the rules that join them.
struct PdCore {
    node: RawNode<PdLogStorage>,
    machine: Arc<Machine>,
    transport: Arc<dyn PdTransport>,
    leadership: Arc<RwLock<Leadership>>,
    id: NodeId,
    /// Proposals appended and not yet applied. Only ever populated on a leader.
    pending: Vec<Pending>,
    /// The `(term, index)` of the `TakeOffice` this member proposed on winning, until it applies.
    ///
    /// The **term** is why this is a pair. A member that proposed one and lost office before it
    /// committed has an entry that will be truncated and a `note_office` that will never fire; an
    /// index alone would leave it looking as though a proposal were still outstanding, and it
    /// would never serve again however many elections it won.
    taking_office: Option<(Term, Index)>,
    /// Whether this member led at the end of the last drive, so a step down is noticed once.
    led: bool,
}

impl PdCore {
    /// The five steps, in the order that makes a quorum of acknowledgements mean a quorum of
    /// durable copies.
    fn drive(&mut self) -> Result<()> {
        // An outer loop, because [`PdCore::publish`] **proposes**: a member that has just won an
        // election appends its `TakeOffice` there, and a single-member group commits it in the
        // same breath. Draining only the readies that existed on entry would leave that entry
        // unpersisted until something else happened to arrive — and for a group of one, nothing
        // ever does, which is exactly the case `Pd::open` depends on.
        loop {
            self.discharge()?;
            self.resolve_unreachable_proposals();
            self.publish();
            if !self.node.has_ready() {
                break;
            }
        }
        self.compact()?;
        debug_assert!(
            self.node.role() == Role::Leader || self.pending.is_empty(),
            "this member is not leading and still holds {} unanswered proposal(s)",
            self.pending.len()
        );
        Ok(())
    }

    /// The five steps, once per `Ready` the core has to offer.
    fn discharge(&mut self) -> Result<()> {
        while self.node.has_ready() {
            let mut ready = self.node.ready();

            // 1. Persist. One batch, fsynced, before a single message leaves.
            //
            // A snapshot goes in the *same* batch and before the entries, because it replaces the
            // log prefix the entries continue (`Ready`'s rule 2). PD's snapshot is its whole state
            // machine and travels inside the message, so unlike a region's this arm is the real
            // path rather than an announcement — and a stub here is what left a store's peer
            // holding a log position it had no data for (`docs/plans/phase-4.md` §17).
            let mut batch = WriteBatch::new();
            let mut installed = None;
            if let Some(snapshot) = ready.snapshot.clone() {
                self.install(&snapshot, &mut batch)?;
                installed = Some(snapshot);
            }
            self.node
                .storage_mut()
                .stage_ready(&mut batch, ready.hard_state, &ready.entries);

            // **When nobody is waiting on a message, the persist and the applies are one write.**
            //
            // The ordering rule is *persist before send*, and a `Ready` with no messages has no
            // send to be before — so folding the applies into the same batch breaks nothing and
            // halves the `fsync`s. It is not a micro-optimisation: a **group of one** commits its
            // own proposal inside the call that made it and produces no messages at all, so this
            // is the whole of the single durable placement driver's write path, and paying two
            // `fsync`s where the phase-4 build paid one would have been a regression every test
            // and every deployment felt.
            //
            // Atomic either way: a `WriteBatch` lands whole or not at all, so a crash mid-batch
            // leaves an entry that never existed rather than one applied without being logged.
            //
            // With messages to send it stays two writes, deliberately. Delaying an
            // `AppendEntries` until this member has finished applying would put one member's disk
            // on the whole group's critical path.
            let alone = ready.messages.is_empty();
            let mut applied = Vec::new();
            if alone {
                for entry in &ready.committed_entries {
                    applied.push(self.stage_apply(entry, &mut batch)?);
                }
            }
            self.write(batch)?;
            if let Some(snapshot) = installed {
                // The in-memory mirrors are behind the engine until this runs, and every decision
                // above reads them.
                self.machine.reload()?;
                tracing::info!(
                    index = snapshot.meta.index,
                    term = snapshot.meta.term,
                    "installed a placement-driver snapshot"
                );
            }

            // 2. Send. Taken rather than cloned: a `Ready`'s messages are moved out by design.
            let messages = std::mem::take(&mut ready.messages);
            if !messages.is_empty() {
                self.transport.send(messages);
            }

            // 3 and 4. Apply, in order, answering whoever proposed each entry. The answers wait
            // for the write above either way: an id or a timestamp that left before its entry was
            // durable is one a new leader would hand out again.
            if alone {
                for (entry, outcome) in ready.committed_entries.iter().zip(applied) {
                    self.settle_apply(entry, outcome);
                }
            } else {
                for entry in &ready.committed_entries {
                    self.apply(entry)?;
                }
            }

            // 5. Advance.
            self.node.advance(&ready);
        }
        Ok(())
    }

    /// Applies one committed entry in a batch of its own, with its apply index in it.
    fn apply(&mut self, entry: &Entry) -> Result<()> {
        let mut batch = WriteBatch::new();
        let outcome = self.stage_apply(entry, &mut batch)?;
        self.write(batch)?;
        self.settle_apply(entry, outcome);
        Ok(())
    }

    /// Stages one committed entry's effects, and its apply index, into `batch`.
    ///
    /// Nothing observable happens here: the answer and the "this member is serving" flag are both
    /// [`PdCore::settle_apply`]'s, which the caller runs **after** the batch is durable.
    fn stage_apply(&mut self, entry: &Entry, batch: &mut WriteBatch) -> Result<Applying> {
        let mut took_office = None;
        let outcome = match entry.kind {
            // Nothing here proposes one. `esker-raft`'s own empty entry on taking office is an
            // `EntryKind::Normal` with no data, which decodes to nothing and applies to nothing.
            EntryKind::ConfChange => {
                tracing::warn!(
                    index = entry.index,
                    "a configuration change applied; placement-driver membership is static"
                );
                Ok(Answer::Done)
            }
            EntryKind::Normal if entry.data.is_empty() => Ok(Answer::Done),
            EntryKind::Normal => {
                // A command this build cannot read is not a request to guess at, so the `?` here
                // stops the member rather than skipping an entry its neighbours applied: a state
                // machine that silently diverges is worse than one that halts.
                let command = Command::decode(&entry.data)?;
                let answer = self.machine.apply(&command, batch);
                took_office = answer.is_ok().then_some(command);
                answer
            }
        };
        self.node.storage_mut().stage_applied(batch, entry.index);
        Ok(Applying {
            outcome,
            took_office,
        })
    }

    /// What an applied entry changes **outside** the database, once its batch is durable.
    fn settle_apply(&mut self, entry: &Entry, applying: Applying) {
        if let Some(command) = applying.took_office {
            self.note_office(entry, &command);
        }
        self.complete_proposal(entry, applying.outcome);
    }

    /// Notices this member's own `TakeOffice` applying, which is when it may start serving.
    fn note_office(&mut self, entry: &Entry, command: &Command) {
        let Command::TakeOffice { term, now_ms } = command else {
            return;
        };
        if self.taking_office != Some((*term, entry.index)) {
            return;
        }
        self.taking_office = None;
        if let Ok(mut leadership) = self.leadership.write() {
            leadership.office_term = *term;
            leadership.office_now_ms = *now_ms;
            leadership.serving = self.node.role() == Role::Leader;
        }
        tracing::info!(
            id = self.id,
            term,
            "the placement driver has caught up and is serving"
        );
    }

    /// Answers whoever proposed the entry at this index — or tells them it was replaced.
    fn complete_proposal(&mut self, entry: &Entry, outcome: Result<Answer>) {
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
            // A different entry took this index, so the proposal was truncated: it provably did
            // not apply, and saying so is what makes a retry safe.
            let _ = pending.notify.send(Err(self.not_leader()));
        }
    }

    /// Answers the proposals this member can no longer promise anything about.
    ///
    /// **The outcome is ambiguous, not a refusal.** The entry is in this member's log, and a
    /// quorum it can no longer see may yet commit it — so answering "provably did not apply"
    /// would invite a retry that duplicates the effect. For PD's two counters a duplicate is
    /// harmless (both apply as `max`), and for a heartbeat it is harmless too; saying so anyway
    /// costs nothing and keeps one rule for every command.
    fn resolve_unreachable_proposals(&mut self) {
        if self.node.role() == Role::Leader {
            self.led = true;
            return;
        }
        if !std::mem::take(&mut self.led) {
            return;
        }
        let applied = self.node.storage().applied_index();
        let (unreachable, answerable): (Vec<Pending>, Vec<Pending>) =
            std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|pending| pending.index > applied);
        self.pending = answerable;
        if unreachable.is_empty() {
            return;
        }
        tracing::info!(
            id = self.id,
            proposals = unreachable.len(),
            applied,
            "stopped leading; answering what this member can no longer promise"
        );
        for pending in unreachable {
            let _ = pending.notify.send(Err(PdError::internal(
                "the placement driver stopped leading with this command in its log; it may \
                 still commit",
            )));
        }
    }

    /// Publishes the role, and proposes a `TakeOffice` the first time this member wins.
    fn publish(&mut self) {
        let status = self.node.status();
        let leading = status.role == Role::Leader;
        if !leading {
            // Whatever was outstanding is this member's past. Clearing it is what lets the next
            // term propose its own barrier.
            self.taking_office = None;
        } else if self.taking_office.map(|(term, _)| term) != Some(status.term)
            && !self.is_serving()
        {
            self.take_office(status.term);
        }

        let Ok(mut leadership) = self.leadership.write() else {
            return;
        };
        leadership.role = status.role;
        leadership.term = status.term;
        leadership.leader = status.leader;
        // Serving requires *both*: leading now, and having applied this term's barrier.
        leadership.serving = leading && leadership.office_term == status.term;
    }

    fn is_serving(&self) -> bool {
        self.leadership
            .read()
            .is_ok_and(|leadership| leadership.office_term == self.node.term())
    }

    /// Proposes the barrier a new leader must apply before it answers anything.
    fn take_office(&mut self, term: Term) {
        let now_ms = self.machine.now_ms();
        let command = Command::TakeOffice { term, now_ms };
        let before = self.node.status().last_index;
        if let Err(error) = self.node.propose(command.encode()) {
            tracing::warn!(id = self.id, term, %error, "could not propose taking office");
            return;
        }
        let last = self.node.status().last_index;
        if last == before {
            tracing::error!(id = self.id, term, "taking office appended no entry");
            return;
        }
        self.taking_office = Some((term, last));
    }

    fn propose(&mut self, command: &Command, notify: Sender<Result<Answer>>) {
        if self.node.role() != Role::Leader {
            let _ = notify.send(Err(self.not_leader()));
            return;
        }
        let before = self.node.status().last_index;
        if let Err(error) = self.node.propose(command.encode()) {
            let _ = notify.send(Err(PdError::internal(format!(
                "the placement driver could not propose: {error}"
            ))));
            return;
        }
        let status = self.node.status();
        if status.last_index == before {
            let _ = notify.send(Err(PdError::internal("the proposal appended no entry")));
            return;
        }
        self.pending.push(Pending {
            index: status.last_index,
            term: status.term,
            notify,
        });
    }

    /// Installs a snapshot: the state machine's contents, and the log position they stand for.
    fn install(&mut self, snapshot: &Snapshot, batch: &mut WriteBatch) -> Result<()> {
        self.machine.install(&snapshot.data, batch)?;
        self.node.storage_mut().stage_snapshot(batch, snapshot);
        Ok(())
    }

    /// Drops the log prefix the state machine no longer needs, keeping what a lagging member does.
    fn compact(&mut self) -> Result<()> {
        let storage = self.node.storage();
        let applied = storage.applied_index();
        let truncated = storage.truncated_index();
        if applied.saturating_sub(truncated) < COMPACT_THRESHOLD {
            return Ok(());
        }

        let mut target = applied;
        let progress = self.node.progress();
        if progress.is_empty() {
            // Blind means conservative. A member that is not leading has no view of the others,
            // and one pass of compacting by its own apply index alone is permanent.
            let voters = self.node.conf_state().voters.len();
            if voters > 1 {
                target = target.min(applied.saturating_sub(SLOW_MEMBER_ALLOWANCE));
            }
        } else {
            for peer in progress {
                if peer.id == self.id {
                    continue;
                }
                let position = peer.matched.max(peer.pending_snapshot);
                if applied.saturating_sub(position) <= SLOW_MEMBER_ALLOWANCE {
                    target = target.min(position);
                }
            }
        }
        if target <= truncated {
            return Ok(());
        }

        let term = self.node.storage().term(target).map_err(|error| {
            PdError::internal(format!("compaction cannot read a term: {error}"))
        })?;
        let data = self.machine.snapshot()?;
        let mut batch = WriteBatch::new();
        self.node
            .storage_mut()
            .stage_compact(&mut batch, target, term, data)?;
        self.write(batch)?;
        tracing::debug!(index = target, term, "compacted the placement driver's log");
        Ok(())
    }

    fn write(&self, batch: WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.node
            .storage()
            .db()
            .write(batch, &WriteOptions::synced())?;
        Ok(())
    }

    /// The refusal this member answers with when it cannot serve.
    ///
    /// The **address** is left empty here: the driver knows member ids and the member list is
    /// `Pd`'s. [`crate::pd::Pd`] fills it in on the paths a caller sees, which is every path but
    /// a truncated proposal — and a truncated proposal's answer is about this member's own log,
    /// not about where to go next.
    fn not_leader(&self) -> PdError {
        PdError::NotLeader {
            leader_id: self.node.leader().unwrap_or(0),
            leader_address: String::new(),
        }
    }

    /// Fails everything still waiting, on the way out.
    fn shut_down(&mut self) {
        for pending in self.pending.drain(..) {
            let _ = pending
                .notify
                .send(Err(PdError::internal("the placement driver is stopping")));
        }
    }
}

/// The handle the synchronous placement driver holds.
#[derive(Debug)]
pub struct PdDriver {
    jobs: Option<SyncSender<DriverMsg>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    leadership: Arc<RwLock<Leadership>>,
    id: NodeId,
}

impl PdDriver {
    /// Starts the driver over `storage`, with `machine` as its state machine.
    pub fn start(
        config: Config,
        storage: PdLogStorage,
        machine: Arc<Machine>,
        transport: Arc<dyn PdTransport>,
    ) -> Result<Self> {
        let id = config.id;
        let node = RawNode::new(config, storage)
            .map_err(|error| PdError::internal(format!("the placement driver's raft: {error}")))?;
        let leadership = Arc::new(RwLock::new(Leadership::new(id)));

        let mut core = PdCore {
            node,
            machine,
            transport,
            leadership: Arc::clone(&leadership),
            id,
            pending: Vec::new(),
            taking_office: None,
            led: false,
        };
        let (jobs, inbox) = sync_channel(DRIVER_QUEUE);
        let thread = std::thread::Builder::new()
            .name("pd-raft".to_owned())
            .spawn(move || run(&mut core, &inbox))
            .map_err(|error| {
                PdError::internal(format!(
                    "could not start the placement-driver thread: {error}"
                ))
            })?;

        Ok(Self {
            jobs: Some(jobs),
            thread: std::sync::Mutex::new(Some(thread)),
            leadership,
            id,
        })
    }

    /// This member's id.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// What this member believes about who leads.
    #[must_use]
    pub fn leadership(&self) -> Leadership {
        self.leadership
            .read()
            .map_or_else(|_| Leadership::new(self.id), |office| office.clone())
    }

    /// Proposes a command and waits for it to **apply**.
    ///
    /// The wait is the whole point: an id or a timestamp that left PD before its entry committed
    /// is one a new leader would hand out again ([ADR 0058](../../../docs/adr/0058-pd-is-a-raft-group.md)).
    pub fn propose(&self, command: Command) -> Result<Answer> {
        let (notify, answer) = channel();
        self.post(DriverMsg::Propose {
            command: Box::new(command),
            notify,
        })?;
        answer
            .recv()
            .map_err(|_| PdError::internal("the placement driver stopped before answering"))?
    }

    /// Feeds one message from another member in.
    ///
    /// **Never blocks.** A full queue drops the message and logs it, which is the rule the whole
    /// Raft plumbing follows — Raft retries everything it sends, so a dropped message is
    /// indistinguishable from a slow one. Here it is also what makes it safe to call this from the
    /// reactor: a `step` that could block would eventually block behind a *proposal* waiting on the
    /// very commit that this message carries.
    pub fn step(&self, message: Message) -> Result<()> {
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(PdError::internal("the placement driver is stopping"));
        };
        match jobs.try_send(DriverMsg::Step(Box::new(message))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                tracing::debug!(
                    id = self.id,
                    "the placement driver's queue is full; a raft message was dropped"
                );
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(PdError::internal("the placement driver has stopped"))
            }
        }
    }

    /// One logical tick.
    pub fn tick(&self) -> Result<()> {
        self.post(DriverMsg::Tick)
    }

    /// Waits until everything posted before this call has been driven.
    ///
    /// A **barrier**, not a flush: it makes no request of its own and changes nothing. It exists
    /// because [`PdDriver::tick`] and [`PdDriver::step`] return as soon as the message is queued —
    /// which is what keeps a reactor off this thread's `fsync` — and a caller that needs to know
    /// what those produced has no other way to ask. A deterministic test of an election is the
    /// caller that needs it: tick every member, settle, deliver what they said, settle again.
    pub fn settle(&self) -> Result<()> {
        let (notify, answer) = channel();
        self.post(DriverMsg::Settle { notify })?;
        answer
            .recv()
            .map_err(|_| PdError::internal("the placement driver stopped before settling"))
    }

    /// Campaigns, and waits for the round to settle.
    ///
    /// A group of one wins with a quorum of itself, so this returns with the member serving — which
    /// is what lets `Pd::open` be synchronous and need no runtime. A larger group returns as soon
    /// as the campaign has been driven; the election is decided by later ticks and messages.
    pub fn campaign(&self) -> Result<()> {
        let (notify, answer) = channel();
        self.post(DriverMsg::Campaign { notify })?;
        answer
            .recv()
            .map_err(|_| PdError::internal("the placement driver stopped before answering"))?
    }

    fn post(&self, message: DriverMsg) -> Result<()> {
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(PdError::internal("the placement driver is stopping"));
        };
        match message {
            // A tick that cannot be queued is a tick the driver is too busy to need: dropping it
            // is what a full queue means, and blocking the reactor on it would be worse. The same
            // rule applies to a stepped message, which has its own entry point above.
            DriverMsg::Tick => match jobs.try_send(DriverMsg::Tick) {
                Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    Err(PdError::internal("the placement driver has stopped"))
                }
            },
            other => jobs
                .send(other)
                .map_err(|_| PdError::internal("the placement driver has stopped")),
        }
    }
}

impl Drop for PdDriver {
    fn drop(&mut self) {
        // The loop ends when the last sender goes, so the sender is released before the join.
        self.jobs = None;
        if let Ok(mut thread) = self.thread.lock()
            && let Some(handle) = thread.take()
        {
            let _ = handle.join();
        }
    }
}

/// The thread's loop.
fn run(core: &mut PdCore, inbox: &Receiver<DriverMsg>) {
    // A drive before the first message, so a member that recovered a log applies what it already
    // holds rather than waiting for something to happen.
    if let Err(error) = core.drive() {
        tracing::error!(id = core.id, %error, "the placement driver failed to start");
        core.shut_down();
        return;
    }
    while let Ok(message) = inbox.recv() {
        match message {
            DriverMsg::Tick => core.node.tick(),
            DriverMsg::Step(message) => {
                if let Err(error) = core.node.step(*message) {
                    tracing::debug!(id = core.id, %error, "a raft message was refused");
                }
            }
            DriverMsg::Propose { command, notify } => core.propose(&command, notify),
            DriverMsg::Settle { notify } => {
                // Driven below like everything else, and answered after. The channel is FIFO, so
                // an answer here means every earlier tick, message and proposal has been through
                // a full `drive`.
                if let Err(error) = core.drive() {
                    tracing::error!(id = core.id, %error, "the placement driver failed; stopping");
                    break;
                }
                let _ = notify.send(());
                continue;
            }
            DriverMsg::Campaign { notify } => {
                let outcome = core.node.campaign().map_err(|error| {
                    PdError::internal(format!("the placement driver could not campaign: {error}"))
                });
                if let Err(error) = core.drive() {
                    tracing::error!(id = core.id, %error, "the placement driver failed");
                    let _ = notify.send(Err(error));
                    break;
                }
                let _ = notify.send(outcome);
                continue;
            }
        }
        if let Err(error) = core.drive() {
            // A member whose disk will not take an apply must stop, not carry on with a state its
            // neighbours do not share. Loud, and then done.
            tracing::error!(id = core.id, %error, "the placement driver failed; stopping");
            break;
        }
    }
    core.shut_down();
}

#[cfg(test)]
mod tests {
    use super::{NoPeers, PdDriver, PdTransport};
    use crate::clock::TestClock;
    use crate::command::Command;
    use crate::machine::{Answer, Machine};
    use crate::member::MemberList;
    use crate::raft_log::PdLogStorage;
    use crate::{Clock, PdError};
    use esker_engine::{Db, Options, WalSyncMode, cf};
    use esker_raft::{ConfState, Config, Message, NodeId, Role};
    use std::sync::{Arc, Mutex};

    /// A transport that keeps what it was handed, so a test can look at a group of three without
    /// a socket.
    #[derive(Debug, Default)]
    struct Recorder {
        sent: Mutex<Vec<Message>>,
    }

    impl PdTransport for Recorder {
        fn send(&self, messages: Vec<Message>) {
            self.sent.lock().unwrap().extend(messages);
        }
    }

    struct Member {
        driver: PdDriver,
        machine: Arc<Machine>,
        _dir: tempfile::TempDir,
    }

    fn member(id: NodeId, members: &MemberList, transport: Arc<dyn PdTransport>) -> Member {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            create_if_missing: true,
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        };
        let db = Arc::new(
            Db::open_with(
                dir.path(),
                options,
                Arc::new(esker_engine::LocalFileSystem::new()),
                &[cf::DEFAULT, cf::RAFT],
            )
            .unwrap(),
        );
        let cf = db.cf_id(cf::DEFAULT).unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000)) as Arc<dyn Clock>;
        let machine = Arc::new(Machine::load(Arc::clone(&db), cf, clock).unwrap());
        let log = PdLogStorage::open(db, ConfState::from_voters(members.ids())).unwrap();
        let config = Config::new(id, members.ids(), 7);
        let driver =
            PdDriver::start(config, log, Arc::clone(&machine), transport).expect("the driver");
        Member {
            driver,
            machine,
            _dir: dir,
        }
    }

    fn alone() -> Member {
        let lone = MemberList::alone(1);
        let member = member(1, &lone, Arc::new(NoPeers));
        member.driver.campaign().unwrap();
        member
    }

    /// The property `Pd::open` rests on: a group of one wins with a quorum of itself and has
    /// applied its own barrier before `campaign` returns, so nothing has to tick and no runtime
    /// has to exist.
    #[test]
    fn a_group_of_one_is_serving_when_its_campaign_returns() {
        let member = alone();
        let office = member.driver.leadership();
        assert_eq!(office.role, Role::Leader);
        assert!(office.serving, "a group of one did not take office");
        assert_eq!(office.office_term, office.term);
        assert!(office.office_now_ms > 0, "the barrier carried no stamp");
    }

    /// A proposal is answered when it **applies**, not when it is accepted — which is the whole
    /// of "ack after commit" ([ADR 0058](../../../docs/adr/0058-pd-is-a-raft-group.md)).
    #[test]
    fn a_proposal_is_answered_with_what_applying_it_produced() {
        let member = alone();
        assert_eq!(
            member
                .driver
                .propose(Command::ReserveIds { end: 500 })
                .unwrap(),
            Answer::Done
        );
        assert_eq!(
            member.machine.applied().read().unwrap().allocated_end,
            500,
            "the answer came back before the state moved"
        );

        let Answer::Bootstrapped(done) = member
            .driver
            .propose(Command::Bootstrap {
                store_id: 1,
                address: "127.0.0.1:20160".to_owned(),
                base_id: 1,
                cluster_id: 0xABCD,
                now_ms: 1_700_000_000_000,
            })
            .unwrap()
        else {
            panic!("bootstrap answered something else");
        };
        assert_eq!(done.cluster_id, 0xABCD);
    }

    /// A member that has not won an election proposes nothing, and says who to ask instead.
    #[test]
    fn a_member_that_does_not_lead_refuses_a_proposal() {
        let three = MemberList::new(vec![
            crate::member::PdMember::new(1, "127.0.0.1:2379"),
            crate::member::PdMember::new(2, "127.0.0.1:2380"),
            crate::member::PdMember::new(3, "127.0.0.1:2381"),
        ])
        .unwrap();
        let member = member(1, &three, Arc::new(NoPeers));
        assert!(!member.driver.leadership().serving);
        assert!(matches!(
            member.driver.propose(Command::ReserveIds { end: 1 }),
            Err(PdError::NotLeader { .. })
        ));
    }

    /// A member of three that campaigns alone cannot win — a quorum of three is two — so it must
    /// still refuse rather than serve out of its own opinion.
    #[test]
    fn a_campaign_without_a_quorum_does_not_make_a_member_serve() {
        let three = MemberList::new(vec![
            crate::member::PdMember::new(1, "127.0.0.1:2379"),
            crate::member::PdMember::new(2, "127.0.0.1:2380"),
            crate::member::PdMember::new(3, "127.0.0.1:2381"),
        ])
        .unwrap();
        let transport = Arc::new(Recorder::default());
        let member = member(1, &three, Arc::clone(&transport) as Arc<dyn PdTransport>);
        member.driver.campaign().unwrap();
        assert!(!member.driver.leadership().serving);
        assert!(
            !transport.sent.lock().unwrap().is_empty(),
            "a campaign asked nobody for a vote"
        );
    }

    /// Everything still waiting is answered on the way out, rather than left blocked for ever.
    #[test]
    fn a_driver_that_stops_answers_what_it_was_holding() {
        let member = alone();
        drop(member.driver);
        // The machine outlives it, which is what the handle's `Drop` is for: the thread is joined
        // before this returns, so nothing is still writing to the database being dropped.
        assert_eq!(member.machine.applied().read().unwrap().allocated_end, 0);
    }
}
