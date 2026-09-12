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
//! # What the push-down did, and what is left
//!
//! Re-measured on 2026-09-11 after merging h1's `#63` variant B, which moved the numbers before
//! anything here changed — the listing reads a **record** per relation where it used to hydrate
//! one, so the baseline for this statement became `3n + 17` rather than `2n + 16`:
//!
//! ```text
//! relations   before A+B    after A+B     what the difference is
//!   20            77            37
//!  100           317           117        2 reads a relation: the hydration, gone
//!  300           917           317
//! ```
//!
//! Three reads a relation were the listing's record, the hydration's record and the hydration's
//! sequence scan; two of the three are the *hydration*, and after the push-down exactly **one
//! relation is hydrated** — the one the statement names. What was left was one read a relation in
//! `pg_relations::Relations::read`, which point-read every table's record to tell a table from a
//! materialized view and to place an index.
//!
//! # The last read a relation, and the flat line
//!
//! That listing now takes the same records as **one scan** of the range they live in
//! (`View::table_records`, the shape `View::matviews` already had), so the per-relation term is
//! gone from this statement too. Measured on the merged tree, 2026-09-11:
//!
//! ```text
//! relations   before A+B    after A      after A+B
//!   20            77            37           17
//!  100           317           117           17
//!  300           917           317            —
//! ```
//!
//! **Seventeen either way.** Both assertions below are green, and they are not the same assertion:
//! the first says there is **no per-relation cost**, the second says **and the constant is small**.
//! A measurement can satisfy the first and fail the second — a flat line whose constant is the
//! whole catalog is exactly what `#49`'s cache produced and what the `#[ignore]` was waiting on.

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

/// **One table's columns hydrate one table.**
///
/// The half `debts-v1.1.md` #63 (c) closed: `pg_attribute`'s row source is asked about the one
/// relation the predicate names instead of hydrating every relation of the tenant and letting the
/// `Filter` above throw all but one away — and `pg_attrdef`, whose tie to the statement is a join
/// condition rather than a constant, is pinned across the `ON` (`exec::query::pinned_across_join`).
///
/// **A slope, because the intercept is somebody else's.** Every catalog view reads the relations
/// listing; what this statement used to add on top was two more reads a relation, which is the
/// hydration.
///
/// The pin was **one** a relation — the listing's own record — which is what the push-down left
/// behind. That read is gone too now: the listing takes every table's record in one scan rather
/// than one point read each (`View::table_records`), so the two halves of `#63 (c)` together leave
/// **no per-relation cost at all**. Measured 2026-09-11 on the merged tree: three a relation
/// before, one after the push-down, **zero after both**.
#[test]
fn column_introspection_hydrates_one_relation() {
    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
    );
    let (small, ..) = first_run(20);
    let (big, ..) = first_run(100);
    assert!(
        small > 0,
        "the trace is empty, so the statement was never instrumented and the bound below would \
         pass by not looking"
    );
    assert_eq!(
        big - small,
        0,
        "{small} reads over 20 relations and {big} over 100 is {} a relation: either \
         `pg_attribute`/`pg_attrdef` is hydrating a relation the statement did not name, or the \
         relations listing is point-reading a record per table again (#63 (c), both halves)",
        (big - small) / 80
    );
}

/// **A pinned view still answers about the catalog's own relations**, which have no row in the
/// relations listing at all.
///
/// The trap in the push-down: `pg_attribute`'s rows are the tenant's relations *plus* a row per
/// column of every `pg_catalog` relation, and only the first half is narrowed. An oid the listing
/// has no row for finds nothing there — and the catalog's own rows, which cost no read, are the
/// whole answer.
#[test]
fn a_pinned_catalog_view_still_describes_the_catalog() {
    let mut node = parity::Node::new(&[SUBJECT]);
    assert_eq!(
        node.rows("SELECT attname FROM pg_attribute WHERE attrelid = 'pg_class'::regclass AND attnum > 0 ORDER BY attnum")
            .first()
            .map(|row| row[0].clone()),
        Some("oid".to_owned()),
        "pg_class describes itself, and the pin must not take that away"
    );
    // And a relation nobody has: no rows, not an error, and not the whole catalog either.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_attribute WHERE attrelid = 2147483647"),
        vec![vec!["0"]]
    );
}

/// **Only a conjunction pins.** A constant under an `OR` is not required by the query, and
/// narrowing on one would return the rows of one table where the statement asks for two.
#[test]
fn a_disjunction_does_not_pin_a_catalog_view() {
    let mut node =
        parity::Node::new(&[SUBJECT, "CREATE TABLE other (a bigint primary key, b text)"]);
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM pg_attribute WHERE (attrelid = '\"holder\"'::regclass \
             OR attrelid = '\"other\"'::regclass) AND attnum > 0"
        ),
        vec![vec!["8"]],
        "six columns of one table and two of the other"
    );
}

/// **The plan says which relation it was narrowed to**, because a push-down a user cannot see is
/// one they cannot tell from a scan that got lucky.
#[test]
fn the_plan_names_the_relation_a_catalog_view_was_pinned_to() {
    let mut node = parity::Node::new(&[SUBJECT]);
    let oid = node.rows("SELECT '\"holder\"'::regclass::oid")[0][0].clone();
    let plan: Vec<String> = node
        .rows("EXPLAIN SELECT attname FROM pg_attribute WHERE attrelid = '\"holder\"'::regclass")
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert!(
        plan.iter()
            .any(|line| line.trim() == format!("Relation: attrelid = {oid}")),
        "the plan does not say what it was narrowed to: {plan:#?}"
    );
}

/// **One table's columns cost one table's columns.**
///
/// Six columns and a catalog of twenty relations, then the same six columns and a catalog of a
/// hundred: the statement names one table by `regclass`, so what it reads must not move.
///
/// It was red at 77 → 317, then at **37 → 117** after the push-down. The read a relation that
/// remained was the relations listing's: `pg_relations::Relations::read` point-read one record per
/// relation to tell a table from a materialized view and to place an index, and narrowing *that*
/// was a change to what a `Relations` memoised per tenant and version means — the other lane's
/// file family, which is why this was `#[ignore]`d rather than left red while one half was in.
///
/// Both halves are in. The listing takes those records as **one scan** of the range they live in
/// (`View::table_records`), so the per-relation term is gone and this reads **17 either way**.
///
/// **It is not the slope test above restated.** That one says there is no cost per relation; this
/// one says the cost is also *small*. A statement whose reads are flat at the size of the whole
/// catalog satisfies the first and fails this — which is exactly the shape `#49`'s version-keyed
/// cache produced, and the reason the bound below is here at all.
#[test]
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
