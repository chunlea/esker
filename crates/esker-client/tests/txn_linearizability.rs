//! Single-key transactional histories, checked against the register model.
//!
//! `prompts/05-txn.md`, last line of the test list: *linearizability of single-key transactional
//! histories through the Porcupine-style checker*. `tests/chaos_linearizability.rs` is the same
//! check for `RawKv`; this one runs it over `TxnKv`, where every operation is a whole
//! transaction and the answers come from a snapshot rather than from the current state.
//!
//! # Why a snapshot-isolated history is linearizable at all
//!
//! Snapshot isolation is famously *not* serializable, and a multi-key transactional history is
//! not linearizable either — `tests/anomalies.rs` ends with a write skew that proves it. A
//! **single-key** history is a different claim, and it does hold:
//!
//! * a read transaction takes effect at its `start_ts`, which the oracle hands out *inside* the
//!   operation's interval — after the caller invoked it, before it answered;
//! * a write transaction takes effect at its `commit_ts`, allocated after its prewrite and
//!   before its acknowledgement, so also inside the interval;
//! * the timestamps are totally ordered by one oracle (`CLAUDE.md` invariant 6) and the store
//!   applies their effects in that order.
//!
//! An effect point inside every operation's own interval, consistent with a single order, is
//! exactly linearizability. So a failure here is a real one: a lost commit, a read that saw
//! through a lock, or a resolution that undid a transaction that had committed.
//!
//! # Read-modify-write is a compare-and-swap
//!
//! A transaction that reads `k` and writes it commits only if nothing else committed `k` after
//! its snapshot — first-committer-wins (`docs/txn-spec.md` §6). That is a compare-and-swap
//! whose `expected` is what the read saw, and the model has one, so that is how it is checked.
//!
//! # What a refusal is checked as
//!
//! A `TxnConflict` means the transaction definitely did not happen, and the register model has
//! no way to say "did not happen" — its outputs all describe an operation that did. So a
//! refusal is checked as **unknown**, the same as an ambiguous answer: the checker may then
//! place it anywhere after its invocation, including at the very end where it is
//! indistinguishable from never having happened. That is weaker than the truth and never
//! wrong, and the claim the run is really making — that no acknowledged write is lost — rests
//! on the operations that *did* answer, and on the final read of every key.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use esker_client::{Error, TxnClient};
use esker_sim::lin::{
    CheckOutcome, Checker, History, OpId, Register, RegisterInput, RegisterOutput,
};

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

/// Keys the clients share, two either side of the region boundary. Few on purpose: collisions
/// are what make a history worth checking.
const KEYS: [&[u8]; 4] = [b"a-lin-1", b"a-lin-2", b"n-lin-1", b"n-lin-2"];

/// Concurrent clients.
const CLIENTS: u64 = 4;

/// How many operations each client **records**. Past it the client keeps working, on a key
/// nothing checks.
///
/// The bound is on the history and not on the run, and that distinction is what makes this test
/// machine-independent: a run that is *faster* gets through more operations in the same
/// wall-clock window and would otherwise build a bigger history. Measured, before this: the same
/// test that concluded on a loaded machine ran out of search budget on an idle one, and reported
/// it as "not linearizable" — because the assertion below treated every non-`Linearizable`
/// outcome as a violation, which the checker never claimed.
const RECORDED_OPS_PER_CLIENT: u64 = 40;

/// How many operations with an **unknown outcome** each key's history may hold.
///
/// See [`Recorder::accepting`]: this is the quantity the search cost is exponential in, and
/// bounding the operations alone left it to chance where the leader kills landed.
const UNKNOWNS_PER_KEY: usize = 6;

/// The key the clients keep working on once they have checked their share.
///
/// **Outside `KEYS`, so nothing checks it** — and it has to be worked on rather than idled,
/// because the leader kills below have to land on a cluster that is under load. Bounding the
/// history is not the same as bounding the run.
const LOAD_KEY: &[u8] = b"z-lin-load";

/// The histories, one per key, each recording every client's operations on it in the order they
/// were observed.
///
/// One mutex per key, taken to stamp an invocation and taken again to stamp the response —
/// never held across the call itself, which is what makes the checked order the *observed*
/// order.
struct Recorder {
    keys: Vec<Mutex<History<RegisterInput, RegisterOutput>>>,
    /// How many operations on each key ended with an outcome the client never learned.
    unknowns: Vec<AtomicUsize>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            keys: KEYS.iter().map(|_| Mutex::new(History::new())).collect(),
            unknowns: KEYS.iter().map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    /// Whether this key's history has room for another operation.
    ///
    /// **The bound is on the unknowns, because they are what the search costs.** An operation
    /// whose outcome the client never learned is checked as `Unknown`, which the checker holds
    /// open to the end of the history — so every later operation overlaps it and the search
    /// branches on each one. A history of ninety operations with five unknowns settles in
    /// milliseconds; the same ninety with twenty-five does not settle in a million steps.
    ///
    /// Bounding the operations instead is what this test did first, and it was not enough: how
    /// many of them come back unknown depends on where the leader kills land, so a run could
    /// still put twenty of them in one history. This is the quantity that actually decides.
    fn accepting(&self, key: usize) -> bool {
        self.unknowns[key].load(Ordering::Relaxed) < UNKNOWNS_PER_KEY
    }

