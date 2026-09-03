//! Three store *processes*, and `kill -9` on whichever one leads.
//!
//! `prompts/03-raft.md`, "Tests for 3e": *start 3 processes, SIGKILL the leader under load; no
//! acknowledged write lost, cluster converges each time within 5 s*. This is that run. The
//! cluster is started the way an operator starts one — `esker cluster start --nodes 3` — and
//! killed the way a machine dies: `SIGKILL`, no unwinding, no flush, no chance to finish what
//! it was doing.
//!
//! # What this proves that the in-process battery cannot
//!
//! `esker-client`'s `chaos_linearizability.rs` runs the same workload against three stores in
//! one process, and stops them politely, because a thread cannot be shot. Everything inside the
//! engine finishes there. Here nothing does: the signal lands between two `write` syscalls as
//! easily as between two requests, and what comes back is whatever was actually on the disk.
//! That is the difference between "the store shuts down correctly" and "an acknowledged write
//! is durable", and only one of them is the invariant.
//!
//! # Convergence
//!
//! The prompt asks for five seconds, so the test measures rather than assumes: after each kill
//! it times how long until a write succeeds again and asserts the worst case. The number is
//! printed, so a regression shows up as a number getting worse before it shows up as a failure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_client::region_cache::StaticRegion;
use esker_client::{Error, RawClient, TcpStores};
use esker_proto::TransportConfig;
use esker_sim::lin::{
    CheckOutcome, Checker, History, OpId, Register, RegisterInput, RegisterOutput,
};
use tempfile::TempDir;

const REGION: u64 = 1;
const NODES: u64 = 3;
/// The same count where a length or a port offset is wanted.
const NODE_COUNT: usize = 3;
const KEYS: usize = 3;
const CLIENTS: usize = 4;
const SEED: u64 = 20_260_830;
/// What `prompts/03-raft.md` allows a cluster to take to come back.
const CONVERGE_WITHIN: Duration = Duration::from_secs(5);

/// One line of `cluster.state`.
#[derive(Debug, Clone)]
struct Node {
    id: u64,
    address: SocketAddr,
    pid: u32,
}

/// A cluster of real processes, plus the supervisor that started them.
struct Cluster {
    supervisor: Child,
    data_dir: TempDir,
    base_port: u16,
    nodes: Vec<Node>,
}

/// A run of `NODES` consecutive ports, **held** until the caller hands them over.
///
/// `cluster start` numbers its nodes from one base port, so they have to be consecutive — which
/// rules out binding port zero. And the servers are separate **processes**, so the sockets cannot
/// be passed to them the way an in-process harness passes a listener to `Server::from_listener`.
/// The hold therefore cannot last all the way to the bind, and pretending otherwise is what the
/// comment here used to do. Three things are available instead, and together they are what closed
/// this:
///
/// * the run is **held through the caller's setup** and released on the line before the child is
///   spawned, rather than at the top of it — the window shrinks from a whole `Cluster::start` to
///   one statement;
/// * the scan **starts at a random slot** rather than walking from the bottom of the range, so two
///   processes running this file at once do not both pick `21_000`;
/// * and the caller **retries with a fresh run** if the cluster does not come up, because the
///   remaining window is real and cannot be closed from here.
///
/// The first two are why a collision is rare; the third is why one is not a failure.
fn reserve_port_run() -> (u16, Vec<TcpListener>) {
    const LOW: u16 = 21_000;
    const HIGH: u16 = 30_000;
    let stride = u16::try_from(NODE_COUNT).expect("a small node count") + 1;
    let slots = (HIGH - LOW) / stride;
    // No `rand` here (`CLAUDE.md`'s dependency policy), and none is needed: the clock and the pid
    // are enough to keep two processes from starting at the same slot.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let start = (nanos ^ std::process::id()) % u32::from(slots);
    for step in 0..slots {
        let slot = (start + u32::from(step)) % u32::from(slots);
        let Some(base) = u16::try_from(slot)
            .ok()
            .and_then(|slot| slot.checked_mul(stride))
            .and_then(|offset| LOW.checked_add(offset))
        else {
            continue;
        };
        let bound: Vec<TcpListener> = (0..NODE_COUNT)
            .filter_map(|at| {
                let offset = u16::try_from(at).ok()?;
                TcpListener::bind(("127.0.0.1", base.checked_add(offset)?)).ok()
            })
            .collect();
        if bound.len() == NODE_COUNT {
            return (base, bound);
        }
    }
    panic!("no run of {NODES} consecutive free ports between {LOW} and {HIGH}");
}

fn address_of(base_port: u16, id: u64) -> SocketAddr {
    format!("127.0.0.1:{}", u64::from(base_port) + id - 1)
        .parse()
        .unwrap()
}

