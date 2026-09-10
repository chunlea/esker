//! Two **processes**, one data directory — which until the engine took a claim was allowed.
//!
//! The engine's own tests prove the claim inside one process. This file proves the case the claim
//! was built for, and it is deliberately the case nothing else decides: the second store is given
//! a **different port**, so the bind cannot be what stops it. Before the claim, both processes
//! opened the same LSM tree and wrote their own WAL segments and manifests into it, for as long as
//! both were left running — which, with different ports, is for ever.
//!
//! The second test is the one that makes a claim safe in this system at all: a store this project
//! `SIGKILL`s fifty times in one acceptance run must be startable again afterwards, so the claim
//! has to die with the process holding it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// How long a store has to print its "listening" line before this gives up on it.
///
/// Generous for the reason `cluster.rs` gives its own budget: the store opens a database first,
/// and a loaded host makes that slower without making it wrong.
const LISTENING_WITHIN: Duration = Duration::from_secs(30);

/// A port nothing is listening on. Held only long enough to learn the number, like `cluster.rs`.
fn free_port() -> u16 {
    let socket = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    socket.local_addr().expect("its address").port()
}

/// A child that is killed however the test leaves.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `esker server` on `dir` and `port`, with its output going to `log`.
fn start(dir: &Path, port: u16, log: &Path) -> Server {
    let out = std::fs::File::create(log).expect("the server log");
    let errors = out.try_clone().expect("the server log");
    let child = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .arg("server")
        .arg("--data-dir")
        .arg(dir)
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--store-id")
        .arg("1")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errors))
        .spawn()
        .expect("the server command starts");
    Server(child)
}

/// Waits for the store's own "listening" line, and says so or says why not.
///
/// **The line and not the port**: a bare connect says somebody is listening, not that it is this
/// child — and here the whole question is which process holds the directory.
fn wait_until_listening(server: &mut Server, log: &Path) -> Result<(), String> {
    let deadline = Instant::now() + LISTENING_WITHIN;
    loop {
        let said = std::fs::read_to_string(log).unwrap_or_default();
        if said.contains("listening on") {
            return Ok(());
        }
        if let Some(status) = server.0.try_wait().expect("waiting on the server") {
            return Err(format!("the store exited with {status}; it said:\n{said}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the store never listened within {LISTENING_WITHIN:?}; it said:\n{said}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits for a child to exit, or says it did not.
fn wait_for_exit(
    server: &mut Server,
    within: Duration,
) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = server.0.try_wait().expect("waiting on the server") {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!("it was still running after {within:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "starts store processes; run with --run-ignored all"]
fn a_second_store_on_one_data_directory_refuses_to_start() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let first_log = dir.path().join("first.log");
    let second_log = dir.path().join("second.log");

    let mut first = start(&data, free_port(), &first_log);
    wait_until_listening(&mut first, &first_log).expect("the first store");

    // **A different port**, so nothing but the directory can refuse this.
    let mut second = start(&data, free_port(), &second_log);
    let status = wait_for_exit(&mut second, LISTENING_WITHIN).unwrap_or_else(|why| {
        panic!(
            "a second store is running on {} — {why}. This is two writers on one LSM tree, and \
             with different ports nothing else was ever going to stop it.",
            data.display()
        )
    });
    assert!(
        !status.success(),
        "the second store exited successfully, which is not a refusal"
    );
    let said = std::fs::read_to_string(&second_log).unwrap_or_default();
    assert!(
        said.contains("open in another process"),
        "the second store failed, but not as a claim on the directory; it said:\n{said}"
    );

    // And the holder is untouched by having refused somebody.
    assert!(
        first.0.try_wait().expect("waiting on the first").is_none(),
        "the first store exited while the second was being refused"
    );
}

#[test]
#[ignore = "starts store processes and kills one; run with --run-ignored all"]
fn a_sigkilled_store_leaves_its_data_directory_free() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let first_log = dir.path().join("first.log");
    let second_log = dir.path().join("second.log");
    let port = free_port();

    let mut first = start(&data, port, &first_log);
    wait_until_listening(&mut first, &first_log).expect("the first store");

    // `kill -9` by pid, for the reason `cluster_chaos.rs` gives: this workspace compiles no C, so
    // `kill(2)` is a process rather than a call.
    let pid = first.0.id();
    let killed = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("running `kill`");
    assert!(killed.success(), "`kill -9 {pid}` failed");
    first.0.wait().expect("reaping the killed store");

    // **The claim died with it.** A lock file left behind by a crash would turn the supervisor's
    // first restart into a node that can never start again — the acceptance run kills a store
    // fifty times.
    let mut second = start(&data, port, &second_log);
    wait_until_listening(&mut second, &second_log)
        .expect("a store restarting on the directory a killed one held");
}

/// `kill` has to exist for the second test to mean anything.
#[test]
fn the_kill_command_exists() {
    match Command::new("kill").arg("-0").arg("1").status() {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            panic!("`kill` is not on PATH, so the crash-release test would prove nothing")
        }
        Err(error) => panic!("running `kill`: {error}"),
    }
}
