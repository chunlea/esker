//! Three real stores, real sockets, concurrent clients, and the leader killed underneath them.
//!
//! `prompts/03-raft.md`, "Tests for 3e": *a Porcupine-style linearizability check of the
//! single-key history must pass* while the leader is killed every few seconds. This is that
//! test, with the whole stack in the loop — the client's retry and redirect logic, the wire, the
//! server, the apply loop, Raft, and the engine. Nothing is stubbed and nothing is mocked; the
//! only thing that is not real is that the stores are threads in this process rather than
//! separate ones, which is what `esker-cli`'s `cluster_chaos.rs` covers instead.
//!
//! # Why the check is the whole assertion
//!
//! The obvious way to test "no acknowledged write is lost" is to remember every acknowledged
//! write and look for it at the end. That is weaker than it sounds: it cannot tell a value that
//! survived from a value that came back *after* a later write had already been acknowledged,
//! which is the failure a lost log entry actually produces. So the final read of each key is
//! recorded into the history as an operation like any other, and linearizability is what decides
//! whether the run was legal. An acknowledged write that vanished makes the final read
//! unexplainable, and the checker says so — with the operation that broke it.
//!
//! # What a failed call is worth recording as
//!
//! A call that fails has one of two fates, and the client already knows which:
//! [`Error::changed_nothing`] is the predicate the transaction layer branches on, and it defers
//! to [`esker_proto::ProtoError::outcome`] so that the client and the protocol can never
//! disagree.
//!
//! * **The store refused.** A `NotLeader`, an epoch that moved, a retry budget spent on
//!   redirects: the request provably did not take effect. It is not in the history at all,
//!   because a history is a record of what the cluster *did* and this is a record of it
//!   declining to.
//! * **Nobody learned.** The request went out and no usable answer came back
//!   ([`Error::AmbiguousResult`]). It is recorded as a *maybe-applied* operation: the model
//!   accepts either, and the checker may place it anywhere after its invocation — including at
//!   the very end, where it is indistinguishable from never having happened. Recording those as
//!   failures instead would be a lie in whichever direction the run happened to go.
//!
//! Keeping them apart is not bookkeeping, it is what makes the check affordable. A maybe-applied
//! operation is **unbounded**: its response time is infinite, so it overlaps every operation
//! after it and the search must consider placing it at every point. Each one roughly doubles the
//! space. Recording refusals as maybe-applied put hundreds of them in a run's histories — a
//! three-second run with four kills recorded *eleven* acknowledged writes and *two hundred and
//! forty* refusals — and a search over fifty unbounded operations does not finish at any budget
//! anybody would wait for. That is the whole of why this test used to exhaust under load: a
//! saturated cluster spends longer with no leader, so it refuses more, and every refusal was
//! being written down as something that might have happened.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_client::Error;
use esker_sim::lin::{
    CheckOutcome, Checker, Completion, History, Op, Register, RegisterInput, RegisterOutput,
};

#[path = "chaos_cluster/mod.rs"]
mod chaos_cluster;

use chaos_cluster::{Cluster, connect};

/// Keys the clients share. Few on purpose: collisions are what make a history worth checking.
///
/// This was raised to five to make per-key histories shorter, on the reading that the search
/// was superlinear in a history's *length*. It is not: it is exponential in how many of the
/// operations are **unbounded**, and the 67-operation histories that could not be decided were
/// about sixty refusals apiece — operations the cluster provably never performed. With those no
/// longer recorded (see the module docs) a key's history is the handful of calls that actually
/// answered, and spreading those over five keys buys nothing while costing exactly what few
/// keys are for: collisions. So it is back to three, where six clients contend hard enough to
/// be worth checking.
const KEYS: usize = 3;
/// Concurrent clients.
const CLIENTS: usize = 6;

/// Whether a call that failed belongs in the history at all.
///
/// The two answers are [`Error::changed_nothing`]'s two answers, and deferring to it is the
/// point: the client and the protocol already agree about what each failure means, and a test
/// that decided it a second time would be a third opinion to keep in step. See the module docs
/// for why the distinction is what makes the search affordable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The store refused, or the request never went out. It did not happen, so there is
    /// nothing to explain and nothing to record.
    DidNotHappen,
    /// It went out and no usable answer came back. Recorded, unbounded.
    MaybeApplied,
}

/// A call that has been stamped but has not ended yet.
///
/// It carries everything its operation will need, because nothing is written down until the
/// call ends and it is known whether there is anything to write down.
struct InFlight {
    key: usize,
    client: u64,
    input: RegisterInput,
    invoked: u64,
}

