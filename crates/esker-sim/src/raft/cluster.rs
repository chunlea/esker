//! The discrete-event loop: N `RawNode`s, one network, one seeded generator.
//!
//! `prompts/03-raft.md` (3b). Every run is a function of `(seed, plan, node count)`: the loop
//! owns one [`Pcg32`] and draws from it in a fixed order every event, so adding a fault to the
//! plan does not shift what the previous events did, and re-running a seed replays the failure
//! exactly. A failing sweep prints `ESKER_SIM_SEED=n` and the last events, and that is enough.
//!
//! # One event
//!
//! 1. **Act** — one of: heal, partition, crash, restart, deliver one message, tick every node,
//!    propose. Drawn from the plan, in a fixed order, with every draw made unconditionally so
//!    that the *state* of the cluster never moves the stream.
//! 2. **Pump** — each node in id order: complete a disk write that has come due, then offer the
//!    core a chance to produce a `Ready`. This is where the driver contract lives.
//! 3. **Check** — all four safety properties, over every node, dead ones included.
//!
//! # The driver contract, and breaking it
//!
//! A `Ready` is discharged in this order and no other: make `hard_state`, entries and snapshot
//! durable; *then* send its messages; then apply its committed entries; then `advance`. While a
//! slow disk holds the write, none of it has happened — which is what makes a crash during the
//! write worth simulating.
//!
//! [`Cluster::violate_persist_order`] sends the messages first, before the write. That is the
//! one bug the whole exercise is about, and `tests/raft_persist_order.rs` uses it to prove the
//! checkers catch it. A checker that has never been shown red is decoration.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bytes::Bytes;
use esker_base::hash::{hash64, hash64_with_seed};
use esker_base::rng::Pcg32;
use esker_raft::{Config, Index, Message, NodeId as RaftId, RawNode, Role, TICK_MS, Term};

use crate::fault::FaultPlan;
use crate::net::{NodeId as WireId, SimNetwork};

use super::checkers::{NodeSnapshot, SafetyChecker, Violation};
use super::driver::{DiskWrite, NodeSlot, PersistedStorage};
use super::report::{Event, Failure, Settled, Stats};

/// Stream selector for the event loop's own choices. Distinct from
/// [`crate::net::FAULT_STREAM`] and [`crate::net::SCENARIO_STREAM`] so that a change to one
/// does not move the others.
pub const EVENT_STREAM: u64 = 3;

/// How many `Ready` rounds one node may discharge within a single event. A fast disk lets a
/// node persist, send, apply and advance several times in a row; the bound is there so that one
/// event cannot become an unbounded amount of work.
const READY_ROUNDS_PER_EVENT: u32 = 8;

/// How many events of trace to keep. Enough to see what led to a violation, few enough that a
/// ten-thousand-seed sweep does not spend its memory on runs that passed.
const TRACE_EVENTS: usize = 96;

/// A cluster of Raft nodes over the simulated network.
#[derive(Debug)]
pub struct Cluster {
    seed: u64,
    plan: FaultPlan,
    net: SimNetwork,
    nodes: BTreeMap<RaftId, NodeSlot>,
    voters: Vec<RaftId>,
    rng: Pcg32,
    checker: SafetyChecker,
    trace: VecDeque<Event>,
    event: u64,
    /// Messages in flight, by the token the network actually carries. The network stays
    /// byte-opaque — it is the same one the real TCP transport will implement — and the token
    /// is what a duplicate duplicates.
    messages: BTreeMap<u64, Message>,
    next_token: u64,
    proposal_counter: u64,
    /// The highest commit index any node has reported at any point. A read index above it would
    /// point past everything the cluster has agreed on.
    commit_high_water: Index,
    violate_persist_order: bool,
    /// Throws away the next `Ready` a node takes instead of discharging it. Used by one test to
    /// show that the driver rule below is actually enforced.
    discard_taken_ready: bool,
    /// The `context` each message went out with, so a duplicate or a reordered copy can be
    /// checked against what was sent rather than assumed identical.
    contexts: BTreeMap<u64, Bytes>,
    /// Tokens already delivered once.
    seen: BTreeSet<u64>,
    stats: Stats,
}

