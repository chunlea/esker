//! Sub-phase 3c: an exhaustive model of a three-node Raft cluster.
//!
//! The simulator next door runs the *real* `esker-raft` code against random seeds. This runs an
//! independently written model of the *rules* against every reachable state. The two find
//! different things, and neither replaces the other:
//!
//! * The simulator can find a bug in the implementation, but only in the interleavings its
//!   seeds happened to produce.
//! * This finds a bug in the algorithm as we have understood it, in *every* interleaving inside
//!   its bounds — but it cannot find a bug in the implementation, because it does not run it.
//!   `RawNode` is not `Clone` or `Hash` and its internals are private, which is right for a
//!   core that has to stay pure; so the model re-derives the rules from the dissertation's
//!   Figure 3.1 rather than wrapping the code. `docs/raft-spec.md` maps those rules to the
//!   functions that implement them, and that mapping is what ties the two together.
//!
//! # The bounds, and what they leave out
//!
//! A passing run claims exactly this: *no reachable state inside these bounds violates a
//! property*. A bound that is not written down turns that into a claim about nothing, so they
//! are all here and all in [`Bounds`].
//!
//! | Bound | CI | Acceptance | Why |
//! |---|---|---|---|
//! | Nodes | 3 | 3 | The smallest cluster with a real quorum. |
//! | Terms | 3 | 3 | Three elections: a leader, its successor, and its successor's. |
//! | Log entries | 2 | 2 | Enough for a divergent tail and a truncation. |
//! | Messages in flight | 1 | 2 | The bound that decides whether the search finishes at all. |
//! | Entries per `AppendEntries` | 1 | 1 | Batching cannot break what one-at-a-time does not. |
//!
//! That is 647k states — 2s in release, 9s in debug, and 28s for this whole file in debug,
//! against a budget of 120s. The wider run is 24M states and about 48s in release, which is why
//! it is `#[ignore]`d rather than quietly dropped. Both are exhaustive; a run that could not
//! finish would fail rather than report a green partial search.
//!
//! Not modelled, deliberately, each with the reason:
//!
//! * **Persistence and crashes.** Every variable here is durable by construction, so a crash is
//!   a no-op and a restart is invisible. The persistence boundary is precisely what the
//!   simulator models, and `tests/raft_persist_order.rs` shows it goes red when it is violated.
//! * **`next_index` back-off.** A leader may send `AppendEntries` from *any* `prev_index`,
//!   which is a superset of what the back-off loop produces — every value it could reach, and
//!   no value a leader could not send. Safety over the superset implies safety over the real
//!   thing, and it removes a per-follower variable from the state.
//! * **Message loss.** A lost message is a message never delivered, and every node state
//!   reachable by dropping one is reachable by simply not delivering it. Dropping would only
//!   enlarge the search.
//! * **Explicit vote and append rejections.** A refusal is modelled as silence. A rejection
//!   carries no information a node acts on here, because `next_index` is abstracted.
//! * **Pre-vote, check-quorum, snapshots, membership change, `ReadIndex`.** Pre-vote and
//!   check-quorum only *restrict* when a node may campaign, so leaving them out explores
//!   strictly more. The rest are covered by the simulator, which runs the real code.
//!
//! # Shown red
//!
//! A model checker that has only ever been run against a correct model proves nothing about
//! itself. [`Rules`] switches off three rules the safety argument depends on:
//!
//! * §5.2 one vote per term → election safety fails, in 4,849 states.
//! * §5.4.1 the candidate's log must be at least as up to date → leader completeness fails, in
//!   33,831 states.
//! * §5.4.2 a leader counts replicas only for entries from its own term → **nothing fails**, and
//!   that negative result is asserted rather than shrugged at. At three nodes the overwrite the
//!   rule prevents is unreachable, for a reason spelled out on the test. The paper draws
//!   Figure 8 with five nodes because it needs five.
//!
//! Two defects in this model were found by running it, and both are the kind that would have
//! made it agree with a broken implementation: a follower advanced its commit index to its own
//! log length instead of the last index the leader's message actually matched (Figure 3.1
//! receiver rule 5), and fusing a campaign's whole broadcast into one step made two candidates
//! in one term unreachable under the in-flight cap.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use stateright::{Checker, Model, Property};

