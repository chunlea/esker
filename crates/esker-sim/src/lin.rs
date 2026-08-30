//! A linearizability checker: does a concurrent history have *some* sequential order that a
//! correct implementation could have produced?
//!
//! `docs/DESIGN.md` §11 puts a "Porcupine-style WGL linearizability checker for a single-key
//! register model" in `esker-sim`, used by phase 3's replicated-region test and again by
//! phase 5's transaction tests. This is that checker: Wing and Gong's search with Lowe's
//! optimisations — lift an operation out of the history, recurse, backtrack — plus the
//! memoisation on `(set of linearized operations, model state)` that turns an intractable
//! search into a practical one.
//!
//! # Invariants
//!
//! * **Deterministic.** No hashing, no `HashMap`: the memo table is a [`BTreeSet`], the event
//!   order is a total order over `(time, call-or-return, operation)`. The same history gives
//!   the same answer, and the same *counter-example*, on every run and every machine.
//! * **Bounded.** The search is exponential in the worst case, so it runs against a step
//!   budget and reports [`CheckOutcome::Inconclusive`] rather than hanging a test. A checker
//!   that hangs is a checker that gets deleted.
//! * **An unknown result is not a free pass, and not a claim either.** An operation that timed
//!   out may have taken effect or not, so it is given a wildcard result *and* an unbounded
//!   response time: the search may place it anywhere after its invocation, including at the
//!   very end, where nothing observes it and it is indistinguishable from never having
//!   happened. What it may not do is invent a value nobody wrote.
//!
//! # Example
//!
//! ```
//! use esker_sim::lin::{Checker, History, Register, RegisterInput, RegisterOutput};
//!
//! let mut history: History<RegisterInput, RegisterOutput> = History::new();
//! let write = history.invoke(1, RegisterInput::Write(b"a".as_slice().into()));
//! let read = history.invoke(2, RegisterInput::Read);
//! // The read overlaps the write, so seeing the new value is legal.
//! history.respond(read, RegisterOutput::Value(Some(b"a".as_slice().into())))?;
//! history.respond(write, RegisterOutput::Written)?;
//!
//! assert!(Checker::new().check(&Register, &history).is_linearizable());
//! # Ok::<(), esker_sim::lin::HistoryError>(())
//! ```

use std::collections::BTreeSet;
use std::fmt;

use bytes::Bytes;
use thiserror::Error;

/// A sequential specification: what a single-threaded, correct implementation would do.
///
/// The model must be deterministic — the same state and input always give the same output —
/// which is what lets the checker compare a produced response against a recorded one instead
/// of searching over responses as well.
pub trait Model {
    /// The model's state. `Ord` rather than `Hash` on purpose: the memo table is ordered, so
    /// no hasher's iteration order can leak into a result.
    type State: Clone + Ord;
    /// What a client asked for.
    type Input: Clone + fmt::Debug;
    /// What a correct implementation answers.
    type Output: Clone + Eq + fmt::Debug;

    /// The state before any operation.
    fn init(&self) -> Self::State;

    /// Applies `input` to `state`, returning the response a correct implementation gives.
    fn apply(&self, state: &mut Self::State, input: &Self::Input) -> Self::Output;
}

/// A single-key register — the model DESIGN §11 names, and the one a Raft cluster serving one
/// key is supposed to implement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Register;

/// What a client can ask a register to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterInput {
    /// Read the current value.
    Read,
    /// Store `value`, whatever was there before.
    Write(Bytes),
    /// Store `new` only if the current value is `expected`.
    Cas {
        /// The value the client believes is there; `None` means "the key is absent".
        expected: Option<Bytes>,
        /// What to store if the belief holds.
        new: Bytes,
    },
}

/// What a register answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutput {
    /// The value a read saw; `None` means the key was absent.
    Value(Option<Bytes>),
    /// A write completed.
    Written,
    /// A compare-and-swap: `true` if the swap happened.
    Swapped(bool),
}

impl Model for Register {
    type State = Option<Bytes>;
    type Input = RegisterInput;
    type Output = RegisterOutput;

    fn init(&self) -> Self::State {
        None
    }

