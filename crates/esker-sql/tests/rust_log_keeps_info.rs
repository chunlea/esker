//! `RUST_LOG` naming one target must not silence this binary's `INFO`.
//!
//! On 2026-09-17 a gate ran with `RUST_LOG=esker_store::transport=debug` and
//! `store_starts_before_its_driver` timed out after 30 s: the filter had **replaced** the default
//! rather than adding to it, so every `INFO` the child process emits was gone and the readiness
//! line the test waits for never appeared. `esker-cli` has a test of that shape; this is the same
//! claim for the other binary that installs a subscriber the same way.
//!
//! # Why this fixture does not reserve a port
//!
//! The line under test is emitted while the node is still being built — before it serves — so this
//! test never needs the bind to succeed. A port collision would make the process exit *after* the
//! line it is waiting for, and the log file already holds it by then. Reserving one would be
//! borrowing `esker-cli`'s `port_band` into a crate that has no other use for it, which the
//! coordinator ruled against on 2026-09-17: keep the fixture minimal, do not lift a shared crate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Generous: the node opens a backend before it reaches the line under test, and a loaded host
/// makes that slower without making it wrong. The same reasoning as `esker-cli`'s own budgets.
const SAYS_IT_WITHIN: Duration = Duration::from_secs(60);

/// The line the node emits when it is given no placement driver — `esker-sql.rs`'s `INFO` on the
/// branch that decides to run unrestricted. It is emitted on **every** run without `--pd`, which
/// is what makes it a readiness signal rather than a race.
const THE_INFO_LINE: &str = "no placement driver given";

fn esker_sql() -> &'static str {
    env!("CARGO_BIN_EXE_esker-sql")
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

/// Starts the node with `RUST_LOG` set as given and waits for its `INFO` line.
fn says_its_info_line_with(rust_log: Option<&str>) {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("node.log");

    let mut node = Command::new(esker_sql());
    match rust_log {
        Some(filter) => node.env("RUST_LOG", filter),
        // **Removed, not empty.** An empty `RUST_LOG` is still a variable that is set, and the
        // fallback under test is the one taken when it is *absent*.
        None => node.env_remove("RUST_LOG"),
    };
    let mut node = spawn(node, &log);

    let deadline = Instant::now() + SAYS_IT_WITHIN;
    while !said(&log).contains(THE_INFO_LINE) {
        if let Some(status) = node.0.try_wait().expect("waiting on the node") {
            panic!(
                "the node exited with {status} under {rust_log:?}: {}",
                said(&log)
            );
        }
        assert!(
            Instant::now() < deadline,
            "no {THE_INFO_LINE:?} under {rust_log:?}. This is the 2026-09-17 shape: a filter \
             naming one target replaces the default and takes every INFO with it. Said: {}",
            said(&log)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// **Nothing set** — the case that always worked, kept so the fix cannot be read as the only
/// thing holding it up.
#[test]
fn with_no_filter_the_node_says_what_it_always_said() {
    says_its_info_line_with(None);
}

/// **A target named** — the case that was broken, and the one the gate paid for.
#[test]
fn a_filter_naming_one_target_leaves_the_rest_at_info() {
    says_its_info_line_with(Some("esker_store::transport=debug"));
}

/// **A bare level** — the operator wins, so this one must *not* see the line.
///
/// The inverse of the two above, and the reason the fix is not `add_directive` alone: an `info`
/// added on top of a bare `warn` would overwrite it, and this is what would catch that.
#[test]
fn a_bare_level_from_the_operator_is_obeyed() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("node.log");

    let mut node = Command::new(esker_sql());
    node.env("RUST_LOG", "warn");
    let node = spawn(node, &log);

    // Long enough that the line would have appeared if it were going to: the two tests above see
    // it well inside this, and the claim here is about its absence.
    std::thread::sleep(Duration::from_secs(5));
    let output = said(&log);
    drop(node);
    assert!(
        !output.contains(THE_INFO_LINE),
        "`RUST_LOG=warn` is the operator saying what they want; an `info` put back on top of it \
         would overwrite it. Said: {output}"
    );
}
