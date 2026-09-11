//! A transaction older than the retention window keeps reading, on real processes.
//!
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md) makes the
//! garbage-collection safepoint `min(now − retention window, the oldest active read)`, and the
//! second half is the one this asserts: a node reports the oldest snapshot it has open, PD
//! publishes no higher, and a store therefore collects nothing the open transaction still needs.
//!
//! # Why it needs real processes
//!
//! Every part of the chain is a different process, and each one is where the design could be
//! wired wrong rather than written wrong: the node has to *attach* the reporter at startup, PD has
//! to keep the registry across heartbeats, and the store has to apply what PD publishes. Three
//! unit tests, all green, would say nothing about whether the binary ever calls
//! `reporting_reads_from`.
//!
//! # Why it needs a small window
//!
//! With PD's hour-long default, `now − retention` underflows to zero on a cluster that started a
//! second ago, and the safepoint would sit at zero whether the reader floor worked or not — a test
//! that passes for the wrong reason. `esker pd serve --retention-ms` is how the window is made
//! small enough to be the thing under test.
//!
//! # What the failure looks like
//!
//! Not a wrong answer: the read is **refused**, by ADR 0110's decision 5, with the sentence that
//! names both numbers. So this test does not need the transaction's `start_ts` — it asks the
//! transaction to read again and requires an answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod port_band;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Small enough that a few seconds is several windows, and large enough that it is not zero.
const RETENTION_MS: u64 = 1_000;

/// How long the transaction stays open.
///
/// **Past a store heartbeat, not merely past the window.** PD can publish a safepoint every second
/// and it changes nothing until a store asks for it, which it does on its 10-second beat — so a
/// five-second test was green with the reader floor deliberately broken, because the store had
/// never heard a safepoint at all. The number that matters here is the store's cadence.
const OPEN_FOR: Duration = Duration::from_secs(15);

fn esker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_esker-cli")
}

/// The SQL node's binary, beside this one — see `tests/pgwire_cluster.rs` for why it is located
/// rather than built here: a stale one reports a bug the fix already removed.
fn esker_sql() -> std::path::PathBuf {
    if let Ok(given) = std::env::var("ESKER_SQL_BIN") {
        return std::path::PathBuf::from(given);
    }
    let beside = Path::new(esker_cli())
        .parent()
        .expect("the test binary has a directory")
        .join("esker-sql");
    assert!(
        beside.exists(),
        "no esker-sql binary at {} — the gate builds `--bins` before the tests",
        beside.display()
    );
    beside
}

/// A child killed however the test leaves.
struct Child_(Child);