impl Cluster {
    /// A cluster of `voters` nodes, ids `1..=voters`, with faults drawn from `seed`.
    pub fn new(seed: u64, plan: FaultPlan, voters: usize) -> Result<Self, Failure> {
        let ids: Vec<RaftId> = (1..=voters as RaftId).collect();
        let wire: Vec<WireId> = ids.iter().map(|id| WireId(*id)).collect();
        let mut cluster = Self {
            seed,
            plan: plan.clone(),
            net: SimNetwork::new(seed, plan, &wire),
            nodes: BTreeMap::new(),
            voters: ids.clone(),
            rng: Pcg32::new(seed, EVENT_STREAM),
            checker: SafetyChecker::new(),
            trace: VecDeque::new(),
            event: 0,
            messages: BTreeMap::new(),
            next_token: 0,
            proposal_counter: 0,
            commit_high_water: 0,
            violate_persist_order: false,
            discard_taken_ready: false,
            contexts: BTreeMap::new(),
            seen: BTreeSet::new(),
            stats: Stats::default(),
        };
        for id in ids {
            let node = cluster.build_node(id, None, 0)?;
            cluster.nodes.insert(id, NodeSlot::new(id, node));
        }
        Ok(cluster)
    }

    /// Sends a `Ready`'s messages *before* making it durable — the violation of the driver
    /// contract that Raft's safety argument rules out. Used by exactly one test, to show that
    /// the checkers go red.
    pub fn violate_persist_order(&mut self, on: bool) {
        self.violate_persist_order = on;
    }

    /// Throws away every `Ready` a node takes, instead of discharging it.
    ///
    /// `docs/plans/phase-3.md` §10.2: a `Ready`'s *state* is re-offered but its *messages* are
    /// moved out, so a driver that takes one and drops it loses them. The rule is that a taken
    /// `Ready` is discharged or dies with its node, and [`Cluster::check`] enforces it. This
    /// switch exists so the enforcement can be shown red.
    pub fn discard_taken_ready(&mut self, on: bool) {
        self.discard_taken_ready = on;
    }

    /// The seed this run came from.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// How many events have run.
    #[must_use]
    pub fn event(&self) -> u64 {
        self.event
    }

