//! The subprocess half of the phase-1 crash loop: a real `SIGKILL`, on a real filesystem.
//!
//! `prompts/01-engine.md` asks for "a test binary that spawns the engine in a child process
//! writing a known pattern with `sync = true`, kills it with SIGKILL at a random moment,
//! reopens, and verifies every acknowledged write (the child reports acks over a pipe) is
//! present". This is that, without a second binary: the test executable re-executes *itself*
//! with `--exact` and an environment variable, so the child is this file's
//! [`the_child_writes_until_it_is_killed`] and nothing has to be built or kept in step.
//!
//! # Why this exists next to `crash_faultfs.rs`
//!
//! The fault injector kills at every operation, deterministically, in memory. It cannot kill
//! *between* operations — between the `fdatasync` returning and the memtable insert, or in the
//! middle of the kernel's own writeback — because it only sees the calls the engine makes.
//! `SIGKILL` can land anywhere, including in all the places a filesystem trait has no name
//! for. One test has perfect coverage of a coarse model; this one has coarse coverage of the
//! real thing. Neither substitutes for the other.
//!
//! # acks ⊆ readable, and nothing more
//!
//! The child writes the ack for operation *n* **after** `write()` has returned. A `SIGKILL`
//! landing in that gap leaves a write that is durable but never acknowledged — perfectly
//! legal, and the reason this test asserts containment rather than equality. The other
//! direction is asserted in full: anything readable must be exactly what was written, and
//! must be an operation that was actually attempted.
//!
//! # One direction is checked as far as it can be
//!
//! `readable ⊆ attempted` is checked by point lookup over the whole key space the child could
//! have written, plus a few keys past the end. It is not yet a *full* scan, because at the
//! time of writing `Db` has no iterator — the merge iterator is step 6's remaining piece. When
//! it lands, `verify` should scan the database end to end and compare the whole key set, which
//! would also catch a recovery that invented a key outside the pattern.
//!
//! # The pipe must not buffer
//!
//! Rust's `stdout` is block-buffered when it is a pipe, so a child that only `println!`s would
//! hand the parent its acks in 8 KiB lumps — and a kill would lose the last lump, turning
//! durable writes into apparently-unacknowledged ones and hiding real losses. Every ack is
//! flushed explicitly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_engine::batch::WriteBatch;
use esker_engine::filename::{self, FileKind};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::sst::{TableOptions, TableReader};
use esker_engine::{Db, cf};

/// Names the child. Its absence is what tells the test it is the parent.
const ENV_DIR: &str = "ESKER_CRASH_DIR";
const ENV_SEED: &str = "ESKER_CRASH_SEED";
const ENV_OPS: &str = "ESKER_CRASH_OPS";

/// The child is this file's own test, selected by name.
const CHILD_TEST: &str = "the_child_writes_until_it_is_killed";

/// Writes the child attempts. Each one is an `fsync`, so this is the knob that decides how
/// long an iteration takes.
const OPS: u32 = 20;

/// Iterations in the default run. `the_kill_loop_ignored` does five times as many.
const ITERATIONS: u32 = 200;

/// The key of operation `op`.
fn key_for(op: u32) -> Vec<u8> {
    format!("key-{op:06}").into_bytes()
}

/// The value of operation `op` under `seed`: varying length and contents, with the operation
/// index stamped into the first four bytes so a value found under the wrong key names the
/// write it really came from.
fn value_for(seed: u64, op: u32) -> Vec<u8> {
    let mut rng = Pcg32::new(seed, u64::from(op));
    let len = 8 + usize::try_from(rng.below(200)).unwrap_or(0);
    let mut value = vec![0u8; len];
    rng.fill_bytes(&mut value);
    value[..4].copy_from_slice(&op.to_le_bytes());
    value
}

// ---------------------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------------------

/// Opens the database and writes the pattern, reporting each acknowledgement on stdout.
///
/// In an ordinary test run [`ENV_DIR`] is unset and this returns immediately; it is only a
/// test at all so that the parent can select it with `--exact`.
#[test]
fn the_child_writes_until_it_is_killed() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let seed: u64 = std::env::var(ENV_SEED)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    let ops: u32 = std::env::var(ENV_OPS)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(OPS);

    let mut out = std::io::stdout().lock();
    let options = Options {
        create_if_missing: true,
        ..Options::default()
    };
    let db = match Db::open(&dir, options) {
        Ok(db) => db,
        Err(error) => {
            // Reported rather than panicked, so the parent gets a sentence instead of a
            // backtrace on a pipe it is about to close.
            let _ = writeln!(out, "FAIL open: {error}");
            let _ = out.flush();
            return;
        }
    };
    let Some(id) = db.cf_id(cf::DEFAULT) else {
        let _ = writeln!(out, "FAIL no default column family");
        let _ = out.flush();
        return;
    };

    for op in 0..ops {
        let mut batch = WriteBatch::new();
        batch.put(id, &key_for(op), &value_for(seed, op));
        match db.write(batch, &WriteOptions::synced()) {
            Ok(_) => {
                // After `write()` returned, never before: the ack means "this was durable".
                // A kill in the gap between these two lines is legal and expected.
                let _ = writeln!(out, "ACK {op}");
                let _ = out.flush();
            }
            Err(error) => {
                let _ = writeln!(out, "FAIL write {op}: {error}");
                let _ = out.flush();
                return;
            }
        }
    }
    let _ = writeln!(out, "DONE");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------