/// How much of Raft's state space one run covers.
///
/// Everything here is a *bound*, not a parameter: the claim a passing run makes is "no reachable
/// state inside these bounds violates a property", and a bound that is not written down turns
/// that into a claim about nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    /// Nodes in the cluster.
    nodes: u8,
    /// The highest term a node may campaign into.
    terms: u8,
    /// The longest log any node may hold.
    log: usize,
    /// How many messages may be in flight at once.
    ///
    /// This is the bound that decides whether the search finishes, because the network is part
    /// of the state and its subsets are what explode. It is also the one that has already
    /// hidden something: with a campaign's whole broadcast fused into one step, a cap of two
    /// made "two candidates soliciting at once" unreachable, and with it the state where two
    /// nodes win the same term. `dropping_one_vote_per_term_breaks_election_safety` is what
    /// noticed; soliciting is now its own step, which costs one slot instead of two.
    in_flight: usize,
}

/// What CI runs: exhaustive, and comfortably inside the two minutes `prompts/03-raft.md`
/// allows. 647k states, about 2s in release and 9s in debug.
const CI_BOUNDS: Bounds = Bounds {
    nodes: 3,
    terms: 3,
    log: 2,
    in_flight: 1,
};

/// What the acceptance run covers: the same, with two messages in flight instead of one, which
/// is where messages can race. 24M states, about 48s in release — too slow for every `cargo
/// test`, which is why it is `#[ignore]`d rather than quietly dropped.
const WIDE_BOUNDS: Bounds = Bounds {
    in_flight: 2,
    ..CI_BOUNDS
};

/// What `prompts/03-raft.md` allows this to take in CI.
const CI_BUDGET_SECS: u64 = 120;
/// How deep the broken-rule searches go. Every counterexample they look for is well inside
/// this; a run that stopped finding one would say so rather than pass.
const RED_DEPTH: usize = 22;

/// What a node believes it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Role {
    Follower,
    Candidate,
    Leader,
}

/// One node. The log holds only each entry's term: two entries with the same index and term are
/// the same entry, because the leader of a term is unique — which is election safety, and is
/// checked rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Node {
    term: u8,
    voted_for: Option<u8>,
    role: Role,
    log: Vec<u8>,
    commit: usize,
    /// Bitmask of who has granted this candidate a vote in `term`.
    votes: u8,
}

impl Node {
    fn new() -> Self {
        Self {
            term: 0,
            voted_for: None,
            role: Role::Follower,
            log: Vec::new(),
            commit: 0,
            votes: 0,
        }
    }

    /// `(index, term)` of the last entry; `(0, 0)` for an empty log.
    fn last(&self) -> (u8, u8) {
        match self.log.last() {
            Some(&term) => (u8::try_from(self.log.len()).unwrap_or(u8::MAX), term),
            None => (0, 0),
        }
    }

    fn step_down(&mut self, term: u8) {
        self.term = term;
        self.role = Role::Follower;
        self.voted_for = None;
        self.votes = 0;
    }
}

/// An entry the cluster has committed, and the term it was committed in. This is the history
/// variable leader completeness is stated against: "committed in a term" is not something a
/// single state can see, so the model remembers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Committed {
    /// The entry's term.
    term: u8,
    /// The term of the leader that committed it.
    in_term: u8,
}

/// A message. The network is a set: a message stays available until it is delivered, and one
/// that is never delivered is a message that was lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Msg {
    RequestVote {
        from: u8,
        to: u8,
        term: u8,
        last_index: u8,
        last_term: u8,
    },
    VoteGranted {
        from: u8,
        to: u8,
        term: u8,
    },
    AppendEntries {
        from: u8,
        to: u8,
        term: u8,
        prev_index: u8,
        prev_term: u8,
        entry: Option<u8>,
        leader_commit: u8,
    },
}

