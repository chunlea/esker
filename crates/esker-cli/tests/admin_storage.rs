//! `esker admin flush` and `esker admin compact` — a flush an operator can ask for from outside.
//!
//! Everything that decides when an SST appears is inside the store: a memtable crosses
//! `write_buffer_size` (64 MiB by default) and a flush job runs. Nothing outside the process can
//! ask for one. `Store::flush` and `Store::compact_write_cf` exist and, until this file, had
//! exactly one caller between them — `esker bench --compact`, which runs its own database rather
//! than talking to a store over a socket.
//!
//! That gap is what stopped #58's arm A from separating its two candidates. Four passes of the
//! same work never crossed 64 MiB, so the node wrote **no SST at all**: the whole accumulated
//! state was memtable plus WAL, and `sst-dump` had nothing to dump. An arm that could flush on
//! demand would have both sides of that measurement instead of one.
//!
//! # What is asserted, and why it is the file count and not the call
//!
//! A verb that returned as soon as the request was accepted would be useless to the arm that has
//! to assert on what it produced. So the receipt is the **files themselves** — every SST the store
//! holds, per column family, with the level each is at — and the test asserts a store that held
//! none now holds some. `Db::flush_all` already waits for the flush job to finish, so the verb has
//! only to answer after it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Generous for the reason `data_dir_lock.rs` gives its own budget: the store opens a database
/// first, and a loaded host makes that slower without making it wrong.
const LISTENING_WITHIN: Duration = Duration::from_secs(30);

/// Enough rows that a flush has something to write, and far below the 64 MiB that would make the
/// store flush on its own — which is the whole point: if the memtable flushed by itself, this test
/// would pass without the verb existing.
const ROWS: usize = 200;

fn free_port() -> u16 {
    let socket = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    socket.local_addr().expect("its address").port()
}

/// A child killed however the test leaves.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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