/// What one child did before it died.
#[derive(Debug, Default)]
struct Report {
    acks: Vec<u32>,
    failure: Option<String>,
}

/// The outcome of one iteration, for the summary the loop prints.
struct Outcome {
    /// The child was still running when it was killed.
    killed: bool,
    /// Sorted string tables the run left behind, all of which were verified.
    tables: usize,
    acks: usize,
}

/// Runs one child, kills it, and returns what it reported. Never panics: killing and reaping
/// happen before anything can fail, so a failing assertion cannot leave a process behind.
fn run_child(dir: &Path, seed: u64) -> (Report, bool) {
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV_DIR, dir)
        .env(ENV_SEED, seed.to_string())
        .env(ENV_OPS, OPS.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the child");

    let stdout = child.stdout.take().expect("the child's stdout is a pipe");
    let report = Arc::new(Mutex::new(Report::default()));
    let settled = Arc::new(AtomicBool::new(false));

    let reader = {
        let report = Arc::clone(&report);
        let settled = Arc::clone(&settled);
        thread::spawn(move || {
            // Reading to EOF rather than stopping at the kill: the acks already in the pipe
            // are exactly the writes the child managed to acknowledge, and dropping them
            // would look like lost writes.
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if let Some(op) = line.strip_prefix("ACK ") {
                    if let Ok(op) = op.parse::<u32>() {
                        report.lock().unwrap().acks.push(op);
                    }
                } else if line == "DONE" {
                    // The child ran out of work before the signal arrived. Legal, and counted
                    // separately so the loop can insist the kill usually wins the race.
                    settled.store(true, Ordering::SeqCst);
                } else if let Some(reason) = line.strip_prefix("FAIL ") {
                    report.lock().unwrap().failure = Some(reason.to_owned());
                    settled.store(true, Ordering::SeqCst);
                }
            }
        })
    };

    // Kill after a random number of acknowledgements, plus a random sub-millisecond delay, so
    // the signal lands anywhere in or around a write rather than always at the same seam.
    let mut rng = Pcg32::from_seed(seed);
    let target = 1 + rng.below(OPS - 1);
    let jitter = u64::from(rng.below(3_000));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if settled.load(Ordering::SeqCst) {
            break;
        }
        if report.lock().unwrap().acks.len() >= target as usize {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }
    thread::sleep(Duration::from_micros(jitter));

    let _ = child.kill();
    let status = child.wait().expect("reaping the child");
    reader.join().expect("the reader thread");

    let killed = status.signal() == Some(9);
    let report = Arc::try_unwrap(report)
        .map(Mutex::into_inner)
        .expect("the reader thread is joined")
        .expect("the report lock");
    (report, killed)
}

/// Reopens the database the child left behind and checks both directions of the contract.
///
/// Returns a description of the first violation rather than panicking, so the caller can keep
/// the temporary directory and name it.
fn verify(dir: &Path, seed: u64, report: &Report) -> Result<usize, String> {
    let db = match Db::open(
        dir,
        Options {
            create_if_missing: false,
            ..Options::default()
        },
    ) {
        Ok(db) => db,
        // Killed before the database existed. Only acceptable if nothing was acknowledged.
        Err(_) if report.acks.is_empty() => return Ok(0),
        Err(error) => {
            return Err(format!(
                "reopen failed with {} acknowledged writes: {error}",
                report.acks.len()
            ));
        }
    };

    for op in 0..OPS {
        let found = match db.get(cf::DEFAULT, &key_for(op), &ReadOptions::default()) {
            Ok(found) => found,
            Err(error) => return Err(format!("reading op {op} failed: {error}")),
        };
        match found {
            // Anything readable must be exactly what was written for that operation.
            Some(value) if value.as_ref() != &value_for(seed, op)[..] => {
                return Err(format!(
                    "op {op} read back {} bytes that were never written",
                    value.len()
                ));
            }
            // Absent is fine unless the child said it was durable.
            None if report.acks.contains(&op) => {
                return Err(format!("acknowledged write {op} was lost"));
            }
            // Either a correct value, or a write that was never acknowledged: both legal.
            _ => {}
        }
    }

    // Nothing beyond what the child could have attempted. A point-lookup approximation of
    // `readable ⊆ attempted`; see the module docs for what it will become.
    for op in OPS..OPS + 4 {
        match db.get(cf::DEFAULT, &key_for(op), &ReadOptions::default()) {
            Ok(Some(_)) => return Err(format!("op {op} is readable but was never written")),
            Ok(None) => {}
            Err(error) => return Err(format!("reading op {op} failed: {error}")),
        }
    }

    verify_tables(dir)
}

