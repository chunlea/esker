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
//! # Ambiguous outcomes
//!
//! A client whose request was sent and never answered does not know whether it happened.
//! [`Error::AmbiguousResult`] is exactly that, and it is recorded as a *maybe-applied*
//! operation: the model accepts either, and the checker is free to place it anywhere after its
//! invocation — including at the very end, where it is indistinguishable from never having
//! happened. Recording those as failures instead would be a lie in whichever direction the run
//! happened to go.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_client::region_cache::StaticRegion;
use esker_client::{Error, RawClient, TcpStores};
use esker_proto::{Server, ServerHandle, TransportConfig};
use esker_raft::Role;
use esker_sim::lin::{
    CheckOutcome, Checker, History, OpId, Register, RegisterInput, RegisterOutput,
};
use esker_store::server::RaftOptions;
use esker_store::{PeerAddress, Store, StoreOptions, StoreService};
use tempfile::TempDir;

const REGION: u64 = 1;
/// Keys the clients share. Few on purpose: collisions are what make a history worth checking.
///
/// Five rather than three, and the reason is the checker's cost rather than its coverage. The
/// search is superlinear in one key's history length: at three keys a run recorded about 67
/// operations per key, and a 67-operation history was measured taking **more than 73 million
/// search steps — 146 seconds — and still not deciding**, where a 66-operation one decided
/// inside a million. That is a cliff, not a slope, and the cheap side of it is a shorter
/// per-key history. Six clients over five keys still collide constantly, which is what the
/// histories are for; the run's total operation count under the kills is unchanged.
const KEYS: usize = 5;
/// Concurrent clients.
const CLIENTS: usize = 6;
/// The seed every node's election timer is drawn from.
const SEED: u64 = 20_260_830;

/// One store: its server and the directory that outlives it.
struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
}

/// A three-node cluster that can lose a node and get it back.
struct Cluster {
    runtime: tokio::runtime::Runtime,
    peers: Vec<PeerAddress>,
    addrs: Vec<SocketAddr>,
    dirs: Vec<TempDir>,
    /// `None` while that node is down.
    nodes: Vec<Mutex<Option<Node>>>,
}

/// Reserves `count` ports by binding and releasing them.
///
/// Every store has to know every peer's address before any server exists, so the addresses
/// cannot come from the servers. A released port is rebound microseconds later; a peer that is
/// briefly unreachable simply has its messages dropped and retried, which is the transport's
/// ordinary behaviour.
fn reserve_ports(count: usize) -> Vec<SocketAddr> {
    let listeners: Vec<std::net::TcpListener> = (0..count)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap())
        .collect()
}

impl Cluster {
    fn start(count: usize) -> Arc<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let addrs = reserve_ports(count);
        let peers: Vec<PeerAddress> = (0..count)
            .map(|at| PeerAddress::new(at as u64 + 1, at as u64 + 1, addrs[at]))
            .collect();
        let dirs: Vec<TempDir> = (0..count).map(|_| TempDir::new().unwrap()).collect();

        let cluster = Arc::new(Self {
            runtime,
            peers,
            addrs,
            dirs,
            nodes: (0..count).map(|_| Mutex::new(None)).collect(),
        });
        for at in 0..count {
            cluster.start_node(at);
        }
        cluster
    }

    /// Starts node `at` on its own address and directory. A restart reopens the same database,
    /// which is the point: what it recovers is what it had made durable.
    fn start_node(&self, at: usize) {
        let id = at as u64 + 1;
        let mut raft = RaftOptions::new(self.peers.clone(), SEED);
        // Shorter than production's 100 ms so an election takes a fraction of a second. The
        // algorithm counts ticks, so nothing about it changes — but this file kills a node every
        // few hundred milliseconds, and a tick that is too short churns leadership on scheduler
        // noise alone.
        raft.tick = Duration::from_millis(25);
        let options = StoreOptions {
            store_id: id,
            peer_id: id,
            region_id: REGION,
            raft: Some(raft),
            ..StoreOptions::new()
        };
        // Inside the runtime: opening a store spawns the transport's per-peer tasks, and
        // `tokio::spawn` needs a runtime to spawn onto.
        let store = {
            let _guard = self.runtime.enter();
            Store::open(self.dirs[at].path(), options).unwrap()
        };
        let addr = self.addrs[at];
        let service = StoreService::new(Arc::clone(&store));
        let handle = self.runtime.block_on(async move {
            Server::bind(addr, service, TransportConfig::new())
                .await
                .unwrap()
                .spawn()
                .unwrap()
        });
        *self.nodes[at].lock().unwrap() = Some(Node { store, handle });
    }

    /// Takes node `at` away: the server stops answering and the store stops replicating.
    ///
    /// This is the in-process stand-in for a `SIGKILL`. It is not one — a thread cannot be shot,
    /// and anything still inside the engine finishes — so what it proves is bounded: every
    /// acknowledged write is durable *and* recoverable across losing the process that
    /// acknowledged it. The unbounded version, with a real signal and a real process, is
    /// `esker-cli/tests/cluster_chaos.rs`.
    fn kill_node(&self, at: usize) {
        let node = self.nodes[at].lock().unwrap().take();
        if let Some(node) = node {
            node.store.stop();
            self.runtime.block_on(async {
                let _ = node.handle.shutdown().await;
            });
        }
    }

    /// The index of the node that believes it leads, if exactly one does.
    fn leader(&self) -> Option<usize> {
        for at in 0..self.nodes.len() {
            let guard = self.nodes[at].lock().unwrap();
            let Some(node) = guard.as_ref() else { continue };
            let Some(peer) = node.store.peer() else {
                continue;
            };
            let role = self.runtime.block_on(peer.status()).ok().map(|s| s.role);
            if role == Some(Role::Leader) {
                return Some(at);
            }
        }
        None
    }

    /// Waits until some node leads and every live node agrees, or the deadline passes.
    fn settle(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(at) = self.leader() {
                let id = at as u64 + 1;
                let agreed = (0..self.nodes.len()).all(|other| {
                    let guard = self.nodes[other].lock().unwrap();
                    guard.as_ref().is_none_or(|node| {
                        node.store
                            .peer()
                            .is_none_or(|peer| peer.leader() == Some(id))
                    })
                });
                if agreed {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn shutdown(&self) {
        for at in 0..self.nodes.len() {
            self.kill_node(at);
        }
    }
}

/// The histories, one per key, each recording every client's operations on it in the order they
/// were observed.
///
/// One mutex per key, taken to stamp an invocation and taken again to stamp the response — never
/// held across the call itself. That is what makes the recorded order the *observed* order: two
/// operations that really overlapped are recorded as overlapping, and two that did not are not.
struct Recorder {
    keys: Vec<Mutex<History<RegisterInput, RegisterOutput>>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            keys: (0..KEYS).map(|_| Mutex::new(History::new())).collect(),
        }
    }

    fn invoke(&self, key: usize, client: u64, input: RegisterInput) -> OpId {
        self.keys[key].lock().unwrap().invoke(client, input)
    }

    fn responded(&self, key: usize, op: OpId, output: RegisterOutput) {
        let _ = self.keys[key].lock().unwrap().respond(op, output);
    }

    /// The client asked and never learned the answer. It may have happened; it may not.
    fn maybe(&self, key: usize, op: OpId) {
        let _ = self.keys[key].lock().unwrap().respond_unknown(op);
    }
}

