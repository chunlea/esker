//! The phase-1 crash loop, moved up a layer: kill the **server**, not the engine.
//!
//! `prompts/02-single-node-server.md` asks for "crash loop re-run through the client with
//! `sync = true` writes". Phase 1 established that the engine never loses an acknowledged
//! write when the process dies. This asks the question the network adds: when a client is
//! told "written", is it still true after the store it was talking to is killed outright?
//!
//! That is not the same question. A `SIGKILL` between the engine's `write()` returning and
//! the response frame reaching the socket produces a write that is durable and *unreported*,
//! which is legal. The failure this test exists to catch is the opposite one: a response
//! frame that goes out before the bytes it describes are durable. Nothing below the transport
//! can see that, because the engine's own crash tests never involve a response.
//!
//! # The shape
//!
//! The parent writes with `sync = true` and records only the writes the client *returned
//! success for*. The child is killed at a random moment; the store is reopened on the same
//! directory; every recorded write must read back with the value it was given. As in phase 1
//! the assertion is **containment, not equality** — `acked ⊆ recovered` — because a kill in
//! the gap above leaves a durable write nobody was told about.
//!
//! The child is this file's [`the_child_serves_until_it_is_killed`], re-executed through the
//! test binary itself with `--exact`, so there is no second binary to build or keep in step.
//! Phase 1's `crash_kill.rs` does the same thing for the same reason.
//!
//! # Durability the request did not ask for
//!
//! The store currently opens its engine with the default `WalSyncMode::PerWrite`, so every
//! write is durable whether or not the request set `sync`. This test therefore passes today
//! for a reason stronger than the one it is asserting. It is written against the *contract* —
//! `sync = true` means durable before the answer — so it keeps its meaning if that default
//! ever changes, and it is the test that would fail if it changed in the wrong direction.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use esker_base::rng::Pcg32;
use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};

/// Set for the child, unset for an ordinary run.
const ENV_DIR: &str = "ESKER_CLIENT_CRASH_DIR";

/// The child's test name, selected with `--exact`.
const CHILD_TEST: &str = "the_child_serves_until_it_is_killed";

/// Kill-and-recover rounds in a normal run. Enough for the random kill point to land in
/// different places; small enough to stay inside `just check`.
const ROUNDS: u32 = 8;

/// Rounds in the on-demand run.
const ROUNDS_IGNORED: u32 = 200;

const REGION: u64 = 1;

/// How long the parent waits for a child to say where it is listening.
///
/// Generous — a cold start under a loaded machine is slow — but finite. The point is that the
/// failure mode is a message, never a wedged test run.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------------------

/// Opens a store on [`ENV_DIR`], serves it on a free port, and prints where.
///
/// In an ordinary run [`ENV_DIR`] is unset and this returns at once; it is a `#[test]` only so
/// that the parent can select it by name.
#[test]
fn the_child_serves_until_it_is_killed() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    let mut out = std::io::stdout().lock();
    let store = match esker_store::Store::open(&dir, esker_store::StoreOptions::new()) {
        Ok(store) => store,
        Err(error) => {
            // Reported rather than panicked, so the parent gets a sentence instead of a
            // backtrace on a pipe it is about to read.
            let _ = writeln!(out, "FAIL open: {error}");
            let _ = out.flush();
            return;
        }
    };
    let service: Arc<dyn esker_proto::transport::Service> = esker_store::StoreService::new(store);

    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            service,
            esker_proto::transport::TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });

    // Two details, both learned the hard way. The **leading newline**: the test harness writes
    // `test <name> ... ` with no newline of its own before handing over, so a first line
    // written here arrives glued to that prefix and a parser looking for a line *starting*
    // with `ADDR` never matches — the parent then blocks on a pipe that will never say
    // anything else. The **flush**: the parent is waiting on this line, and this process never
    // exits, so nothing else will ever push it out of the buffer.
    let _ = writeln!(out, "\nADDR {}", handle.local_addr());
    let _ = out.flush();

    // Serve until killed. There is no clean exit path on purpose: the point is `SIGKILL`.
    loop {
        std::thread::sleep(Duration::from_secs(3_600));
    }
}

// ---------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------

/// A running child, and where it is listening.
struct Child {
    process: std::process::Child,
    addr: SocketAddr,
}

