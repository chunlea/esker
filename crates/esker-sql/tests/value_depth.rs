//! **Invariant 9 inside a value**: a deeply nested `jsonb` or `tsquery` is refused, never a crash —
//! debt #101.
//!
//! The four bounds this node has are all on **statement text**: `MAX_NESTING_DEPTH` counts the
//! brackets a scanner sees, `MAX_BOOLEAN_CHAIN` its `OR`s, a third count its set operators, and
//! `MAX_PLAN_DEPTH` the lowered `plan::Expr`. That is a deliberate design and a good one —
//! `MAX_PLAN_DEPTH`'s own doc says why: *a tree that cannot be deeper than this cannot overflow any
//! of them, including the ones not written yet*. **The hole is that the scanner never looks inside a
//! string literal**, which is equally deliberate: it is how `UNION` as a column name is counted and
//! a comment is not. So `'[[[[…]]]]'::jsonb` is **one token at depth one**, and every tree behind it
//! is unbounded — `value::json`'s recursive descent and its walkers, `value::tsquery`'s and the six
//! walks over the `Node` it builds, and `value::xml`, `value::ltree`, `value::regex`, `value::range`.
//!
//! PostgreSQL 19beta1, measured in `esker-coord/s2-h101.out`: a `jsonb` nested **10,000** deep is
//! answered and **100,000** is `54001 stack depth limit exceeded`; a `tsquery` of 10,000 is that
//! same `54001`. Both halves are asserted here, because contract C1 tolerates a limit and not a
//! *low* one: refusing what a real server answers is as wrong as crashing on what it refuses.
//!
//! # Why this file re-executes itself
//!
//! The same reason `lowering_depth.rs` does: a stack overflow is not catchable. Rust's handler
//! prints and calls `abort`, taking the test process with it, so the probe runs in a **child** and
//! the parent reads its exit status. **A dead child is the red this file is opened for** — before
//! the fix, `run_probe` answers `None` for every deep shape below.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What a tokio worker gets by default, and therefore the stack the guard has to be safe on.
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

const DEPTH: &str = "ESKER_VALUE_DEPTH";
/// Which value's nesting to build. Each is a *value* and not statement text: the node sees one
/// string literal and a cast, and the whole tree is inside it.
const SHAPE: &str = "ESKER_VALUE_SHAPE";

/// One deep value, **written into the statement as a literal**.
///
/// The oracle captures built these with `repeat('[', n) || … ` so that the *statement* stayed short,
/// and the first run of this file proved that shape cannot be used here: every probe came back
/// `REFUSED 0A000`, so the statement was stopped by a feature gap — a computed value cast at
/// runtime — and never reached the parser this file is about. A probe that dies before the
/// mechanism it is aimed at measures the wrong thing, and it measured it twice: two tests "passed"
/// because the child had been refused rather than killed.
///
/// A literal is also the faithful shape. **A string literal is exactly what the scanner does not
/// look inside** — deliberately, since that is how `UNION` as a column name is counted and a comment
/// is not — so `'[[[[…]]]]'::jsonb` is one token at depth one, which is the hole #101 names. The
/// statement is large (about two bytes a level) and that is the point rather than a cost.
fn nested(depth: usize, open: &str, middle: &str, close: &str, cast: &str) -> String {
    let mut sql = String::with_capacity(depth * (open.len() + close.len()) + 64);
    sql.push_str("SELECT '");
    for _ in 0..depth {
        sql.push_str(open);
    }
    sql.push_str(middle);
    for _ in 0..depth {
        sql.push_str(close);
    }
    sql.push_str("'::");
    sql.push_str(cast);
    sql.push_str(" IS NOT NULL");
    sql
}

fn json_array(depth: usize) -> String {
    nested(depth, "[", "1", "]", "jsonb")
}

fn json_object(depth: usize) -> String {
    nested(depth, "{\"a\":", "1", "}", "jsonb")
}

fn tsquery(depth: usize) -> String {
    nested(depth, "a&(", "b", ")", "tsquery")
}