impl Drop for Child_ {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_port(what: &str, port: u16, child: &mut Child_, seconds: u64, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Some(status) = child.0.try_wait().expect("waiting on the child") {
            // **Its own words, not just its exit code.** A store or a node that refuses to start
            // says why on stderr, and a test that reports only `exit status: 101` makes the
            // reader go and find the log — see `#59`, which was readable at a glance only because
            // `cluster_pd_group.rs` kept its children's output.
            let said = std::fs::read_to_string(log).unwrap_or_default();
            panic!("{what} exited with {status} instead of listening on {port}. It said:\n{said}");
        }
        assert!(Instant::now() < deadline, "{what} never listened on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One pgwire connection, kept open — which is the whole point: a transaction lives in a session.
struct Session(TcpStream);

impl Session {
    fn connect(port: u16) -> Self {
        let socket = TcpStream::connect(("127.0.0.1", port)).expect("the SQL node accepts");
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut session = Self(socket);

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
        session.0.write_all(&packet).unwrap();
        session.read_until_ready();
        session
    }

    /// Runs one statement and answers with everything the server said — tags, and the text of any
    /// error, because an error's sentence is the whole evidence when this test fails.
    fn run(&mut self, sql: &str) -> String {
        let mut query = vec![b'Q'];
        query.extend_from_slice(&i32::try_from(sql.len() + 5).unwrap().to_be_bytes());
        query.extend_from_slice(sql.as_bytes());
        query.push(0);
        self.0.write_all(&query).unwrap();
        self.read_until_ready()
    }

    fn read_until_ready(&mut self) -> String {
        let mut said = String::new();
        loop {
            let mut header = [0_u8; 5];
            if self.0.read_exact(&mut header).is_err() {
                return said;
            }
            let tag = header[0] as char;
            said.push(tag);
            let length = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let body = usize::try_from(length - 4).unwrap_or(0);
            let mut rest = vec![0_u8; body];
            if self.0.read_exact(&mut rest).is_err() {
                return said;
            }
            if tag == 'E' {
                use std::fmt::Write as _;
                let _ = write!(
                    said,
                    " [{}]",
                    String::from_utf8_lossy(&rest).replace('\0', " ").trim()
                );
            }
            if tag == 'Z' {
                return said;
            }
        }
    }
}

fn spawn(command: &mut Command, log: &Path) -> Child_ {
    let out = std::fs::File::create(log).expect("a log file");
    let err = out.try_clone().expect("the same file twice");
    Child_(
        command
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("the process starts"),
    )
}

/// The three processes, plus the second node that makes the test able to fail.
struct Cluster {
    _dir: TempDir,
    _driver: Child_,
    _store: Child_,
    _node: Child_,
    _other: Child_,
    sql_port: u16,
    store_port: u16,
}

/// **The acceptance ADR 0110 is built for**, end to end and on real processes.
#[test]
fn a_transaction_older_than_the_window_still_reads() {
    let cluster = start_cluster();
    let (sql_port, store_port) = (cluster.sql_port, cluster.store_port);
    read_across_the_window(sql_port, store_port);
}

fn start_cluster() -> Cluster {
    let dir = TempDir::new().unwrap();
    let ports = port_band::reserve(4);
    let (pd_port, store_port, sql_port, other_port) =
        (ports.at(0), ports.at(1), ports.at(2), ports.at(3));
    let base = ports.into_base();
    assert_eq!(base, pd_port);

    let mut driver = spawn(
        Command::new(esker_cli())
            .args(["pd", "serve", "--data-dir"])
            .arg(dir.path().join("pd"))
            .args(["--listen", &format!("127.0.0.1:{pd_port}")])
            // **The flag this test exists around.** At the hour-long default the window is zero on
            // a cluster this young and the reader floor would never be what is being measured.
            .args(["--retention-ms", &RETENTION_MS.to_string()]),
        &dir.path().join("pd.log"),
    );
    wait_for_port(
        "the placement driver",
        pd_port,
        &mut driver,
        30,
        &dir.path().join("pd.log"),
    );

    let mut store = spawn(
        Command::new(esker_cli())
            .args(["server", "--data-dir"])
            .arg(dir.path().join("store"))
            .args(["--listen", &format!("127.0.0.1:{store_port}")])
            .args(["--store-id", "1"])
            .args(["--pd", &format!("127.0.0.1:{pd_port}")]),
        &dir.path().join("store.log"),
    );
    wait_for_port(
        "the store",
        store_port,
        &mut store,
        30,
        &dir.path().join("store.log"),
    );

    let mut node = spawn(
        Command::new(esker_sql())
            .arg(format!("127.0.0.1:{sql_port}"))
            .arg(format!("127.0.0.1:{store_port}"))
            .args(["--pd", &format!("127.0.0.1:{pd_port}")]),
        &dir.path().join("sql.log"),
    );
    wait_for_port(
        "the SQL node",
        sql_port,
        &mut node,
        30,
        &dir.path().join("sql.log"),
    );

    // **A second node, and it is what makes this test able to fail.** With one node, taking its
    // reporting away empties PD's registry — and an empty registry does not advance the safepoint
    // at all, so the read below would succeed for a *different* rule and the test would prove
    // nothing. A second node keeps the registry non-empty with "nothing open", which is exactly
    // the state where the window applies and a missing reader floor would step over the
    // transaction. The unit test for the TTL needed a second reporter for the same reason.
    let mut other = spawn(
        Command::new(esker_sql())
            .arg(format!("127.0.0.1:{other_port}"))
            .arg(format!("127.0.0.1:{store_port}"))
            .args(["--pd", &format!("127.0.0.1:{pd_port}")]),
        &dir.path().join("sql-other.log"),
    );
    wait_for_port(
        "the second SQL node",
        other_port,
        &mut other,
        30,
        &dir.path().join("sql-other.log"),
    );

    Cluster {
        _dir: dir,
        _driver: driver,
        _store: store,
        _node: node,
        _other: other,
        sql_port,
        store_port,
    }
}

fn read_across_the_window(sql_port: u16, store_port: u16) {
    let mut writer = Session::connect(sql_port);
    let made = writer.run("CREATE TABLE t (id bigint primary key, v text)");
    assert!(!made.contains('E'), "creating the table failed: {made}");
    let inserted = writer.run("INSERT INTO t VALUES (1, 'before')");
    assert!(!inserted.contains('E'), "the insert failed: {inserted}");

    // **The long read.** Its snapshot is taken here and must survive everything below.
    let mut reader = Session::connect(sql_port);
    // **REPEATABLE READ, and that is the whole test.** `BEGIN` alone is READ COMMITTED, where
    // every statement takes a fresh snapshot — so the second read below would ask at a timestamp
    // seconds old at most and no safepoint could ever be above it. The first version of this test
    // did exactly that and was green with the reader floor deliberately broken. One snapshot for
    // the transaction is what makes "the oldest active read" a thing the cluster has to protect.
    let began = reader.run("BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert!(
        !began.contains('E'),
        "BEGIN REPEATABLE READ failed: {began}"
    );
    let first = reader.run("SELECT v FROM t");
    assert!(
        first.contains('D') && !first.contains('E'),
        "the transaction's first read did not answer: {first}"
    );

    // Time passes — several retention windows — and the cluster keeps working, so `now` moves and
    // a safepoint that ignored the open read would climb past its snapshot.
    let until = Instant::now() + OPEN_FOR;
    let mut wrote = 1;
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(250));
        wrote += 1;
        let inserted = writer.run(&format!("INSERT INTO t VALUES ({wrote}, 'after')"));
        assert!(!inserted.contains('E'), "a write failed: {inserted}");
    }

    // **The denominator.** A safepoint that never moved makes every assertion below pass for the
    // emptiest possible reason — and it did: at five seconds this test was green with the reader
    // floor deliberately broken, because the store's ten-second beat had not yet asked PD for a
    // number. So the run says what the store is actually working to, and fails if it is nothing.
    let in_force = Command::new(esker_cli())
        .args(["admin", "gc", "--safepoint", "0", "--store"])
        .arg(format!("127.0.0.1:{store_port}"))
        .output()
        .expect("`admin gc` runs");
    let said = String::from_utf8_lossy(&in_force.stdout).to_string();
    eprintln!("  {}", said.lines().next().unwrap_or("(nothing)"));
    let published: u64 = said
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse().ok())
        .unwrap_or(0);
    assert!(
        published > 0,
        "the store is still working to a safepoint of zero after {OPEN_FOR:?}, so nothing below \
         is being tested: {said}"
    );

    // **And the transaction reads again.** Refused is the failure ADR 0110 decision 5 produces,
    // and it says both numbers — so the assertion needs no timestamp of its own.
    let again = reader.run("SELECT v FROM t");
    assert!(
        again.contains('D') && !again.contains('E'),
        "a transaction open for {OPEN_FOR:?} — {} retention windows — could not read: {again}",
        OPEN_FOR.as_millis() / u128::from(RETENTION_MS),
    );
    let ended = reader.run("COMMIT");
    assert!(!ended.contains('E'), "COMMIT failed: {ended}");
}
