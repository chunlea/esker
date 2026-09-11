//! **What `ActiveRecord`'s column introspection reads, and whether it grows with the catalog** —
//! debt #63(c).
//!
//! `postgresql_adapter.rb:1058`'s `column_definitions` is the statement the adapter sends before it
//! can describe any table at all, and r1 priced it at **13.06 s per statement** inside
//! `array_test.rb`'s window on run 127 attempt 3 (785 of them in the pass, 2,990 ms mean over the
//! whole round). It asks for **one table's columns** and this node charges it for the whole tenant:
//! `catalog::pg_catalog`'s `pg_attribute` row source builds a row for every column of every
//! relation and the executor then filters by `attrelid = '…'::regclass`.
//!
//! # Why this only became visible on 2026-09-11
//!
//! It used to look free. h1's #61/#63 work stopped `Relations::read` hydrating every relation, and
//! the numbers moved like this (BACKGROUND=300, one real cluster, `esker-coord/h1-63.md` §4b):
//!
//! ```text
//! shape                     before                    after
//! pg_class listing          307 reads + 305 scans     313 reads +   4 scans     -62% wall
//! DROP EXTENSION CASCADE    307 reads + 314 scans     316 reads +  16 scans     -58% wall
//! column introspection        6 reads +   0 scans     323 reads + 307 scans     <- this file
//! ```
//!
//! **The hydration cost did not appear; it changed which statement pays it.** Before, the
//! `pg_class` listing hydrated all 300 tables into the catalog cache, so the introspection that
//! followed was a cache hit — its "6 reads, 0 scans" was somebody else's bill. After, the listing
//! caches records only, so the first statement that needs the derived half pays, and that is this
//! one. The win on the other two shapes is real; this one's was never real.
//!
//! # What is asserted, and why it is the **first** run of the statement
//!
//! Measured on this tree, 2026-09-11, one fresh session per size — the statement asks about the
//! same six-column table every time:
//!
//! ```text
//! relations   first run              second run
//!   20         56 reads  2.17 ms      4 reads  1.09 ms
//!  100        216 reads  3.89 ms      4 reads  1.66 ms
//!  300        616 reads 10.09 ms      4 reads  3.19 ms
//! ```
//!
//! **The first run reads `2n + 16` keys for a catalog of `n` relations** and answers six rows. The
//! second run reads four, because the version-keyed cache of #49 (b) holds what the first one
//! materialised — which is the same masking h1's arm hit from the other side, one layer up: a
//! number that looks flat because something else already paid.
//!
//! So the assertion is on the **first** statement in a session, which is the one an
//! `ActiveRecord` process actually sends: the adapter describes a table once per connection and
//! per schema reload, not in a loop. And the second run is printed beside it, because its wall
//! clock still grows — 1.09 → 3.19 ms — while its read count does not: the row source is still
//! **building** a row per column of every relation out of the cache and filtering afterwards. The
//! reads are the visible half of that; the rest is CPU no counter here can see.
//!
//! Red today at 56 → 216.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **`ActiveRecord`'s `column_definitions`, verbatim**, with the table name left to the caller.
/// The same text `tests/collation.rs` carries, which took it from `postgresql_adapter.rb:1058` —
/// nine projected columns over four catalog relations plus `col_description`, because a join that
/// works on its own can still be the one a nine-column projection cannot plan.
fn column_definitions(table: &str) -> String {
    format!(
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), \
         pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod, \
         c.collname, col_description(a.attrelid, a.attnum) AS comment, \
         attidentity AS identity, attgenerated as attgenerated \
         FROM pg_attribute a \
         LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
         LEFT JOIN pg_type t ON a.atttypid = t.oid \
         LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation \
         WHERE a.attrelid = '\"{table}\"'::regclass \
         AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
    )
}