fn statement(depth: usize) -> String {
    match std::env::var(SHAPE).as_deref() {
        Ok("object") => json_object(depth),
        Ok("tsquery") => tsquery(depth),
        _ => json_array(depth),
    }
}

/// The set when the probe should call the parser **in stages** instead of running a statement.
const STAGED: &str = "ESKER_VALUE_STAGED";
/// The stack the staged probe runs on, in bytes; a worker's 2 MiB when unset.
const STACK: &str = "ESKER_VALUE_STACK";

/// The `tsquery` value itself — `a&(a&(…b…))` — with no statement around it.
fn tsquery_value(depth: usize) -> String {
    let mut text = String::with_capacity(depth * 4 + 8);
    for _ in 0..depth {
        text.push_str("a&(");
    }
    text.push('b');
    for _ in 0..depth {
        text.push(')');
    }
    text
}

/// **Which step dies**, printed one marker at a time.
///
/// A whole statement says only that the child died; it cannot say whether the parse, the render or
/// the **drop** was the frame that ran out — and those three have wanted different fixes twice in
/// this row already. `esker_sql::value::tsquery` is a public module, so the three steps can be taken
/// apart here and each one can announce itself before the next begins. Whatever the last marker is,
/// the step after it is the one that died.
fn staged_probe(depth: usize) -> ! {
    // **Which stack, as well as which step.** The staged probe calls the parser directly, so it
    // measures the raw cost rather than what `value::tsquery`'s wrapped entries get. Running it at
    // both sizes is what separates "the sized thread is too small" from "the sized thread is never
    // reached" — two diagnoses that want opposite fixes.
    let stack = std::env::var(STACK)
        .ok()
        .and_then(|bytes| bytes.parse().ok())
        .unwrap_or(WORKER_STACK_BYTES);
    let handle = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let text = tsquery_value(depth);
            println!("BUILT {}", text.len());
            match esker_sql::value::tsquery::from_text(&text) {
                Ok(node) => {
                    println!("PARSED");
                    let rendered = esker_sql::value::tsquery::to_text(&node);
                    println!("RENDERED {}", rendered.len());
                    drop(node);
                    println!("DROPPED");
                }
                Err(error) => println!("REFUSED {}", error.sqlstate()),
            }
        })
        .expect("the probe thread starts");
    handle.join().expect("the probe thread finishes");
    std::process::exit(0);
}

/// Runs the staged probe in a child and answers every marker it managed to print.
fn run_staged(depth: usize, stack: usize) -> Vec<String> {
    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .env(DEPTH, depth.to_string())
        .env(STAGED, "1")
        .env(STACK, stack.to_string())
        .arg("--exact")
        .arg("the_probe_entry_point")
        .arg("--nocapture")
        .output()
        .expect("the child runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            ["BUILT", "PARSED", "RENDERED", "DROPPED", "REFUSED"]
                .iter()
                .any(|marker| line.starts_with(marker))
        })
        .map(str::to_owned)
        .collect()
}

/// **The diagnostic**: at a depth that kills the child, name the step it died in.
///
/// Not an assertion about the fix — an instrument. It prints what it found and only fails if the
/// child survived every step, because then there is nothing to diagnose and the test above it is
/// the one that should be speaking.
#[test]
fn which_step_dies_on_a_worker_stack() {
    // The sized thread `value::tsquery` hands a deep value to: MAX_VALUE_DEPTH * 12 KiB + 4 MiB.
    // **Copied rather than derived**, because `DEEP_VALUE_STACK_BYTES` is private to `value`. That
    // is why this figure drifts from the constant whenever the per-level cost moves; it drifted
    // once already, and the comment said 4 KiB while the line below said 12.
    let deep_stack = 10_000 * 12 * 1024 + 4 * 1024 * 1024;
    for stack in [WORKER_STACK_BYTES, deep_stack] {
        for depth in [1_000, 5_000, 10_000] {
            let markers = run_staged(depth, stack);
            let reached = markers.last().map_or("nothing", String::as_str);
            println!(
                "tsquery {depth} on {}MiB: {} -> died after {reached}",
                stack / (1024 * 1024),
                markers.join(" ")
            );
        }
    }
    let deep = run_staged(10_000, deep_stack);
    assert!(
        deep.last()
            .is_some_and(|last| last.starts_with("DROPPED") || last.starts_with("REFUSED")),
        "10,000 levels died after {:?} — that is the step to fix, and the markers above say which",
        deep.last()
    );
}