fn node_dir(data_dir: &Path, id: u64) -> PathBuf {
    data_dir.join(format!("node-{id}"))
}

/// Reads `cluster.state`, or `None` until the supervisor has written it.
fn read_state(data_dir: &Path) -> Option<Vec<Node>> {
    let text = std::fs::read_to_string(data_dir.join("cluster.state")).ok()?;
    let nodes: Vec<Node> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Node {
                id: fields.next()?.parse().ok()?,
                address: fields.next()?.parse().ok()?,
                pid: fields.next()?.parse().ok()?,
            })
        })
        .collect();
    (nodes.len() == NODE_COUNT).then_some(nodes)
}

impl Cluster {
    /// Starts the cluster, **retrying on a fresh port run** if it does not come up.
    ///
    /// The reservation cannot be handed to a child process, so a run this test holds is released
    /// a moment before the supervisor's nodes bind it and another process can still take one in
    /// between (see [`reserve_port_run`]). That window is narrow and it is not zero, so losing it
    /// is treated as a retry rather than as a failure — three attempts, each on a different run.
    fn start() -> Self {
        for attempt in 1..=3_u32 {
            if let Some(cluster) = Self::try_start() {
                return cluster;
            }
            eprintln!("the cluster did not come up on attempt {attempt}; trying another port run");
        }
        panic!("the supervisor never wrote cluster.state, on three different port runs");
    }

