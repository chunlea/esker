//! The load arm keeps writing across a placement-driver kill.
//!
//! `esker durability record` is the arm every durability run measures with, and **every write it
//! makes starts with a timestamp from the placement driver** (`CLAUDE.md` invariant 6). So an
//! oracle that holds one member stops handing out timestamps when that member is the one that
//! dies, and every writer stops with it — a run that killed the driver would measure its own load
//! generator rather than the cluster.
//!
//! That is not hypothetical arithmetic: `esker-coord/h1-run123-2501ms-gap.md` showed that run 123's
//! only *leader* kill of eight was the whole of its worst gap, and the tool built to kill leaders
//! on purpose (`esker-rails-harness/leader-kill.py`) arrived the same afternoon. A run that uses
//! it against a three-driver cluster needs the load arm to survive the thing it is killing.
//!
//! # The assertion is a clock, not a count
//!
//! Column five of the writes log is **microseconds from the start of the run to the moment
//! `commit` returned**. So "writes were still being acknowledged five seconds after the driver
//! that was leading was killed" is one number read off the file, and it does not depend on how
//! fast this machine is: a slow box makes fewer writes, not later ones.
//!
//! The counterfactual is `--pd <the member that gets killed>` alone, and it is not a subtle one:
//! the last acknowledged write is at **3001 ms** against a kill at 3000 ms, while two other
//! members go on serving.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const NODES: u16 = 1;
const DRIVERS: u16 = 3;
const STARTUP_SECONDS: u64 = 120;

/// How long the load arm runs. Long enough that "still writing well after the kill" is a real
/// interval rather than a rounding error, short enough to belong in a gate.
const RECORD_FOR: &str = "12s";

/// When the driver is killed, measured from the moment `record` was started.
const KILL_AT: Duration = Duration::from_secs(3);

/// The write that has to exist: one acknowledged this long into the run, which is five seconds
/// after the kill.
const STILL_WRITING_AT_MICROS: u64 = 8_000_000;

#[test]
fn the_load_arm_keeps_writing_when_the_leading_driver_is_killed() {
    let data_dir = TempDir::new().unwrap();
    let base = free_ports(NODES + DRIVERS);
    let store_port = base;
    let driver_ports: Vec<u16> = (0..DRIVERS).map(|at| base + NODES + at).collect();
    warm();

    let mut cluster = Supervisor(
        Command::new(esker_cli())
            .args([
                "cluster",
                "start",
                "--nodes",
                &NODES.to_string(),
                "--data-dir",
            ])
            .arg(data_dir.path())
            .args(["--base-port", &store_port.to_string(), "--pd"])
            .args(["--pd-nodes", &DRIVERS.to_string()])
            // The kill has to stay killed, or this passes on a cluster with no group at all.
            .arg("--no-respawn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the cluster starts"),
    );
    wait_for_port("the store", store_port, &mut cluster, STARTUP_SECONDS);
    for (at, port) in driver_ports.iter().enumerate() {
        wait_for_port(
            &format!("placement driver {}", at + 1),
            *port,
            &mut cluster,
            STARTUP_SECONDS,
        );
    }
    let endpoints: Vec<String> = driver_ports
        .iter()
        .map(|port| format!("127.0.0.1:{port}"))
        .collect();

    let writes = data_dir.path().join("writes.log");
    let mut record = Supervisor(
        Command::new(esker_cli())
            .args(["durability", "record", "--pd", &endpoints.join(",")])
            .args(["--out", writes.to_str().unwrap()])
            .args(["--clients", "2", "--for", RECORD_FOR])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`durability record` starts"),
    );

    // Let the run get going, then take the member that is leading out from under it.
    std::thread::sleep(KILL_AT);
    let leader = leading(&endpoints);
    let pid = pid_of(data_dir.path(), &leader);
    kill9(pid);

    let status = record.0.wait().expect("waiting on `durability record`");
    assert!(
        status.success(),
        "`durability record` exited with {status} after the driver that was leading ({leader}) \
         was killed"
    );

    let log = std::fs::read_to_string(&writes).expect("the writes log");
    let last = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| line.split('\t').nth(4)?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    assert!(
        last >= STILL_WRITING_AT_MICROS,
        "the last acknowledged write was {} ms into the run, and the driver that was leading \
         ({leader}, pid {pid}) was killed at {} ms. The load arm stopped with the member it was \
         talking to while {} others were serving.",
        last / 1_000,
        KILL_AT.as_millis(),
        endpoints.len() - 1
    );
}

/// The address of the member `esker pd members` says is leading.
///
/// Matched against the endpoints this test started rather than read out of a column: the listing's
/// address field is empty for a member of a group of one (ADR 0061), so a positional read returns
/// the word `voter`.
fn leading(endpoints: &[String]) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = String::new();
    loop {
        for endpoint in endpoints {
            let out = Command::new(esker_cli())
                .args(["pd", "members", "--pd", endpoint])
                .output()
                .expect("`pd members` runs");
            last = String::from_utf8_lossy(&out.stdout).into_owned();
            let Some(line) = last
                .lines()
                .find(|line| line.contains("leader") && !line.contains("no leader"))
            else {
                continue;
            };
            if let Some(address) = endpoints.iter().find(|end| line.contains(end.as_str())) {
                return address.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no placement driver named a leader among {endpoints:?} within a minute. Last \
             answer:\n{last}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The pid the state file records for `address`. By address, because every driver's line has id 0.
fn pid_of(data_dir: &Path, address: &str) -> u32 {
    let text = std::fs::read_to_string(data_dir.join("cluster.state")).expect("the state file");
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let [_, listed, pid] = fields[..]
            && listed == address
        {
            return pid.parse().expect("a pid");
        }
    }
    panic!("the state file does not name {address}:\n{text}");
}

fn kill9(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("`kill` runs");
    assert!(status.success(), "could not kill pid {pid}");
}

struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn esker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_esker-cli")
}

fn warm() {
    let _unused = Command::new(esker_cli())
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// This file's own port band, for the reason `cluster_start.rs` gives: releasing a bound run and
/// returning the base races whoever binds next, and only a private band makes that harmless
/// between test binaries.
fn free_ports(span: u16) -> u16 {
    for base in (33_100_u16..34_000).step_by(span as usize) {
        let bound: Vec<TcpListener> = (0..span)
            .filter_map(|at| TcpListener::bind(("127.0.0.1", base.checked_add(at)?)).ok())
            .collect();
        if bound.len() == span as usize {
            return base;
        }
    }
    panic!("no run of {span} consecutive free ports in 33,100–34,000, this file's own band");
}

fn wait_for_port(what: &str, port: u16, child: &mut Supervisor, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Some(status) = child.0.try_wait().expect("waiting on the child") {
            panic!("{what} exited with {status} instead of listening on {port}");
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not listen on {port} within {seconds}s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