    /// The counters, including what the run actually got committed and applied.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            committed: self.checker.committed_upto(),
            applied: self
                .nodes
                .values()
                .map(|slot| slot.applied_index)
                .max()
                .unwrap_or(0),
            ..self.stats
        }
    }

    /// Who leads, if anyone does. When two nodes both believe they lead — which is legal, one of
    /// them is stale — the one with the higher term is returned.
    #[must_use]
    pub fn leader(&self) -> Option<RaftId> {
        self.nodes
            .values()
            .filter(|slot| slot.is_leader())
            .max_by_key(|slot| slot.term())
            .map(|slot| slot.id)
    }

    /// The highest index any node has been observed to commit.
    #[must_use]
    pub fn committed_upto(&self) -> Index {
        self.checker.committed_upto()
    }

    /// How many nodes are running.
    #[must_use]
    pub fn online(&self) -> usize {
        self.nodes.values().filter(|slot| slot.online()).count()
    }

    /// Every node's log, for debugging a violation.
    #[must_use]
    pub fn dump_logs(&self) -> String {
        let mut out = Vec::new();
        for slot in self.nodes.values() {
            out.push(format!(
                "n{} online={} leader={} term={} commit={} applied={} pending={:?}",
                slot.id,
                slot.online(),
                slot.is_leader(),
                slot.term(),
                slot.commit(),
                slot.applied_index,
                slot.pending
                    .as_ref()
                    .map(|w| (w.due, w.ready.entries.len())),
            ));
            for entry in &slot.log {
                out.push(format!(
                    "    i{} t{} p{:#x}",
                    entry.index, entry.term, entry.payload
                ));
            }
        }
        out.join("\n")
    }

    /// The last events, one per line.
    #[must_use]
    pub fn trace_report(&self) -> String {
        let lines: Vec<String> = self
            .trace
            .iter()
            .map(|event| format!("    {event}"))
            .collect();
        format!(
            "  --- last {} of {} events ---\n{}",
            self.trace.len(),
            self.event,
            lines.join("\n")
        )
    }

    /// Replaces the fault plan mid-run.
    ///
    /// A scenario uses this to stop injecting faults before asking for progress: the generator
    /// keeps its position, so everything that already happened is untouched.
    pub fn calm(&mut self, plan: FaultPlan) {
        self.plan = plan.clone();
        self.net.set_plan(plan);
    }

    /// Cuts `side` off from every node that is not in it, replacing any partition in place.
    ///
    /// The random `Partition` action draws its own split; this is for a scenario that wants a
    /// particular one — "the follower that is about to be left behind is *this* one".
    pub fn partition(&mut self, side: &[RaftId]) {
        let wire: Vec<WireId> = side.iter().map(|id| WireId(*id)).collect();
        self.net.heal();
        self.net.partition(&wire);
        self.stats.partitions += 1;
        self.record(Event::Partition {
            side: side.to_vec(),
        });
    }

    /// Runs `events` events, checking every property after each one.
    pub fn run(&mut self, events: u64) -> Result<(), Failure> {
        for _ in 0..events {
            self.step()?;
        }
        Ok(())
    }

    /// Runs one event.
    pub fn step(&mut self) -> Result<(), Failure> {
        self.event += 1;
        self.stats.events += 1;
        self.act()?;
        self.pump()?;
        self.check()
    }

    /// Heals every fault at once: the network is whole and every dead node is back.
    pub fn heal(&mut self) -> Result<(), Failure> {
        if self.net.severed_links() > 0 {
            self.net.heal();
            self.record(Event::Heal);
        }
        let dead: Vec<RaftId> = self
            .nodes
            .values()
            .filter(|slot| !slot.online())
            .map(|slot| slot.id)
            .collect();
        for id in dead {
            self.restart(id)?;
        }
        Ok(())
    }

    /// Runs a healed cluster until a leader has been elected and a proposal made under it has
    /// been applied, or `max_ticks` tick rounds have passed without that happening.
    ///
    /// A "tick round" is one tick of every node plus everything that becomes deliverable
    /// because of it — the unit the election timeout is counted in, so the bound means what
    /// `prompts/03-raft.md` says it should: *a leader is elected and a proposal commits within a
    /// bounded number of ticks*.
    pub fn settle(&mut self, max_ticks: u64) -> Result<Settled, Failure> {
        let payload = self.next_proposal();
        let digest = hash64(&payload);
        // A proposal made to a leader that is deposed before it replicates is simply lost —
        // Raft promises nothing about one attempt, only that a client that retries eventually
        // gets through. So the proposal is re-offered once per leadership, exactly as a real
        // client would, and `attempts` is what a failure reports.
        let mut offered_to: Option<(RaftId, Term)> = None;
        let mut attempts = 0_u32;

        for round in 1..=max_ticks {
            // Everything that can be delivered, is; then one tick of every node. Each of those
            // is an *event*, and the counter has to move: a slow disk's write comes due at an
            // event number, so a settle loop that froze the counter would wedge every node that
            // happened to have a write outstanding when the faults stopped.
            let mut guard = 0;
            loop {
                let delivered = self.tick_event(Cluster::deliver_next)?;
                guard += 1;
                if !delivered || guard > 4_096 {
                    break;
                }
            }
            self.tick_event(|cluster| {
                cluster.tick_all();
                Ok(true)
            })?;

            if self.applied_payload(digest).is_none()
                && let Some(leader) = self.leader()
            {
                let term = self.nodes.get(&leader).map_or(0, NodeSlot::term);
                if offered_to != Some((leader, term)) {
                    let payload = payload.clone();
                    self.tick_event(|cluster| Ok(cluster.propose_on(leader, payload)))?
                        .then(|| {
                            offered_to = Some((leader, term));
                            attempts += 1;
                        });
                }
            }
            if let Some(index) = self.applied_payload(digest)
                && let Some(leader) = self.leader()
            {
                return Ok(Settled {
                    ticks: round,
                    leader,
                    index,
                });
            }
        }
        Err(self.liveness(format!(
            "after {max_ticks} ticks with every fault healed and {attempts} proposal attempts, {}",
            if attempts > 0 {
                "nothing the leader accepted ever applied".to_owned()
            } else {
                format!(
                    "no leader emerged (online: {}/{}, links cut: {})",
                    self.online(),
                    self.voters.len(),
                    self.net.severed_links(),
                )
            }
        )))
    }

    /// Runs `action` as one event: the counter moves, then every node gets its pump and every
    /// property is checked. This is what [`Cluster::step`] does around a drawn action, and what
    /// a scenario that chooses its own actions has to do too.
    fn tick_event(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<bool, Failure>,
    ) -> Result<bool, Failure> {
        self.event += 1;
        self.stats.events += 1;
        let acted = action(self)?;
        self.pump()?;
        self.check()?;
        Ok(acted)
    }

    /// The index a payload was applied at, on any node.
    #[must_use]
    pub fn applied_payload(&self, digest: u64) -> Option<Index> {
        self.nodes.values().find_map(|slot| {
            slot.applied
                .iter()
                .find(|entry| entry.payload == digest)
                .map(|entry| entry.index)
        })
    }

    /// Offers a proposal to `node`. Returns whether it took it.
    pub fn propose_on(&mut self, node: RaftId, payload: Bytes) -> bool {
        let accepted = self
            .nodes
            .get_mut(&node)
            .and_then(|slot| slot.node.as_mut())
            .is_some_and(|raw| raw.propose(payload).is_ok());
        if accepted {
            self.stats.proposals += 1;
        }
        self.record(Event::Propose { node, accepted });
        accepted
    }

    /// Asks `node` for a read index: the index a linearizable read may be served at, once the
    /// state machine has applied it (`docs/DESIGN.md` §5).
    ///
    /// The answer comes back later, in a `Ready`'s read states, carrying `ctx` — and that
    /// context rides on `AppendEntries`, which is why the fault model has to carry it verbatim.
    pub fn read_index_on(&mut self, node: RaftId, ctx: Bytes) {
        if let Some(raw) = self
            .nodes
            .get_mut(&node)
            .and_then(|slot| slot.node.as_mut())
        {
            raw.read_index(ctx);
        }
    }

    /// A fresh, recognisable payload.
    pub fn next_proposal(&mut self) -> Bytes {
        self.proposal_counter += 1;
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&self.seed.to_le_bytes());
        bytes[8..].copy_from_slice(&self.proposal_counter.to_le_bytes());
        Bytes::copy_from_slice(&bytes)
    }

    // --- the event loop ------------------------------------------------------------------

    /// Draws and performs one action.
    ///
    /// Every draw happens, in this order, whether or not it is used: a cluster whose state
    /// decided how many numbers to consume would replay differently the moment a fault landed
    /// one event earlier.
    fn act(&mut self) -> Result<(), Failure> {
        let heal = self.rng.chance(self.plan.heal);
        let partition = self.rng.chance(self.plan.partition);
        let crash = self.rng.chance(self.plan.crash);
        let restart = self.rng.chance(self.plan.restart);
        let choice = self.rng.below(10);
        let split = self.rng.next_u64();
        let who = self.rng.next_u64();

        if heal && self.net.severed_links() > 0 {
            self.net.heal();
            self.record(Event::Heal);
            return Ok(());
        }
        if partition && self.voters.len() > 1 {
            self.split(split);
            return Ok(());
        }
        if crash && let Some(id) = self.pick(who, true) {
            self.crash(id);
            return Ok(());
        }
        if restart && let Some(id) = self.pick(who, false) {
            self.restart(id)?;
            return Ok(());
        }
        match choice {
            0..=5 if self.deliver_next()? => Ok(()),
            8 => {
                if let Some(id) = self.pick(who, true) {
                    let ctx = self.next_proposal();
                    self.read_index_on(id, ctx);
                }
                Ok(())
            }
            9 => {
                if let Some(id) = self.pick(who, true) {
                    let payload = self.next_proposal();
                    self.propose_on(id, payload);
                }
                Ok(())
            }
            _ => {
                self.tick_all();
                Ok(())
            }
        }
    }

    /// One node, drawn from `pick`, that is online (or offline when `online` is false).
    fn pick(&self, pick: u64, online: bool) -> Option<RaftId> {
        let candidates: Vec<RaftId> = self
            .nodes
            .values()
            .filter(|slot| slot.online() == online)
            .map(|slot| slot.id)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let at = usize::try_from(pick % candidates.len() as u64).unwrap_or(0);
        candidates.get(at).copied()
    }

    /// Cuts the cluster into two non-empty halves.
    fn split(&mut self, pick: u64) {
        let count = self.voters.len();
        // A bitmask that is neither empty nor everything: `1 ..= 2^n - 2`.
        let masks = (1_u64 << count) - 2;
        let mask = pick % masks + 1;
        let side: Vec<RaftId> = self
            .voters
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, id)| *id)
            .collect();
        let wire: Vec<WireId> = side.iter().map(|id| WireId(*id)).collect();
        self.net.heal();
        self.net.partition(&wire);
        self.stats.partitions += 1;
        self.record(Event::Partition { side });
    }

    fn crash(&mut self, id: RaftId) {
        if let Some(slot) = self.nodes.get_mut(&id) {
            if slot.pending.is_some() {
                // Not a dropped `Ready`: the process that would have sent its messages is gone.
                self.stats.readys_lost_to_crash += 1;
            }
            slot.crash();
        }
        self.stats.crashes += 1;
        self.record(Event::Crash { node: id });
    }

    fn restart(&mut self, id: RaftId) -> Result<(), Failure> {
        let Some(slot) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        let Some(durable) = slot.take_durable() else {
            return Ok(());
        };
        let life = slot.restarts + 1;
        let node = self.build_node(id, Some(durable), life)?;
        if let Some(revived) = self.nodes.get_mut(&id) {
            revived.revive(node);
        }
        self.stats.restarts += 1;
        self.record(Event::Restart { node: id });
        Ok(())
    }

    /// Ticks every online node, after letting logical time reach the next tick so that anything
    /// due in between lands in an inbox.
    fn tick_all(&mut self) {
        let deadline = self.net.now().saturating_add(TICK_MS);
        self.net.run_until(deadline);
        for slot in self.nodes.values_mut() {
            if let Some(node) = slot.node.as_mut() {
                node.tick();
            }
        }
        self.record(Event::Tick { at: self.net.now() });
    }

    /// Takes one message out of one inbox and steps it into its recipient.
    ///
    /// Which inbox is a draw from the event stream, not node order: a driver that always served
    /// node 1 first would never find the bugs that need node 3 to go first.
    fn deliver_next(&mut self) -> Result<bool, Failure> {
        let mut waiting = self.inboxes_with_mail();
        if waiting.is_empty() {
            if self.net.next_delivery().is_none() {
                return Ok(false);
            }
            self.net.step();
            waiting = self.inboxes_with_mail();
            if waiting.is_empty() {
                return Ok(false);
            }
        }
        let pick = self.rng.next_u64();
        let at = usize::try_from(pick % waiting.len() as u64).unwrap_or(0);
        let Some(&target) = waiting.get(at) else {
            return Ok(false);
        };
        let Some(envelope) = self.net.recv(WireId(target)) else {
            return Ok(false);
        };
        let Some(token) = token_of(&envelope.payload) else {
            return Err(self.driver_error("decode a message token".to_owned()));
        };
        let Some(message) = self.messages.get(&token).cloned() else {
            return Err(self.driver_error(format!("find message {token}")));
        };

        let kind = message.kind_name();
        let term = message.term();
        let from = message.sender();
        self.stats.delivered += 1;
        if !self.seen.insert(token) {
            self.stats.duplicate_deliveries += 1;
        }
        // `ReadIndex` rides on `AppendEntries`'s context (`docs/plans/phase-3.md` §10.3), so the
        // fault model has to carry it verbatim — a duplicate is the *same* message, not a
        // rebuilt one. The network is byte-opaque and carries a token, so nothing can rewrite a
        // field; this is what would notice if that ever stopped being true.
        if let Some(sent) = self.contexts.get(&token) {
            self.stats.contexts_delivered += 1;
            if context_of(&message).as_ref() != Some(sent) {
                return Err(self.driver_error(format!(
                    "message {token} ({kind}) arrived with a different context than it was sent                      with"
                )));
            }
        }

        let mut rejected = None;
        let lost = match self
            .nodes
            .get_mut(&target)
            .and_then(|slot| slot.node.as_mut())
        {
            Some(node) => {
                if let Err(error) = node.step(message) {
                    rejected = Some(error.to_string());
                }
                false
            }
            None => true,
        };
        self.record(Event::Deliver {
            from,
            to: target,
            kind,
            term,
            lost,
        });
        if let Some(reason) = rejected {
            self.stats.rejected += 1;
            self.record(Event::Rejected {
                node: target,
                reason,
            });
        }
        Ok(true)
    }

    fn inboxes_with_mail(&self) -> Vec<RaftId> {
        self.voters
            .iter()
            .copied()
            .filter(|id| self.net.inbox_len(WireId(*id)) > 0)
            .collect()
    }

    // --- the driver contract -------------------------------------------------------------

    /// Gives every node its chance to complete a disk write and to produce a `Ready`.
    fn pump(&mut self) -> Result<(), Failure> {
        for id in self.voters.clone() {
            for _ in 0..READY_ROUNDS_PER_EVENT {
                if !self.pump_once(id)? {
                    break;
                }
            }
        }
        Ok(())
    }

    /// One round for one node. Returns whether anything happened.
    ///
    /// The order below *is* the contract: persist, then send, then apply, then advance. Only
    /// [`Cluster::violate_persist_order`] moves the send, and it moves it to before the write
    /// rather than skipping it.
    fn pump_once(&mut self, id: RaftId) -> Result<bool, Failure> {
        let pending = self
            .nodes
            .get(&id)
            .and_then(|slot| slot.pending.as_ref())
            .map(|write| write.due);
        match pending {
            Some(due) if due <= self.event => self.complete_write(id),
            // A write that has not come due blocks everything behind it. That is the point of it.
            Some(_) => Ok(false),
            None => Ok(self.take_ready(id)),
        }
    }

    /// The fsync landed: make it durable, send, apply, advance — in that order.
    fn complete_write(&mut self, id: RaftId) -> Result<bool, Failure> {
        let mut outgoing: Vec<Message> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        let compact_after = self.plan.compact_after;
        let (mut installed, mut compacted) = (false, false);
        let mut reads: Vec<Index> = Vec::new();

        let Some(slot) = self.nodes.get_mut(&id) else {
            return Ok(false);
        };
        // The `Ready` is taken out only once it is certain to be discharged. Taking it first and
        // bailing out on the next guard would drop it, and a dropped `Ready` loses its messages
        // for good (`docs/plans/phase-3.md` §10.2).
        if slot.node.is_none() {
            return Ok(false);
        }
        let Some(write) = slot.pending.take() else {
            return Ok(false);
        };
        let Some(node) = slot.node.as_mut() else {
            return Ok(false);
        };

        // 1. The fsync.
        if let Err(error) = node.storage_mut().persist(&write.ready) {
            return Err(self.driver_error(format!("persist on n{id}: {error}")));
        }
        events.push(Event::Persist {
            node: id,
            entries: write.ready.entries.len(),
            upto: write.ready.entries.last().map_or(0, |entry| entry.index),
            held: write.held,
        });

        // 2. Only now may the messages go — unless they were already sent on purpose.
        if !write.sent_early && !write.ready.messages.is_empty() {
            outgoing.extend(write.ready.messages.iter().cloned());
            events.push(Event::Emit {
                node: id,
                messages: write.ready.messages.len(),
                early: false,
            });
        }

        // 3. A snapshot the write installed is state the machine now holds without having
        //    applied it entry by entry; then apply whatever is left, in order.
        if let Some(snapshot) = &write.ready.snapshot {
            slot.install_snapshot(snapshot.meta.index);
            events.push(Event::Installed {
                node: id,
                through: snapshot.meta.index,
                term: snapshot.meta.term,
            });
            installed = true;
        }
        if !write.ready.committed_entries.is_empty() {
            slot.apply(&write.ready.committed_entries);
            events.push(Event::Apply {
                node: id,
                upto: slot.applied_index,
            });
        }

        // A read index may be ahead of *this* node's commit index — a follower's read index is
        // the leader's — but never ahead of everything the cluster has committed.
        for read in &write.ready.read_states {
            reads.push(read.index);
            events.push(Event::Read {
                node: id,
                index: read.index,
            });
        }

        // 4. Tell the core it may move on.
        if let Some(node) = slot.node.as_mut() {
            node.advance(&write.ready);
        }

        // 5. The store's own housekeeping: fold applied entries into the snapshot so the log
        //    does not grow without bound. This is what makes `InstallSnapshot` reachable — a
        //    follower that fell behind the compaction boundary cannot be repaired by an append,
        //    because the entries it needs no longer exist.
        if compact_after > 0
            && let Some(through) = compaction_point(slot, compact_after)
            && let Some(node) = slot.node.as_mut()
            && node.storage_mut().compact(through).is_ok()
        {
            compacted = true;
            events.push(Event::Compact { node: id, through });
        }
        slot.refresh();

        let high_water = self.commit_high_water();
        for index in reads {
            self.stats.reads_served += 1;
            if index > high_water {
                return Err(Failure::Safety {
                    seed: self.seed,
                    event: self.event,
                    violation: Violation::ReadIndexBeyondCommit {
                        node: id,
                        index,
                        high_water,
                    },
                    trace: self.trace_report(),
                });
            }
        }
        self.stats.readys_discharged += 1;
        if installed {
            self.stats.snapshots_installed += 1;
        }
        if compacted {
            self.stats.compactions += 1;
        }
        self.emit(outgoing);
        for event in events {
            self.record(event);
        }
        Ok(true)
    }

    /// Takes a `Ready` from the core and hands it to the disk. Returns whether the disk was
    /// fast enough that the caller should come straight back for the next round.
    fn take_ready(&mut self, id: RaftId) -> bool {
        let has_ready = self
            .nodes
            .get(&id)
            .and_then(|slot| slot.node.as_ref())
            .is_some_and(RawNode::has_ready);
        if !has_ready {
            return false;
        }

        // The disk's speed is drawn before the `Ready` is taken, so it does not depend on what
        // the `Ready` turned out to contain.
        let held = if self.rng.chance(self.plan.slow_disk) {
            self.rng
                .range_inclusive(1, self.plan.slow_disk_events.max(1))
        } else {
            0
        };
        let violate = self.violate_persist_order;
        let discard = self.discard_taken_ready;
        let mut outgoing: Vec<Message> = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        let taken;

        let (leads, term) = {
            let Some(slot) = self.nodes.get_mut(&id) else {
                return false;
            };
            let Some(node) = slot.node.as_mut() else {
                return false;
            };
            let ready = node.ready();
            let leads = node.role() == Role::Leader;
            let term = node.term();
            taken = true;

            if discard {
                // The bug the rule forbids: the `Ready` goes out of scope here, and its
                // messages — which `ready()` moved out of the core — go with it.
                events.push(Event::Discarded {
                    node: id,
                    messages: ready.messages.len(),
                });
            } else {
                if violate && !ready.messages.is_empty() {
                    outgoing.extend(ready.messages.iter().cloned());
                    events.push(Event::Emit {
                        node: id,
                        messages: ready.messages.len(),
                        early: true,
                    });
                }
                slot.pending = Some(DiskWrite {
                    ready,
                    due: self.event + held,
                    held,
                    sent_early: violate,
                });
                slot.refresh();
            }
            (leads, term)
        };

        if taken {
            self.stats.readys_taken += 1;
        }
        if held > 0 {
            self.stats.slow_writes += 1;
        }
        if leads && self.checker.leader_of(term).is_none() {
            events.push(Event::Lead { node: id, term });
            self.stats.elections += 1;
        }
        self.emit(outgoing);
        for event in events {
            self.record(event);
        }
        held == 0
    }

    /// Hands messages to the network, one token each.
    fn emit(&mut self, messages: Vec<Message>) {
        for message in messages {
            let (from, to) = (message.sender(), message.recipient());
            let token = self.next_token;
            self.next_token += 1;
            if let Some(context) = context_of(&message)
                && !context.is_empty()
            {
                self.contexts.insert(token, context);
            }
            self.messages.insert(token, message);
            let _ = self.net.send(
                WireId(from),
                WireId(to),
                Bytes::copy_from_slice(&token.to_le_bytes()),
            );
            self.stats.sent += 1;
        }
    }

    // --- checking ------------------------------------------------------------------------

    /// The highest commit index any node has reported, refreshed from what they hold now.
    /// Monotonic: a node whose commit index goes backwards across a restart — which is legal,
    /// a `HardState` that was never fsynced is gone — does not lower the mark.
    fn commit_high_water(&mut self) -> Index {
        let now = self.nodes.values().map(NodeSlot::commit).max().unwrap_or(0);
        self.commit_high_water = self.commit_high_water.max(now);
        self.commit_high_water
    }

    /// The driver rule: a `Ready` that has been taken is discharged, is still on its way to the
    /// disk, or died with the node that took it. Nothing else.
    ///
    /// `docs/plans/phase-3.md` §10.2: `ready()` *moves* the messages out of the core, so a
    /// driver that takes a `Ready` and drops it loses them — silently, because the state will
    /// be offered again and the log will look fine. This is the arithmetic that catches it.
    fn check_driver_rule(&self) -> Result<(), Failure> {
        let outstanding = self
            .nodes
            .values()
            .filter(|slot| slot.pending.is_some())
            .count() as u64;
        let accounted =
            self.stats.readys_discharged + self.stats.readys_lost_to_crash + outstanding;
        if accounted == self.stats.readys_taken {
            return Ok(());
        }
        Err(self.driver_error(format!(
            "{} Ready(s) were taken but only {accounted} are accounted for              ({} discharged, {} lost with a crashed node, {outstanding} still on a disk) —              a taken Ready was dropped, and its messages went with it",
            self.stats.readys_taken, self.stats.readys_discharged, self.stats.readys_lost_to_crash,
        )))
    }

    /// Checks all four properties over every node, dead ones included.
    fn check(&mut self) -> Result<(), Failure> {
        self.commit_high_water();
        self.check_driver_rule()?;
        let anchors: Vec<u64> = self
            .nodes
            .values()
            .map(|slot| {
                if slot.compacted_through == 0 {
                    0
                } else {
                    // The anchor goes with the entry the snapshot ends at, whose term the
                    // metadata carries — not with whatever term the node happens to be in now.
                    self.checker
                        .prefix_digest(slot.compacted_through, slot.snapshot_term)
                        .unwrap_or(0)
                }
            })
            .collect();

        let outcome = {
            let snapshots: Vec<NodeSnapshot<'_>> = self
                .nodes
                .values()
                .zip(anchors.iter())
                .map(|(slot, &prefix_anchor)| NodeSnapshot {
                    id: slot.id,
                    online: slot.online(),
                    is_leader: slot.is_leader(),
                    term: slot.term(),
                    commit: slot.commit(),
                    compacted_through: slot.compacted_through,
                    snapshot_term: slot.snapshot_term,
                    prefix_anchor,
                    settled: slot.settled(),
                    log: &slot.log,
                    applied: &slot.applied,
                })
                .collect();
            self.checker.observe(&snapshots)
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(violation) => Err(Failure::Safety {
                seed: self.seed,
                event: self.event,
                violation,
                trace: self.trace_report(),
            }),
        }
    }

    // --- plumbing ------------------------------------------------------------------------

    /// Builds a core for `id`, over `durable` if it is a restart.
    ///
    /// A restarted node gets a *different* random stream each life, derived from the seed, the
    /// node and how many times it has died — otherwise every life would draw the same election
    /// timeout and a restart would be invisible to the algorithm.
    fn build_node(
        &self,
        id: RaftId,
        durable: Option<esker_raft::MemStorage>,
        life: u64,
    ) -> Result<RawNode<PersistedStorage>, Failure> {
        let conf = esker_raft::ConfState::from_voters(self.voters.clone());
        let storage = match durable {
            Some(durable) => PersistedStorage::from_durable(durable),
            None => PersistedStorage::new(conf),
        };
        let mut config = Config::new(id, self.voters.clone(), self.seed);
        let mut life_bytes = [0_u8; 16];
        life_bytes[..8].copy_from_slice(&id.to_le_bytes());
        life_bytes[8..].copy_from_slice(&life.to_le_bytes());
        config.rng = Pcg32::new(hash64_with_seed(self.seed, &life_bytes), id);
        // What the state machine has already consumed. A real store writes the applied index
        // atomically with the data, so a restart must not be handed those entries again.
        config.applied = self
            .nodes
            .get(&id)
            .map_or(0, |existing| existing.applied_index);
        RawNode::new(config, storage).map_err(|error| Failure::Driver {
            seed: self.seed,
            event: self.event,
            what: format!("build n{id}: {error}"),
            trace: self.trace_report(),
        })
    }

    fn record(&mut self, event: Event) {
        if self.trace.len() == TRACE_EVENTS {
            self.trace.pop_front();
        }
        self.trace.push_back(event);
    }

    fn liveness(&self, what: String) -> Failure {
        Failure::Liveness {
            seed: self.seed,
            event: self.event,
            what,
            trace: self.trace_report(),
        }
    }

    fn driver_error(&self, what: String) -> Failure {
        Failure::Driver {
            seed: self.seed,
            event: self.event,
            what,
            trace: self.trace_report(),
        }
    }
}