    fn apply(&self, state: &mut Self::State, input: &Self::Input) -> Self::Output {
        match input {
            RegisterInput::Read => RegisterOutput::Value(state.clone()),
            RegisterInput::Write(value) => {
                *state = Some(value.clone());
                RegisterOutput::Written
            }
            RegisterInput::Cas { expected, new } => {
                if state == expected {
                    *state = Some(new.clone());
                    RegisterOutput::Swapped(true)
                } else {
                    RegisterOutput::Swapped(false)
                }
            }
        }
    }
}

/// Identifies one operation within one [`History`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub usize);

impl fmt::Display for OpId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "op{}", self.0)
    }
}

/// How an operation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion<O> {
    /// It responded at `at` with `output`.
    Ok {
        /// When the response was observed.
        at: u64,
        /// What the response said.
        output: O,
    },
    /// It responded at `at`, but what it did is unknown — a timeout, a dropped connection, an
    /// ambiguous error. It may or may not have taken effect, and the checker must consider
    /// both.
    ///
    /// `at` is kept for the report and is deliberately *not* used to bound the operation: an
    /// outcome nobody learned is not a claim about when it happened, so the checker may place
    /// it anywhere after its invocation.
    Unknown {
        /// When the client gave up.
        at: u64,
    },
    /// It never responded. It may take effect at any point after its invocation, or never.
    Pending,
}

impl<O> Completion<O> {
    /// When the operation stopped being able to affect other operations.
    ///
    /// Only a completion that *said what it did* bounds anything. An operation whose outcome is
    /// unknown may or may not have taken effect at all, so it is unbounded exactly like a
    /// pending one: the checker may place it anywhere after its invocation, including at the
    /// very end, where nothing observes it and it is indistinguishable from never having
    /// happened.
    ///
    /// Bounding it by the moment the client gave up would be a different and wrong claim — that
    /// it definitely happened, and happened by then. A three-node cluster losing its leader
    /// produces those constantly, and a checker that insists they all happened rejects
    /// histories that are perfectly legal. That is how this was found.
    fn response_time(&self) -> u64 {
        match self {
            Completion::Ok { at, .. } => *at,
            Completion::Unknown { .. } | Completion::Pending => u64::MAX,
        }
    }

    /// The response the model must produce, or `None` when any response is acceptable.
    fn expected(&self) -> Option<&O> {
        match self {
            Completion::Ok { output, .. } => Some(output),
            Completion::Unknown { .. } | Completion::Pending => None,
        }
    }
}

/// One operation: who asked, what they asked, when, and what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op<I, O> {
    /// Which client issued it. Reporting only; the checker never uses it.
    pub client: u64,
    /// What was asked.
    pub input: I,
    /// When it was invoked.
    pub invoked: u64,
    /// How it ended.
    pub completion: Completion<O>,
}

/// Why a history could not be built.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// A response was recorded at or before its own invocation, which would make the event
    /// order ambiguous.
    #[error(
        "{op}: responded at {at} but was invoked at {invoked}; a response must be strictly later"
    )]
    ResponseNotAfterInvocation {
        /// The offending operation.
        op: OpId,
        /// Its invocation time.
        invoked: u64,
        /// Its recorded response time.
        at: u64,
    },
    /// A response was recorded for an operation that is not in this history.
    #[error("{op}: no such operation in this history")]
    NoSuchOp {
        /// The identifier that did not resolve.
        op: OpId,
    },
    /// A response was recorded twice for one operation.
    #[error("{op}: already responded")]
    AlreadyResponded {
        /// The operation that responded twice.
        op: OpId,
    },
}

/// A concurrent history: every invocation and every response, in the order they were observed.
///
/// Built either by recording as a run happens ([`History::invoke`] / [`History::respond`],
/// which stamp a monotonic counter so no two events share a time) or from operations with
/// explicit times ([`History::from_ops`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History<I, O> {
    ops: Vec<Op<I, O>>,
    next_time: u64,
}

