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

#[path = "parity_harness/mod.rs"]
mod parity;

/// What a tokio worker gets by default, and therefore the stack the guard has to be safe on.
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

/// The environment variable that turns this binary into the probe.
const DEPTH: &str = "ESKER_LOWER_DEPTH";
/// Which walk the probe should make: `lower` stops at the plan, `execute` runs the statement.
const MODE: &str = "ESKER_LOWER_MODE";

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
    let execute = std::env::var(MODE).is_ok_and(|mode| mode == "execute");
    let handle = std::thread::Builder::new()
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let sql = or_chain(depth);
            // **The whole client path, not just the plan.** Lowering was where the node died, but
            // a plan the lowering guard admits is then walked by the resolver, the type pass and
            // the evaluator — each recursive, each on this same worker stack. If any of them
            // cannot take a tree the guard allows, the guard is in the wrong place.
            if execute {
                let mut node = parity::Node::new(&["CREATE TABLE t (a integer)"]);
                match node.run(&sql) {
                    Ok(_) => println!("EXECUTED"),
                    Err(error) => println!("REFUSED {}", error.sqlstate()),
                }
                return;
            }
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

/// [`run_probe`], but the child runs the statement instead of stopping at the plan.
fn run_probe_executing(depth: usize) -> Option<String> {
    run_probe_in(depth, Some("execute"))
}

/// Runs the probe in a child and answers what it printed, or `None` if it died.
fn run_probe(depth: usize) -> Option<String> {
    run_probe_in(depth, None)
}

fn run_probe_in(depth: usize, mode: Option<&str>) -> Option<String> {
    let mut command = Command::new(std::env::current_exe().expect("this test binary"));
    if let Some(mode) = mode {
        command.env(MODE, mode);
    }
    let output = command
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
            line.starts_with("LOWERED")
                || line.starts_with("EXECUTED")
                || line.starts_with("REFUSED")
                || line.starts_with("PARSE ")
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
/// A guard set too low would refuse ordinary SQL, which is the other way to fail this.
#[test]
fn an_ordinary_boolean_chain_still_lowers() {
    assert_eq!(run_probe(16).as_deref(), Some("LOWERED"));
}

/// **The whole client path survives the deepest plan this node will build** — on a worker's stack.
///
/// This is the claim the bound exists to make, and it is made about *execution* rather than about
/// lowering because that is where the second overflow was: guarding `lower_expr` alone left a
/// 500-term chain lowering happily and dying in the resolver. There are some forty recursive walks
/// over `plan::Expr` — the resolver, the type pass, the evaluator, the binder, the columnar
/// pushdown, the printer — and none of them carries a counter. None needs one: a tree that cannot
/// be deeper than `MAX_PLAN_DEPTH` cannot overflow a walk of it, including a walk nobody has
/// written yet.
///
/// Measured, and the margin is deliberate: the same path overflowed at **138** levels in a debug
/// build, and the bound is 42.
#[test]
fn the_deepest_plan_this_node_builds_executes_on_a_worker_stack() {
    let deepest = esker_sql::parse::MAX_PLAN_DEPTH - 1;
    assert_eq!(
        run_probe_executing(deepest).as_deref(),
        Some("EXECUTED"),
        "a chain at the limit did not survive the whole path on a {}-byte stack",
        WORKER_STACK_BYTES
    );
}

/// One past it is `54001`, and still not a crash.
#[test]
fn a_plan_deeper_than_the_bound_is_refused_rather_than_fatal() {
    for depth in [
        esker_sql::parse::MAX_PLAN_DEPTH,
        esker_sql::parse::MAX_PLAN_DEPTH + 1,
        1_000,
    ] {
        let answer = run_probe_executing(depth);
        assert!(
            answer.is_some(),
            "the child died executing a {depth}-term OR chain: invariant 9"
        );
        assert_eq!(answer.as_deref(), Some("REFUSED 54001"), "at depth {depth}");
    }
}
