//! **ADR 0108's acceptance**: kill the placement driver that was leading, and a statement still
//! returns.
//!
//! Every statement a SQL node runs starts by asking the placement driver for a timestamp
//! (`CLAUDE.md` invariant 6), so a node that cannot reach one runs nothing at all — not a `SELECT`,
//! not a `BEGIN` — and every session sees `08006`. `esker-coord/h1-driver-kill.md` §2 measured that
//! exposure as total, and it was, because every cluster this project could start had exactly one
//! driver.
//!
//! This is the test that says it is no longer total. Three drivers, one store, a real `esker-sql`
//! process, the wire spoken directly — and then `SIGKILL` to the member `esker pd members` says is
//! leading, with `--no-respawn` so it stays dead.
//!
//! # What makes it a real test rather than a slow one
//!
//! With one driver, the same arrangement can never go green: the thing that hands out timestamps
//! is gone and nothing brings it back. So the counterfactual is `--pd-nodes 1`, and it is not a
//! subtle one — no statement returns, ever, until the window runs out.
//!
//! # Why a window rather than "the very next statement"
//!
//! The surviving members have to *elect* before any of them can answer, and an election is
//! one to two seconds by configuration plus a pre-vote round. A client's own redirect budget is
//! shorter than that on purpose — a client that waited out every election would be a client that
//! never failed — so the honest claim is the one an operator cares about: within seconds of losing
//! the driver that was leading, statements are being answered again. The failure message carries
//! how long it actually took.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod port_band;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// One store: what is under test is the drivers, and one store is enough to make the SQL node's
/// backend a real `TcpStores` — which is the condition that makes a statement go all the way down.
const NODES: u16 = 1;

/// Three drivers: the smallest group that survives losing one.
const DRIVERS: u16 = 3;

/// How long a process gets to bind its port.
const STARTUP_SECONDS: u64 = 120;

/// How long after the kill a statement has to come back.
///
/// Generous against an election (one to two seconds) plus a loaded gate. What it is *not* is a
/// measurement: it is the bound on a run that has gone wrong, and the assertion prints the elapsed
/// time so a run that only just made it is visible rather than merely green.
const RECOVERY: Duration = Duration::from_secs(60);

