//! One statement over pgwire, against a **real cluster of stores**.
//!
//! # Why this test exists, and why it speaks the protocol itself
//!
//! Every connection to a SQL node backed by real stores completed its startup burst and then died:
//!
//! ```text
//! thread 'tokio-rt-worker' panicked at tokio/.../multi_thread/mod.rs:91:9:
//! Cannot start a runtime from within a runtime.
//! ```
//!
//! `Session::run` called `Executors::for_session` directly on a tokio worker. Against real stores
//! that begins a transaction and reads the catalog — `StoreTxn` → `Router` → `TcpStores` →
//! `BlockingTransport::call` → `Runtime::block_on` — and building a runtime inside
//! `#[tokio::main]`'s is a panic. It dated from `0510b44e`, the commit that made the startup packet
//! select the database, and **v1.0.0 shipped with it**: no client could run one statement against a
//! real cluster.
//!
//! **The same class of bug, in the same binary, already had a comment on it.** `bin/esker-sql.rs`,
//! where the client is built:
//!
//! > Onto a blocking thread, because connecting is a **synchronous** client building its own
//! > runtime and this function is inside `#[tokio::main]`'s. Doing it here panicked with "Cannot
//! > start a runtime from within a runtime" — on the first line of every node started against real
//! > stores, which is the one path no test took until this phase started one from a shell.
//!
//! "The one path no test took." It still was not taken: the test that starts a cluster, starts the
//! SQL node and runs statements (`columnar_cluster.rs`) is `#[ignore]`d **and** skips when the
//! machine has no `psql` — and the gate container has none, so it printed `skipping` and passed.
//! Coverage that exists and cannot run is what let this reach a release.
//!
//! So this test **cannot skip**: it is not ignored, and it speaks the wire itself rather than
//! shelling out to a client that may not be installed. One store rather than four, because one is
//! enough to make the backend a real `TcpStores` and that is the whole condition.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

