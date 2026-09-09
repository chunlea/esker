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

use std::fmt::Write as _;

use std::process::Command;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What a tokio worker gets by default, and therefore the stack the guard has to be safe on.
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

/// The environment variable that turns this binary into the probe.
const DEPTH: &str = "ESKER_LOWER_DEPTH";
/// Which walk the probe should make: `lower` stops at the plan, `execute` runs the statement.
const MODE: &str = "ESKER_LOWER_MODE";
/// Which pathological shape to build. Depth is not one thing: `or` and `parens` are deep in the
/// source, while `in` and `row` are **flat** there and only become deep — if they do — inside
/// lowering. A guard that counts brackets sees the first pair and not the second, which is the
/// defect this file was opened for.
const SHAPE: &str = "ESKER_LOWER_SHAPE";

/// `a = 0 OR a = 1 OR …`, which is `depth` terms and **one** bracket.
fn or_chain(depth: usize) -> String {
    let mut sql = String::from("SELECT * FROM t WHERE ");
    for term in 0..depth {
        if term > 0 {
            sql.push_str(" OR ");
        }
        let _ = write!(sql, "a = {term}");
    }
    sql
}

/// `SELECT * FROM t WHERE a + 1 + 1 + … = 0`: no brackets, no boolean chain, and one
/// `plan::Expr` level per term.
///
/// **The shape that still tests the plan bound.** An `OR` chain used to be it, and is not any more:
/// the lowering folds a boolean chain into a balanced tree, so a thousand `OR`s are a dozen levels.
/// Arithmetic is not folded that way — it is not associative over the types this node has, where
/// `a + 1 + 1` and `a + (1 + 1)` can differ — so it stays one level per operator and is what a
/// guard on `plan::Expr` depth has to catch.
fn sum_chain(depth: usize) -> String {
    let mut sql = String::from("SELECT * FROM t WHERE a");
    for _ in 0..depth {
        sql.push_str(" + 1");
    }
    sql.push_str(" = 0");
    sql
}

/// `SELECT ((((… 1 …))))`: deep in the source, and every level a bracket.
fn parens(depth: usize) -> String {
    let mut sql = String::from("SELECT ");
    sql.push_str(&"(".repeat(depth));
    sql.push('1');
    sql.push_str(&")".repeat(depth));
    sql
}

/// `a IN (0, 1, …)`: one bracket and `depth` elements, flat in the source.
fn in_list(depth: usize) -> String {
    let mut sql = String::from("SELECT * FROM t WHERE a IN (");
    for value in 0..depth {
        if value > 0 {
            sql.push(',');
        }
        let _ = write!(sql, "{value}");
    }
    sql.push(')');
    sql
}

/// `(a, a, …) = (0, 1, …)`: two brackets and `depth` columns, flat in the source — and a row
/// comparison is *defined* as a conjunction over its columns, so lowering is where the depth
/// would appear if it appears anywhere.
fn row_value(depth: usize) -> String {
    let mut left = String::new();
    let mut right = String::new();
    for column in 0..depth {
        if column > 0 {
            left.push(',');
            right.push(',');
        }
        left.push('a');
        let _ = write!(right, "{column}");
    }
    format!("SELECT * FROM t WHERE ({left}) = ({right})")
}

/// `((((SELECT 1))))`: a **query** in parentheses, which is not an expression in them — the
/// grammar merges the layers rather than nesting them, and lowering peels them. Deep in the source,
/// so the scanner sees it; what the scanner admits, lowering has to take on one frame.
fn query_parens(depth: usize) -> String {
    format!("{}SELECT 1{}", "(".repeat(depth), ")".repeat(depth))
}

/// `SELECT 1 UNION ALL SELECT 1 UNION ALL …`: `depth` arms and **no bracket at all**, so the
/// scanner's count sees nothing and the parser leans the tree left one level per operator. The
/// fifth shape, and the second that is flat where the scanner looks.
fn union_chain(depth: usize) -> String {
    let mut sql = String::from("SELECT 1");
    for _ in 1..depth {
        sql.push_str(" UNION ALL SELECT 1");
    }
    sql
}

/// The statement this probe should build, by shape.
fn statement(depth: usize) -> String {
    match std::env::var(SHAPE).as_deref() {
        Ok("parens") => parens(depth),
        Ok("query_parens") => query_parens(depth),
        Ok("union") => union_chain(depth),
        Ok("in") => in_list(depth),
        Ok("row") => row_value(depth),
        Ok("sum") => sum_chain(depth),
        _ => or_chain(depth),
    }
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
            let sql = statement(depth);
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
    run_probe_shaped(depth, mode, None)
}

fn run_probe_shaped(depth: usize, mode: Option<&str>, shape: Option<&str>) -> Option<String> {
    let mut command = Command::new(std::env::current_exe().expect("this test binary"));
    if let Some(mode) = mode {
        command.env(MODE, mode);
    }
    if let Some(shape) = shape {
        command.env(SHAPE, shape);
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
/// `100_000` terms is far past anything a guard could admit and far past what 2 MiB can hold — it is
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
        "a chain at the limit did not survive the whole path on a {WORKER_STACK_BYTES}-byte stack"
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
        let answer = run_probe_shaped(depth, Some("execute"), Some("sum"));
        assert!(
            answer.is_some(),
            "the child died executing a {depth}-term arithmetic chain: invariant 9"
        );
        assert_eq!(answer.as_deref(), Some("REFUSED 54001"), "at depth {depth}");
    }
}

