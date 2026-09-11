//! `esker server --region-census-ms` reaches a real process's log.
//!
//! `esker-store/tests/census.rs` proves the library emits on its cadence. This proves the other
//! half, which is the half that was actually missing: **this binary had no `tracing` subscriber at
//! all**, so every event the store and the Raft core emitted went nowhere. An instrument that
//! emits into a void is worse than no instrument — a run would come back with nothing to say and
//! nothing to say why.
//!
//! What run 121 will do is exactly what this test does, one node instead of four: pass
//! `--region-census-ms`, and read the census out of the process's own output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod port_band;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// How long the store has to produce its second census before this gives up.
///
/// Generous against the period below, because what is being tested is that the line arrives at
/// all — the *cadence* is `esker-store`'s test, which can watch a clock the store shares.
const WITHIN: Duration = Duration::from_secs(30);
const EVERY_MS: u64 = 100;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One free port, held until the child that binds it is spawned.
///
/// See `tests/port_band/mod.rs`: binding `:0` and releasing immediately is the narrow half of the
/// same race the fixed bands had, and one allocator is how it stays fixed everywhere at once.
fn free_port() -> u16 {
    port_band::reserve_one().into_base()
}

fn start(dir: &Path, log: &Path, census_ms: Option<u64>) -> Server {
    let out = std::fs::File::create(log).expect("the server log");
    let errors = out.try_clone().expect("the server log");
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
    command
        .arg("server")
        .arg("--data-dir")
        .arg(dir)
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--store-id")
        .arg("1")
        // A peer list of one: the region elects itself in a tick, so there is a leader to have a
        // term and a role within the first census rather than after an election.
        .arg("--peer")
        .arg(format!("1@127.0.0.1:{port}"));
    if let Some(every) = census_ms {
        command.arg("--region-census-ms").arg(every.to_string());
    }
    Server(
        command
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the server command starts"),
    )
}

/// Waits for `wanted` census lines, or says what the store said instead.
fn wait_for_census(server: &mut Server, log: &Path, wanted: usize) -> Result<Vec<String>, String> {
    let deadline = Instant::now() + WITHIN;
    loop {
        let said = std::fs::read_to_string(log).unwrap_or_default();
        let lines: Vec<String> = said
            .lines()
            .filter(|line| line.contains("region census"))
            .map(str::to_owned)
            .collect();
        if lines.len() >= wanted {
            return Ok(lines);
        }
        if let Some(status) = server.0.try_wait().expect("waiting on the server") {
            return Err(format!("the store exited with {status}; it said:\n{said}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} census lines in {WITHIN:?}, wanted {wanted}; the store said:\n{said}",
                lines.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "starts a store process; run with --run-ignored all"]
fn a_store_started_with_a_census_period_logs_one() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let mut server = start(&data, &log, Some(EVERY_MS));

    // Two, not one: the first would pass on a line emitted once at startup, which is the shape
    // this instrument must not have.
    let lines = wait_for_census(&mut server, &log, 2).expect("a census from a real store process");
    let last = lines.last().expect("a census line");
    for field in [
        "region=1",
        "handle_peer=1",
        "answered_by=1",
        "term=",
        "role=",
        "is_leader=",
        "core_voters=[1]",
        "record_peers=[1]",
        "campaigns_pre=",
        "check_quorum_step_downs=",
    ] {
        assert!(
            last.contains(field),
            "a census from the binary is missing `{field}`:\n{last}"
        );
    }
}

/// And a store started without the flag says nothing, which is what "off by default" means from
/// outside the process.
#[test]
#[ignore = "starts a store process; run with --run-ignored all"]
fn a_store_started_without_one_logs_none() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let mut server = start(&data, &log, None);

    // Long enough that the flagged run above would have produced dozens.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        server
            .0
            .try_wait()
            .expect("waiting on the server")
            .is_none(),
        "the store exited: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !said.contains("region census"),
        "a store with no --region-census-ms took one anyway:\n{said}"
    );
    // The store is up and logging *something*, so the assertion above is about the census and not
    // about a binary that logs nothing at all — which is what it would have proved yesterday.
    assert!(
        said.contains("listening on"),
        "the store never said it was listening, so this test proved nothing:\n{said}"
    );
}
