//! **The rehearsal for `CLAUDE.md` invariant 1**, and the negative control that makes it evidence.
//!
//! `esker durability record|chaos|verify` proves that no acknowledged write is lost when a store is
//! killed. Before that can be run on the real topology it has to be shown to *work* — and the half
//! of "work" nobody checks is that the checker can go **red**. A checker that has never failed is a
//! checker nobody has tested, and this file is where it fails on purpose.
//!
//! What this is not: the acceptance. Three store processes on one host, killed with `SIGKILL`, is
//! the shape; the real topology is four stores, real Raft over a real network, and r1's cluster.
//! `esker-coord/h1-kill9-plan.md` says which parts transfer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const NODES: usize = 3;

/// A run of `NODES + 1` consecutive ports — the stores, and the placement driver above them.
fn reserve() -> (u16, Vec<TcpListener>) {
    const LOW: u16 = 31_000;
    const HIGH: u16 = 39_000;
    let stride = u16::try_from(NODES).unwrap() + 2;
    let slots = (HIGH - LOW) / stride;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let start = (nanos ^ std::process::id()) % u32::from(slots);
    for step in 0..slots {
        let slot = (start + u32::from(step)) % u32::from(slots);
        let base = LOW + u16::try_from(slot).unwrap() * stride;
        let held: Vec<TcpListener> = (0..=u16::try_from(NODES).unwrap())
            .map(|offset| TcpListener::bind(("127.0.0.1", base + offset)))
            .collect::<Result<_, _>>()
            .unwrap_or_default();
        if held.len() == NODES + 1 {
            return (base, held);
        }
    }
    panic!(
        "no free run of {} ports between {LOW} and {HIGH}",
        NODES + 1
    );
}

/// The supervisor, killed when this drops so a failed test leaves nothing behind.
struct Cluster {
    supervisor: Child,
    #[allow(dead_code)]
    dir: TempDir,
    pd: String,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

fn start() -> Cluster {
    let dir = TempDir::new().unwrap();
    let (base, held) = reserve();
    let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
    command
        .arg("cluster")
        .arg("start")
        .arg("--nodes")
        .arg(NODES.to_string())
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--base-port")
        .arg(base.to_string())
        .arg("--pd")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    drop(held);
    let supervisor = command.spawn().expect("the cluster starts");
    let pd = format!("127.0.0.1:{}", base + u16::try_from(NODES).unwrap());
    // **Owned before the wait, so the supervisor is killed on every path out of here** — including
    // the panic below, which is the path a leak hides behind.
    let cluster = Cluster {
        supervisor,
        dir,
        pd,
    };

    // **Readiness is proved by a connection that answers**, not by a port being open: a bare TCP
    // connect says somebody is listening, not that it is the driver and not that it leads.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let ready = esker(&[
            "durability",
            "record",
            "--pd",
            &cluster.pd,
            "--out",
            "/dev/null",
            "--for",
            "1s",
        ])
        .0;
        if ready {
            return cluster;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("the cluster never answered a write within 30 s");
}

