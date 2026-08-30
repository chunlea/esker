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
use esker_raft::{Config, Index, Message, NodeId as RaftId, RawNode, TICK_MS, Term};

use crate::fault::FaultPlan;
use crate::net::{NodeId as WireId, SimNetwork};

use super::checkers::SafetyChecker;
use super::driver::{NodeSlot, PersistedStorage};
use super::report::{Event, Failure, Settled, Stats};

/// Stream selector for the event loop's own choices. Distinct from
/// [`crate::net::FAULT_STREAM`] and [`crate::net::SCENARIO_STREAM`] so that a change to one
/// does not move the others.
pub const EVENT_STREAM: u64 = 3;

mod contract;
mod membership;

/// How many `Ready` rounds one node may discharge within a single event. A fast disk lets a
/// node persist, send, apply and advance several times in a row; the bound is there so that one
/// event cannot become an unbounded amount of work.
pub(super) const READY_ROUNDS_PER_EVENT: u32 = 8;

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
    /// Every node id that exists, started or not.
    population: Vec<RaftId>,
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
        Self::with_spares(seed, plan, voters, 0)
    }

    /// A cluster of `voters` nodes plus `spares` that exist but have not been started.
    ///
    /// A spare has a node id and an inbox and nothing else — which is what a server waiting to
    /// be added to a group actually is. It starts the moment a configuration names it, and
    /// catches up like any other follower that is behind: by appends if the entries it needs
    /// are still there, by a snapshot if they are not.
    pub fn with_spares(
        seed: u64,
        plan: FaultPlan,
        voters: usize,
        spares: usize,
    ) -> Result<Self, Failure> {
        let ids: Vec<RaftId> = (1..=voters as RaftId).collect();
        let population: Vec<RaftId> = (1..=(voters + spares) as RaftId).collect();
        let wire: Vec<WireId> = population.iter().map(|id| WireId(*id)).collect();
        let bootstrap = esker_raft::ConfState::from_voters(ids.clone());
        let mut cluster = Self {
            seed,
            plan: plan.clone(),
            net: SimNetwork::new(seed, plan, &wire),
            nodes: BTreeMap::new(),
            population: population.clone(),
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
        for id in &ids {
            let node = cluster.build_node(*id, None, 0, &bootstrap)?;
            cluster
                .nodes
                .insert(*id, NodeSlot::new(*id, node, bootstrap.clone()));
        }
        for id in &population[ids.len()..] {
            cluster
                .nodes
                .insert(*id, NodeSlot::spare(*id, bootstrap.clone()));
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
    /// `Ready` is discharged or dies with its node, and the check that runs after every event enforces it. This
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
            replicated_overwrites: self.checker.replicated_overwrites(),
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

    /// Every node that has ever run — the population minus the spares still waiting to be
    /// added.
    #[must_use]
    pub fn started(&self) -> usize {
        self.nodes.values().filter(|slot| slot.started()).count()
    }

    /// Every node's log, for debugging a violation.
    #[must_use]
    pub fn dump_logs(&self) -> String {
        let mut out = Vec::new();
        for slot in self.nodes.values() {
            out.push(format!(
                "n{} online={} leader={} term={} commit={} applied={} pending={:?} \
                 cfg={:?} durable={:?} core={:?} cfg_idx={} snap={} restarts={}",
                slot.id,
                slot.online(),
                slot.is_leader(),
                slot.term(),
                slot.commit(),
                slot.applied_index,
                slot.pending
                    .as_ref()
                    .map(|w| (w.due, w.ready.entries.len())),
                slot.config.voters,
                slot.durable_config.voters,
                slot.core_config().voters,
                slot.config_index,
                slot.compacted_through,
                slot.restarts,
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

    /// Checks the deferred property: every committed entry was held by a quorum of the
    /// configuration in force at its index.
    ///
    /// Deferred because an observation lags the acknowledgement it followed. Call it once a run
    /// has settled — the sweeps do, at the end of every seed.
    pub fn verify_quorums(&self) -> Result<(), Failure> {
        self.checker
            .verify_quorums()
            .map_err(|violation| Failure::Safety {
                seed: self.seed,
                event: self.event,
                violation,
                trace: self.trace_report(),
            })
    }

    /// The membership every online node has derived, for a test that wants to see it.
    #[must_use]
    pub fn configurations(&self) -> Vec<(RaftId, Vec<RaftId>)> {
        self.nodes
            .values()
            .filter(|slot| slot.online())
            .map(|slot| (slot.id, slot.config_of().voters))
            .collect()
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
            .filter(|slot| !slot.online() && slot.started())
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
                    self.started(),
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
        if let Some(slot) = self.nodes.get_mut(&node) {
            slot.refresh();
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
        if partition && self.population.len() > 1 {
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
            7 => {
                if self.plan.membership > 0.0
                    && let Some(change) = self.draw_conf_change(who)
                {
                    self.propose_conf_change(change);
                } else {
                    self.tick_all();
                }
                Ok(())
            }
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
    ///
    /// "Offline" means *crashed*, not "has never run": a spare waiting to be added to the group
    /// is not a node that can be restarted, and reviving one would hand it an empty
    /// configuration and a core that believes it belongs to no cluster at all.
    fn pick(&self, pick: u64, online: bool) -> Option<RaftId> {
        let candidates: Vec<RaftId> = self
            .nodes
            .values()
            .filter(|slot| slot.online() == online && (online || slot.started()))
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
        let count = self.population.len();
        // A bitmask that is neither empty nor everything: `1 ..= 2^n - 2`.
        let masks = (1_u64 << count) - 2;
        let mask = pick % masks + 1;
        let side: Vec<RaftId> = self
            .population
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
        let conf = slot.config.clone();
        let node = self.build_node(id, Some(durable), life, &conf)?;
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
            // A tick can start an election, and a node that wins one appends a no-op the moment
            // the last vote arrives — so a tick is not free of log changes either.
            slot.refresh();
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
        // Its log may have changed — the observation is the *core's* log now, not storage's —
        // so the node that was stepped is refreshed here rather than only when a write lands.
        if let Some(slot) = self.nodes.get_mut(&target) {
            slot.refresh();
        }
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
        self.population
            .iter()
            .copied()
            .filter(|id| self.net.inbox_len(WireId(*id)) > 0)
            .collect()
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
        conf: &esker_raft::ConfState,
    ) -> Result<RawNode<PersistedStorage>, Failure> {
        let storage = match durable {
            Some(durable) => PersistedStorage::from_durable(durable),
            None => PersistedStorage::new(conf.clone()),
        };
        let mut config = Config::new(id, conf.voters.clone(), self.seed);
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
pub(super) fn context_of(message: &Message) -> Option<Bytes> {
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
pub(super) fn compaction_point(slot: &NodeSlot, keep: u64) -> Option<Index> {
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
/// starting at `from` — or as many as `ESKER_SIM_SEEDS` asks for.
///
/// The count is overridable because "the sweep is clean" is a claim about how many seeds were
/// looked at, and the number in the source is the one CI can afford rather than the largest one
/// worth running. Widening it is how a fix is checked past the range that found the bug.
#[must_use]
pub fn seeds(from: u64, count: u64) -> Vec<u64> {
    if let Some(seed) = seed_override() {
        return vec![seed];
    }
    let count = std::env::var("ESKER_SIM_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(count);
    (from..from.saturating_add(count)).collect()
}