/// The whole cluster.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RaftState {
    nodes: Vec<Node>,
    network: BTreeSet<Msg>,
    committed: Vec<Committed>,
}

/// One step the world can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Step {
    /// A follower or candidate gives up waiting and campaigns. It votes for itself and sends
    /// nothing: soliciting is a separate step, so that a campaign does not need room for a
    /// whole broadcast at once. (With the broadcast fused into the timeout, two candidates
    /// could never be in flight together under the in-flight cap, and the state where two of
    /// them win the same term became unreachable — which is what
    /// `dropping_one_vote_per_term_breaks_election_safety` noticed.)
    Timeout(u8),
    /// A candidate asks one node for its vote.
    Solicit { candidate: u8, to: u8 },
    /// One message is delivered.
    Deliver(Msg),
    /// A leader accepts a client entry.
    Propose(u8),
    /// A leader sends `AppendEntries` starting after `prev_index`.
    Replicate { leader: u8, to: u8, prev_index: u8 },
    /// A leader advances its commit index by counting replicas.
    Commit(u8),
}

/// The three rules the safety argument depends on, each switchable so that the checker can be
/// shown finding the counterexample its absence produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rules {
    /// §5.2: a node grants at most one vote per term.
    one_vote_per_term: bool,
    /// §5.4.1: a voter refuses a candidate whose log is not at least as up to date as its own.
    up_to_date_check: bool,
    /// §5.4.2: a leader counts replicas only for entries from its *own* term.
    commit_only_current_term: bool,
}

impl Rules {
    /// Raft as specified.
    fn correct() -> Self {
        Self {
            one_vote_per_term: true,
            up_to_date_check: true,
            commit_only_current_term: true,
        }
    }
}

/// The model.
#[derive(Clone, Debug)]
struct RaftModel {
    bounds: Bounds,
    rules: Rules,
    /// When set, the model declares only this one property.
    ///
    /// Stateright stops as soon as every property has a discovery, so a run that declares one
    /// property stops at its first counterexample instead of exploring the whole space to
    /// re-confirm the six that will never fire. The exhaustive run declares all of them; a
    /// broken-rule run declares the one it expects to break.
    probe: Option<&'static str>,
}

impl RaftModel {
    fn new(bounds: Bounds, rules: Rules) -> Self {
        Self {
            bounds,
            rules,
            probe: None,
        }
    }

    fn probing(bounds: Bounds, rules: Rules, property: &'static str) -> Self {
        Self {
            probe: Some(property),
            ..Self::new(bounds, rules)
        }
    }

    fn majority(&self, count: usize) -> bool {
        count * 2 > self.bounds.nodes as usize
    }
}

impl Model for RaftModel {
    type State = RaftState;
    type Action = Step;