/// **Four pathological shapes, ten thousand deep, none of them a crash.**
///
/// Invariant 9 says never panic on user input, and a stack overflow is a panic that takes the
/// process with it. The shapes differ in *where* their depth lives, which is the whole reason to
/// test more than one:
///
/// * `parens` and `or` are deep **in the source**. The parser's own bracket count sees the first;
///   the second has one bracket and an N-deep tree, which is the miss this file was opened for.
/// * `in` and `row` are **flat** in the source. A guard counting brackets sees nothing at all, so
///   whether they are safe depends on what lowering does with them — and a row comparison is
///   defined as a conjunction over its columns, so it is the one shape whose depth is *created*
///   after the parser has finished counting.
///
/// The assertion is deliberately not "54001": a shape this node does not implement may answer
/// `0A000` instead, and that is equally not a crash. What is asserted is that the child **reached
/// the end of the probe and printed an outcome**, because the only failure this test can have is
/// the child dying — `None` here is `SIGABRT`.
///
/// What the four answered when this was written, which is worth recording because two of them are
/// not what a reader would guess:
///
/// | shape | at ten thousand |
/// |---|---|
/// | `parens` | `PARSE 54001` — the parser's bracket count |
/// | `or` | `PARSE 54001` — deep enough that the parser catches it before lowering does |
/// | `in` | **`LOWERED`, then `EXECUTED`** — refused by nothing, because it is flat at every layer |
/// | `row` | **`REFUSED 0A000`** — row comparisons are not implemented, so this shape is turned
///   away before any depth guard sees it |
///
/// So `row` proves nothing about depth *yet*, and that is the point of leaving it here: a row
/// comparison is defined as a conjunction over its columns, so the day it is implemented this
/// shape stops being flat and starts being ten thousand deep — after the parser has finished
/// counting brackets. This test will be waiting for it.
#[test]
fn every_pathological_shape_at_ten_thousand_is_an_answer_and_not_a_crash() {
    const DEEP: usize = 10_000;

    for shape in ["parens", "or", "in", "row", "query_parens", "union"] {
        let outcome = run_probe_shaped(DEEP, None, Some(shape));
        let text = outcome.unwrap_or_else(|| {
            panic!(
                "the `{shape}` probe at {DEEP} did not survive: the child died rather than \
                 answering, which is a stack overflow and invariant 9 forbids it"
            )
        });
        assert!(
            !text.is_empty(),
            "the `{shape}` probe printed nothing, so it is not known which phase it reached"
        );
        println!("{shape} at {DEEP}: {text}");

        // **A shape that lowers is not a shape that is safe.** `in` is refused by nothing — it is
        // flat in the source and stays flat through lowering — so the plan it produces is then
        // walked by the resolver, the type pass and the evaluator, each recursive and each on the
        // worker's own 2 MiB. Stopping at `LOWERED` would be checking the one phase that had
        // already answered.
        if text.starts_with("LOWERED") {
            let executed = run_probe_shaped(DEEP, Some("execute"), Some(shape)).unwrap_or_else(|| {
                panic!(
                    "the `{shape}` probe lowered at {DEEP} and then died executing: the guard is                      in the wrong place if a plan it admits cannot be walked"
                )
            });
            println!("{shape} at {DEEP}, executed: {executed}");
        }
    }
}

/// **The two shapes that are deep after the scanner has finished counting, at the depth it
/// admits.** A bracketed query is peeled by `lower_parenthesised` and a set-operation chain is
/// walked by `set_operation::lower`; both used to recurse once per level on the caller's stack,
/// which the probe at ten thousand never reached — the scanner refuses first. This is the other
/// side of that bound: what the scanner admits has to lower, and then run, on a worker's 2 MiB.
#[test]
fn the_admissible_depth_of_a_query_paren_and_a_union_chain_lowers_and_runs() {
    let deepest = esker_sql::parse::MAX_NESTING_DEPTH - 1;
    for shape in ["query_parens", "union"] {
        assert_eq!(
            run_probe_shaped(deepest, None, Some(shape)).as_deref(),
            Some("LOWERED"),
            "{shape} at {deepest} did not lower on a worker stack"
        );
        assert_eq!(
            run_probe_shaped(deepest, Some("execute"), Some(shape)).as_deref(),
            Some("EXECUTED"),
            "{shape} at {deepest} lowered and then did not run"
        );
    }
}

/// **A thousand `OR`s is a chain, not a depth.**
///
/// `or_test.rb`'s *too many or*: Active Record ORs 1001 relations into one predicate and counts the
/// rows. PostgreSQL 19 answers a number — measured, a flat chain of twenty thousand terms is fine
/// there and so is five thousand levels of brackets — and this node answered `54001 stack depth
/// limit exceeded`, because `a OR b OR c` parses leaning left and the lowering built one
/// `plan::Expr` level per term.
///
/// The bound it hit is real and stays: [`esker_sql::parse::MAX_PLAN_DEPTH`] is what keeps forty
/// recursive walks over `plan::Expr` off the end of a `tokio` worker's stack. What was wrong is
/// that a chain of a thousand *siblings* was being counted as a thousand *generations*.
#[test]
fn a_thousand_ors_is_a_chain_and_not_a_depth() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE paragraphs (id bigint primary key, book_id bigint)",
        "INSERT INTO paragraphs VALUES (1, 1), (2, 4), (3, 9), (4, 17)",
    ]);
    // The shape Active Record sends: `id = $1 AND book_id = $2 OR id = $3 AND …`, no brackets
    // around the pairs, 1001 of them.
    let terms = (0..1001)
        .map(|i| format!("id = {i} AND book_id = {}", i * i))
        .collect::<Vec<_>>()
        .join(" OR ");
    assert_eq!(
        node.rows(&format!("SELECT COUNT(*) FROM paragraphs WHERE {terms}")),
        vec![vec!["3".to_string()]],
        "rows 1, 2 and 3 match `book_id = id * id`; row 4 does not"
    );
}
