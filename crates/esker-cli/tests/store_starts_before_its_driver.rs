//! #59, second face — a store started before its placement driver's port is up must wait for it.
//!
//! # The window, constructed rather than raced for
//!
//! `esker cluster start` waits for each driver to *answer* before it starts a store, so on a quiet
//! machine this window does not exist. Under load it does: two of three gates on 2026-09-11 failed
//! with `the store exited with exit status: 1 instead of listening on 33100`, at 2.4 s and 5.8 s,
//! in `cluster_pd_member_change` and `durability_pd_failover` — the same two tests that had been
//! green on every gate before.
//!
//! A test that reproduced it by making the machine busy would be a test that passes whenever the
//! machine is not. So this one **builds the window**: the store is started against a port nothing
//! is listening on, and the driver is not started until the store has *said* it is waiting.
//!
//! **The first version of this test held the driver back for two seconds and passed against the
//! broken rule** — a store opens its database before it dials, so on a warm machine the driver was
//! already listening by the time the dial happened, and the window under test never existed. A
//! constructed window still has to be observed rather than assumed: what makes this deterministic
//! is waiting for the line the store emits on the branch that decides to wait, which is proof the
//! dial happened and was survived.
//!
//! # What the narrow rule got wrong
//!
//! The store waited on a member that could not be dialled only **after** some other member had
//! answered — reasoning that `esker cluster start` orders the driver first, so a refused connection
//! must be a mistyped `--pd` and should fail in a second rather than in half a minute.
//!
//! It is also what a store sees when a driver's process is up and its port is not listening yet.
//! The two share a wire error and nothing inside the startup window separates them, so the narrow
//! reading was taken whenever the store won the race — and which way a race goes is decided by the
//! machine's load.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// How long to give the store to reach its placement-driver call and be refused.
///
/// **A deadline, not a delay.** Holding the driver back for a fixed two seconds is what the first
/// version of this test did, and it passed against the broken rule: a store opens its database
/// before it dials, so on a warm machine the driver was already listening by the time the dial
/// happened and the window under test never existed. The test now waits for the store to *say* it
/// is waiting, which is the event, and this is only the bound on how long that may take.
const REACHES_ITS_DRIVER_WITHIN: Duration = Duration::from_secs(30);

/// Generous, for the reason `data_dir_lock.rs` gives its own budget: the store opens a database
/// first, and a loaded host makes that slower without making it wrong.
const LISTENING_WITHIN: Duration = Duration::from_secs(60);

fn esker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_esker-cli")
}

fn free_port() -> u16 {
    let socket = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    socket.local_addr().expect("its address").port()
}

/// A child killed however the test leaves.
struct Supervised(Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(mut command: Command, log: &Path) -> Supervised {
    let out = std::fs::File::create(log).expect("a log file");
    let errors = out.try_clone().expect("a log file");
    Supervised(
        command
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the command starts"),
    )
}

fn said(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// **The store must come up.** It is started against a driver that does not exist yet.
#[test]
fn a_store_waits_for_a_driver_that_is_not_listening_yet() {
    let dir = TempDir::new().unwrap();
    let store_port = free_port();
    let driver_port = free_port();
    let store_log = dir.path().join("store.log");
    let driver_log = dir.path().join("driver.log");

    let mut store = Command::new(esker_cli());
    store
        .args(["server", "--data-dir"])
        .arg(dir.path().join("node-1"))
        .args(["--listen", &format!("127.0.0.1:{store_port}")])
        .args(["--store-id", "1"])
        .args(["--pd", &format!("127.0.0.1:{driver_port}")]);
    let mut store = spawn(store, &store_log);

    // Nothing is listening on the driver's port. Wait for the store to reach it, be refused, and
    // **say so** — that line is emitted only on the branch that decides to wait, so seeing it is
    // proof the dial happened and was survived. The failing alternative is the store exiting,
    // which is #59's second face and is what the narrow rule does here.
    let deadline = Instant::now() + REACHES_ITS_DRIVER_WITHIN;
    while !said(&store_log).contains("waiting for a placement driver") {
        if let Some(status) = store.0.try_wait().expect("waiting on the store") {
            panic!(
                "the store exited with {status} while its driver's port was not up yet, instead \
                 of waiting for it. That is #59's second face. What it said:\n{}",
                said(&store_log)
            );
        }
        assert!(
            Instant::now() < deadline,
            "the store never reached its placement driver within \
             {REACHES_ITS_DRIVER_WITHIN:?}, so this test never built the window it is about. \
             What it said:\n{}",
            said(&store_log)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut driver = Command::new(esker_cli());
    driver
        .args(["pd", "serve", "--data-dir"])
        .arg(dir.path().join("pd-1"))
        .args(["--listen", &format!("127.0.0.1:{driver_port}")])
        .args(["--id", "1"]);
    let _driver = spawn(driver, &driver_log);

    let deadline = Instant::now() + LISTENING_WITHIN;
    loop {
        if said(&store_log).contains("listening on") {
            break;
        }
        if let Some(status) = store.0.try_wait().expect("waiting on the store") {
            panic!(
                "the store exited with {status} after its driver arrived. It said:\n{}\n\
                 the driver said:\n{}",
                said(&store_log),
                said(&driver_log)
            );
        }
        assert!(
            Instant::now() < deadline,
            "the store never listened within {LISTENING_WITHIN:?}. It said:\n{}\n\
             the driver said:\n{}",
            said(&store_log),
            said(&driver_log)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
