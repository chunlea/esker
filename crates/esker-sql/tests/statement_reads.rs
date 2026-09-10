//! **What one statement reads, listed** — `docs/plans/debts-v1.1.md` #49's first question.
//!
//! Run 114's desktop projection put `transactions_test` at **519 ms a statement** on the real
//! topology, and `pk_and_sequence_for` at 447 statements × 1.05 s = 477 s = **26.5%** of it. r1
//! measured one of those at **35 round trips** — 23 point reads and 12 range scans — and taking
//! that to 3 buys back about 24% of the 26.5%, which leaves **three quarters of the cost in the
//! 907 ordinary statements nobody has priced**.
//!
//! A count cannot say what to do about either. A statement that reads the version key 35 times
//! and one that reads 35 different relations are the same number and different problems — one is
//! a cache and the other is a batch — so this prints **which key** each read touched, using
//! `stmt_stats`'s trace and `catalog::record::describe_key`.
//!
//! **Topology-independent on purpose.** Which keys a statement reads is a property of the catalog
//! code; only the *round-trip* count depends on the client and its buffer. So this runs on the
//! in-process node and prints the composition, and the round-trip number stays r1's to take on a
//! real cluster (`results/run-114-transactions-projection.md`, and run 117's per-statement tap).
//!
//! **A measurement, not a gate**: it prints and asserts only that the trace is not empty, because
//! an assertion on a count here would go red on any catalog change and say nothing about #49.
//! `#[ignore]` for the same reason `routing_differential`'s ADR 0102 measurement carries one, and
//! it needs `ESKER_STMT_STATS=1 ESKER_STMT_STATS_TRACE=1` to say anything at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `ActiveRecord`'s `pk_and_sequence_for`, verbatim from `postgresql/schema_statements.rb:382`.
const PK_AND_SEQUENCE_FOR: &str = "SELECT attr.attname, nsp.nspname, seq.relname \
     FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp \
     WHERE seq.oid = dep.objid AND seq.relkind = 'S' \
       AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid \
       AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] \
       AND seq.relnamespace = nsp.oid AND cons.contype = 'p' \
       AND dep.classid = 'pg_class'::regclass \
       AND dep.refobjid = '\"pk0\"'::regclass";

/// Prints one statement's reads, grouped and in order.
fn census(node: &mut parity::Node, label: &str, statement: &str) {
    // **Cleared first**, so that a statement reporting nothing means it read nothing rather than
    // that it never reached the instrument: transaction control does not go through the guard,
    // and without this it would report the statement before it.
    esker_sql::stmt_stats::clear_trace();
    node.run(statement).ok();
    let trace = esker_sql::stmt_stats::last_trace();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for line in &trace {
        *counts.entry(line.as_str()).or_default() += 1;
    }
    let points = trace.iter().filter(|line| line.starts_with("get")).count();
    let ranges = trace.len() - points;
    println!("\n=== {label}\n    {statement}");
    println!("    {} reads: {points} point, {ranges} range", trace.len());
    for (line, times) in counts {
        println!("      {times:3} x  {line}");
    }
    println!("    in order:");
    for (n, line) in trace.iter().enumerate() {
        println!("      {:3}. {line}", n + 1);
    }
}

#[test]
#[ignore = "a #49 measurement: wants ESKER_STMT_STATS=1 ESKER_STMT_STATS_TRACE=1"]
fn what_four_statements_read() {
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "set ESKER_STMT_STATS=1 and ESKER_STMT_STATS_TRACE=1, or this says nothing"
    );
    let mut node = parity::Node::new(&[
        "CREATE TABLE pk0 (id bigserial primary key, a int8, b text)",
        "INSERT INTO pk0 (a, b) VALUES (1, 'x')",
    ]);
    census(&mut node, "pk_and_sequence_for", PK_AND_SEQUENCE_FOR);
    census(
        &mut node,
        "a point select",
        "SELECT a FROM pk0 WHERE id = 1",
    );
    census(
        &mut node,
        "an insert returning",
        "INSERT INTO pk0 (a, b) VALUES (2, 'y') RETURNING id",
    );
    census(&mut node, "begin", "BEGIN");
    census(&mut node, "savepoint", "SAVEPOINT s1");
    census(&mut node, "release", "RELEASE SAVEPOINT s1");
    census(&mut node, "commit", "COMMIT");
    // Nothing is asserted about a count. What this test guarantees is that the instrument was on.
    esker_sql::stmt_stats::clear_trace();
    census(
        &mut node,
        "a point select, again",
        "SELECT a FROM pk0 WHERE id = 1",
    );
    assert!(!esker_sql::stmt_stats::last_trace().is_empty());
}