impl InFlight {
    fn into_op(self, completion: Completion<RegisterOutput>) -> Op<RegisterInput, RegisterOutput> {
        Op {
            client: self.client,
            input: self.input,
            invoked: self.invoked,
            completion,
        }
    }
}

/// One key's operations and the clock that stamps them.
///
/// The clock ticks once per observed event — a call starting, a call ending — whether or not
/// the event becomes an operation. Ticking for a refusal too keeps the stamps a record of
/// *when things were observed* rather than of what survived the recording.
struct Log {
    ops: Vec<Op<RegisterInput, RegisterOutput>>,
    clock: u64,
}

impl Log {
    fn tick(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }
}

/// The logs, one per key, each recording every client's operations on it in the order they were
/// observed.
///
/// One mutex per key, taken to stamp the start of a call and taken again when it ends — never
/// held across the call itself. That is what makes the recorded order the *observed* order: two
/// operations that really overlapped are recorded as overlapping, and two that did not are not.
struct Recorder {
    keys: Vec<Mutex<Log>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            keys: (0..KEYS)
                .map(|_| {
                    Mutex::new(Log {
                        ops: Vec::new(),
                        clock: 0,
                    })
                })
                .collect(),
        }
    }

    /// Stamps the start of a call.
    fn begin(&self, key: usize, client: u64, input: RegisterInput) -> InFlight {
        let invoked = self.keys[key].lock().unwrap().tick();
        InFlight {
            key,
            client,
            input,
            invoked,
        }
    }

    /// The call answered, and said what it did.
    fn responded(&self, op: InFlight, output: RegisterOutput) {
        let mut log = self.keys[op.key].lock().unwrap();
        let at = log.tick();
        log.ops.push(op.into_op(Completion::Ok { at, output }));
    }

    /// The call failed. Whether it is recorded at all is [`Fate`]'s answer, not this
    /// function's.
    fn ended(&self, op: InFlight, fate: Fate) {
        let mut log = self.keys[op.key].lock().unwrap();
        let at = log.tick();
        if fate == Fate::MaybeApplied {
            log.ops.push(op.into_op(Completion::Unknown { at }));
        }
    }

    /// One key's history, in invocation order.
    ///
    /// Built at the end rather than kept: an operation is appended when its call *ends*, so the
    /// log is in completion order, and [`History::ops`] promises invocation order.
    fn history(&self, key: usize) -> History<RegisterInput, RegisterOutput> {
        let mut ops = self.keys[key].lock().unwrap().ops.clone();
        ops.sort_by_key(|op| op.invoked);
        History::from_ops(ops).expect("the clock stamps every ending after its own beginning")
    }
}

/// How many of a history's operations the checker may place anywhere.
///
/// The ones nobody learned the outcome of: their response time is infinite, so each of them
/// overlaps everything after it. This is the number the search's cost is exponential in, which
/// is why it is the number the reports carry — the count of *pending* operations, which is what
/// they used to carry, is zero in every run this file can produce.
fn unbounded(history: &History<RegisterInput, RegisterOutput>) -> usize {
    history
        .ops()
        .iter()
        .filter(|op| !matches!(op.completion, Completion::Ok { .. }))
        .count()
}

fn key_bytes(key: usize) -> Vec<u8> {
    format!("chaos-{key:02}").into_bytes()
}

/// What one client did, for the report.
#[derive(Default)]
struct Tally {
    acked_writes: u64,
    reads: u64,
    swaps: u64,
    ambiguous: u64,
    refused: u64,
}

