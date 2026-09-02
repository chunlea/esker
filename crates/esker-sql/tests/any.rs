//! `= ANY(…)` and the schema functions, against PostgreSQL 19beta1's own answers.
//!
//! Half of rung 4's blocker. `ActiveRecord` asks `WHERE n.nspname = ANY (current_schemas(false))`
//! in every one of its relation-listing statements; the other half is `pg_class` and
//! `pg_namespace` as computed views, which is a separate commit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A real server types these `name` and `name[]` — `name` being the 64-byte identifier type
        // that `pg_class.relname` and `pg_namespace.nspname` are. This node has no `name`, so it
        // answers `text`, whose *values* are identical and which is what every comparison against
        // one already does. The array form never reaches a client at all: `current_schemas` is
        // legal here only as an `ANY` operand, where it becomes an `IN` list before the planner
        // sees it, so there is no array value for a `RowDescription` to describe.
        "SELECT current_schema()",
    ],
    answers: &[
        (
            "SELECT current_schemas(false)",
            "`{public}` on a real server, and `0A000` here: selecting it **returns an array**, and \
         this node has array *expressions* only — no array `Datum`, no array column, nothing a \
         `RowDescription` could type. Inside an `= ANY` it is a list of names and answers exactly; \
         on its own it would need the stored-array unit that ADR 0033's roadmap puts in tier 2. \
         Refusing by name is the honest half: `ActiveRecord` only ever writes it inside an `ANY`.",
        ),
        (
            "SELECT current_schemas(true)",
            "The same refusal for the same reason: it is the form that also lists `pg_catalog`, \
             and selecting either returns an array.",
        ),
    ],
};

#[test]
fn every_any_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_any.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `= ANY` is three-valued, and an empty array is `false` rather than NULL.
///
/// The four that decide whether lowering `ANY` to `IN` is free or a guess. This node's `IN` was
/// measured against a real server's before the `ANY` arm was written; this asserts the two agree
/// *through* the new path, which is a different question from whether `IN` alone was right.
#[test]
fn any_carries_three_valued_logic_through_from_in() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        ("SELECT NULL = ANY(ARRAY['a'])", "\\N"),
        ("SELECT 'a' = ANY(ARRAY['a', NULL])", "t"),
        ("SELECT 'z' = ANY(ARRAY['a', NULL])", "\\N"),
        ("SELECT 'x' = ANY(ARRAY[]::text[])", "f"),
    ] {
        assert_eq!(node.rows(sql), vec![vec![expected.to_owned()]], "{sql}");
    }
}

/// The three spellings of an array an `ANY` operand may take.
#[test]
fn every_spelling_of_an_array_operand_answers() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        // The constructor.
        ("SELECT 'a' = ANY(ARRAY['a','b'])", "t"),
        // The text literal PostgreSQL's array input function reads.
        ("SELECT 'a' = ANY('{a,b}')", "t"),
        ("SELECT 'c' = ANY('{a,b}')", "f"),
        // A function that returns one.
        ("SELECT 'public' = ANY(current_schemas(false))", "t"),
        ("SELECT 'pg_catalog' = ANY(current_schemas(true))", "t"),
        ("SELECT 'pg_catalog' = ANY(current_schemas(false))", "f"),
        // And the elements take their type from what they are compared against, as an `IN` list's
        // do — an integer array is not a separate feature.
        ("SELECT 1 = ANY(ARRAY[1,2,3])", "t"),
    ] {
        assert_eq!(node.rows(sql), vec![vec![expected.to_owned()]], "{sql}");
    }
}

/// A quantifier this node does not have is named rather than answered by the one it does.
#[test]
fn another_quantifier_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    for sql in ["SELECT 1 > ANY(ARRAY[1,2])", "SELECT 1 = ALL(ARRAY[1,2])"] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "{sql}");
    }
}