/// The probe: run the statement on a worker-sized stack and say what happened.
///
/// Exits 0 either way — an answer and a refusal are both correct outcomes, and the only incorrect
/// one is not reaching the last line at all.
fn probe(depth: usize) -> ! {
    let handle = std::thread::Builder::new()
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let sql = statement(depth);
            let mut node = parity::Node::new(&[]);
            match node.run(&sql) {
                Ok(_) => println!("ANSWERED"),
                Err(error) => println!("REFUSED {}", error.sqlstate()),
            }
        })
        .expect("the probe thread starts");
    handle.join().expect("the probe thread finishes");
    std::process::exit(0);
}

/// Runs the probe in a child and answers what it printed, or `None` if it died.
fn run_probe(depth: usize, shape: &str) -> Option<String> {
    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .env(DEPTH, depth.to_string())
        .env(SHAPE, shape)
        .arg("--exact")
        .arg("the_probe_entry_point")
        .arg("--nocapture")
        .output()
        .expect("the child runs");
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.starts_with("ANSWERED") || line.starts_with("REFUSED"))
        .map(str::to_owned)
}

/// Not a test: the entry point the child re-enters through. It exits before asserting anything.
///
/// **Two probes behind one entry**, and the dispatch is the part worth getting right: a staged child
/// that fell through to the statement probe would print `ANSWERED` where the caller was filtering
/// for `PARSED`/`RENDERED`/`DROPPED`, so every depth would report "died after nothing" — an
/// instrument that looks confident and measures the wrong thing, which this row has already been
/// fooled by once.
#[test]
fn the_probe_entry_point() {
    if let Ok(depth) = std::env::var(DEPTH) {
        let depth = depth.parse().expect("a depth");
        if std::env::var(STAGED).is_ok() {
            staged_probe(depth);
        }
        probe(depth);
    }
}

/// **The invariant, stated as plainly as it can be**: whatever the value, the child comes back.
///
/// This is the assertion that goes red before the fix, and `None` is what a stack overflow looks
/// like from here — the child took `SIGABRT` and printed nothing this side could read.
#[test]
fn a_deeply_nested_value_never_kills_the_process() {
    for (shape, depth) in [
        ("array", 100_000),
        ("object", 100_000),
        ("array", 1_000_000),
    ] {
        assert!(
            run_probe(depth, shape).is_some(),
            "a {shape} nested {depth} deep killed the child: a stack overflow is not an answer \
             (invariant 9)"
        );
    }
}

/// **And the answer is the one a real server gives**: `54001`, the same condition PostgreSQL raises
/// when `max_stack_depth` is exceeded.
#[test]
fn a_value_too_deep_is_54001_and_not_something_else() {
    for (shape, depth) in [("array", 100_000), ("object", 100_000)] {
        assert_eq!(
            run_probe(depth, shape).as_deref(),
            Some("REFUSED 54001"),
            "{shape} at {depth}"
        );
    }
}