#[test]
fn a_statement_returns_after_the_leading_driver_is_killed() {
    let data_dir = TempDir::new().unwrap();
    let base = free_ports(NODES + DRIVERS + 1);
    let store_port = base;
    let driver_ports: Vec<u16> = (0..DRIVERS).map(|at| base + NODES + at).collect();
    let sql_port = base + NODES + DRIVERS;
    warm(esker_cli());
    warm(esker_sql());

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
            // **The kill has to stay killed.** A respawned driver would make this test pass on a
            // cluster with no high availability at all: the node would simply wait for the one
            // member it knows to come back.
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
    let mut sql = Supervisor(
        Command::new(esker_sql())
            .arg(format!("127.0.0.1:{sql_port}"))
            .arg(format!("127.0.0.1:{store_port}"))
            // **The whole group.** A node given one member stops serving when that member is the
            // one that dies, which is the thing three of them exist to prevent.
            .args(["--pd", &endpoints.join(",")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`esker-sql` runs"),
    );
    wait_for_port("the SQL node", sql_port, &mut sql, STARTUP_SECONDS);

    // Before anything is killed, so a failure below is about the kill and not about the cluster.
    let before = one_query(sql_port, "SELECT 1");
    assert!(
        before.contains('D') && before.contains('C'),
        "the cluster could not answer a statement before the kill: {before:?}"
    );

    // **The member that is leading**, from the group's own answer rather than from a guess: any
    // member will say, which is why `pd members` exists.
    let leader = leading(&endpoints);
    let pid = pid_of(data_dir.path(), &leader);
    kill9(pid);

    let began = Instant::now();
    loop {
        let last = one_query(sql_port, "SELECT 1");
        if last.contains('D') && last.contains('C') {
            break;
        }
        assert!(
            began.elapsed() < RECOVERY,
            "no statement returned in {:?} after the driver that was leading ({leader}, pid \
             {pid}) was killed. The node still has {} other members and could not use either. \
             Last answer: {last:?}",
            began.elapsed(),
            endpoints.len() - 1
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    println!(
        "a statement returned {:?} after the leading driver was killed",
        began.elapsed()
    );

    // And it keeps answering: one lucky reply is not a node that has settled on a live member.
    for _ in 0..3 {
        let again = one_query(sql_port, "SELECT 1");
        assert!(
            again.contains('D') && again.contains('C'),
            "the node answered once and then stopped: {again:?}"
        );
    }
}

/// The address of the member `esker pd members` says is leading, asked at every endpoint until one
/// of them has an opinion.
///
/// **Matched against the endpoints this test started**, not read out of a column. The listing's
/// address field is *empty* for a member of a group of one — ADR 0061's "one member needs no
/// addresses" — so taking the second whitespace-separated field returns the word `voter`, silently,
/// and the test then goes looking for a pid for it. That is not hypothetical: it is what this
/// file's own counterfactual did on its first run.
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
            // A group of one names no address, and there is only one member it could be.
            if let [only] = endpoints {
                return only.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no placement driver named a leader among {endpoints:?} within a minute. Last              answer:\n{last}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The pid the state file records for `address`.
///
/// **By address**, because every driver's line has id `0` — which is the whole point of ADR 0108's
/// choice not to change this file's shape.
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
    // `/bin/kill` rather than `libc`, because this workspace compiles no C.
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("`kill` runs");
    assert!(status.success(), "could not kill pid {pid}");
}

/// Connects, completes the startup exchange, runs one query, and answers with the message tags
/// that came back. Raw bytes, like `pgwire_cluster.rs`, so this runs wherever the gate runs.
fn one_query(port: u16, sql: &str) -> String {
    let Ok(mut socket) = TcpStream::connect(("127.0.0.1", port)) else {
        return "<the SQL node would not accept a connection>".to_owned();
    };
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
    if socket.write_all(&packet).is_err() {
        return "<the startup packet could not be sent>".to_owned();
    }
    read_until_ready(&mut socket);

    let mut query = vec![b'Q'];
    query.extend_from_slice(&i32::try_from(sql.len() + 5).unwrap().to_be_bytes());
    query.extend_from_slice(sql.as_bytes());
    query.push(0);
    if socket.write_all(&query).is_err() {
        return "<the query could not be sent>".to_owned();
    }
    read_until_ready(&mut socket)
}

/// Reads whole messages until `ReadyForQuery`, answering with the tags seen. A closed socket ends
/// the read, so a dead session is a readable assertion rather than a timeout.
fn read_until_ready(socket: &mut TcpStream) -> String {
    let mut tags = String::new();
    loop {
        let mut header = [0_u8; 5];
        if socket.read_exact(&mut header).is_err() {
            return tags;
        }
        tags.push(header[0] as char);
        let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let body = usize::try_from(length - 4).unwrap_or(0);
        let mut rest = vec![0_u8; body];
        if socket.read_exact(&mut rest).is_err() {
            return tags;
        }
        if header[0] == b'Z' {
            return tags;
        }
    }
}

/// A child stopped however the test ends.
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

/// The `esker-sql` binary, beside this one in the same target directory.
///
/// `CARGO_BIN_EXE_` only names binaries of the crate under test, and `esker-sql` belongs to
/// another — so it is found by path, the way `pgwire_cluster.rs` finds it.
fn esker_sql() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_esker-cli"));
    path.pop();
    path.push("esker-sql");
    assert!(
        path.exists(),
        "{} is not built; this test needs the workspace's binaries",
        path.display()
    );
    path
}

/// Pays the first-execution cost before any clock starts.
fn warm(binary: impl AsRef<std::ffi::OsStr>) {
    let _unused = Command::new(binary)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A run of `span` consecutive free ports, from the one allocator every real-process test uses.
///
/// This file used to scan a fixed band of its own. See `tests/port_band/mod.rs` for why that
/// deterministically collided with the other tests in this same binary, and what four gates it
/// cost before anyone read the stderr.
fn free_ports(span: u16) -> u16 {
    port_band::reserve(span).into_base()
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