/// Waits for the store's own "listening" line — the line and not the port, because a bare connect
/// says somebody is listening and not that it is this child.
fn wait_until_listening(server: &mut Server, log: &Path) {
    let deadline = Instant::now() + LISTENING_WITHIN;
    loop {
        let said = std::fs::read_to_string(log).unwrap_or_default();
        if said.contains("listening on") {
            return;
        }
        if let Some(status) = server.0.try_wait().expect("waiting on the server") {
            panic!("the store exited with {status}; it said:\n{said}");
        }
        assert!(
            Instant::now() < deadline,
            "the store never listened within {LISTENING_WITHIN:?}; it said:\n{said}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs `esker-cli` with `args` and returns everything it said, plus whether it succeeded.
fn cli(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .args(args)
        .output()
        .expect("esker-cli runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Every `.sst` under `dir`, at any depth. The **on-disk** count, so that what the verb reports
/// can be checked against something it did not produce itself.
fn ssts_on_disk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|end| end == "sst") {
                out.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// **The gap #58 ran into, closed**: rows written, nothing flushed, and a flush an operator asks
/// for from outside the process.
///
/// The `assert` before the flush is half the test. Without it a store that happened to flush on
/// its own would make the verb look like it worked, which is the mistake this whole family of
/// measurement is prone to — the arm that cannot flush and the arm that flushed by accident report
/// the same number.
#[test]
fn a_flush_an_operator_asked_for_writes_the_ssts() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let port = free_port();
    let address = format!("127.0.0.1:{port}");

    let mut server = start(&data, port, &log);
    wait_until_listening(&mut server, &log);

    for at in 0..ROWS {
        let key = format!("k{at:05}");
        let value = format!("v{at:05}");
        let (ok, said) = cli(&["raw", "put", &key, &value, "--addr", &address]);
        assert!(ok, "`raw put {key}` failed: {said}");
    }

    assert!(
        ssts_on_disk(&data).is_empty(),
        "the store flushed on its own, so this test would pass without the verb: {:?}",
        ssts_on_disk(&data)
    );

    let (ok, said) = cli(&["admin", "flush", "--store", &address]);
    assert!(ok, "`admin flush` failed: {said}");

    let after = ssts_on_disk(&data);
    assert!(
        !after.is_empty(),
        "`admin flush` returned but wrote no SST. It said:\n{said}"
    );
    // The receipt names what it wrote, so an arm can assert on the answer rather than on the
    // directory — which is the whole reason the verb answers at all.
    assert!(
        said.contains("sst"),
        "`admin flush` wrote {} SSTs and said nothing about them:\n{said}",
        after.len()
    );
}

/// **Compaction, asked for the same way**, and answering with what the store holds afterwards.
///
/// Asserted as "it answers with a count" rather than "the count fell": one flush of one memtable
/// makes a single L0 file and compacting it need not merge anything away. What the arm needs is a
/// number it can take before and after, and that is what this pins.
#[test]
fn a_compaction_an_operator_asked_for_answers_with_what_is_left() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let port = free_port();
    let address = format!("127.0.0.1:{port}");

    let mut server = start(&data, port, &log);
    wait_until_listening(&mut server, &log);

    for at in 0..ROWS {
        let (ok, said) = cli(&[
            "raw",
            "put",
            &format!("k{at:05}"),
            "value",
            "--addr",
            &address,
        ]);
        assert!(ok, "`raw put` failed: {said}");
    }
    let (ok, said) = cli(&["admin", "flush", "--store", &address]);
    assert!(ok, "`admin flush` failed: {said}");
    assert!(!ssts_on_disk(&data).is_empty(), "nothing to compact");

    let (ok, said) = cli(&["admin", "compact", "--store", &address, "--cf", "write"]);
    assert!(ok, "`admin compact` failed: {said}");
    assert!(
        said.contains("sst"),
        "`admin compact` said nothing about what is left:\n{said}"
    );
}

/// **`esker sst-dump` must open the SSTs this system writes.** Until this test it could not.
///
/// Every table a store writes is built with `esker.InternalKeyComparator` — the MVCC suffix is
/// part of a key's order — and `sst-dump` opened files with `TableOptions::default()`, which names
/// `esker.BytewiseComparator`. `TableReader::open` refuses the mismatch, correctly and loudly:
///
/// ```text
/// table was built with comparator "esker.InternalKeyComparator" but is being read with
/// "esker.BytewiseComparator"; its keys would be searched in the wrong order
/// ```
///
/// So the one tool for counting dead versus live versions in a table could not open a single real
/// one, which is the instrument gap #58's isolation ran into. `reconcile.rs` and
/// `manifest_dump.rs` already wrap the internal comparator; this is the third reader of the same
/// fact, and it was the one that did not.
///
/// It is an end-to-end test on purpose: the file it opens is one a store wrote, flushed by the
/// verb above, rather than one the test built to its own taste.
#[test]
fn sst_dump_opens_a_table_a_store_wrote() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let port = free_port();
    let address = format!("127.0.0.1:{port}");

    let mut server = start(&data, port, &log);
    wait_until_listening(&mut server, &log);
    for at in 0..ROWS {
        let (ok, said) = cli(&[
            "raw",
            "put",
            &format!("k{at:05}"),
            "value",
            "--addr",
            &address,
        ]);
        assert!(ok, "`raw put` failed: {said}");
    }
    let (ok, said) = cli(&["admin", "flush", "--store", &address]);
    assert!(ok, "`admin flush` failed: {said}");

    let mut opened = 0;
    let mut entries = 0;
    for name in ssts_on_disk(&data) {
        let mut stack = vec![data.clone()];
        let mut found = None;
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().is_some_and(|it| it == name.as_str()) {
                    found = Some(path);
                }
            }
        }
        let path = found.expect("the sst that was just listed");
        let (ok, report) = cli(&["sst-dump", path.to_str().unwrap()]);
        assert!(
            ok,
            "sst-dump refused {}:
{report}",
            path.display()
        );
        assert!(
            report.contains("comparator            esker.InternalKeyComparator"),
            "a store's table is built with the internal comparator:
{report}"
        );
        // The count is the point: an `entry_count` of zero would mean the tool opened the file
        // and read nothing out of it, which is the same amount of use to r1 as refusing it.
        let count: u64 = report
            .lines()
            .find_map(|line| line.trim().strip_prefix("entry_count"))
            .and_then(|rest| rest.trim().parse().ok())
            .unwrap_or_else(|| panic!("no entry_count in:\n{report}"));
        assert!(count > 0, "the table dumped zero entries:\n{report}");
        entries += count;
        opened += 1;
    }
    assert!(opened > 0, "the flush wrote no sst to dump");
    assert!(entries > 0, "{opened} tables held no entries between them");
}