    /// One attempt, on one reserved run. `None` if the cluster did not come up.
    fn try_start() -> Option<Self> {
        let data_dir = TempDir::new().unwrap();
        let (base_port, held) = reserve_port_run();
        let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
        command
            .arg("cluster")
            .arg("start")
            .arg("--nodes")
            .arg(NODES.to_string())
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--base-port")
            .arg(base_port.to_string())
            .arg("--seed")
            .arg(SEED.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // **Released here and nowhere earlier.** Everything above this line happened while the run
        // was still ours; the child binds it on the next.
        drop(held);
        let supervisor = command.spawn().expect("the cluster command starts");

        let deadline = Instant::now() + Duration::from_secs(30);
        let nodes = loop {
            if let Some(nodes) = read_state(data_dir.path()) {
                break nodes;
            }
            if Instant::now() >= deadline {
                // Built and dropped so the supervisor is killed the way it is anywhere else.
                drop(Self {
                    supervisor,
                    data_dir,
                    base_port,
                    nodes: Vec::new(),
                });
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        Some(Self {
            supervisor,
            data_dir,
            base_port,
            nodes,
        })
    }

    fn addrs(&self) -> Vec<SocketAddr> {
        self.nodes.iter().map(|node| node.address).collect()
    }

    /// `kill -9`, by pid, with no chance to clean up.
    ///
    /// Shelling out to `kill` for the same reason `esker-cli` does: `libc` is a `*-sys`-shaped
    /// dependency this project does not take (`CLAUDE.md`), and a test is not worth an
    /// exception.
    fn kill(&self, id: u64) {
        let Some(node) = self.nodes.iter().find(|node| node.id == id) else {
            return;
        };
        let _ = Command::new("kill")
            .arg("-9")
            .arg(node.pid.to_string())
            .status();
    }

    /// Starts node `id` again, with the arguments the supervisor gave it.
    ///
    /// The supervisor does not restart what dies — it waits for ctrl-C — so a test that wants
    /// the node back has to bring it back itself, which is also what an operator would do. The
    /// new pid is recorded so a later kill finds the process that is actually running.
    fn restart(&mut self, id: u64) {
        let peers: Vec<String> = (1..=NODES)
            .map(|peer| format!("{peer}@{}", address_of(self.base_port, peer)))
            .collect();
        let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
        command
            .arg("server")
            .arg("--data-dir")
            .arg(node_dir(self.data_dir.path(), id))
            .arg("--listen")
            .arg(address_of(self.base_port, id).to_string())
            .arg("--store-id")
            .arg(id.to_string())
            .arg("--seed")
            .arg(SEED.to_string());
        for peer in &peers {
            command.arg("--peer").arg(peer);
        }
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the node restarts");
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.pid = child.id();
        }
        // The child is deliberately not waited on: it runs until this test kills it, and
        // `stop` kills every recorded pid.
        std::mem::forget(child);
    }

    /// Kills every node and the supervisor. Called on the way out, including after a panic, so
    /// a failed test does not leave three stores holding ports.
    fn stop(&mut self) {
        for node in &self.nodes {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(node.pid.to_string())
                .status();
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One history per key, stamped in the order the events were observed.
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

    fn maybe(&self, key: usize, op: OpId) {
        let _ = self.keys[key].lock().unwrap().respond_unknown(op);
    }
}

fn key_bytes(key: usize) -> Vec<u8> {
    format!("chaos-{key:02}").into_bytes()
}

/// Connects to whatever is listening, retrying while a node is coming back.
fn connect(addrs: &[SocketAddr], deadline: Instant) -> Option<RawClient> {
    while Instant::now() < deadline {
        if let Ok(stores) = TcpStores::connect_all(addrs, TransportConfig::new()) {
            let ids = stores.store_ids();
            if !ids.is_empty() {
                return Some(RawClient::new(
                    Arc::new(stores),
                    Arc::new(StaticRegion::replicated(REGION, &ids)),
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Writes one key until it is acknowledged, and says how long that took.
///
/// This is the convergence measurement: the cluster has come back when it can take a write
/// again, which is what a caller actually cares about.
fn time_to_serve(addrs: &[SocketAddr], within: Duration) -> Option<Duration> {
    let started = Instant::now();
    let deadline = started + within;
    while Instant::now() < deadline {
        if let Some(client) = connect(addrs, deadline)
            && client.put(b"converge", b"ok").is_ok()
        {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Which node the client believes leads, learned from the redirects it followed.
fn believed_leader(addrs: &[SocketAddr]) -> Option<u64> {
    let client = connect(addrs, Instant::now() + CONVERGE_WITHIN)?;
    // A write goes to the leader or is redirected to it; either way the cache ends up naming
    // it. Asking the cluster this way rather than through a side channel is the point: it is
    // what a caller can see.
    client.put(b"whoami", b"?").ok()?;
    let route = client.cache().lookup(b"whoami")?;
    route.target().map(|peer| peer.store_id)
}

#[derive(Default)]
struct Tally {
    acked: u64,
    ambiguous: u64,
    refused: u64,
}

fn drive(
    client_id: u64,
    addrs: &[SocketAddr],
    recorder: &Recorder,
    stop: &AtomicBool,
    acked: &AtomicU64,
) -> Tally {
    let mut tally = Tally::default();
    let Some(mut client) = connect(addrs, Instant::now() + Duration::from_secs(20)) else {
        return tally;
    };
    let mut sequence = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        sequence += 1;
        let key = usize::try_from((client_id + sequence) % KEYS as u64).unwrap_or(0);
        let bytes = key_bytes(key);
        let value = Bytes::from(format!("c{client_id}-w{sequence}").into_bytes());

        let failed = if sequence % 3 == 2 {
            let op = recorder.invoke(key, client_id, RegisterInput::Read);
            match client.get(&bytes) {
                Ok(found) => {
                    recorder.responded(key, op, RegisterOutput::Value(found));
                    false
                }
                Err(error) => {
                    count(&error, &mut tally);
                    recorder.maybe(key, op);
                    true
                }
            }
        } else {
            let op = recorder.invoke(key, client_id, RegisterInput::Write(value.clone()));
            match client.put(&bytes, &value) {
                Ok(()) => {
                    recorder.responded(key, op, RegisterOutput::Written);
                    tally.acked += 1;
                    acked.fetch_add(1, Ordering::Relaxed);
                    false
                }
                Err(error) => {
                    count(&error, &mut tally);
                    recorder.maybe(key, op);
                    true
                }
            }
        };

        if failed && let Some(fresh) = connect(addrs, Instant::now() + Duration::from_secs(5)) {
            client = fresh;
        }
    }
    tally
}

fn count(error: &Error, tally: &mut Tally) {
    if matches!(error, Error::AmbiguousResult { .. }) {
        tally.ambiguous += 1;
    } else {
        tally.refused += 1;
    }
}

/// Kills the leader `kills` times under load, and checks what came out.
fn battery(kills: u32) {
    let mut cluster = Cluster::start();
    let addrs = cluster.addrs();
    assert!(
        time_to_serve(&addrs, Duration::from_secs(30)).is_some(),
        "the cluster never served a write to begin with"
    );

    let recorder = Arc::new(Recorder::new());
    let stop = Arc::new(AtomicBool::new(false));
    let acked = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..CLIENTS)
            .map(|at| {
                let (addrs, recorder, stop, acked) = (&addrs, &*recorder, &*stop, &*acked);
                scope.spawn(move || drive(at as u64 + 1, addrs, recorder, stop, acked))
            })
            .collect();

        let mut worst = Duration::ZERO;
        let mut killed = 0;
        for _ in 0..kills {
            std::thread::sleep(Duration::from_millis(700));
            let Some(leader) = believed_leader(&addrs) else {
                continue;
            };
            cluster.kill(leader);
            killed += 1;

            let converged = time_to_serve(&addrs, CONVERGE_WITHIN);
            let took = converged.unwrap_or_else(|| {
                panic!(
                    "after killing node {leader}, the cluster did not serve a write within \
                     {CONVERGE_WITHIN:?}"
                )
            });
            worst = worst.max(took);
            cluster.restart(leader);
        }

        stop.store(true, Ordering::Relaxed);
        let tallies: Vec<Tally> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let total: u64 = tallies.iter().map(|t| t.acked).sum();
        let ambiguous: u64 = tallies.iter().map(|t| t.ambiguous).sum();
        let refused: u64 = tallies.iter().map(|t| t.refused).sum();
        println!(
            "{killed} SIGKILLs of the leader, {CLIENTS} clients: {total} acknowledged writes, \
             {ambiguous} ambiguous, {refused} refused; worst convergence {worst:?} against a \
             {CONVERGE_WITHIN:?} budget"
        );
        assert!(killed > 0, "no leader was ever killed");
        assert!(total > 0, "not one write was acknowledged");
        assert!(worst < CONVERGE_WITHIN);
    });

    // Settle, then read every key back into the history: a write that did not survive a kill
    // leaves a final read no ordering can explain.
    assert!(
        time_to_serve(&addrs, Duration::from_secs(20)).is_some(),
        "the cluster never came back for the final reads"
    );
    final_reads(&addrs, &recorder);
    cluster.stop();
    check_histories(&recorder, kills);
}

fn final_reads(addrs: &[SocketAddr], recorder: &Recorder) {
    let Some(client) = connect(addrs, Instant::now() + Duration::from_secs(20)) else {
        panic!("no store was reachable for the final read");
    };
    for key in 0..KEYS {
        let bytes = key_bytes(key);
        let op = recorder.invoke(key, 0, RegisterInput::Read);
        let mut found = None;
        for _ in 0..60 {
            if let Ok(value) = client.get(&bytes) {
                found = Some(value);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match found {
            Some(value) => recorder.responded(key, op, RegisterOutput::Value(value)),
            None => recorder.maybe(key, op),
        }
    }
}

fn check_histories(recorder: &Recorder, kills: u32) {
    for key in 0..KEYS {
        let history = recorder.keys[key].lock().unwrap();
        if history.is_empty() {
            continue;
        }
        match Checker::new().check(&Register, &history) {
            CheckOutcome::Linearizable { order } => {
                println!("key {key}: {} operations linearizable", order.len());
            }
            other => panic!(
                "key {key}: the history of three store processes with {kills} SIGKILLs of the \
                 leader is not linearizable.\n{other}"
            ),
        }
    }
}

/// The short run: three real processes, the leader shot twice.
#[test]
fn a_sigkilled_leader_process_never_costs_an_acknowledged_write() {
    if std::env::var_os("ESKER_SKIP_PROCESS_TESTS").is_some() {
        return;
    }
    battery(2);
}

/// The acceptance run from `prompts/03-raft.md`: fifty kills under load.
///
/// ```text
/// cargo test -p esker-cli --release --test cluster_chaos -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the 50-SIGKILL acceptance run; minutes, not seconds"]
fn fifty_sigkills_of_the_leader_process() {
    battery(50);
}

/// `kill` has to exist for any of this to mean anything; a missing one would make every kill a
/// silent no-op and every assertion vacuous.
#[test]
fn the_kill_command_exists() {
    let status = Command::new("kill")
        .arg("-0")
        .arg(std::process::id().to_string())
        .status();
    match status {
        Ok(status) => assert!(status.success(), "`kill -0` on this process failed"),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            panic!("`kill` is not on PATH, so the chaos tests would kill nothing")
        }
        Err(error) => panic!("running `kill`: {error}"),
    }
}

/// **A reserved port run has to still be ours when the child processes bind it.**
///
/// Two claims, and the scan satisfies neither. It binds a run to prove it is free and then drops
/// every listener, so the ports belong to nobody until the supervisor's children get to them; and
/// it walks from the same base every time, so two processes running this file at once pick the
/// same run and one of them loses.
#[test]
fn a_reserved_port_run_is_held_and_never_handed_out_twice() {
    let (first, held) = reserve_port_run();
    for at in 0..NODE_COUNT {
        let offset = u16::try_from(at).expect("a small offset");
        let port = first.checked_add(offset).expect("in range");
        assert!(
            TcpListener::bind(("127.0.0.1", port)).is_err(),
            "port {port} of the reserved run was still bindable, so the reservation holds nothing"
        );
    }
    // And a second reservation cannot be the same run while the first is held.
    let (second, _also_held) = reserve_port_run();
    assert_ne!(
        first, second,
        "two reservations returned the same run, so two processes doing this at once collide"
    );
    drop(held);
}