/// The table the statement asks about: six columns, and it never changes while the catalog grows.
const SUBJECT: &str = "CREATE TABLE holder (id bigserial PRIMARY KEY, name character varying, \
     settings character varying(1024), tag character(3), note text NOT NULL, code integer DEFAULT 7)";

fn grow_to(node: &mut parity::Node, target: usize, made: &mut usize) {
    while *made < target {
        node.run(&format!(
            "CREATE TABLE bg{made} (id bigserial primary key, a int8, b text)"
        ))
        .unwrap();
        *made += 1;
    }
}

/// The keys the **first** run of `sql` reads in a fresh session, with what it cost and what it
/// answered. First, because that is the run an adapter sends: it describes a table once per
/// connection, and the second run is served by the version-keyed cache whatever the first one had
/// to do.
fn first_run(size: usize) -> (usize, usize, std::time::Duration, std::time::Duration) {
    let mut node = parity::Node::new(&[SUBJECT]);
    let mut made = 0;
    grow_to(&mut node, size, &mut made);
    let sql = column_definitions("holder");

    esker_sql::stmt_stats::clear_trace();
    let start = std::time::Instant::now();
    let rows = node.rows(&sql);
    let cold_took = start.elapsed();
    let cold = esker_sql::stmt_stats::last_trace().len();

    // The answer, so a plan that got cheap by getting wrong fails here rather than passing
    // quietly: six columns, in `attnum` order.
    assert_eq!(
        rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
        ["id", "name", "settings", "tag", "note", "code"],
        "the six columns of the table the statement names, over {size} relations"
    );

    esker_sql::stmt_stats::clear_trace();
    let start = std::time::Instant::now();
    node.rows(&sql);
    let warm_took = start.elapsed();
    let warm = esker_sql::stmt_stats::last_trace().len();
    (cold, warm, cold_took, warm_took)
}

/// **One table's columns cost one table's columns.**
///
/// Six columns and a catalog of twenty relations, then the same six columns and a catalog of a
/// hundred: the statement names one table by `regclass`, so what it reads must not move.
///
/// **Red today at 56 → 216**, which is `2n + 16`: the `pg_attribute` row source builds a row for
/// every column of every relation in the tenant and the equality is applied afterwards, so the
/// reads are the catalog's and the answer is one table's. The fix is the predicate —
/// `attrelid = <oid>` is a key, not a filter.
/// `#[ignore]`d rather than left red: the fix is one file away from h1's #63 change, which is on
/// hold and not yet on main, and two lanes editing `pg_attribute`'s row source in the same hour is
/// the merge this queue does not need. Removing the attribute **is** the acceptance.
#[test]
#[ignore = "#63(c): the acceptance for the predicate push-down; the diagnosis is in this header"]
fn column_introspection_reads_one_tables_columns() {
    /// Room over the six columns for the four catalog relations the statement joins and their
    /// version counters — generous on purpose, because what is being separated is a constant from
    /// a curve, not a constant from a smaller constant.
    const BOUND: usize = 40;

    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
    );

    let (small, small_warm, small_took, small_warm_took) = first_run(20);
    let (big, big_warm, big_took, big_warm_took) = first_run(100);
    println!(
        "   20 relations: first {small:4} reads {small_took:?} · second {small_warm} reads \
         {small_warm_took:?}\n  100 relations: first {big:4} reads {big_took:?} · second \
         {big_warm} reads {big_warm_took:?}"
    );
    assert!(
        small > 0,
        "the trace is empty, so the statement was never instrumented and the bounds below would \
         pass by not looking"
    );
    assert_eq!(
        small, big,
        "five times the catalog moved this statement from {small} reads to {big}, and it asks \
         about one table by name: `pg_attribute`'s row source is building a row per column of \
         every relation and the executor is filtering afterwards (#63)"
    );
    assert!(
        big <= BOUND,
        "one table's six columns cost {big} reads, over a bound of {BOUND}: flat is not enough if \
         the constant is the whole catalog"
    );
}