/// Sums `entry_count` over every SST under `dir`, by dumping each one.
///
/// An **independent** count: `sst-dump` reads the files from disk and knows nothing about the verb
/// under test, so a receipt that agreed with it agreed with something it did not produce.
fn entries_on_disk(data: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![data.to_path_buf()];
    let mut files = Vec::new();
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|end| end == "sst") {
                files.push(path);
            }
        }
    }
    for path in files {
        let (ok, report) = cli(&["sst-dump", path.to_str().unwrap()]);
        assert!(ok, "sst-dump refused {}:\n{report}", path.display());
        let count: u64 = report
            .lines()
            .find_map(|line| line.trim().strip_prefix("entry_count"))
            .and_then(|rest| rest.trim().parse().ok())
            .unwrap_or_else(|| panic!("no entry_count in:\n{report}"));
        total += count;
    }
    total
}

/// **ADR 0110 step ① over a socket**: an operator hands a store a safepoint and gets a receipt.
///
/// What collection *does* is asserted where the versions are real —
/// `esker-store/tests/safepoint_collects.rs`, through prewrite and commit, `write` 96 → 8. This is
/// the other half: that the verb reaches a running store, that the safepoint it reports is the one
/// in force, and that the counts it answers with are the counts on disk.
///
/// It writes through `RawKV` because that is what the CLI has, and a raw key has no MVCC versions —
/// so this asserts the **round trip and the receipt**, not the collection. Saying which test proves
/// which half is the point: a single test that did both badly would prove neither.
#[test]
fn a_safepoint_an_operator_handed_over_comes_back_with_a_receipt() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let port = free_port();
    let address = format!("127.0.0.1:{port}");

    let mut server = start(&data, port, &log);
    wait_until_listening(&mut server, &log);
    for at in 0..ROWS {
        let (ok, said) = cli(&[
            "raw",
            "put",
            &format!("k{at:05}"),
            "value",
            "--addr",
            &address,
        ]);
        assert!(ok, "`raw put` failed: {said}");
    }

    let (ok, said) = cli(&["admin", "gc", "--safepoint", "4096", "--store", &address]);
    assert!(ok, "`admin gc` failed: {said}");
    assert!(
        said.contains("safepoint 4096 in force"),
        "the receipt must name the safepoint that took effect:\n{said}"
    );
    assert!(
        said.contains("write") && said.contains("entries"),
        "the receipt must name each column family and its entries:\n{said}"
    );

    // The verb compacts, so the rows written above are on disk by now, and the entry total it
    // reported has to be the one `sst-dump` reads back out of the files it left behind.
    let on_disk = entries_on_disk(&data);
    assert!(on_disk > 0, "the collection left no SST to count:\n{said}");
    assert!(
        said.contains(&on_disk.to_string()),
        "the receipt does not name the {on_disk} entries sst-dump counts:\n{said}"
    );

    // A safepoint never moves backwards, and the receipt says so rather than pretending.
    let (ok, lowered) = cli(&["admin", "gc", "--safepoint", "1", "--store", &address]);
    assert!(ok, "`admin gc` failed: {lowered}");
    assert!(
        lowered.contains("safepoint 4096 in force"),
        "a lower safepoint must not lower the one in force:\n{lowered}"
    );
}