fn key_bytes(key: usize) -> Vec<u8> {
    format!("chaos-{key:02}").into_bytes()
}

/// Connects a client to every store, retrying until the cluster has someone listening.
///
/// Rebuilt rather than repaired after a node is killed: `TcpStores` opens its connections once,
/// so a client that has lost one reconnects, which is what a real client does.
fn connect(addrs: &[SocketAddr], deadline: Instant) -> Option<RawClient> {
    while Instant::now() < deadline {
        if let Ok(stores) = TcpStores::connect_all(addrs, TransportConfig::new()) {
            let ids = stores.store_ids();
            return Some(RawClient::new(
                Arc::new(stores),
                Arc::new(StaticRegion::replicated(REGION, &ids)),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
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
                let op = recorder.invoke(key, client_id, RegisterInput::Write(value.clone()));
                match client.put(&bytes, &value) {
                    Ok(()) => {
                        recorder.responded(key, op, RegisterOutput::Written);
                        tally.acked_writes += 1;
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        reconnect = classify(&error, &mut tally);
                        recorder.maybe(key, op);
                    }
                }
            }
            2 => {
                let op = recorder.invoke(key, client_id, RegisterInput::Read);
                match client.get(&bytes) {
                    Ok(found) => {
                        recorder.responded(key, op, RegisterOutput::Value(found));
                        tally.reads += 1;
                    }
                    Err(error) => {
                        reconnect = classify(&error, &mut tally);
                        recorder.maybe(key, op);
                    }
                }
            }
            _ => {
                // Compare-and-swap from whatever the client last saw. `expected` being wrong is
                // ordinary — another client got there first — and the model says so.
                let expected = client.get(&bytes).ok().flatten();
                let op = recorder.invoke(
                    key,
                    client_id,
                    RegisterInput::Cas {
                        expected: expected.clone(),
                        new: value.clone(),
                    },
                );
                match client.compare_and_swap(&bytes, expected.as_deref(), Some(&value)) {
                    Ok((swapped, _)) => {
                        recorder.responded(key, op, RegisterOutput::Swapped(swapped));
                        tally.swaps += 1;
                        if swapped {
                            writes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        reconnect = classify(&error, &mut tally);
                        recorder.maybe(key, op);
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

/// Counts an error, and says whether the client should reconnect.
fn classify(error: &Error, tally: &mut Tally) -> bool {
    if matches!(error, Error::AmbiguousResult { .. }) {
        tally.ambiguous += 1;
    } else {
        tally.refused += 1;
    }
    // Either way the client reconnects: `TcpStores` opens its connections once, so a client
    // that has lost the store it was talking to has to build a new book to find another.
    true
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
        let op = recorder.invoke(key, 0, RegisterInput::Read);
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
            Some(value) => recorder.responded(key, op, RegisterOutput::Value(value)),
            None => recorder.maybe(key, op),
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
const BUDGET_ATTEMPTS: u32 = 3;

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
        let history = recorder.keys[key].lock().unwrap();
        if history.is_empty() {
            continue;
        }
        match verdict_for(&history) {
            Verdict::Linearizable { placed } => {
                println!(
                    "key {key}: {placed} operations linearizable ({} pending)",
                    history.pending()
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
                 operations ({} pending); raise FIRST_BUDGET or shorten the run.",
                history.len(),
                history.pending()
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
