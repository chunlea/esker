//! The whole thing, from a shell: `esker cluster start --pd`, a SQL node over it, and `psql`
//! asking a question a columnar learner answers.
//!
//! `docs/plans/phase-10-routing.md` U6. Every other test of this feature runs the pieces in one
//! process — the joint gate, the differential, the planner's own tests — which is the right shape
//! for asserting behaviour and the wrong one for asserting that a person can reach it. This is the
//! only test in the repository where the path is the one an operator takes: two binaries started
//! from a command line, a client nobody here wrote, and no Rust between them.
//!
//! It is what closes phase 8's *"nothing on a real cluster can ask a fragment"* at the level a
//! release note would claim it.
//!
//! # `#[ignore]`d, and why that is not a way of hiding it
//!
//! It starts six processes, waits on a placement decision, and takes the better part of a minute
//! when nothing is wrong. The in-process form of the same claim runs in the gate every time
//! (`esker-sql/tests/routing_differential.rs`, `a_real_cluster_answers_a_select_from_its_columnar_learner`),
//! so what is `#[ignore]`d here is the *shell*, not the assertion.
//!
//! # Two facts about this machine it has to know
//!
//! **Gatekeeper.** macOS evaluates a freshly linked binary on its first execution, at 0% CPU in
//! `_dyld_start`, and on a loaded box that has taken tens of seconds — long enough to eat a
//! deadline meant for the cluster (`cluster_start.rs`, which paid it three times in one session).
//! Both binaries are warmed with `--help` before any clock starts.
//!
//! **The `esker-sql` binary is another package's.** `CARGO_BIN_EXE_*` exists only for binaries of
//! the package the test is in, so it is found beside this one in the same target directory — which
//! is where cargo puts every binary of a workspace build, and is the only way a test can drive two
//! packages' binaries at once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Four stores: a columnar learner goes on the healthiest store **without** a peer, so three
/// voters need a fourth or PD has nowhere to put one.
const NODES: u64 = 4;

/// Rows the table holds, and therefore what `count(*)` must answer.
const ROWS: i64 = 20;

#[test]
#[ignore = "starts six processes and waits on a placement decision; the in-process form of this \
            claim runs in the gate"]