/// Opens and fully scans every sorted string table the crash left behind.
///
/// This is the `sst-dump` check the brief asks for, at library level: `TableReader::open`
/// verifies the footer, properties, filter and index, and scanning to the end verifies every
/// data block's checksum. Until the engine flushes memtables to L0 there are none of these to
/// find, which is itself worth knowing — the count comes back to the caller and the loop
/// reports it.
fn verify_tables(dir: &Path) -> Result<usize, String> {
    let fs = LocalFileSystem::new();
    let entries = fs
        .list(dir)
        .map_err(|error| format!("listing {}: {error}", dir.display()))?;

    let mut verified = 0;
    for path in entries {
        let Some(FileKind::Sst(number)) = filename::classify_path(&path) else {
            continue;
        };
        let file = fs
            .open(&path)
            .map_err(|error| format!("opening {}: {error}", path.display()))?;
        let table = TableReader::open(file, number, TableOptions::default(), None)
            .map_err(|error| format!("{} is not a readable table: {error}", path.display()))?;

        let mut iter = table.iter();
        iter.seek_to_first();
        while iter.valid() {
            iter.next();
        }
        iter.status()
            .map_err(|error| format!("{} failed a full scan: {error}", path.display()))?;
        verified += 1;
    }
    Ok(verified)
}

/// One iteration: spawn, kill, reopen, check.
fn one_iteration(seed: u64) -> Result<Outcome, String> {
    let dir = tempfile::Builder::new()
        .prefix("esker-crash-")
        .tempdir()
        .map_err(|error| format!("creating a temporary directory: {error}"))?;

    let (report, killed) = run_child(dir.path(), seed);
    if let Some(failure) = &report.failure {
        // The child could not do its job, which is a failure of the test rather than of the
        // engine's crash safety — but it still has to stop the run.
        let kept = dir.keep();
        return Err(format!(
            "seed {seed}: the child reported `{failure}` (state kept at {})",
            kept.display()
        ));
    }

    match verify(dir.path(), seed, &report) {
        Ok(tables) => Ok(Outcome {
            killed,
            tables,
            acks: report.acks.len(),
        }),
        Err(reason) => {
            // Keep the evidence, and say where it is.
            let kept = dir.keep();
            Err(format!(
                "seed {seed}: {reason} (killed: {killed}, {} acks: {:?}, state kept at {})",
                report.acks.len(),
                report.acks,
                kept.display()
            ))
        }
    }
}

/// Runs `iterations` children and reports what happened.
fn kill_loop(iterations: u32, first_seed: u64) {
    let (mut killed, mut clean, mut tables, mut acks) = (0u32, 0u32, 0usize, 0usize);

    for i in 0..iterations {
        let seed = first_seed + u64::from(i);
        match one_iteration(seed) {
            Ok(outcome) => {
                if outcome.killed {
                    killed += 1;
                } else {
                    clean += 1;
                }
                tables += outcome.tables;
                acks += outcome.acks;
            }
            Err(reason) => panic!("crash loop failed at iteration {i}: {reason}"),
        }
    }

    println!(
        "kill loop: {iterations} iterations, {killed} died to SIGKILL, {clean} finished first, \
         {acks} acknowledged writes verified, {tables} sorted string tables scanned"
    );

    // A loop where the signal always lost the race would be a loop that never tested a crash.
    assert!(
        killed * 2 > iterations,
        "only {killed} of {iterations} children were actually killed; the kill is losing the \
         race and this loop is testing a clean shutdown"
    );
    assert!(
        acks > usize::try_from(iterations).unwrap_or(0),
        "only {acks} writes were acknowledged across {iterations} iterations"
    );
}

/// The child is inert in an ordinary run, which is what lets it live in this file.
#[test]
fn the_child_does_nothing_unless_it_is_asked_to() {
    assert!(
        std::env::var(ENV_DIR).is_err(),
        "the parent test is running inside a child invocation"
    );
    // Calling it directly is a no-op — this is the property that keeps `cargo test` honest.
    the_child_writes_until_it_is_killed();
}

/// **The kill loop.** 200 children, each killed by `SIGKILL` at a random moment, each
/// database reopened and checked.
#[test]
fn the_kill_loop() {
    kill_loop(ITERATIONS, 1);
}

/// The acceptance run: 1,000 iterations, as `prompts/01-engine.md` asks for.
///
/// Ignored by default because it is a minute of wall clock, not because it is any less
/// trustworthy. Run it with `cargo test -p esker-engine --test crash_kill -- --ignored`.
#[test]
#[ignore = "the 1,000-iteration acceptance run; minutes, not seconds"]
fn the_kill_loop_acceptance_run() {
    kill_loop(1_000, 100_000);
}
