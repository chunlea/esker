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

/// Held for the length of each test in this file, so the two never overlap.
///
/// **They cannot share a port space and one of them squats a port on purpose.** `free_port_run`
/// binds a run, releases it and returns the base, which is the usual trick and is a race between
/// the release and the cluster's own bind — harmless against other test binaries **only because
/// they scan other bands**, which [`free_port_run`] now makes true and did not, and not harmless
/// against the test next door. Run in parallel under a loaded box they
/// picked the same base, [`a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster`]'s squatter
/// took the *other* test's driver port, and the four-node start failed with a placement driver
/// that could never listen: `stores (0)`, `(not bootstrapped)`. Which is, to be fair, the failure
/// this file is about — arriving from the wrong direction.
static PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs the binary once, and throws the result away.
///
/// **macOS charges for the first execution of a freshly linked binary.** `syspolicyd` evaluates it
/// — Gatekeeper, notarisation, the provenance check — while the process sits at `_dyld_start` at
/// 0% CPU, and on a loaded box that has taken tens of seconds. This test spawns the binary as a
/// subprocess and then gives the cluster a sixty-second budget to register four stores, so on the
/// first run after a rebuild the two overlap and the budget pays for the evaluation. Seen three
/// times in one session, every time at 60.2 s and every time on the run right after a build, with
/// the same command passing in **0.33 s** immediately afterwards.
///
/// Paying it here, before the clock starts, is the whole fix: the second execution is free, so the
/// budget measures the cluster rather than the operating system. An invocation that fails is
/// ignored on purpose — this is a warm-up and not an assertion, and
/// [`the_kill_command_exists`](super) is the file that checks the tools are there.
fn warm_the_binary() {
    let _unused = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A run of `NODES + 1` free ports: the stores' own, and the driver's one above them.
///
/// # The band is this file's alone, and it was not
///
/// Binding a run, releasing it and returning the base is a race against whoever binds next, and
/// [`PORTS`] closes it only for the *other test in this file* — a `Mutex` is process-local, and
/// every other cluster test is another binary. So the band has to be the mitigation, and each of
/// these four scans a different one:
///
/// | test | band |
/// |---|---|
/// | `cluster_chaos` | 21,000–30,000 |
/// | **this file** | **30,100–31,000** |
/// | `tier_acceptance` | 31,000–39,000 |
/// | `columnar_cluster` | 41,000–50,000 |
///
/// This one used to scan **30,100–40,000**, which swallows `tier_acceptance`'s whole band. That
/// one is `#[ignore]`d and so does not run in the gate — it needs a `MinIO` container — which is
/// why this is a trap rather than a diagnosis: run its three tests beside a workspace run, which
/// is exactly what somebody checking phase 6b does, and the failure lands *here*, as a placement
/// driver that could never listen and a four-node start that announced four nodes and has
/// `stores (0)`. Sixty seconds later, in another crate, with nothing pointing back
/// (`docs/plans/phase-14-flakes.md` U3).
///
/// A band that is exhausted panics by name. That is the right failure: it says the ports ran out,
/// where a collision says nothing at all.
fn free_port_run() -> u16 {
    let span = usize::try_from(NODES).unwrap() + 1;
    for base in (30_100_u16..31_000).step_by(span) {
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
    panic!(
        "no run of {} consecutive free ports in 30,100–31,000, this file's own band",
        NODES + 1
    );
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
/// timing from the question entirely, and the unit tests on `wait_until_the_driver_answers`.
#[test]
fn a_four_node_cluster_with_a_driver_registers_four_stores() {
    let _ports = PORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();
    warm_the_binary();

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

/// A **node** that cannot listen fails the command too, which is the same rule reached from the
/// other side of the start.
///
/// The driver's port is left free here and node 1's is taken, so the driver comes up, every store
/// is spawned, and one of them can never bind. Before the announcement waited for the stores to
/// *answer*, what decided this was a 250 ms sleep and a "has anybody died yet" — so the outcome
/// depended on whether node 1's `Address already in use` landed inside that window. Measured at
/// the base of this change: 0.36 s and a correct refusal on a quiet box, and at eighty busy
/// threads `4 nodes started`, a state file naming a dead pid, and a command that never returned.
///
/// The state-file assertion is the one that matters most: `stop` reads those pids and signals
/// them, and a pid the operating system has since reused belongs to something else entirely.
#[test]
fn a_node_that_cannot_listen_is_a_failure_and_not_a_cluster() {
    let _ports = PORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();

    // Held for the whole test: node 1's port belongs to somebody else. The driver's, one above
    // the last node, is free — this test is about the stores.
    let _squatter = TcpListener::bind(("127.0.0.1", base_port)).expect("node 1's port is free");

    warm_the_binary();
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
    // Sixty seconds is this file's wedge-detector, not the assertion: the command settles in
    // about a second at every load measured, and the four points are in the doc above.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.0.try_wait().expect("waiting on the cluster command") {
            Some(status) => break status,
            None => assert!(
                Instant::now() < deadline,
                "`cluster start` is still running with a node that can never listen: it \
                 announced a cluster and is supervising less than one"
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        !status.success(),
        "`cluster start` reported success with a node that never listened"
    );
    assert!(
        !data_dir.path().join("cluster.state").exists(),
        "a cluster that never started left a state file, so `cluster stop` would signal pids that \
         belong to whatever the OS has reused them for"
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
    let _ports = PORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();
    let pd_port = base_port + u16::try_from(NODES).unwrap();

    // Held for the whole test: the driver's port belongs to somebody else.
    let _squatter = TcpListener::bind(("127.0.0.1", pd_port)).expect("the driver's port is free");

    // Under nextest each test is its own process, so the other one's warm-up does not help.
    warm_the_binary();
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