/// The `context` a message carries, if its kind has one.
fn context_of(message: &Message) -> Option<Bytes> {
    match message {
        Message::AppendEntries { context, .. } | Message::AppendEntriesResponse { context, .. } => {
            Some(context.clone())
        }
        _ => None,
    }
}

/// The index a node should compact to, or `None` if it should not.
///
/// Only entries the state machine has applied may be folded away, and `keep` of them are always
/// left behind: a leader whose log is nothing but a snapshot has no `prev_log_term` to offer a
/// follower that is one entry behind, and would send a whole snapshot where an append would do.
fn compaction_point(slot: &NodeSlot, keep: u64) -> Option<Index> {
    let first = slot.compacted_through + 1;
    (slot.applied_index >= first + keep).then(|| slot.applied_index - keep)
}

/// The token a payload carries, or `None` if the network handed back something else.
fn token_of(payload: &Bytes) -> Option<u64> {
    let bytes: [u8; 8] = payload.as_ref().try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Every node's id, for a test that wants to name them.
#[must_use]
pub fn voter_ids(count: usize) -> Vec<RaftId> {
    (1..=count as RaftId).collect()
}

/// The seed a run should use: `ESKER_SIM_SEED` when it is set, otherwise `default`.
///
/// This is how a failing sweep is reproduced — the failure prints `ESKER_SIM_SEED=n`, and
/// setting it runs that one seed.
#[must_use]
pub fn seed_override() -> Option<u64> {
    std::env::var("ESKER_SIM_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
}

/// The seeds a sweep should run: the one in `ESKER_SIM_SEED` if it is set, else `count` of them
/// starting at `from`.
#[must_use]
pub fn seeds(from: u64, count: u64) -> Vec<u64> {
    match seed_override() {
        Some(seed) => vec![seed],
        None => (from..from + count).collect(),
    }
}
