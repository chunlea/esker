//! `array_agg`, its `ORDER BY`, and `pg_enum` — boot statement 26.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Fourteen, and every one is the array's declared type.** `array_agg` answers an array *of
    // the argument's type* on a real server — `bigint[]`, `integer[]`, `text[]` — and `text` here,
    // which is what an array is on this node (`crate::value::vector`). Every row is
    // byte-identical, `{10,20,30,NULL}` and the NULL-first descending form included.
    types: &[
        "SELECT type.typname AS name, type.OID AS oid, n.nspname AS schema, array_agg(enum.enumlabel ORDER BY enum.enumsortorder) AS value FROM pg_enum AS enum JOIN pg_type AS type ON (type.oid = enum.enumtypid) JOIN pg_namespace n ON type.typnamespace = n.oid WHERE n.nspname = ANY (current_schemas(false)) GROUP BY type.OID, n.nspname, type.typname",
    ],
    answers: &[],
};

#[test]
fn every_array_agg_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_agg.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 18,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// Over no rows it is **NULL**, where `count` is 0 — and an empty array would be neither.
///
/// The three answers are different values and a caller can tell them apart: `NULL IS NULL` is
/// true, `'{}' IS NULL` is false, and `array_length('{}', 1)` is NULL for its own reason. An
/// implementation that answered `{}` here passes every test that only looks at the rows.
#[test]
fn an_aggregate_over_no_rows_is_null_and_not_an_empty_array() {
    let mut node = parity::Node::new(&["CREATE TABLE ag (id int8 PRIMARY KEY)"]);
    assert_eq!(
        node.rows("SELECT array_agg(id), count(id), array_agg(id) IS NULL FROM ag WHERE id > 99"),
        [["\\N", "0", "t"]]
    );
}

/// The `ORDER BY` inside the parentheses sorts by expressions the aggregate does not return.
///
/// `{2,1,4,3}` is neither input order nor `id` order, so a sort key that was recovered from the
/// collected value — rather than carried beside it — cannot produce it.
#[test]
fn the_aggregates_own_order_by_sorts_by_what_it_does_not_return() {
    let mut node = parity::Node::new(&["CREATE TABLE ag (id int8 PRIMARY KEY, g text, n int4)"]);
    for row in [
        "(1, 'a', 10)",
        "(2, 'a', 30)",
        "(3, 'b', 20)",
        "(4, 'b', NULL)",
    ] {
        node.run(&format!("INSERT INTO ag VALUES {row}")).unwrap();
    }
    assert_eq!(
        node.rows("SELECT array_agg(id ORDER BY g, id DESC) FROM ag"),
        [["{2,1,4,3}"]]
    );
    // NULLs are **kept**, and where they land follows the direction — one rule, two answers.
    assert_eq!(
        node.rows("SELECT array_agg(n ORDER BY n), array_agg(n ORDER BY n DESC) FROM ag"),
        [["{10,20,30,NULL}", "{NULL,30,20,10}"]]
    );
    // Per group, and `DISTINCT` inside the parentheses is the same clause it is elsewhere.
    assert_eq!(
        node.rows("SELECT g, array_agg(id ORDER BY id) FROM ag GROUP BY g ORDER BY g"),
        vec![vec!["a", "{1,2}"], vec!["b", "{3,4}"]]
    );
    assert_eq!(
        node.rows("SELECT array_agg(DISTINCT g) FROM ag"),
        [["{a,b}"]]
    );
}

/// A clause this node does not honour is still refused by name — accepting `ORDER BY` did not
/// open the others.
#[test]
fn the_other_aggregate_clauses_are_still_named() {
    let mut node = parity::Node::new(&["CREATE TABLE ag (id int8 PRIMARY KEY)"]);
    for statement in [
        "SELECT array_agg(id) FILTER (WHERE id > 1) FROM ag",
        "SELECT array_agg(id) OVER () FROM ag",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {statement}");
    }
}
