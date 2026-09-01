//! `esker cluster start --pd` starts a cluster, not a fraction of one.
//!
//! The command spawns a placement driver and then every store, and each store's first act is to
//! ask that driver whether to bootstrap. Starting the driver first is not the same as waiting for
//! it: on a real four-node run, three stores exited with `connecting to 127.0.0.1:21264:
//! Connection refused` before the driver had printed its own "listening" line, and none of it was
//! visible — the children inherit the supervisor's stdio and the supervisor then blocks on a
//! signal, so the announcement ("4 nodes started", four pids) was printed and the failures behind
//! it were not flushed until the whole thing was stopped. What was left running was one store,
//! which registered, bootstrapped region 1 alone and heartbeated happily
//! (`docs/bench/columnar-learner.md`, "One more thing the real binaries said").
//!
//! So this asserts the thing the announcement claims, from the **driver's** side: four stores
//! registered. Nothing else in the suite does — `cluster_chaos.rs` starts a cluster without `--pd`
//! and asks the stores directly, which is exactly the shape that cannot see this.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Four, because that is the shape the defect was found in and the one the phase-8 story uses: a
/// columnar learner goes on the healthiest store *without* a peer, so three voters need a fourth.
const NODES: u64 = 4;

/// A run of `NODES + 1` free ports: the stores' own, and the driver's one above them.
fn free_port_run() -> u16 {
    let span = usize::try_from(NODES).unwrap() + 1;
    for base in (30_100_u16..40_000).step_by(span) {
        let bound: Vec<TcpListener> = (0..span)
            .filter_map(|at| {
                let offset = u16::try_from(at).ok()?;
                TcpListener::bind(("127.0.0.1", base.checked_add(offset)?)).ok()
            })
            .collect();
        if bound.len() == span {
            return base;
        }
    }
    panic!("no run of {} consecutive free ports", NODES + 1);
}

/// The supervisor, stopped however the test ends.
struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What `esker pd inspect` says about the driver's own directory.
fn inspect(pd_dir: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .arg("pd")
        .arg("inspect")
        .arg("--data-dir")
        .arg(pd_dir)
        .output()
        .expect("`pd inspect` runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every store the command started registers with the driver it started for them.
///
/// The assertion is `stores (4)` and not "four processes are alive", because a store that is alive
/// and has not registered is the same failure to anyone using the cluster — and because PD's view
/// is what every later operator is decided from.
///
/// **This is an outcome test and not a race-forcing one**, and it is worth being plain about that:
/// on a warm binary the driver binds in a few milliseconds and the stores would very likely have
/// won anyway. It says that a start which announces four nodes has four; the deterministic half of
/// the pair is [`a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster`], which removes the
/// timing from the question entirely, and the unit tests on `wait_until_listening`.
#[test]
fn a_four_node_cluster_with_a_driver_registers_four_stores() {
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();

    let supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_esker-cli"))
            .arg("cluster")
            .arg("start")
            .arg("--nodes")
            .arg(NODES.to_string())
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--base-port")
            .arg(base_port.to_string())
            .arg("--pd")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the cluster command starts"),
    );

    // Registration is the store's first act after opening, so this is seconds rather than the
    // minute a *placement* decision takes: nothing here waits on a region heartbeat.
    let pd_dir = data_dir.path().join("pd");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = String::new();
    while Instant::now() < deadline {
        seen = inspect(&pd_dir);
        if seen.contains(&format!("stores ({NODES})")) {
            drop(supervisor);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(supervisor);
    panic!(
        "the driver never saw {NODES} stores; `esker cluster start --pd` announced a cluster it \
         had not started. what it did see:\n{seen}"
    );
}

/// A driver that **cannot** listen fails the command, rather than leaving an announcement and four
/// stores that quietly die behind it.
///
/// The race made deterministic: its port is taken before the command runs, so the driver can never
/// bind, and every store is therefore guaranteed to lose. Without the wait, `cluster start` prints
/// "4 nodes started" with four pids, writes the state file, and blocks on a signal for ever, while
/// every one of those pids is already gone — which is precisely what happened on the run this was
/// written from, only with the driver arriving a moment late rather than never.
#[test]
fn a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster() {
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();
    let pd_port = base_port + u16::try_from(NODES).unwrap();

    // Held for the whole test: the driver's port belongs to somebody else.
    let _squatter = TcpListener::bind(("127.0.0.1", pd_port)).expect("the driver's port is free");

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
        .arg("--pd")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = Supervisor(command.spawn().expect("the cluster command starts"));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.0.try_wait().expect("waiting on the cluster command") {
            Some(status) => break status,
            None => assert!(
                Instant::now() < deadline,
                "`cluster start` is still running with a driver that can never listen: it \
                 announced a cluster and is supervising nothing"
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        !status.success(),
        "`cluster start` reported success with a driver that never listened"
    );
    assert!(
        !data_dir.path().join("cluster.state").exists(),
        "a cluster that never started left a state file, so `cluster stop` would signal pids that \
         belong to whatever the OS has reused them for"
    );
}