impl<I, O> Default for History<I, O> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, O> History<I, O> {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            next_time: 0,
        }
    }

    /// Records an invocation at the next event time and returns its identifier.
    pub fn invoke(&mut self, client: u64, input: I) -> OpId {
        let invoked = self.tick();
        self.ops.push(Op {
            client,
            input,
            invoked,
            completion: Completion::Pending,
        });
        OpId(self.ops.len() - 1)
    }

    /// Records that `op` responded with `output`, at the next event time.
    pub fn respond(&mut self, op: OpId, output: O) -> Result<(), HistoryError> {
        let at = self.tick();
        self.finish(op, Completion::Ok { at, output })
    }

    /// Records that `op` responded, but that what it did is unknown.
    pub fn respond_unknown(&mut self, op: OpId) -> Result<(), HistoryError> {
        let at = self.tick();
        self.finish(op, Completion::Unknown { at })
    }

    /// Builds a history from operations that already carry times.
    ///
    /// Equal times are resolved by treating a response as ordered *before* an invocation at
    /// the same instant — the strict reading of real-time order, under which the two do not
    /// overlap. Give operations distinct times if that is not what you mean.
    pub fn from_ops(ops: Vec<Op<I, O>>) -> Result<Self, HistoryError> {
        let mut next_time = 0;
        for (index, op) in ops.iter().enumerate() {
            let at = op.completion.response_time();
            if !matches!(op.completion, Completion::Pending) && at <= op.invoked {
                return Err(HistoryError::ResponseNotAfterInvocation {
                    op: OpId(index),
                    invoked: op.invoked,
                    at,
                });
            }
            next_time = next_time.max(op.invoked.saturating_add(1));
            if at != u64::MAX {
                next_time = next_time.max(at.saturating_add(1));
            }
        }
        Ok(Self { ops, next_time })
    }

    /// Every operation, in invocation order.
    #[must_use]
    pub fn ops(&self) -> &[Op<I, O>] {
        &self.ops
    }

    /// How many operations the history holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the history is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// How many operations never responded.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op.completion, Completion::Pending))
            .count()
    }

    fn tick(&mut self) -> u64 {
        let now = self.next_time;
        self.next_time = self.next_time.saturating_add(1);
        now
    }

    fn finish(&mut self, op: OpId, completion: Completion<O>) -> Result<(), HistoryError> {
        let entry = self
            .ops
            .get_mut(op.0)
            .ok_or(HistoryError::NoSuchOp { op })?;
        if !matches!(entry.completion, Completion::Pending) {
            return Err(HistoryError::AlreadyResponded { op });
        }
        entry.completion = completion;
        Ok(())
    }
}

impl<I: fmt::Debug, O: fmt::Debug> History<I, O> {
    /// A compact, line-per-operation rendering, with the operations in `linearized` marked.
    ///
    /// This is what a failing test prints, so it is deliberately terse: one line per
    /// operation, the interval it occupied, and whether the search managed to place it.
    #[must_use]
    pub fn render(&self, linearized: &[OpId]) -> String {
        let placed: BTreeSet<usize> = linearized.iter().map(|op| op.0).collect();
        let lines: Vec<String> = self
            .ops
            .iter()
            .enumerate()
            .map(|(index, op)| {
                let response = match &op.completion {
                    Completion::Ok { at, output } => format!("{at} -> {output:?}"),
                    Completion::Unknown { at } => format!("{at} -> <unknown>"),
                    Completion::Pending => "never responded".to_owned(),
                };
                let mark = if placed.contains(&index) { "  " } else { "! " };
                format!(
                    "{mark}op{index:<4} client {:<3} [{:>6}, {response}] {:?}",
                    op.client, op.invoked, op.input
                )
            })
            .collect();
        lines.join("\n")
    }
}

/// What the checker concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Some sequential order explains the history; `order` is one such order.
    Linearizable {
        /// The operations, in the order the search placed them.
        order: Vec<OpId>,
    },
    /// No sequential order explains the history.
    NotLinearizable {
        /// The largest set of operations the search ever placed, in the order it placed them.
        /// The first operation *missing* from it is where the history stopped making sense.
        linearized: Vec<OpId>,
        /// A rendering of the history with those operations marked, for a failure message.
        report: String,
    },
    /// The search ran out of budget. Neither answer is claimed.
    Inconclusive {
        /// How many search steps were taken before giving up.
        steps: u64,
    },
}

