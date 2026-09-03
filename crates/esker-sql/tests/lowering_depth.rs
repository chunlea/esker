//! **Invariant 9 at the lowering layer**: a deeply nested expression is refused, never a crash.
//!
//! Run 46's node aborted with a stack overflow in `esker_sql::parse::lower::lower_expr`, at the
//! boundary of `test/cases/relation/or_test.rb` — `ActiveRecord`'s `.or()` builds long boolean
//! chains. The parser was already guarded (`crate::parse::MAX_NESTING_DEPTH`, `54001`), and the
//! guard missed this shape for a precise reason: it counts **brackets**, and `a OR b OR c` has one
//! bracket and builds an N-deep tree. Lowering then recursed once per term on whatever stack the
//! caller had — a tokio worker's, which is 2 MiB.
//!
//! # Why this file re-executes itself
//!
//! A stack overflow is not catchable: Rust's handler prints and calls `abort`, taking the whole
//! test process with it. So the depth probe runs in a **child process** and the parent reads its
//! exit status. Before the guard the child dies of `SIGABRT` and this file says so; after it the
//! child exits 0 having been told `54001`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// What a tokio worker gets by default, and therefore the stack the guard has to be safe on.
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

/// The environment variable that turns this binary into the probe.
const DEPTH: &str = "ESKER_LOWER_DEPTH";

/// `a = 0 OR a = 1 OR …`, which is `depth` terms and **one** bracket.
fn or_chain(depth: usize) -> String {
    let mut sql = String::from("SELECT * FROM t WHERE ");
    for term in 0..depth {
        if term > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str(&format!("a = {term}"));
    }
    sql
}

/// The probe: lower an `OR` chain of `depth` terms on a worker-sized stack.
///
/// Prints what happened and exits 0 either way — a refusal is a correct outcome, and the only
/// incorrect one is not reaching this line at all.
fn probe(depth: usize) -> ! {
    let handle = std::thread::Builder::new()
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let sql = or_chain(depth);
            // Printed as each phase completes, so a crash says which one it died in rather than
            // leaving the caller to guess between parsing, lowering and dropping the tree.
            let parsed = match esker_sql::parse::parse_statements(&sql) {
                Err(error) => {
                    println!("PARSE {}", error.sqlstate());
                    return;
                }
                Ok(parsed) => parsed,
            };
            println!("PARSED");
            match parsed[0].lower() {
                Ok(plan) => {
                    println!("LOWERED");
                    drop(plan);
                    println!("DROPPED-PLAN");
                }
                Err(error) => println!("REFUSED {}", error.sqlstate()),
            }
            drop(parsed);
            println!("DROPPED-PARSED");
        })
        .expect("the probe thread starts");
    handle.join().expect("the probe thread finishes");
    std::process::exit(0);
}

/// Runs the probe in a child and answers what it printed, or `None` if it died.
fn run_probe(depth: usize) -> Option<String> {
    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .env(DEPTH, depth.to_string())
        .arg("--exact")
        .arg("the_probe_entry_point")
        .arg("--nocapture")
        .output()
        .expect("the child runs");
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // `PARSED` is a progress marker and `PARSE 54001` is an outcome, so the match is on the
    // outcomes by name rather than on a prefix that covers both.
    text.lines()
        .find(|line| {
            line.starts_with("LOWERED") || line.starts_with("REFUSED") || line.starts_with("PARSE ")
        })
        .map(str::to_owned)
}

/// Not a test: the entry point the child re-enters through. It exits before asserting anything.
#[test]
fn the_probe_entry_point() {
    if let Ok(depth) = std::env::var(DEPTH) {
        probe(depth.parse().expect("a depth"));
    }
}

/// **A chain deep enough to overflow a worker's stack is refused, not fatal.**
///
/// 100_000 terms is far past anything a guard could admit and far past what 2 MiB can hold — it is
/// the shape that aborted run 46's node. The claim is only that the process survives to tell us.
#[test]
fn a_boolean_chain_too_deep_to_lower_is_refused_rather_than_fatal() {
    let answer = run_probe(100_000);
    assert!(
        answer.is_some(),
        "the child died lowering a 100000-term OR chain: a statement a client can send crashed \
         the node, which is invariant 9"
    );
    let answer = answer.unwrap_or_default();
    assert!(
        answer.starts_with("REFUSED 54001") || answer.starts_with("PARSE 54001"),
        "expected `54001 stack depth limit exceeded`, got {answer:?}"
    );
}

/// **And a chain a client might really write still works.**
///
/// A guard set too low would refuse ordinary SQL, which is the other way to fail this. 64 terms is
/// a plausible `.or()` chain and has to lower.
#[test]
fn an_ordinary_boolean_chain_still_lowers() {
    assert_eq!(run_probe(64).as_deref(), Some("LOWERED"));
}