/// Runs operations against the cluster until `stop`, recording every one.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "each argument is moved across a thread boundary, so it has to be owned"
)]
fn drive(
    client_id: u64,
    addrs: Vec<SocketAddr>,
    recorder: Arc<Recorder>,
    stop: Arc<AtomicBool>,
    writes: Arc<AtomicU64>,
) -> Tally {
    let mut tally = Tally::default();
    let Some(mut client) = connect(&addrs, Instant::now() + Duration::from_secs(10)) else {
        return tally;
    };
    let mut sequence = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        sequence += 1;
        let key = usize::try_from((client_id + sequence) % KEYS as u64).unwrap_or(0);
        let bytes = key_bytes(key);
        // A value that names the write that made it, so a value appearing where it should not is
        // traceable to one call rather than to a count that does not add up.
        let value = Bytes::from(format!("c{client_id}-w{sequence}").into_bytes());

        let choice = sequence % 4;
        let mut reconnect = false;
        match choice {
            0 | 1 => {
                let op = recorder.begin(key, client_id, RegisterInput::Write(value.clone()));
                match client.put(&bytes, &value) {
                    Ok(()) => {
                        recorder.responded(op, RegisterOutput::Written);
                        tally.acked_writes += 1;
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        reconnect = true;
                        recorder.ended(op, fate(&error, &mut tally));
                    }
                }
            }
            2 => {
                let op = recorder.begin(key, client_id, RegisterInput::Read);
                match client.get(&bytes) {
                    Ok(found) => {
                        recorder.responded(op, RegisterOutput::Value(found));
                        tally.reads += 1;
                    }
                    Err(error) => {
                        reconnect = true;
                        recorder.ended(op, fate(&error, &mut tally));
                    }
                }
            }
            _ => {
                // Compare-and-swap from whatever the client last saw. `expected` being wrong is
                // ordinary — another client got there first — and the model says so.
                let expected = client.get(&bytes).ok().flatten();
                let op = recorder.begin(
                    key,
                    client_id,
                    RegisterInput::Cas {
                        expected: expected.clone(),
                        new: value.clone(),
                    },
                );
                match client.compare_and_swap(&bytes, expected.as_deref(), Some(&value)) {
                    Ok((swapped, _)) => {
                        recorder.responded(op, RegisterOutput::Swapped(swapped));
                        tally.swaps += 1;
                        if swapped {
                            writes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        reconnect = true;
                        recorder.ended(op, fate(&error, &mut tally));
                    }
                }
            }
        }

        if reconnect && let Some(fresh) = connect(&addrs, Instant::now() + Duration::from_secs(5)) {
            client = fresh;
        }
    }
    tally
}

/// What a failed call means for the history, counted on the way past.
///
/// The answer is [`Error::changed_nothing`]'s: the client already carries the protocol's
/// verdict outwards, and the transaction layer branches on the same call. Deciding it here a
/// second time would be a third opinion to keep in step with the other two.
fn fate(error: &Error, tally: &mut Tally) -> Fate {
    if error.changed_nothing() {
        tally.refused += 1;
        Fate::DidNotHappen
    } else {
        tally.ambiguous += 1;
        Fate::MaybeApplied
    }
}

/// Runs the battery: `kills` leader kills, then settles and checks every key's history.
fn battery(kills: u32, between: Duration) {
    let cluster = Cluster::start(3);
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never elected a leader to begin with"
    );

    let recorder = Arc::new(Recorder::new());
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let addrs = cluster.addrs.clone();

    let clients: Vec<_> = (0..CLIENTS)
        .map(|at| {
            let (addrs, recorder, stop, writes) = (
                addrs.clone(),
                Arc::clone(&recorder),
                Arc::clone(&stop),
                Arc::clone(&writes),
            );
            std::thread::spawn(move || drive(at as u64 + 1, addrs, recorder, stop, writes))
        })
        .collect();

    let mut killed = 0;
    if kills == 0 {
        // A control run: the same clients on the same cluster with nothing killed. If a history
        // does not linearize here, the fault is in the modelling, not in what killing does.
        std::thread::sleep(between * 4);
    }
    for _ in 0..kills {
        std::thread::sleep(between);
        let Some(at) = cluster.leader() else { continue };
        cluster.kill_node(at);
        killed += 1;
        // Long enough for the survivors to notice and elect, then the victim comes back and has
        // to catch up — by appends, or by a snapshot if it fell far enough behind.
        std::thread::sleep(between);
        cluster.start_node(at);
    }

    stop.store(true, Ordering::Relaxed);
    let tallies: Vec<Tally> = clients.into_iter().map(|t| t.join().unwrap()).collect();

    // Let the cluster come back before the final reads: a read taken while nobody leads would be
    // a refusal, not evidence.
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never came back after {killed} kills"
    );
    final_reads(&addrs, &recorder);
    cluster.shutdown();

    let acked: u64 = tallies.iter().map(|t| t.acked_writes).sum();
    let ambiguous: u64 = tallies.iter().map(|t| t.ambiguous).sum();
    let refused: u64 = tallies.iter().map(|t| t.refused).sum();
    let reads: u64 = tallies.iter().map(|t| t.reads).sum();
    let swaps: u64 = tallies.iter().map(|t| t.swaps).sum();
    println!(
        "{killed} leader kills, {CLIENTS} clients: {acked} acknowledged writes, {reads} reads, \
         {swaps} compare-and-swaps, {ambiguous} ambiguous, {refused} refused"
    );

    assert!(
        killed > 0 || kills == 0,
        "no leader was ever killed, so nothing was tested"
    );
    assert!(
        acked > 0,
        "not one write was acknowledged; the run proves nothing about losing them"
    );
    check_histories(&recorder, killed);
}