#[test]
fn a_client_runs_one_statement_against_a_real_cluster() {
    let data_dir = TempDir::new().unwrap();
    let base = free_ports(3);
    let (store_port, pd_port, sql_port) = (base, base + 1, base + 2);
    warm(esker_cli());
    warm(esker_sql());

    let _cluster = Supervisor(
        Command::new(esker_cli())
            .args(["cluster", "start", "--nodes", "1", "--data-dir"])
            .arg(data_dir.path())
            .args(["--base-port", &store_port.to_string(), "--pd"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the cluster starts"),
    );
    wait_for("the store to listen", 60, || {
        TcpStream::connect(("127.0.0.1", store_port)).is_ok()
    });
    wait_for("the driver to listen", 60, || {
        TcpStream::connect(("127.0.0.1", pd_port)).is_ok()
    });

    let _sql = Supervisor(
        Command::new(esker_sql())
            .arg(format!("127.0.0.1:{sql_port}"))
            .arg(format!("127.0.0.1:{store_port}"))
            .args(["--pd", &format!("127.0.0.1:{pd_port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`esker-sql` runs"),
    );
    wait_for("the SQL node to listen", 60, || {
        TcpStream::connect(("127.0.0.1", sql_port)).is_ok()
    });

    // **The assertion is that a statement answers at all.** Before the fix the connection got its
    // whole startup burst — `AuthenticationOk`, the `ParameterStatus` run, `BackendKeyData`,
    // `ReadyForQuery` — and then the socket was reset when the worker panicked, so a test that
    // checked only that the port accepted a connection would have passed.
    let answer = one_query(sql_port, "SELECT 1");
    assert!(
        answer.contains('D'),
        "no DataRow came back; the session died after startup: {answer:?}"
    );
    assert!(
        answer.contains('C'),
        "no CommandComplete came back: {answer:?}"
    );
}

/// Connects, completes the startup exchange, runs one query, and answers with the message tags
/// that came back.
///
/// Raw bytes rather than a client library: this crate has no pgwire client and the point of the
/// test is that it runs anywhere the gate runs.
fn one_query(port: u16, sql: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("the SQL node accepts");
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608_i32.to_be_bytes()); // protocol 3.0
    for (key, value) in [("user", "esker"), ("database", "esker")] {
        startup.extend_from_slice(key.as_bytes());
        startup.push(0);
        startup.extend_from_slice(value.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut packet = Vec::new();
    packet.extend_from_slice(&i32::try_from(startup.len() + 4).unwrap().to_be_bytes());
    packet.extend_from_slice(&startup);
    socket.write_all(&packet).unwrap();
    read_until_ready(&mut socket);

    let mut query = vec![b'Q'];
    query.extend_from_slice(&i32::try_from(sql.len() + 5).unwrap().to_be_bytes());
    query.extend_from_slice(sql.as_bytes());
    query.push(0);
    socket.write_all(&query).unwrap();
    read_until_ready(&mut socket)
}

/// Reads whole messages until `ReadyForQuery`, answering with the tags seen.
///
/// A closed socket ends the read: that is what the panic looked like from the client's side, and
/// returning what arrived rather than blocking is what makes the failure a readable assertion
/// instead of a thirty-second timeout.
fn read_until_ready(socket: &mut TcpStream) -> String {
    let mut tags = String::new();
    loop {
        let mut header = [0_u8; 5];
        if socket.read_exact(&mut header).is_err() {
            return tags;
        }
        let tag = header[0] as char;
        tags.push(tag);
        let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let body = usize::try_from(length - 4).unwrap_or(0);
        let mut rest = vec![0_u8; body];
        if socket.read_exact(&mut rest).is_err() {
            return tags;
        }
        // **An `ErrorResponse` carries its own sentence, so say it.** A failure reported as the
        // single letter `E` is a test that knows something is wrong and refuses to say what.
        if tag == 'E' {
            let text = String::from_utf8_lossy(&rest)
                .split('\0')
                .filter(|field| field.len() > 1)
                .map(|field| field[1..].to_owned())
                .collect::<Vec<_>>()
                .join(" | ");
            tags.push_str(&format!("<{text}>"));
        }
        if tag == 'Z' {
            return tags;
        }
    }
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

/// The SQL node's binary, beside this one — **built first, because nothing else builds it.**
///
/// `CARGO_BIN_EXE_esker-sql` exists only for binaries of this test's own package, so the path is
/// the sibling in the same target directory. Cargo will not have put a *current* one there:
/// `cargo nextest run -p esker-cli` builds this package's test targets and not another package's
/// binary, so whatever is on disk may be any age at all.
///
/// The first red run of this test was against a binary twelve minutes stale — it reported the very
/// bug the fix had already removed, which is the most misleading way a test can fail. Building it
/// here costs a no-op cargo invocation when it is fresh and makes the test unable to lie about
/// which code it exercised.
fn esker_sql() -> PathBuf {
    let built = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(["build", "-p", "esker-sql", "--bin", "esker-sql"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert!(
        built.is_ok_and(|status| status.success()),
        "the esker-sql binary must build before this test can drive it"
    );
    Path::new(esker_cli())
        .parent()
        .expect("the test binary has a directory")
        .join("esker-sql")
}

/// macOS evaluates a freshly linked binary on its first execution, at 0% CPU in `_dyld_start`, and
/// on a loaded box that has eaten deadlines meant for the cluster. Both binaries are warmed before
/// any clock starts.
fn warm(binary: impl AsRef<std::ffi::OsStr>) {
    let _unused = Command::new(binary)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn free_ports(span: u16) -> u16 {
    for base in (41_000_u16..50_000).step_by(usize::from(span) + 1) {
        let bound: Vec<TcpListener> = (0..span)
            .filter_map(|at| TcpListener::bind(("127.0.0.1", base.checked_add(at)?)).ok())
            .collect();
        if bound.len() == usize::from(span) {
            return base;
        }
    }
    panic!("no run of {span} consecutive free ports");
}

fn wait_for(what: &str, seconds: u64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
