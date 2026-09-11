//! **#54's measurement, before the fix.** `pg_class ⋈ pg_depend` at three catalog sizes, and the
//! plan it takes.
//!
//! `catalog_read_slope.rs`'s `a_repeated_statement_stops_tracking_the_catalog` is `#[ignore]`d on
//! this, with b4's numbers in its header:
//!
//! ```text
//! SELECT count(*) FROM pg_class seq, pg_depend dep
//!                WHERE seq.oid = dep.objid       1552 us -> 24646 us   15.9x  (5x the catalog)
//! ```
//!
//! 15.9× for 5× the catalog is the curve of a product, and the read count is **flat** — so it is
//! not reads. This file is the measurement that says which of the two shapes it is, because the
//! fix differs:
//!
//! * **a nested loop that re-scans the inner side per outer row** — then the reads would grow too,
//!   and they do not;
//! * **the equality not being used as a lookup key** — the inner side read once, every outer row
//!   paired against all of it, and the join condition applied as a *filter* on the pairs. Then the
//!   reads are flat and the comparisons are `outer × inner`.
//!
//! Three sizes rather than two, because two points cannot tell a slope from a constant: 20, 100 and
//! 400 relations are 1×, 5× and 20×, and a product predicts 1 : 25 : 400 where a lookup predicts
//! 1 : 5 : 20.
//!
//! This file is **not** an acceptance test and asserts no bound. It prints, and it is `#[ignore]`d
//! so it never costs the gate the 400-relation build. The acceptance is
//! `a_repeated_statement_stops_tracking_the_catalog`, which loses its own `#[ignore]` when this is
//! fixed.
//!
//! # What it measured, 2026-09-11, and it is neither of the two shapes above
//!
//! ```text
//!   n     scan       comma+WHERE      product (no ON)   JOIN..ON
//!  20   286.0 us       1.57 ms          2.28 ms         290.6 us
//! 100   706.1 us      24.96 ms         31.14 ms         974.0 us
//! 400     2.73 ms     352.37 ms       454.90 ms           3.55 ms
//! ```
//!
//! **The explicit `JOIN … ON` is already linear** — 291 µs → 974 µs → 3.55 ms, tracking the plain
//! catalog scan beside it almost exactly, and costing about one scan at every size. The comma form
//! is quadratic and costs very nearly what the *bare cross product* costs: 352 ms against 455 ms
//! at four hundred relations.
//!
//! So the executor already does the right thing with an equijoin. **The whole of the defect is that
//! a comma join's `WHERE` equality never becomes a join condition**, and `EXPLAIN` says so in one
//! line of difference:
//!
//! ```text
//! FROM pg_class seq, pg_depend dep WHERE seq.oid = dep.objid
//!     Filter                          <- above the loop: n x n pairs are built, then filtered
//!       Condition: (oid = objid)
//!       Nested Loop
//!         Inner: Materialize on pg_depend
//!
//! FROM pg_class seq JOIN pg_depend dep ON seq.oid = dep.objid
//!     Nested Loop
//!       Inner: Materialize on pg_depend
//!             Join Filter: (oid = objid)   <- on the join
//! ```
//!
//! That matters for the size of the fix: **no hash probe needs building.** What needs building is
//! the recognition — a top-level `WHERE` equality between the two sides of a comma join is a join
//! condition and belongs on the join. `pk_and_sequence_for` is `ActiveRecord`'s own query and it is
//! written with commas, which is why this is the shape that hurts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use std::time::{Duration, Instant};

/// The join b4's census isolated: two computed catalog views and an equality between them.
const JOIN: &str = "SELECT count(*) FROM pg_class seq, pg_depend dep WHERE seq.oid = dep.objid";

/// One view alone, so a slope in the join can be told from a slope in walking a catalog at all.
const SCAN: &str = "SELECT count(*) FROM pg_class";