/// Reads every key once more, into the history, after the cluster has settled.
///
/// This is what turns "no acknowledged write is lost" into a property the checker enforces: a
/// write that vanished leaves a final read that no ordering can explain.
fn final_reads(addrs: &[SocketAddr], recorder: &Recorder) {
    let Some(client) = connect(addrs, Instant::now() + Duration::from_secs(20)) else {
        panic!("no store was reachable for the final read");
    };
    for key in 0..KEYS {
        let bytes = key_bytes(key);
        let op = recorder.begin(key, 0, RegisterInput::Read);
        // Retried, because a settled cluster can still refuse one call while a connection is
        // being re-established, and a missing final read would weaken the check rather than fail
        // it honestly.
        let mut found = None;
        for _ in 0..40 {
            match client.get(&bytes) {
                Ok(value) => {
                    found = Some(value);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        match found {
            Some(value) => recorder.responded(op, RegisterOutput::Value(value)),
            // A read that never answered saw nothing and changed nothing, so it anchors
            // nothing. Recording it as maybe-applied would cost the search a dimension to say
            // exactly as little.
            None => recorder.ended(op, Fate::DidNotHappen),
        }
    }
}

/// The first search budget, and how it grows when the search runs out of it.
///
/// The search is deterministic, so a history that exhausts a budget exhausts it again at the same
/// size: the only way forward is a bigger one. Growth is by a fixed factor from a fixed start, so
/// which budgets were tried is a property of the code and not of the day — a run that reports
/// exhaustion reports exactly which three numbers it tried.
const FIRST_BUDGET: u64 = 1_000_000;
const BUDGET_GROWTH: u64 = 8;
/// Two attempts — 1M then 8M — and the ceiling is deliberately low.
///
/// The cost is not in the history's *length* but in how much of it is concurrent, and that makes
/// the search a cliff rather than a slope: every history ever observed here either decided inside
/// the first million steps or did not decide at 64 million either. A third attempt was measured
/// costing **146 seconds** and deciding nothing, twice. Spending two minutes to reach the same
/// answer eight times slower is not thoroughness, so the ceiling stops where the evidence does.
const BUDGET_ATTEMPTS: u32 = 2;

/// What checking one key's history concluded. **Exhaustion is not a violation**, and keeping them
/// apart is the whole point of this type.
///
/// A checker that fails on exhaustion cries wolf: the run is reported as a lost write when nothing
/// was lost, and the next person to see it re-runs the test until it passes. One that *passes* on
/// exhaustion is worse — it is blind on exactly the histories that are hardest to explain, which
/// are the ones a real violation lives in. So the search is retried at a larger budget, a bounded
/// number of times, and if it still cannot decide the run fails **naming exhaustion** rather than
/// claiming a violation it did not find.
#[derive(Debug)]
enum Verdict {
    /// Some sequential order explains the history.
    Linearizable {
        /// How many operations the order placed.
        placed: usize,
    },
    /// No order explains it. This is the failure the test exists for.
    Violation {
        /// The checker's rendering, for the message.
        report: String,
    },
    /// The search could not decide within the budgets it was given.
    Exhausted {
        /// Steps taken on the final, largest attempt.
        steps: u64,
        /// The budget that attempt was given.
        budget: u64,
    },
}

/// Checks one history, growing the budget while the search keeps running out of it.
fn verdict_for(history: &History<RegisterInput, RegisterOutput>) -> Verdict {
    verdict_from(history, FIRST_BUDGET)
}

/// [`verdict_for`], starting from a given budget so that the controls can reach the exhaustion
/// path without building a history that takes a million steps to decide.
fn verdict_from(history: &History<RegisterInput, RegisterOutput>, first: u64) -> Verdict {
    let mut budget = first;
    for attempt in 1..=BUDGET_ATTEMPTS {
        match Checker::with_budget(budget).check(&Register, history) {
            CheckOutcome::Linearizable { order } => {
                return Verdict::Linearizable {
                    placed: order.len(),
                };
            }
            // A decided violation is decided at any budget: the search proved no order exists.
            CheckOutcome::NotLinearizable { report, .. } => return Verdict::Violation { report },
            CheckOutcome::Inconclusive { steps } => {
                if attempt == BUDGET_ATTEMPTS {
                    return Verdict::Exhausted { steps, budget };
                }
                budget *= BUDGET_GROWTH;
            }
        }
    }
    unreachable!("the loop returns on its last attempt")
}

/// Every key's history has to be linearizable against the register model.
fn check_histories(recorder: &Recorder, killed: u32) {
    for key in 0..KEYS {
        let history = recorder.history(key);
        if history.is_empty() {
            continue;
        }
        match verdict_for(&history) {
            Verdict::Linearizable { placed } => {
                println!(
                    "key {key}: {placed} operations linearizable ({} unbounded)",
                    unbounded(&history)
                );
            }
            Verdict::Violation { report } => panic!(
                "key {key}: the history of a three-node cluster with {killed} leader kills is \
                 NOT linearizable — an acknowledged write was lost or reordered.\n{report}"
            ),
            Verdict::Exhausted { steps, budget } => panic!(
                "key {key}: the linearizability search ran out of budget after {steps} steps at \
                 {budget}, having grown it {BUDGET_ATTEMPTS} times from {FIRST_BUDGET}. This is \
                 EXHAUSTION, not a violation: no order was ruled out. The history has {} \
                 operations, of which {} are unbounded — and it is that second number the cost \
                 is exponential in, so a run that reaches this has started recording operations \
                 nobody learned the outcome of far more often than a leader kill can explain.",
                history.len(),
                unbounded(&history)
            ),
        }
    }
}

/// The short run CI does on every change.
#[test]
fn a_killed_leader_never_costs_an_acknowledged_write() {
    battery(4, Duration::from_millis(400));
}

/// The acceptance run from `prompts/03-raft.md`: fifty kills.
///
/// ```text
/// cargo test -p esker-client --release --test chaos_linearizability -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the 50-kill acceptance run; minutes, not seconds"]
fn fifty_leader_kills_under_load() {
    battery(50, Duration::from_millis(500));
}

// ---------------------------------------------------------------------------------------------
// The controls
//
// House doctrine: a test that cannot fail proves nothing on the day the mechanism breaks. The
// linearizability check is the whole assertion of this file, so these three run in the gate — not
// behind `#[ignore]` — and each one puts a *known* answer through the same `verdict_from` the real
// test uses, rather than through a copy of it.
// ---------------------------------------------------------------------------------------------

/// A history in which an acknowledged write vanished. Strictly sequential — the write is answered
/// before the read is invoked — so the only possible order is `[write, read]` and the read must
/// see the written value. Seeing an absent key instead is exactly the failure this whole file
/// exists to catch: a lost log entry.
fn a_lost_write() -> History<RegisterInput, RegisterOutput> {
    let mut history = History::new();
    let write = history.invoke(1, RegisterInput::Write(Bytes::from_static(b"1")));
    let _ = history.respond(write, RegisterOutput::Written);
    let read = history.invoke(2, RegisterInput::Read);
    let _ = history.respond(read, RegisterOutput::Value(None));
    history
}

/// The control that matters: the checker still catches a real violation.
#[test]
fn the_checker_still_catches_a_lost_write() {
    match verdict_for(&a_lost_write()) {
        Verdict::Violation { report } => assert!(!report.is_empty(), "a violation with no report"),
        other => panic!(
            "a lost acknowledged write was not reported as a violation: {other:?}\n\
             the assertion of this whole file is this check, and it just proved it cannot fail"
        ),
    }
}

/// The other half of the same claim: exhaustion is **never** dressed up as a violation.
///
/// The same history, given a budget of nothing. The search cannot rule anything out, so the
/// honest answer is that it does not know — and the code says so with a different variant, which
/// is what stops a hard history from being reported as a lost write.
#[test]
fn exhaustion_is_never_reported_as_a_violation() {
    match verdict_from(&a_lost_write(), 0) {
        Verdict::Exhausted { budget, .. } => assert_eq!(budget, 0, "the budget grew from nothing"),
        other => panic!("a search with no budget claimed to have decided something: {other:?}"),
    }
}

/// And the retry is a retry, not a shrug: a history the first budget cannot decide is decided by
/// a later one, and comes back as the *decision*.
///
/// One step is not enough to disprove even a two-operation history; eight is. So this run goes
/// through the exhaustion branch, grows the budget, and still ends at `Violation` — which is the
/// behaviour that makes the flake fix safe rather than merely quiet.
#[test]
fn a_grown_budget_still_reaches_the_decision() {
    assert!(
        BUDGET_ATTEMPTS > 1 && BUDGET_GROWTH > 1,
        "there is no growth to test"
    );
    match verdict_from(&a_lost_write(), 1) {
        Verdict::Violation { .. } => {}
        other => panic!("growing the budget lost the decision: {other:?}"),
    }
}