    fn invoke(&self, key: usize, client: u64, input: RegisterInput) -> OpId {
        self.keys[key].lock().unwrap().invoke(client, input)
    }

    fn responded(&self, key: usize, op: OpId, output: RegisterOutput) {
        let _ = self.keys[key].lock().unwrap().respond(op, output);
    }

    /// It may have happened; it may not. A refusal and an ambiguous answer are both this, for
    /// the reason in this file's header.
    fn maybe(&self, key: usize, op: OpId) {
        self.unknowns[key].fetch_add(1, Ordering::Relaxed);
        let _ = self.keys[key].lock().unwrap().respond_unknown(op);
    }
}

#[derive(Debug, Default)]
struct Tally {
    reads: u64,
    writes: u64,
    swaps: u64,
    refused: u64,
    unknown: u64,
}

impl Tally {
    fn merge(&mut self, other: &Self) {
        self.reads += other.reads;
        self.writes += other.writes;
        self.swaps += other.swaps;
        self.refused += other.refused;
        self.unknown += other.unknown;
    }
}

/// Runs single-key transactions until `stop`, recording every one.
fn drive(client_id: u64, cluster: &Cluster, recorder: &Recorder, stop: &AtomicBool) -> Tally {
    let mut tally = Tally::default();
    let Some(mut client) = cluster.client_within(client_id, Duration::from_secs(20)) else {
        return tally;
    };
    let mut sequence = 0u64;

    while !stop.load(Ordering::Relaxed) {
        sequence += 1;
        // Past its share a client stops *recording* and keeps *working*; see
        // `RECORDED_OPS_PER_CLIENT`.
        let key = usize::try_from((client_id + sequence) % KEYS.len() as u64).unwrap_or(0);
        let checked = sequence <= RECORDED_OPS_PER_CLIENT && recorder.accepting(key);
        let target: &[u8] = if checked { KEYS[key] } else { LOAD_KEY };
        // A value that names the write that made it, so no two writes share a value — which is
        // what lets a compare-and-swap's `expected` mean something.
        let value = Bytes::from(format!("c{client_id}-w{sequence}").into_bytes());

        let mut reconnect = false;
        match sequence % 4 {
            0 | 1 => {
                let op = checked
                    .then(|| recorder.invoke(key, client_id, RegisterInput::Write(value.clone())));
                match write(&client, target, &value) {
                    Ok(()) => {
                        if let Some(op) = op {
                            recorder.responded(key, op, RegisterOutput::Written);
                        }
                        tally.writes += 1;
                    }
                    Err(refused) => {
                        reconnect = classify(&refused, &mut tally);
                        if let Some(op) = op {
                            recorder.maybe(key, op);
                        }
                    }
                }
            }
            2 => {
                let op = checked.then(|| recorder.invoke(key, client_id, RegisterInput::Read));
                match read(&client, target) {
                    Ok(seen) => {
                        if let Some(op) = op {
                            recorder.responded(key, op, RegisterOutput::Value(seen));
                        }
                        tally.reads += 1;
                    }
                    Err(refused) => {
                        reconnect = classify(&refused, &mut tally);
                        if let Some(op) = op {
                            recorder.maybe(key, op);
                        }
                    }
                }
            }
            _ => {
                // Read and write in **one** transaction: a compare-and-swap, decided by
                // first-committer-wins on the key.
                let Ok(mut txn) = client.begin() else {
                    continue;
                };
                let expected = match txn.get(target) {
                    Ok(seen) => seen,
                    Err(refused) => {
                        reconnect = classify(&refused, &mut tally);
                        if reconnect && let Some(fresh) = reconnected(cluster, client_id) {
                            client = fresh;
                        }
                        continue;
                    }
                };
                let op = checked.then(|| {
                    recorder.invoke(
                        key,
                        client_id,
                        RegisterInput::Cas {
                            expected: expected.clone(),
                            new: value.clone(),
                        },
                    )
                });
                txn.put(target, &value);
                match txn.commit() {
                    Ok(_) => {
                        if let Some(op) = op {
                            recorder.responded(key, op, RegisterOutput::Swapped(true));
                        }
                        tally.swaps += 1;
                    }
                    Err(refused) => {
                        reconnect = classify(&refused, &mut tally);
                        if let Some(op) = op {
                            recorder.maybe(key, op);
                        }
                    }
                }
            }
        }

        if reconnect && let Some(fresh) = reconnected(cluster, client_id) {
            client = fresh;
        }
    }
    tally
}

fn write(client: &TxnClient, key: &[u8], value: &Bytes) -> Result<(), Error> {
    let mut txn = client.begin()?;
    txn.put(key, value);
    txn.commit().map(|_| ())
}