impl Child {
    /// Starts a child on `dir` and waits for it to say where it is.
    fn start(dir: &Path) -> Self {
        let mut process =
            Command::new(std::env::current_exe().expect("the test binary has a path"))
                .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(ENV_DIR, dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawning the child");

        let stdout = process.stdout.take().expect("the child's stdout is a pipe");

        // Read on another thread and wait with a deadline. A test that can block forever is
        // worse than one that fails: it wedges `just check` for whoever runs it next, with no
        // output to say why. Anything that stops the child from reporting — a store that will
        // not open, a port that will not bind, a panic before the first write — has to come
        // back as a failed assertion, not as silence.
        let (found, addr_line) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Searched for rather than stripped as a prefix: the harness writes
                // `test <name> ... ` with no trailing newline, so whatever the child prints
                // first shares a line with it. The child writes a leading newline to avoid
                // that, and this handles it anyway — a parser that can only match at column
                // zero turns a formatting detail into a hang.
                if let Some(at) = line.find("ADDR ") {
                    let _ = found.send(Ok(line[at + "ADDR ".len()..].trim().to_owned()));
                    return;
                }
                if line.contains("FAIL") {
                    let _ = found.send(Err(line));
                    return;
                }
            }
            let _ = found.send(Err(
                "the child ended without reporting an address".to_owned()
            ));
        });

        let reported = match addr_line.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(why)) => {
                let _ = process.kill();
                let _ = process.wait();
                panic!("the child failed to start: {why}");
            }
            Err(_) => {
                let _ = process.kill();
                let _ = process.wait();
                panic!(
                    "the child did not report an address within {STARTUP_TIMEOUT:?}; \
                     killed rather than left to block the run"
                );
            }
        };
        let addr = reported.parse::<SocketAddr>().unwrap_or_else(|err| {
            let _ = process.kill();
            let _ = process.wait();
            panic!("the child reported {reported:?}, which is not an address: {err}")
        });
        Self { process, addr }
    }

    /// `SIGKILL`, then reap. No shutdown, no drain, no flush.
    fn kill(mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A client pointed at `addr`, routing the way `esker raw` does.
fn client_for(addr: SocketAddr) -> RawClient {
    let stores = TcpStores::connect(addr).expect("the client connects");
    let store_id = stores.only_store().expect("the server named its store");
    RawClient::new(
        Arc::new(stores),
        Arc::new(StaticRegion::whole_key_space(REGION, store_id, 0)),
    )
}

fn key_of(index: u32) -> Vec<u8> {
    format!("crash{index:08}").into_bytes()
}

fn value_of(index: u32) -> Vec<u8> {
    // Length varies with the index, so a value recovered from the wrong write is visible.
    let mut value = format!("v{index}").into_bytes();
    value.resize(16 + (index as usize % 48), b'.');
    value
}

/// Writes durably until something fails, recording only what the client said succeeded.
fn write_until_it_breaks(client: &RawClient, acked: &Arc<Mutex<Vec<u32>>>, stop: &AtomicBool) {
    let mut index = 0u32;
    while !stop.load(Ordering::Relaxed) {
        // `put` asks for durability: `sync = true` is the default, and it is the whole point.
        match client.put(&key_of(index), &value_of(index)) {
            Ok(()) => acked.lock().unwrap().push(index),
            // The server is gone. Whether this one landed is unknowable, which is exactly why
            // it is not recorded as acknowledged.
            Err(_) => return,
        }
        index += 1;
    }
}

fn one_round(dir: &Path, seed: u64) -> usize {
    let child = Child::start(dir);
    let addr = child.addr;

    let acked = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let acked = Arc::clone(&acked);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let client = client_for(addr);
            write_until_it_breaks(&client, &acked, &stop);
        })
    };

    // **Kill after a random number of acknowledged writes, not after a random number of
    // milliseconds.**
    //
    // The cut still lands in a different place each round — inside a write, between two, or
    // while a response frame is on its way out — because the offset below still varies it. What
    // changed is the *precondition*: the round has something to verify by construction, instead
    // of by the writer having outrun a wall clock.
    //
    // It used to sleep 15-250 ms and kill. The writes it interrupts are CPU-bound and that sleep
    // is not, so on a loaded box the kill landed before the first acknowledgement and the round
    // had nothing to check — 6 failures in 10 under twenty-four spinning threads, in a single
    // container where no port collision is possible, every one of them at the `acked > 0` guard
    // below and none at the durability assertion (`docs/plans/debt-c6.md` §12).
    let mut rng = Pcg32::from_seed(seed);
    let target = usize::try_from(rng.range_inclusive(1, 8)).unwrap_or(1);
    let waiting_since = std::time::Instant::now();
    while acked.lock().unwrap().len() < target {
        assert!(
            waiting_since.elapsed() < Duration::from_secs(30),
            "the writer acknowledged {} writes in 30 s, short of the {target} this round waits \
             for: the server is not answering, which is a failure and not a slow box",
            acked.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // A small offset so the cut is not always immediately after an acknowledgement. It cannot
    // make the round vacuous: `target` acknowledgements have already happened.
    std::thread::sleep(Duration::from_millis(rng.range_inclusive(0, 15)));
    child.kill();

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("the writer did not panic");
    let acked = Arc::try_unwrap(acked)
        .expect("the writer is done")
        .into_inner()
        .unwrap();

    // Reopen on the same directory and check every write the client was told had happened.
    let child = Child::start(dir);
    let client = client_for(child.addr);
    for index in &acked {
        let found = client
            .get(&key_of(*index))
            .unwrap_or_else(|err| panic!("reading back write {index}: {err}"));
        assert_eq!(
            found.as_deref(),
            Some(&value_of(*index)[..]),
            "acknowledged write {index} did not survive the kill"
        );
    }
    child.kill();

    acked.len()
}

fn crash_loop(rounds: u32) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut total = 0usize;
    let mut killed_mid_flight = 0u32;

    for round in 0..rounds {
        // A fresh directory per round: this test is about one crash, not about what a
        // sequence of them leaves behind. That is phase 4's question.
        let path = dir.path().join(format!("round{round}"));
        let acked = one_round(&path, u64::from(round) * 0x9E37 + 1);
        assert!(
            acked > 0,
            "round {round} acknowledged nothing, so it proved nothing"
        );
        total += acked;
        killed_mid_flight += 1;
    }

    assert!(
        total > 0,
        "the loop verified no writes at all across {rounds} rounds"
    );
    println!(
        "crash loop: {killed_mid_flight} kills, {total} acknowledged writes verified after restart"
    );
}

/// Every write the client was told succeeded must still be there after the server is killed.
#[test]
fn every_acknowledged_write_survives_a_kill_of_the_server() {
    if std::env::var(ENV_DIR).is_ok() {
        return; // A child re-executing the whole file must not start its own loop.
    }
    crash_loop(ROUNDS);
}

/// The same, for long enough to be worth quoting at a gate.
#[test]
#[ignore = "the acceptance run; minutes rather than seconds"]
fn the_long_crash_loop() {
    crash_loop(ROUNDS_IGNORED);
}