impl CheckOutcome {
    /// Whether the history is linearizable. An inconclusive search is not.
    #[must_use]
    pub fn is_linearizable(&self) -> bool {
        matches!(self, CheckOutcome::Linearizable { .. })
    }
}

impl fmt::Display for CheckOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckOutcome::Linearizable { order } => {
                write!(formatter, "linearizable ({} operations)", order.len())
            }
            CheckOutcome::NotLinearizable { linearized, report } => write!(
                formatter,
                "NOT linearizable: no order explains this history; \
                 the deepest search placed {} operations (marked with `!` below)\n{report}",
                linearized.len()
            ),
            CheckOutcome::Inconclusive { steps } => write!(
                formatter,
                "inconclusive: the search budget of {steps} steps ran out"
            ),
        }
    }
}

/// The Wing–Gong search, with a budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checker {
    budget: u64,
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker {
    /// The default budget: a million search steps, which is far more than any history a test
    /// in this repository produces and still finishes in well under a second.
    pub const DEFAULT_BUDGET: u64 = 1_000_000;

    /// A checker with [`Checker::DEFAULT_BUDGET`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            budget: Self::DEFAULT_BUDGET,
        }
    }

    /// A checker that gives up after `budget` search steps.
    #[must_use]
    pub fn with_budget(budget: u64) -> Self {
        Self { budget }
    }

    /// Decides whether `history` is linearizable with respect to `model`.
    pub fn check<M: Model>(
        &self,
        model: &M,
        history: &History<M::Input, M::Output>,
    ) -> CheckOutcome {
        if history.is_empty() {
            return CheckOutcome::Linearizable { order: Vec::new() };
        }
        Search::new(model, history, self.budget).run()
    }
}

/// A fixed-width bit set with a total order, used as half the memo key.
///
/// `Vec<u64>` compares lexicographically, which is a total order — that is all the memo table
/// needs, and it avoids introducing a hasher into a component whose whole value is
/// reproducibility.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    fn new(bits: usize) -> Self {
        Self {
            words: vec![0; bits.div_ceil(64)],
        }
    }

    fn insert(&mut self, bit: usize) {
        self.words[bit / 64] |= 1 << (bit % 64);
    }

    fn remove(&mut self, bit: usize) {
        self.words[bit / 64] &= !(1 << (bit % 64));
    }
}

/// One event in the history: an operation's invocation or its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Event {
    time: u64,
    /// `0` for a response, `1` for an invocation, so that at an equal time the response comes
    /// first and the two operations are treated as not overlapping.
    kind: u8,
    op: usize,
}

/// The search state: a doubly linked list of events that operations are lifted out of and put
/// back into, plus the memo table and the backtracking stack.
struct Search<'a, M: Model> {
    model: &'a M,
    history: &'a History<M::Input, M::Output>,
    budget: u64,
    /// Circular linked list over event positions; index `sentinel` is the head.
    next: Vec<usize>,
    prev: Vec<usize>,
    sentinel: usize,
    /// Event position of each operation's invocation and response.
    call_at: Vec<usize>,
    return_at: Vec<usize>,
    /// For each event position: which operation, and whether it is the invocation.
    op_of: Vec<usize>,
    is_call: Vec<bool>,
}

/// One entry of the backtracking stack: the operation that was placed and the model state
/// from before it was.
struct Placed<S> {
    op: usize,
    state_before: S,
}

