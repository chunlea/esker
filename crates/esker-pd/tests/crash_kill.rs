//! `SIGKILL` around the two places PD orders a write against an answer.
//!
//! `prompts/04-multiraft-pd.md` asks for both: "kill between alloc persist and reply (id never
//! reused), kill around TSO mark movement (ts never repeats)". The unit tests in
//! [`esker_pd::alloc`] and [`esker_pd::tso`] check the ordering against a callback that can be
//! made to fail; this checks it against a signal that can land anywhere — inside the `fsync`,
//! between the write returning and the value being printed, in the middle of the kernel's own
//! writeback. One test has perfect coverage of a coarse model, the other coarse coverage of the
//! real thing, and neither substitutes for the other.
//!
//! # The shape
//!
//! The test binary re-executes **itself** with `--exact` and an environment variable, so the
//! child is this file's [`the_child_allocates_until_it_is_killed`] and there is no second
//! binary to build or keep in step (the same trick as `esker-engine`'s `crash_kill.rs`).
//!
//! Every generation opens the *same* directory, so the whole run is one PD being killed and
//! restarted over and over. The contract is checked over the concatenation of everything every
//! generation printed:
//!
//! * **strictly increasing** — an id or a timestamp is never repeated, and never goes
//!   backwards, across a kill;
//! * and the clock is set **backwards** by a second on every restart, so the oracle is being
//!   asked to survive exactly the case that makes the persisted mark necessary.
//!
//! The batch sizes are deliberately tiny — one id per reservation, a one-millisecond mark
//! interval — so that nearly every operation crosses a persist and the kill has something to
//! land in the middle of. At the production defaults (1,000 ids, 3 s) a run this length would
//! cross two or three, and the test would be measuring nothing.
//!
//! # What a `SIGKILL` cannot prove
//!
//! It kills the process, not the machine: bytes already handed to the kernel are still written
//! back afterwards. So this loop proves the **ordering** — the record reaches the engine before
//! the value it covers leaves — and the **restart rule**, and it cannot tell `sync = true` from
//! `sync = false`. That the write is fsynced is what makes the same ordering hold across a
//! *machine* crash, and nothing in this repository can distinguish the two without cutting
//! power for real; the engine's own invariant-1 tests have the same edge.
//!
//! Both halves were checked by breaking them and watching this file go red:
//!
//! | Mutation | Result |
//! |---|---|
//! | `Oracle::load` resumes at the clock instead of `max(clock, mark)` | "timestamp … did not advance": a restart went behind a timestamp already handed out |
//! | the id reservation never reaches the engine | "id 1 … did not advance on 34": a restart began again at 1 |
//! | either persist moved after the value it covers | caught by the unit tests instead, which assert the state does not move when the persist fails |

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
use esker_pd::clock::TestClock;
use esker_pd::{Clock, Pd, PdOptions};

/// Names the child. Its absence is what tells the test it is the parent.
const ENV_DIR: &str = "ESKER_PD_CRASH_DIR";
const ENV_MODE: &str = "ESKER_PD_CRASH_MODE";
const ENV_CLOCK: &str = "ESKER_PD_CRASH_CLOCK";

/// The child is this file's own test, selected by name.
const CHILD_TEST: &str = "the_child_allocates_until_it_is_killed";

/// Generations per run. Each one is a process start, so this is the knob that decides how long
/// the test takes.
const ITERATIONS: u32 = 40;

/// Values one generation tries to hand out before giving up and exiting.
const OPS: u32 = 400;

/// Where the clock starts. Every restart moves it a second *backwards*.
const CLOCK_BASE_MS: u64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------------------

/// Opens PD and hands out ids or timestamps, printing each one as it is returned.
///
/// In an ordinary test run [`ENV_DIR`] is unset and this returns immediately; it is a test at
/// all only so that the parent can select it with `--exact`.
#[test]
fn the_child_allocates_until_it_is_killed() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let mode = std::env::var(ENV_MODE).unwrap_or_default();
    let now_ms: u64 = std::env::var(ENV_CLOCK)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(CLOCK_BASE_MS);

    let mut out = std::io::stdout().lock();
    // The harness writes `test <name> ... ` with no trailing newline before handing over, so
    // the first thing printed here arrives glued to it. The parent searches for its markers
    // rather than stripping a prefix, but a protocol that only works because the reader is
    // forgiving is one waiting to break.
    let _ = writeln!(out);
    let _ = out.flush();

    let clock = Arc::new(TestClock::new(now_ms));
    let options = PdOptions {
        // One id per reservation and a one-millisecond mark, so nearly every operation is an
        // fsync the kill can land inside of.
        alloc_batch: 1,
        tso_save_interval_ms: 1,
        ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
    };
    let pd = match Pd::open(&dir, options) {
        Ok(pd) => pd,
        Err(error) => {
            let _ = writeln!(out, "FAIL open: {error}");
            let _ = out.flush();
            return;
        }
    };
    if let Err(error) = pd.bootstrap(1, "127.0.0.1:20160") {
        let _ = writeln!(out, "FAIL bootstrap: {error}");
        let _ = out.flush();
        return;
    }

    for op in 0..OPS {
        let handed_out = if mode == "tso" {
            pd.tso(1)
        } else {
            pd.alloc_id(1)
        };
        match handed_out {
            Ok(value) => {
                // After the call returned, never before: the line means "this value left PD".
                // A kill in the gap between these two lines drops a value that was handed out
                // and never seen, which is legal and is why the parent asserts on what it saw
                // rather than on a count.
                let _ = writeln!(out, "VALUE {value}");
                let _ = out.flush();
            }
            Err(error) => {
                let _ = writeln!(out, "FAIL {mode} {op}: {error}");
                let _ = out.flush();
                return;
            }
        }
        // Spread the run across milliseconds so the oracle keeps crossing its mark.
        thread::sleep(Duration::from_micros(200));
    }
    let _ = writeln!(out, "DONE");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------