    fn init_states(&self) -> Vec<Self::State> {
        vec![RaftState {
            nodes: (0..self.bounds.nodes).map(|_| Node::new()).collect(),
            network: BTreeSet::new(),
            committed: Vec::new(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for (index, node) in state.nodes.iter().enumerate() {
            let id = u8::try_from(index).unwrap_or(0);
            if node.term < self.bounds.terms && node.role != Role::Leader {
                actions.push(Step::Timeout(id));
            }
            if node.role == Role::Candidate {
                for to in 0..self.bounds.nodes {
                    if to != id {
                        actions.push(Step::Solicit { candidate: id, to });
                    }
                }
            }
            if node.role == Role::Leader {
                if node.log.len() < self.bounds.log {
                    actions.push(Step::Propose(id));
                }
                actions.push(Step::Commit(id));
                for to in 0..self.bounds.nodes {
                    if to == id {
                        continue;
                    }
                    for prev_index in 0..=u8::try_from(node.log.len()).unwrap_or(0) {
                        actions.push(Step::Replicate {
                            leader: id,
                            to,
                            prev_index,
                        });
                    }
                }
            }
        }
        actions.extend(state.network.iter().copied().map(Step::Deliver));
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last.clone();
        match action {
            Step::Timeout(id) => self.timeout(&mut state, id)?,
            Step::Solicit { candidate, to } => Self::solicit(&mut state, candidate, to)?,
            Step::Deliver(msg) => self.deliver(&mut state, msg)?,
            Step::Propose(id) => self.propose(&mut state, id)?,
            Step::Replicate {
                leader,
                to,
                prev_index,
            } => Self::replicate(&mut state, leader, to, prev_index)?,
            Step::Commit(id) => self.commit(&mut state, id)?,
        }
        if state.network.len() > self.bounds.in_flight {
            return None;
        }
        (state != *last).then_some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let all = Self::all_properties();
        match self.probe {
            Some(name) => all.into_iter().filter(|p| p.name == name).collect(),
            None => all,
        }
    }
}

impl RaftModel {
    fn all_properties() -> Vec<Property<Self>> {
        vec![
            // §3.4: at most one leader per term.
            Property::<Self>::always("election safety", |_, state| {
                for (at, left) in state.nodes.iter().enumerate() {
                    for right in &state.nodes[at + 1..] {
                        if left.role == Role::Leader
                            && right.role == Role::Leader
                            && left.term == right.term
                        {
                            return false;
                        }
                    }
                }
                true
            }),
            // §3.5: same index and term implies the same prefix.
            Property::<Self>::always("log matching", |_, state| {
                for (at, left) in state.nodes.iter().enumerate() {
                    for right in &state.nodes[at + 1..] {
                        let shared = left.log.len().min(right.log.len());
                        for index in 0..shared {
                            if left.log[index] == right.log[index]
                                && left.log[..=index] != right.log[..=index]
                            {
                                return false;
                            }
                        }
                    }
                }
                true
            }),
            // §3.6: an entry committed in one term is in every later leader's log.
            Property::<Self>::always("leader completeness", |_, state| {
                for node in &state.nodes {
                    if node.role != Role::Leader {
                        continue;
                    }
                    for (index, entry) in state.committed.iter().enumerate() {
                        if entry.in_term >= node.term {
                            continue;
                        }
                        if node.log.get(index) != Some(&entry.term) {
                            return false;
                        }
                    }
                }
                true
            }),
            // §3.6: no node applies a different entry at an index another has committed.
            Property::<Self>::always("state machine safety", |_, state| {
                for node in &state.nodes {
                    for index in 0..node.commit.min(state.committed.len()) {
                        if node.log.get(index) != Some(&state.committed[index].term) {
                            return false;
                        }
                    }
                }
                true
            }),
            // A model that cannot elect or commit would satisfy every `always` property while
            // exploring nothing. These are what stop that from passing for a win.
            Property::<Self>::sometimes("a leader is elected", |_, state| {
                state.nodes.iter().any(|node| node.role == Role::Leader)
            }),
            Property::<Self>::sometimes("an entry commits", |_, state| !state.committed.is_empty()),
            Property::<Self>::sometimes("a follower's tail is overwritten", |_, state| {
                state.nodes.iter().any(|node| {
                    state
                        .nodes
                        .iter()
                        .any(|other| other.log.len() > node.log.len() && !node.log.is_empty())
                }) && state.nodes.iter().any(|node| node.term >= 2)
            }),
        ]
    }

    /// §3.4: a follower that hears nothing campaigns for the next term, voting for itself.
    fn timeout(&self, state: &mut RaftState, id: u8) -> Option<()> {
        let node = state.nodes.get_mut(id as usize)?;
        if node.term >= self.bounds.terms || node.role == Role::Leader {
            return None;
        }
        node.term += 1;
        node.role = Role::Candidate;
        node.voted_for = Some(id);
        node.votes = 1 << id;
        Some(())
    }

    /// A candidate asks one node for its vote. It may ask again; a duplicate request is
    /// already covered, because the network is a set and a message stays available until it is
    /// delivered.
    fn solicit(state: &mut RaftState, candidate: u8, to: u8) -> Option<()> {
        let node = state.nodes.get(candidate as usize)?;
        if node.role != Role::Candidate {
            return None;
        }
        let (last_index, last_term) = node.last();
        state
            .network
            .insert(Msg::RequestVote {
                from: candidate,
                to,
                term: node.term,
                last_index,
                last_term,
            })
            .then_some(())
    }

    fn deliver(&self, state: &mut RaftState, msg: Msg) -> Option<()> {
        if !state.network.remove(&msg) {
            return None;
        }
        match msg {
            Msg::RequestVote {
                from,
                to,
                term,
                last_index,
                last_term,
            } => {
                let node = state.nodes.get_mut(to as usize)?;
                if term > node.term {
                    node.step_down(term);
                }
                if term != node.term {
                    return Some(());
                }
                let unspent = !self.rules.one_vote_per_term
                    || node.voted_for.is_none()
                    || node.voted_for == Some(from);
                let up_to_date = !self.rules.up_to_date_check || {
                    let (my_index, my_term) = node.last();
                    (last_term, last_index) >= (my_term, my_index)
                };
                if unspent && up_to_date {
                    node.voted_for = Some(from);
                    let term = node.term;
                    state.network.insert(Msg::VoteGranted {
                        from: to,
                        to: from,
                        term,
                    });
                }
            }
            Msg::VoteGranted { from, to, term } => {
                let node = state.nodes.get_mut(to as usize)?;
                if term > node.term {
                    node.step_down(term);
                } else if term == node.term && node.role == Role::Candidate {
                    node.votes |= 1 << from;
                    if self.majority(node.votes.count_ones() as usize) {
                        node.role = Role::Leader;
                    }
                }
            }
            Msg::AppendEntries {
                from: _,
                to,
                term,
                prev_index,
                prev_term,
                entry,
                leader_commit,
            } => {
                let node = state.nodes.get_mut(to as usize)?;
                if term > node.term {
                    node.step_down(term);
                }
                if term < node.term {
                    return Some(());
                }
                node.role = Role::Follower;
                let at = prev_index as usize;
                let consistent = at == 0 || (at <= node.log.len() && node.log[at - 1] == prev_term);
                if !consistent {
                    return Some(());
                }
                if let Some(entry) = entry {
                    match node.log.get(at) {
                        Some(&held) if held == entry => {}
                        Some(_) => {
                            node.log.truncate(at);
                            node.log.push(entry);
                        }
                        None => node.log.push(entry),
                    }
                }
                // Figure 3.1, AppendEntries receiver rule 5: the commit index moves to
                // `min(leaderCommit, index of the last *new* entry)` — the last index this
                // message matched, not the follower's whole log. The difference is not
                // cosmetic, and the model checker found it: a follower still holding a longer
                // tail from an earlier term would otherwise mark entries committed that the
                // leader has never seen, which is a state-machine-safety violation.
                let matched = at + usize::from(entry.is_some());
                node.commit = node.commit.max((leader_commit as usize).min(matched));
            }
        }
        Some(())
    }

    fn propose(&self, state: &mut RaftState, id: u8) -> Option<()> {
        let node = state.nodes.get_mut(id as usize)?;
        if node.role != Role::Leader || node.log.len() >= self.bounds.log {
            return None;
        }
        let term = node.term;
        node.log.push(term);
        Some(())
    }

    fn replicate(state: &mut RaftState, leader: u8, to: u8, prev_index: u8) -> Option<()> {
        let node = state.nodes.get(leader as usize)?;
        if node.role != Role::Leader {
            return None;
        }
        let at = prev_index as usize;
        if at > node.log.len() {
            return None;
        }
        let msg = Msg::AppendEntries {
            from: leader,
            to,
            term: node.term,
            prev_index,
            prev_term: if at == 0 { 0 } else { node.log[at - 1] },
            entry: node.log.get(at).copied(),
            leader_commit: u8::try_from(node.commit).unwrap_or(u8::MAX),
        };
        state.network.insert(msg).then_some(())
    }

    /// §3.6.2: a leader advances its commit index to the highest entry replicated on a majority
    /// — and, when the rule is on, only for an entry from its own term.
    fn commit(&self, state: &mut RaftState, id: u8) -> Option<()> {
        let node = state.nodes.get(id as usize)?;
        if node.role != Role::Leader {
            return None;
        }
        let (term, commit) = (node.term, node.commit);
        let mut highest = commit;
        for index in (commit + 1)..=node.log.len() {
            let entry = node.log[index - 1];
            if self.rules.commit_only_current_term && entry != term {
                continue;
            }
            let replicas = state
                .nodes
                .iter()
                .filter(|other| other.log.len() >= index && other.log[index - 1] == entry)
                .count();
            if self.majority(replicas) {
                highest = highest.max(index);
            }
        }
        if highest == commit {
            return None;
        }
        for index in (commit + 1)..=highest {
            let entry = state.nodes[id as usize].log[index - 1];
            if state.committed.len() < index {
                state.committed.push(Committed {
                    term: entry,
                    in_term: term,
                });
            }
        }
        state.nodes.get_mut(id as usize)?.commit = highest;
        Some(())
    }
}

/// What one exhaustive run cost.
struct Explored<C> {
    checker: C,
    states: usize,
    elapsed: Duration,
    /// Whether the whole space was covered. A checker that ran out of budget has proved
    /// nothing, so this is asserted before the properties are.
    exhaustive: bool,
}

/// Explores the space and reports what it cost.
///
/// `depth`, when given, bounds the length of the paths explored. The exhaustive run does not
/// use it — a bounded search proves nothing about the states beyond the bound. The three
/// broken-rule runs do: a counterexample to a rule this basic is a dozen steps long, and
/// searching the entire space to re-confirm one is time CI does not need to spend.
fn explore(bounds: Bounds, rules: Rules) -> Explored<impl Checker<RaftModel>> {
    run(
        RaftModel::new(bounds, rules),
        Duration::from_secs(CI_BUDGET_SECS),
        None,
    )
}

/// Runs one model, however it was configured.
fn run(
    model: RaftModel,
    budget: Duration,
    depth: Option<usize>,
) -> Explored<impl Checker<RaftModel>> {
    let started = Instant::now();
    // One thread: the counterexample a failure prints is then the *shortest* one, and the same
    // one on every machine.
    let mut builder = model.checker().threads(1).timeout(budget);
    if let Some(depth) = depth {
        builder = builder.target_max_depth(depth);
    }
    let checker = builder.spawn_bfs().join();
    let elapsed = started.elapsed();
    let states = checker.unique_state_count();
    let exhaustive = checker.is_done();
    Explored {
        checker,
        states,
        elapsed,
        exhaustive,
    }
}

/// The exhaustive run: every `always` property holds over every reachable state inside
/// [`CI_BOUNDS`], every `sometimes` property is witnessed, and the whole thing fits in CI's
/// budget.
#[test]
fn every_reachable_state_of_three_nodes_is_safe() {
    check_exhaustively(CI_BOUNDS);
}

/// The same, with two messages in flight instead of one — where messages can race, and 37×
/// the states. Run at acceptance:
///
/// ```text
/// cargo test -p esker-sim --release --test stateright_raft -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the wide model: 24M states, ~48s in release"]
fn every_reachable_state_with_messages_racing_is_safe() {
    check_exhaustively(WIDE_BOUNDS);
}

fn check_exhaustively(bounds: Bounds) {
    let run = explore(bounds, Rules::correct());
    println!(
        "stateright: {} nodes, terms <= {}, log <= {} entries, <= {} in flight — \
         {} unique states in {:.2?} (budget {CI_BUDGET_SECS}s)",
        bounds.nodes, bounds.terms, bounds.log, bounds.in_flight, run.states, run.elapsed,
    );
    assert!(
        run.exhaustive,
        "the model did not finish inside {CI_BUDGET_SECS}s ({} states in {:.2?}). It has \
         therefore proved nothing: shrink the bounds and say in this file's documentation what \
         is no longer covered.",
        run.states, run.elapsed,
    );
    run.checker.assert_properties();
}

/// Looks for a counterexample to one property, and says how long it took.
///
/// Only the named property is declared, so the search stops the moment it finds one rather than
/// exploring the rest of the space to re-confirm the properties that will never fire.
fn probe(rules: Rules, property: &'static str) -> bool {
    let run = run(
        RaftModel::probing(CI_BOUNDS, rules, property),
        Duration::from_secs(CI_BUDGET_SECS),
        Some(RED_DEPTH),
    );
    let found = run.checker.discovery(property).is_some();
    println!(
        "probe {property:<24} {} after {} states in {:.2?}",
        if found {
            "BROKEN (as expected)"
        } else {
            "not found"
        },
        run.states,
        run.elapsed,
    );
    found
}

/// §5.2. Without one-vote-per-term, two candidates win the same term.
#[test]
fn dropping_one_vote_per_term_breaks_election_safety() {
    assert!(
        probe(
            Rules {
                one_vote_per_term: false,
                ..Rules::correct()
            },
            "election safety",
        ),
        "two nodes were allowed to collect a majority in the same term and the checker did not \
         notice — either the property is not being evaluated or the model can no longer reach \
         the state inside its bounds"
    );
}

/// §5.4.1. Without the up-to-date check, a node with a short log wins an election and a
/// committed entry is lost.
#[test]
fn dropping_the_up_to_date_check_loses_a_committed_entry() {
    let rules = Rules {
        up_to_date_check: false,
        ..Rules::correct()
    };
    assert!(
        probe(rules, "leader completeness") || probe(rules, "state machine safety"),
        "a voter that ignores the candidate's log lost no committed entry; the model is not \
         exploring what it claims to"
    );
}

/// §5.4.2, the figure-8 case — and a negative result worth writing down.
///
/// Switching the rule off breaks *nothing* at three nodes, and that is not a gap in the search:
/// with a quorum of two, an entry replicated to a majority is on two of the three nodes, so any
/// future candidate must ask one of them for a vote — and §5.4.1's up-to-date check makes it
/// refuse, because the candidate cannot have a longer or later log without having had the entry
/// in the first place. The overwrite the rule prevents needs a node that can win an election
/// *while a minority holds the entry*, which needs five nodes; that is why the paper's Figure 8
/// is drawn with five.
///
/// So this test asserts the negative, and asserting it is the point: if the reasoning above is
/// wrong, or the model grows to five nodes, this fails and says so. The rule itself is not
/// left untested — the core lane mutation-tested §5.4.2's term condition directly
/// (`docs/plans/phase-3.md` §10), and the simulator runs the real implementation of it across
/// ten thousand seeds.
#[test]
fn the_figure_eight_case_needs_five_nodes_and_so_is_out_of_reach_here() {
    let run = explore(
        CI_BOUNDS,
        Rules {
            commit_only_current_term: false,
            ..Rules::correct()
        },
    );
    assert!(
        run.exhaustive,
        "the search did not finish, so it proves nothing either way"
    );
    let discoveries = run.checker.discoveries();
    let broken: Vec<&str> = [
        "leader completeness",
        "state machine safety",
        "log matching",
    ]
    .into_iter()
    .filter(|name| discoveries.contains_key(name))
    .collect();
    assert!(
        broken.is_empty(),
        "counting replicas from an earlier term DID break {broken:?} at {} nodes. The comment \
         above says that is impossible below five; it is wrong, and the counterexample is the \
         interesting part of this phase.",
        CI_BOUNDS.nodes,
    );
    println!(
        "figure-8: unreachable at {} nodes as expected ({} states explored)",
        CI_BOUNDS.nodes, run.states,
    );
}