impl<'a, M: Model> Search<'a, M> {
    fn new(model: &'a M, history: &'a History<M::Input, M::Output>, budget: u64) -> Self {
        let count = history.len();
        let mut events = Vec::with_capacity(count * 2);
        for (index, op) in history.ops().iter().enumerate() {
            events.push(Event {
                time: op.invoked,
                kind: 1,
                op: index,
            });
            events.push(Event {
                time: op.completion.response_time(),
                kind: 0,
                op: index,
            });
        }
        events.sort_unstable();

        let total = events.len();
        let sentinel = total;
        let mut next = vec![0; total + 1];
        let mut prev = vec![0; total + 1];
        for position in 0..total {
            next[position] = position + 1;
            prev[position] = if position == 0 {
                sentinel
            } else {
                position - 1
            };
        }
        next[sentinel] = 0;
        prev[sentinel] = total - 1;

        let mut call_at = vec![0; count];
        let mut return_at = vec![0; count];
        let mut op_of = vec![0; total];
        let mut is_call = vec![false; total];
        for (position, event) in events.iter().enumerate() {
            op_of[position] = event.op;
            if event.kind == 1 {
                is_call[position] = true;
                call_at[event.op] = position;
            } else {
                return_at[event.op] = position;
            }
        }

        Self {
            model,
            history,
            budget,
            next,
            prev,
            sentinel,
            call_at,
            return_at,
            op_of,
            is_call,
        }
    }

    /// Removes an operation's invocation and response from the list.
    fn lift(&mut self, op: usize) {
        for position in [self.call_at[op], self.return_at[op]] {
            let (before, after) = (self.prev[position], self.next[position]);
            self.next[before] = after;
            self.prev[after] = before;
        }
    }

    /// Puts them back. The neighbours were never overwritten, and lifts nest, so restoring in
    /// the reverse order of removal is exact.
    fn unlift(&mut self, op: usize) {
        for position in [self.return_at[op], self.call_at[op]] {
            let (before, after) = (self.prev[position], self.next[position]);
            self.next[before] = position;
            self.prev[after] = position;
        }
    }

    fn is_empty(&self) -> bool {
        self.next[self.sentinel] == self.sentinel
    }

    fn run(mut self) -> CheckOutcome {
        let mut state = self.model.init();
        let mut linearized = BitSet::new(self.history.len());
        let mut visited: BTreeSet<(BitSet, M::State)> = BTreeSet::new();
        let mut stack: Vec<Placed<M::State>> = Vec::new();
        let mut deepest: Vec<OpId> = Vec::new();
        let mut steps = 0_u64;

        let mut cursor = self.next[self.sentinel];
        while !self.is_empty() {
            steps += 1;
            if steps > self.budget {
                return CheckOutcome::Inconclusive { steps: self.budget };
            }

            if cursor != self.sentinel && self.is_call[cursor] {
                let op = self.op_of[cursor];
                if let Some(after) = self.try_place(&state, op) {
                    let mut candidate = linearized.clone();
                    candidate.insert(op);
                    if visited.insert((candidate.clone(), after.clone())) {
                        stack.push(Placed {
                            op,
                            state_before: state,
                        });
                        linearized = candidate;
                        state = after;
                        self.lift(op);
                        if stack.len() > deepest.len() {
                            deepest = stack.iter().map(|placed| OpId(placed.op)).collect();
                        }
                        cursor = self.next[self.sentinel];
                        continue;
                    }
                }
                cursor = self.next[cursor];
                continue;
            }

            // A response whose invocation is still in the list: every operation that could
            // come next has to be linearized before this one returns, and none of them can
            // be. Undo the last placement and try the next candidate after it.
            let Some(undo) = stack.pop() else {
                let report = self.history.render(&deepest);
                return CheckOutcome::NotLinearizable {
                    linearized: deepest,
                    report,
                };
            };
            linearized.remove(undo.op);
            state = undo.state_before;
            self.unlift(undo.op);
            cursor = self.next[self.call_at[undo.op]];
        }

        CheckOutcome::Linearizable {
            order: stack.iter().map(|placed| OpId(placed.op)).collect(),
        }
    }

    /// The model state after `op`, or `None` if the model's response disagrees with the one
    /// the history recorded.
    fn try_place(&self, state: &M::State, op: usize) -> Option<M::State> {
        let entry = &self.history.ops()[op];
        let mut after = state.clone();
        let produced = self.model.apply(&mut after, &entry.input);
        match entry.completion.expected() {
            Some(expected) if *expected != produced => None,
            _ => Some(after),
        }
    }
}