/// Runs the CLI and answers `(succeeded, output)`.
fn esker(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .args(args)
        .output()
        .expect("the binary runs");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// **The rehearsal, and the negative control that gives it meaning.**
///
/// Three real store processes with a placement driver above them: write for a few seconds
/// recording every acknowledged commit, then read every one of them back at the timestamp it was
/// acknowledged at. That must be green — and then a line for a write that never happened is
/// appended, and the same checker must call it **lost**.
///
/// Without the second half the first proves only that a checker which always says yes says yes.
#[test]
#[ignore = "starts three store processes; run with --run-ignored all"]
fn the_checker_says_yes_to_what_was_written_and_no_to_what_was_not() {
    let cluster = start();
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("writes.log");
    let log_path = log.to_str().unwrap();

    let (ok, said) = esker(&[
        "durability",
        "record",
        "--pd",
        &cluster.pd,
        "--out",
        log_path,
        "--clients",
        "2",
        "--for",
        "3s",
    ]);
    assert!(ok, "the recorder failed: {said}");
    let recorded = std::fs::read_to_string(&log).unwrap();
    let lines = recorded.lines().filter(|l| !l.is_empty()).count();
    assert!(
        lines > 0,
        "the recorder acknowledged nothing, so this test checked nothing: {said}"
    );

    // **Green**: every acknowledged write is there.
    let (ok, said) = esker(&[
        "durability",
        "verify",
        "--pd",
        &cluster.pd,
        "--in",
        log_path,
    ]);
    assert!(
        ok,
        "a write that was acknowledged could not be read back: {said}"
    );

    // **Red**: a write that never happened, in the same file, through the same checker. The line
    // number continues the file's, so this is a *missing write* and not the recorder's own gap —
    // the checker refuses those separately and the two must not be confused.
    let mut appended = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    let commit_ts = recorded
        .lines()
        .last()
        .and_then(|line| line.split('\t').nth(3))
        .and_then(|ts| ts.parse::<u64>().ok())
        .expect("the last record has a commit timestamp");
    writeln!(
        appended,
        "{}\tdurability/never/00000001\tnever-written\t{commit_ts}\t0",
        lines + 1
    )
    .unwrap();
    drop(appended);

    let (ok, said) = esker(&[
        "durability",
        "verify",
        "--pd",
        &cluster.pd,
        "--in",
        log_path,
    ]);
    assert!(
        !ok,
        "the checker passed a write that was never made — it cannot report a lost one either:\n{said}"
    );
    assert!(
        said.contains("invariant 1") && said.contains("missing"),
        "the failure must name the invariant and the line: {said}"
    );
    // And it must leave the replayable list behind.
    assert!(
        Path::new(&format!("{log_path}.failures")).exists(),
        "no replayable failure list was written"
    );
}

/// **Four clients, ten thousand acknowledged writes, no gap in the record.**
///
/// The negative control for run 118's second finding. The recorder appended from four threads,
/// numbering each line from an atomic counter and writing under a mutex — so the order lines were
/// *numbered* in and the order they were *written* in were two different orders, and the file had a
/// hole at line 15 of 3,550, nowhere near any kill. `verify` reads a hole as "records were lost, so
/// this run does not count", which is how a whole real-topology run produced no verdict.
///
/// A gap is invisible to any assertion about *content*: every line present is correct, and the file
/// looks fine unless something counts. So this counts — and it counts under the concurrency that
/// produced the fault, because one client could never have shown it.
#[test]
#[ignore = "starts three store processes and writes ten thousand rows; run with --run-ignored all"]
fn four_clients_leave_no_hole_in_the_record() {
    /// Enough chances for the fault to show. The field gap was at **line 15 of 3,550**, so the
    /// mechanism does not need volume to appear — it needs concurrent writers, which is what the
    /// four clients are. `--for 80s` records upwards of ten thousand on an idle host; this floor is
    /// what remains true when something else is using the machine, and the count is printed so a
    /// thin run is visible rather than quietly weak.
    ///
    /// A higher bar made this test fail when nextest ran it beside its neighbour — a pass that
    /// depends on what else is running is not a pass.
    const FLOOR: usize = 2_000;

    let cluster = start();
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("many.log");
    let log_path = log.to_str().unwrap();

    // Long enough for four clients to reach `WANTED` on this harness, which acknowledges a couple
    // of hundred a second — measured, not guessed, and with room for a loaded host. The assertion
    // is on the count actually recorded, so a slow run says so rather than passing on a thin
    // sample.
    let (ok, said) = esker(&[
        "durability",
        "record",
        "--pd",
        &cluster.pd,
        "--out",
        log_path,
        "--clients",
        "4",
        "--for",
        "80s",
        "--keyspace",
        "gapless",
    ]);
    assert!(ok, "the recorder failed: {said}");

    let text = std::fs::read_to_string(&log).unwrap();
    let numbers: Vec<u64> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split('\t')
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("a record has no line number: {line}"))
        })
        .collect();
    println!(
        "  recorded {} acknowledged writes from four clients",
        numbers.len()
    );
    assert!(
        numbers.len() >= FLOOR,
        "only {} writes were acknowledged, which is too few for the absence of a gap to mean \
         anything",
        numbers.len()
    );
    let expected: Vec<u64> = (1..=numbers.len() as u64).collect();
    assert_eq!(
        numbers, expected,
        "the record's line numbers are not 1..n, so records were lost — which `verify` reads as a \
         broken recorder and refuses to give a verdict on"
    );
}