/// **The measurement the bound is derived from, and the guard that keeps it honest.**
///
/// The depth at which the child *dies* is a stack's own limit, and the bound has to sit below it.
/// **Which stack, though, is the part the first run of this got wrong.** Once a deep value is parsed
/// on a thread sized for the bound, the parse is no longer what dies: the tree comes back and is
/// walked and **dropped** on the caller's 2 MiB, one frame per level, so what this bisection finds
/// afterwards is the *drop* ceiling and not the parse's. The arithmetic below divides by the
/// caller's stack for that reason, and says so.
///
/// So: double until the child dies, then bisect, and report where. **Before the fix this finds a
/// boundary and fails**; after it, no depth kills the child and the search runs out, which is the
/// permanent assertion — a refusal is always reached before any stack is.
///
/// `tsquery` is not probed here: its half of #101 is unpaid and lives in the `#[ignore]`d test below.
#[test]
fn the_bound_is_below_the_depth_at_which_a_stack_dies() {
    for shape in ["array"] {
        let died = |depth: usize| run_probe(depth, shape).is_none();

        let mut ceiling = None;
        let mut depth = 1_000;
        while depth <= 4_000_000 {
            if died(depth) {
                ceiling = Some(depth);
                break;
            }
            depth *= 2;
        }

        // Nothing in the range killed it: every depth was answered or refused, which is what the
        // bound is for. Four million levels is a multi-megabyte statement and the end of the search.
        let Some(found) = ceiling else { continue };

        let (mut low, mut high) = (found / 2, found);
        while high - low > high / 20 {
            let middle = low + (high - low) / 2;
            if died(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        let per_level = WORKER_STACK_BYTES / high.max(1);
        panic!(
            "a {shape} nested {high} deep killed the child, so a stack is reached before any \
             refusal is: about {per_level} bytes a level against the caller's \
             {WORKER_STACK_BYTES}-byte stack — which is the **drop** cost once the parse itself has \
             moved to a sized thread, since the tree is freed one frame per level wherever it is \
             last owned. See `scratchpad/h/101-design.md`."
        );
    }
}

/// **The other half of contract C1: the limit must not be a low one.**
///
/// 19beta1 answers a `jsonb` nested ten thousand deep (`esker-coord/s2-h101.out`), so refusing it
/// here would be this node being *stricter* than the server it copies — which the contract treats as
/// a defect of the same family as crashing, and which is the reason the bound is derived from a
/// measured per-level cost rather than picked. **This is the assertion that ruled out the cheap
/// route**: a constant bound inside a worker's 2 MiB holds about 375 levels, and this test is what
/// says that trading the crash for such a bound would not have been a fix.
#[test]
fn a_value_postgresql_19_answers_is_answered_here() {
    assert_eq!(
        run_probe(10_000, "array").as_deref(),
        Some("ANSWERED"),
        "19beta1 answers this one, so a refusal here is a limit that is too low"
    );
}

/// **The tsquery half of #101, and it is not paid.** `#[ignore]` because it fails, not because it is
/// slow: the row is committed as *partial payment* with json's half done.
///
/// What is measured. 19beta1 answers `a&(…)` nested **6,000** and refuses **10,000** with `54001`
/// (`esker-coord/s2-h101c.out`). Here a tsquery of ten thousand **kills the child** — in parallel
/// and, when that was checked, **serially too**, so the probes starving each other is not the
/// explanation. A twenty-thousand query should be the bound's own `54001`, and the counterfactual
/// showed the bound is otherwise unreached, so this test is also the only thing that would redden if
/// it were removed.
///
/// **One difference is measured and deliberately not claimed as a cause.** Putting four
/// `crate::error::` path prefixes back into `value::on_a_deep_stack` — a change clippy asks to
/// remove and which cannot alter a stack — took the same file from three reds to 6/6 green. A path
/// prefix does not change a stack size, and these probes sit near the margin (a tsquery level is
/// five frames and the 124 MiB thread was sized from a bracket that may be optimistic), so
/// **flakiness at the edge explains the evidence just as well**. Separating them is a few runs of
/// each form, not an argument, and until that is done nothing here is attributed.
#[test]
fn a_deep_tsquery_is_refused_and_never_kills_the_child() {
    assert!(
        run_probe(10_000, "tsquery").is_some(),
        "a tsquery of ten thousand must be an answer or a refusal, never a dead child"
    );
    assert_eq!(
        run_probe(20_000, "tsquery").as_deref(),
        Some("REFUSED 54001"),
        "a tsquery past MAX_VALUE_DEPTH is the bound's own answer"
    );
}