/// The same join written as an explicit `JOIN … ON`, which is the same relational algebra. Where it
/// differs from [`JOIN`] is where the condition ends up, and whether that changes the cost is the
/// question that decides how big the fix is.
const EXPLICIT: &str =
    "SELECT count(*) FROM pg_class seq JOIN pg_depend dep ON seq.oid = dep.objid";

/// The same two views with **no** join condition — an honest cross product, so the measurement has
/// the shape it is testing *for* beside the shape it is testing.
const PRODUCT: &str = "SELECT count(*) FROM pg_class seq, pg_depend dep";

fn grow_to(node: &mut parity::Node, target: usize, made: &mut usize) {
    while *made < target {
        node.run(&format!(
            "CREATE TABLE pk{made} (id bigserial primary key, a int8, b text)"
        ))
        .unwrap();
        *made += 1;
    }
}

/// The **second** run of `sql`, so a cache that fills on the first is filled.
fn elapsed(node: &mut parity::Node, sql: &str) -> Duration {
    node.rows(sql);
    let start = Instant::now();
    node.rows(sql);
    start.elapsed()
}

#[test]
#[ignore = "a measurement, not an acceptance test: it builds 400 relations and asserts no bound"]
fn the_shape_of_the_catalog_join_at_three_sizes() {
    let mut node = parity::Node::new(&[]);
    let mut made = 0;
    let mut rows = Vec::new();

    for target in [20_usize, 100, 400] {
        grow_to(&mut node, target, &mut made);
        let scan = elapsed(&mut node, SCAN);
        let join = elapsed(&mut node, JOIN);
        let product = elapsed(&mut node, PRODUCT);
        let explicit = elapsed(&mut node, EXPLICIT);
        // The answer, so a plan that got cheap by getting wrong is visible here rather than in a
        // ratio that looks excellent.
        let answer = node.rows(JOIN);
        rows.push((target, scan, join, product, explicit, answer));
    }

    println!("\n  n     scan       comma+WHERE      product (no ON)   JOIN..ON        rows");
    for (target, scan, join, product, explicit, answer) in &rows {
        println!(
            "{target:5}   {scan:>8?}   {join:>14?}   {product:>15?}   {explicit:>12?}   {answer:?}"
        );
    }
    // The ratios against the smallest, which is what separates the two shapes: a product predicts
    // 1 : 25 : 400 across these sizes and a lookup predicts 1 : 5 : 20.
    #[allow(
        clippy::cast_precision_loss,
        reason = "microseconds of a test that prints; the ratio is read by a human"
    )]
    let base = rows[0].2.as_micros().max(1) as f64;
    println!("\n  join, relative to n=20:");
    #[allow(
        clippy::cast_precision_loss,
        reason = "microseconds of a test that prints; the ratio is read by a human"
    )]
    for (target, _, join, _, _, _) in &rows {
        println!(
            "{target:5}   {:.1}x   (a product would be {:.0}x)",
            join.as_micros() as f64 / base,
            (*target as f64 / 20.0).powi(2)
        );
    }
}

/// **The plan, which is the other half of the question.** A number says a shape is wrong; the plan
/// says which shape it is, and `EXPLAIN` is where this node writes it down.
#[test]
fn the_catalog_join_says_in_explain_what_it_does() {
    let mut node = parity::Node::new(&[]);
    let mut made = 0;
    grow_to(&mut node, 20, &mut made);

    for sql in [JOIN, EXPLICIT] {
        let plan = node
            .rows(&format!("EXPLAIN {sql}"))
            .into_iter()
            .map(|row| row.join(" "))
            .collect::<Vec<_>>()
            .join("\n");
        println!("\nEXPLAIN {sql}\n{plan}");
    }

    let plan = node
        .rows(&format!("EXPLAIN {JOIN}"))
        .into_iter()
        .map(|row| row.join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    println!("\nEXPLAIN {JOIN}\n{plan}\n");
    assert!(
        !plan.is_empty(),
        "EXPLAIN said nothing, so this measurement has no plan half"
    );
}