fn read(client: &TxnClient, key: &[u8]) -> Result<Option<Bytes>, Error> {
    client.begin()?.get(key)
}

fn reconnected(cluster: &Cluster, client_id: u64) -> Option<TxnClient> {
    cluster.client_within(client_id, Duration::from_secs(5))
}

/// Counts an outcome, and says whether the client should rebuild its connections.
///
/// A `TxnConflict` is an ordinary race lost, not a broken cluster: the client keeps the
/// connections it has. Anything else means the store it was talking to may be gone.
fn classify(error: &Error, tally: &mut Tally) -> bool {
    if matches!(error, Error::TxnConflict { .. } | Error::TxnSettled { .. }) {
        tally.refused += 1;
        return false;
    }
    tally.unknown += 1;
    true
}

/// Reads every key once more, into the history, after the cluster has settled.
///
/// This is what turns "no acknowledged write is lost" into a property the checker enforces: a
/// write that vanished leaves a final read that no ordering can explain.
fn final_reads(cluster: &Cluster, recorder: &Recorder) {
    let client = cluster
        .client_within(900, Duration::from_secs(30))
        .expect("a client for the final reads");
    for (index, key) in KEYS.iter().enumerate() {
        let op = recorder.invoke(index, 0, RegisterInput::Read);
        let mut seen = None;
        for _ in 0..40 {
            if let Ok(value) = read(&client, key) {
                seen = Some(value);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match seen {
            Some(value) => recorder.responded(index, op, RegisterOutput::Value(value)),
            None => recorder.maybe(index, op),
        }
    }
}

/// Runs the battery: `kills` leader kills under transactional load, then checks every key.
fn battery(kills: u32, between: Duration) {
    let cluster = Cluster::start(Topology::two_regions(0x11_2026));
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never elected a leader to begin with"
    );

    let recorder = Arc::new(Recorder::new());
    let stop = Arc::new(AtomicBool::new(false));
    let clients: Vec<_> = (0..CLIENTS)
        .map(|at| {
            let (cluster, recorder, stop) = (
                Arc::clone(&cluster),
                Arc::clone(&recorder),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || drive(at + 1, &cluster, &recorder, &stop))
        })
        .collect();

    let mut killed = 0;
    if kills == 0 {
        // A control run: the same clients on the same cluster with nothing killed. If a history
        // does not linearize here, the fault is in the modelling, not in what killing does.
        std::thread::sleep(between * 4);
    }
    for round in 0..kills {
        std::thread::sleep(between);
        let group = usize::try_from(u64::from(round) % 2).unwrap_or(0);
        let Some(at) = cluster.leader_of(group) else {
            continue;
        };
        cluster.kill(at);
        killed += 1;
        std::thread::sleep(between);
        cluster.start_node(at);
    }

    stop.store(true, Ordering::Relaxed);
    let mut tally = Tally::default();
    for handle in clients {
        tally.merge(&handle.join().expect("a client thread"));
    }

    assert!(
        cluster.settle(Duration::from_secs(60)),
        "the cluster never came back after {killed} kills"
    );
    final_reads(&cluster, &recorder);
    cluster.shutdown();

    println!(
        "{killed} leader kills, {CLIENTS} clients: {} reads, {} writes, {} swaps, {} refused, \
         {} unknown",
        tally.reads, tally.writes, tally.swaps, tally.refused, tally.unknown
    );
    assert!(
        tally.writes + tally.swaps > 0,
        "not one transaction committed; the run proves nothing about losing them"
    );

    for (index, key) in KEYS.iter().enumerate() {
        let history = recorder.keys[index].lock().unwrap();
        if history.is_empty() {
            continue;
        }
        match Checker::new().check(&Register, &history) {
            CheckOutcome::Linearizable { order } => println!(
                "{}: {} operations linearizable ({} pending)",
                String::from_utf8_lossy(key),
                order.len(),
                history.pending()
            ),
            // **An inconclusive search is not a violation.** The checker says so itself — it
            // claims neither answer — and a message that calls it one sends the next reader
            // looking for a transaction bug that was never reported. It still fails the run,
            // because a run that verified nothing has not verified anything.
            CheckOutcome::Inconclusive { steps } => panic!(
                "the transactional history of {} could not be settled: the checker gave up after \
                 {steps} steps. That is not a linearizability violation — it is a history too \
                 wide to search, which `UNKNOWNS_PER_KEY` exists to bound.",
                String::from_utf8_lossy(key)
            ),
            other @ CheckOutcome::NotLinearizable { .. } => panic!(
                "the single-key transactional history of {} is not linearizable after {killed} \
                 leader kills.\n{other}",
                String::from_utf8_lossy(key)
            ),
        }
    }
}

/// The control run: no faults, so a failure here is about transactions and nothing else.
#[test]
fn transactional_histories_of_one_key_linearize() {
    battery(0, Duration::from_millis(400));
}

/// The same, with the leader killed underneath — where a commit can land on one leader and be
/// read from another.
#[test]
fn transactional_histories_linearize_across_leader_changes() {
    battery(3, Duration::from_millis(400));
}