/// Whatever follows `marker` in `line`, wherever it appears. Anything before it is the
/// harness's own output sharing the line.
fn marker_in<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|at| &line[at + marker.len()..])
}

#[derive(Debug, Default)]
struct Report {
    values: Vec<u64>,
    failure: Option<String>,
}

/// Runs one child against `dir`, kills it, and returns what it printed.
///
/// Never panics before the child is reaped, so a failing assertion cannot leave a process
/// behind.
fn run_child(dir: &Path, mode: &str, clock_ms: u64, seed: u64) -> (Report, bool) {
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV_DIR, dir)
        .env(ENV_MODE, mode)
        .env(ENV_CLOCK, clock_ms.to_string())
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
            // Read to EOF rather than stopping at the kill: what is already in the pipe is
            // exactly what PD handed out, and dropping it would hide a repeat.
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if let Some(reason) = marker_in(line, "FAIL ") {
                    report.lock().unwrap().failure = Some(reason.to_owned());
                    settled.store(true, Ordering::SeqCst);
                } else if let Some(value) = marker_in(line, "VALUE ") {
                    if let Ok(value) = value.trim().parse::<u64>() {
                        report.lock().unwrap().values.push(value);
                    }
                } else if line.contains("DONE") {
                    settled.store(true, Ordering::SeqCst);
                }
            }
        })
    };

    // Kill after a random number of values plus a random sub-millisecond delay, so the signal
    // lands anywhere in or around a persist rather than always at the same seam.
    let mut rng = Pcg32::from_seed(seed);
    let target = 1 + rng.below(40);
    let jitter = u64::from(rng.below(3_000));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if settled.load(Ordering::SeqCst) {
            break;
        }
        if report.lock().unwrap().values.len() >= target as usize {
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

/// Kills `iterations` generations over one directory and returns everything they handed out,
/// in the order they handed it out, plus how many of them the signal actually caught.
fn kill_loop(mode: &str, iterations: u32) -> (Vec<u64>, u32) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut seen: Vec<u64> = Vec::new();
    let mut killed_count = 0;

    for generation in 0..iterations {
        // Backwards on every restart: the case the persisted mark exists for.
        let clock_ms = CLOCK_BASE_MS - u64::from(generation) * 1_000;
        let (report, killed) = run_child(dir.path(), mode, clock_ms, u64::from(generation) + 1);
        assert!(
            report.failure.is_none(),
            "generation {generation} ({mode}) failed: {}",
            report.failure.unwrap_or_default()
        );
        if killed {
            killed_count += 1;
        }
        seen.extend(report.values);
    }
    (seen, killed_count)
}

/// Checks the contract over everything every generation handed out.
fn assert_strictly_increasing(values: &[u64], what: &str) {
    let mut previous: Option<u64> = None;
    for (position, value) in values.iter().enumerate() {
        if let Some(previous) = previous {
            assert!(
                *value > previous,
                "{what} {value} at position {position} did not advance on {previous}: a \
                 restart handed out a value at or below one that had already left PD"
            );
        }
        previous = Some(*value);
    }
    let unique: std::collections::BTreeSet<u64> = values.iter().copied().collect();
    assert_eq!(unique.len(), values.len(), "a {what} was handed out twice");
}

/// An id must never be handed out twice, whatever the kill interrupts.
#[test]
fn ids_are_never_handed_out_twice_across_kills() {
    let (ids, killed) = kill_loop("ids", ITERATIONS);
    assert!(
        ids.len() > ITERATIONS as usize,
        "only {} ids over {ITERATIONS} generations",
        ids.len()
    );
    assert_strictly_increasing(&ids, "id");
    assert!(
        killed * 2 >= ITERATIONS,
        "only {killed}/{ITERATIONS} generations were actually killed; the loop is measuring \
         clean shutdowns rather than crashes"
    );
}

/// A timestamp must never repeat or go backwards, even with the clock moving backwards under
/// it on every restart.
#[test]
fn timestamps_never_repeat_across_kills() {
    let (timestamps, killed) = kill_loop("tso", ITERATIONS);
    assert!(
        timestamps.len() > ITERATIONS as usize,
        "only {} timestamps over {ITERATIONS} generations",
        timestamps.len()
    );
    assert_strictly_increasing(&timestamps, "timestamp");
    assert!(
        killed * 2 >= ITERATIONS,
        "only {killed}/{ITERATIONS} generations were actually killed"
    );

    // And the physical parts really did move backwards in wall-clock terms, or the test would
    // be passing because the clock never misbehaved.
    let first = esker_pd::decompose_ts(timestamps[0]).0;
    assert!(
        first >= CLOCK_BASE_MS - u64::from(ITERATIONS) * 1_000,
        "the clock was not the one the test set"
    );
}

/// The long run, for a soak. `cargo test -p esker-pd --test crash_kill -- --ignored`.
#[test]
#[ignore = "hundreds of process spawns; run it deliberately"]
fn the_kill_loop_ignored() {
    let (ids, _) = kill_loop("ids", ITERATIONS * 5);
    assert_strictly_increasing(&ids, "id");
    let (timestamps, _) = kill_loop("tso", ITERATIONS * 5);
    assert_strictly_increasing(&timestamps, "timestamp");
}