fn psql_asks_a_question_a_columnar_learner_answers() {
    let Some(psql) = psql() else {
        eprintln!("skipping: no psql on this machine");
        return;
    };

    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();
    let pd_port = base_port + u16::try_from(NODES).unwrap();
    let sql_port = pd_port + 1;
    warm(esker_cli());
    warm(esker_sql());

    let _cluster = Supervisor(
        Command::new(esker_cli())
            .args(["cluster", "start", "--nodes", &NODES.to_string()])
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["--base-port", &base_port.to_string(), "--pd"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`esker cluster start` runs"),
    );

    // Registration is the store's first act after opening, so this is seconds. Asserted from the
    // **driver's** side, because a store that is alive and has not registered is the same failure
    // to anyone using the cluster (`cluster_start.rs`).
    let pd_dir = data_dir.path().join("pd-1");
    wait_for("the driver to see four stores", 60, || {
        inspect(&pd_dir).contains(&format!("stores ({NODES})"))
    });

    let stores: Vec<String> = (1..=NODES)
        .map(|id| format!("127.0.0.1:{}", base_port + u16::try_from(id).unwrap() - 1))
        .collect();
    let _sql = Supervisor(
        Command::new(esker_sql())
            .arg(format!("127.0.0.1:{sql_port}"))
            .args(&stores)
            .args(["--pd", &format!("127.0.0.1:{pd_port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`esker-sql` runs"),
    );
    wait_for("the SQL node to listen", 60, || {
        std::net::TcpStream::connect(("127.0.0.1", sql_port)).is_ok()
    });

    // The table, its rows, and the flag that asks for a columnar copy. `settle` rather than a bare
    // run: a cluster this young is still electing, and a statement that meets a leadership gap is
    // retried rather than failed.
    let sql = |statement: &str| run_all(&psql, sql_port, statement);
    settle(
        &psql,
        sql_port,
        "CREATE TABLE t (id int8 PRIMARY KEY, region text, amount int8)",
    );
    for id in 1..=ROWS {
        let region = if id % 3 == 0 { "north" } else { "south" };
        settle(
            &psql,
            sql_port,
            &format!("INSERT INTO t VALUES ({id}, '{region}', {})", id * 5),
        );
    }
    settle(&psql, sql_port, "ALTER TABLE t SET (columnar_replicas = 1)");

    // **Wait on the answer, never on a sleep.** A learner that exists and has not caught up
    // refuses, and the honest observable for "the columnar copy is usable" is that it answered.
    let deadline = Instant::now() + Duration::from_secs(120);
    let last = loop {
        let plan = sql("EXPLAIN ANALYZE SELECT count(*) FROM t");
        if plan.contains("Fragments: 1 asked, 1 answered") {
            break plan;
        }
        assert!(
            Instant::now() < deadline,
            "the columnar learner never answered a fragment. the last plan was:\n{plan}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };

    // What an operator would type, and what they would see.
    let plan = sql("EXPLAIN SELECT count(*) FROM t");
    assert!(
        plan.contains("Columnar Aggregate on t"),
        "EXPLAIN does not name the engine:\n{plan}"
    );
    assert!(plan.contains("Engine: columnar"), "{plan}");

    let counted = sql("SELECT count(*) FROM t");
    assert_eq!(
        counted.trim(),
        ROWS.to_string(),
        "the fragment answered {counted}, not {ROWS}. the plan that ran was:\n{last}"
    );

    // And the override reaches all the way out to a shell.
    let forced = run_all(
        &psql,
        sql_port,
        "SET esker.engine = 'row'; EXPLAIN SELECT count(*) FROM t",
    );
    assert!(
        forced.contains("Engine: rows") && forced.contains("esker.engine = 'row'"),
        "the session override did not reach the plan:\n{forced}"
    );
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

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

/// The SQL node's binary, beside this one.
///
/// `CARGO_BIN_EXE_esker-sql` does not exist here — cargo defines it only for binaries of the
/// package the test belongs to — and a workspace build puts every binary in the same directory, so
/// the sibling path is both correct and the only thing available.
fn esker_sql() -> PathBuf {
    Path::new(esker_cli())
        .parent()
        .expect("the test binary has a directory")
        .join("esker-sql")
}

/// `psql`, if this machine has one. A missing client is a fact about the machine.
fn psql() -> Option<String> {
    Command::new("psql")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|_| "psql".to_owned())
}

/// Runs a binary once and throws the result away, so the first-execution cost is paid before any
/// deadline starts. See the module docs.
fn warm(binary: impl AsRef<std::ffi::OsStr>) {
    let _unused = Command::new(binary)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A run of `NODES + 2` free ports: the stores', the driver's, and the SQL node's.
fn free_port_run() -> u16 {
    let span = usize::try_from(NODES).unwrap() + 2;
    for base in (41_000_u16..50_000).step_by(span) {
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
    panic!("no run of {} consecutive free ports", NODES + 2);
}

fn inspect(pd_dir: &Path) -> String {
    let out = Command::new(esker_cli())
        .args(["pd", "inspect", "--data-dir"])
        .arg(pd_dir)
        .output()
        .expect("`pd inspect` runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_for(what: &str, seconds: u64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One `;`-separated script through `psql`, tuples only and unaligned — the shape a script reads.
///
/// `psql -c` runs the whole thing in one transaction, which is what makes
/// `SET esker.engine = 'row'; EXPLAIN …` a single session's two statements rather than two
/// sessions' one each.
fn run_all(psql: &str, port: u16, script: &str) -> String {
    let out = Command::new(psql)
        .args(["-h", "127.0.0.1", "-p", &port.to_string()])
        .args(["-U", "esker", "-d", "esker", "-tAX", "-c", script])
        .output()
        .expect("`psql` runs");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text
}

/// A statement that has to succeed, retried through the leadership gap a young cluster produces.
///
/// **Only the transient failures**, and a duplicate counts as success: a retry of an idempotent
/// statement whose first answer was lost says "already exists", and treating that as a failure to
/// wait out is how a helper spends its whole deadline on a statement that worked
/// (`esker-sql/tests/routing_differential.rs` learned this the hard way).
fn settle(psql: &str, port: u16, statement: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = run_all(psql, port, statement);
        if !out.contains("ERROR") || out.contains("already exists") || out.contains("duplicate key")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "`{statement}` never settled. the last answer was:\n{out}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}
