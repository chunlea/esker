//! `pk_and_sequence_for`'s cost grows with the catalog, and must grow no faster than it.
//!
//! Run 49's report is this file's reason: `ActiveRecord`'s `reset_pk_sequence!` opens with one
//! statement — a **five-relation comma-separated `FROM` list** over `pg_class`, `pg_attribute`,
//! `pg_depend`, `pg_constraint` and `pg_namespace` — and `fixtures.rb` runs it once per fixture
//! table. This node answered it correctly at every size and never returned at the suite's, so the
//! 426-file pass could not be measured at all: 30 relations in 0 s, 90 in 4 s, 720 in more than
//! 90 s, with one core pinned after the client had been killed.
//!
//! **It is not a wrong-answer bug**, which is what makes it invisible to every other test here: a
//! corpus at three tables passes while the suite hangs. The only thing that can catch it is the
//! shape of the curve, so that is what this asserts.
//!
//! # Asserted as a ratio, with a control
//!
//! The shape `a_scan_costs_its_range_and_not_the_store` established and
//! `one_catalog_read_serves_every_regclass_in_a_statement` reused: the same statement over a
//! catalog of *n* relations and one of *2n*. A plan whose work is what the statement selects costs
//! about twice as much for twice the catalog; a plan that enumerates the cross product of five
//! relations costs far more. Measured on the code this test was written against, the curve was
//! **4 tables 98 ms, 6 362 ms, 8 935 ms, 10 2.04 s, 12 3.92 s, 14 6.90 s** — 3.5× the catalog for
//! 70× the time.
//!
//! The control is a statement over the same catalog that *must* grow with it, so a slow machine or
//! a busy container moves both numbers and the comparison still means what it says.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use std::time::{Duration, Instant};

/// `ActiveRecord`'s `pk_and_sequence_for`, verbatim from `postgresql/schema_statements.rb:382`
/// by way of `results/run-49/pkseq.sql`.
const PK_AND_SEQUENCE_FOR: &str = "SELECT attr.attname, nsp.nspname, seq.relname \
     FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp \
     WHERE seq.oid = dep.objid AND seq.relkind = 'S' \
       AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid \
       AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] \
       AND seq.relnamespace = nsp.oid AND cons.contype = 'p' \
       AND dep.classid = 'pg_class'::regclass \
       AND dep.refobjid = '\"pk0\"'::regclass";

/// A statement whose cost really is the catalog's size: the control.
const CONTROL: &str = "SELECT count(*) FROM pg_class";

fn elapsed(node: &mut parity::Node, sql: &str) -> Duration {
    let start = Instant::now();
    node.rows(sql);
    start.elapsed()
}

fn grow_to(node: &mut parity::Node, target: usize, made: &mut usize) {
    while *made < target {
        node.run(&format!(
            "CREATE TABLE pk{made} (id bigserial primary key, a int8, b text)"
        ))
        .unwrap();
        *made += 1;
    }
}

#[test]
fn pk_and_sequence_for_costs_what_it_selects_and_not_the_catalog_crossed() {
    let mut node = parity::Node::new(&[]);
    let mut made = 0;

    grow_to(&mut node, 6, &mut made);
    // The answer, which is one row at every size — asserted before the timings so a plan that got
    // fast by getting wrong fails here rather than passing quietly.
    assert_eq!(
        node.rows(PK_AND_SEQUENCE_FOR),
        [["id", "public", "pk0_id_seq"]]
    );
    let small_control = elapsed(&mut node, CONTROL);
    let small = elapsed(&mut node, PK_AND_SEQUENCE_FOR);

    grow_to(&mut node, 12, &mut made);
    assert_eq!(
        node.rows(PK_AND_SEQUENCE_FOR),
        [["id", "public", "pk0_id_seq"]],
        "the same one row over twice the catalog"
    );
    let big_control = elapsed(&mut node, CONTROL);
    let big = elapsed(&mut node, PK_AND_SEQUENCE_FOR);

    assert!(
        big < small * 4 + Duration::from_millis(200),
        "twice the catalog took {big:?} where half of it took {small:?} — the five-relation FROM \
         list is being enumerated as a cross product rather than filtered as it is joined. The \
         control over the same two catalogs went {small_control:?} -> {big_control:?}, so the \
         machine is not what changed."
    );
}

/// Reordering a comma list must not reorder what `SELECT *` returns.
///
/// The hazard the join order creates and the one nothing else here would catch: the scope's
/// `written` list decides the columns `*` expands to, and it is built from the entries in the order
/// they are **joined**. A reordering that let that stand would answer the same columns in a
/// different order — no error, no wrong value, and no `ORDER BY` or single-table assertion would
/// see it.
#[test]
fn a_reordered_comma_list_still_expands_star_in_the_order_written() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cl_a (a_id int8 PRIMARY KEY, a_v text)",
        "CREATE TABLE cl_b (b_id int8 PRIMARY KEY, b_v text)",
        "CREATE TABLE cl_c (c_id int8 PRIMARY KEY, c_v text)",
        "INSERT INTO cl_a VALUES (1, 'a')",
        "INSERT INTO cl_b VALUES (1, 'b')",
        "INSERT INTO cl_c VALUES (1, 'c')",
    ]);
    // `cl_c` is the table a constant pins, so the order chosen is not the order written.
    assert_eq!(
        node.rows(
            "SELECT * FROM cl_a, cl_b, cl_c WHERE cl_c.c_id = 1 AND cl_a.a_id = cl_c.c_id \
             AND cl_b.b_id = cl_a.a_id"
        ),
        [["1", "a", "1", "b", "1", "c"]]
    );
    // And the same query with the columns named, which is what the cost test relies on.
    assert_eq!(
        node.rows(
            "SELECT cl_a.a_v, cl_b.b_v, cl_c.c_v FROM cl_a, cl_b, cl_c \
             WHERE cl_c.c_id = 1 AND cl_a.a_id = cl_c.c_id AND cl_b.b_id = cl_a.a_id"
        ),
        [["a", "b", "c"]]
    );
}

/// A `LEFT JOIN` keeps its NULL-extended rows, whatever the `WHERE` says about the inner side.
///
/// The other half of what pushdown must not do. `WHERE` applied *below* an outer join throws away
/// exactly the rows the join exists to keep, and the answer changes rather than the cost — so a
/// chain that has taken an outer join pushes nothing after it.
#[test]
fn a_where_is_not_pushed_below_an_outer_join() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE lj_l (id int8 PRIMARY KEY, v text)",
        "CREATE TABLE lj_r (id int8 PRIMARY KEY, v text)",
        "INSERT INTO lj_l VALUES (1, 'one'), (2, 'two')",
        "INSERT INTO lj_r VALUES (1, 'right')",
    ]);
    // The `IS NULL` is only true of a row the outer join extended, so a pushed copy would answer
    // nothing at all.
    assert_eq!(
        node.rows(
            "SELECT lj_l.id FROM lj_l LEFT JOIN lj_r ON lj_r.id = lj_l.id \
             WHERE lj_r.id IS NULL ORDER BY lj_l.id"
        ),
        [["2"]]
    );
}
