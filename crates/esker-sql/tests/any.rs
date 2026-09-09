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
    // **Empty.** `current_schemas(false)` and `(true)` stood here, first as refusals — selecting
    // one returned an array and this node had no array value — then as declared types, because a
    // real server calls them `name[]` and this node called them `text`. Both halves are closed:
    // `name[]` is a type here (ADR 0086) and `current_schemas` folds to a real array of it.
    types: &[],
    answers: &[],
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

/// An unquoted `NULL` element is a SQL NULL; a quoted `"NULL"` is the four characters.
///
/// A regression test. The first version of this unit handed the *word* `NULL` to the element's
/// input function, which `text` accepted as a string and `int4` refused with `22P02` — so
/// `1 = ANY('{NULL,1}'::int[])` was an error where a real server answers `t`. Rails'
/// `where(id: [1, nil])` emits exactly that shape.
///
/// The comment that shipped with the bug said the simplification "cannot be reached from anything
/// `ActiveRecord` sends". It could. A claim about what a client sends belongs in a capture.
#[test]
fn a_null_element_is_null_and_a_quoted_null_is_a_string() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        // The integer case, which is the one that used to raise rather than answer.
        ("SELECT 1 = ANY('{NULL,1}'::int[])", "t"),
        ("SELECT 2 = ANY('{NULL,1}'::int[])", "\\N"),
        ("SELECT 1 = ANY(ARRAY[1,NULL])", "t"),
        // And the quoting rule, which is what makes the two spellings different values.
        ("SELECT 'a' = ANY('{NULL,a}'::text[])", "t"),
        ("SELECT 'z' = ANY('{NULL,a}'::text[])", "\\N"),
        ("SELECT 'NULL' = ANY('{NULL,a}'::text[])", "\\N"),
        ("SELECT 'NULL' = ANY('{\"NULL\",a}'::text[])", "t"),
        ("SELECT 'null' = ANY('{null,a}'::text[])", "\\N"),
    ] {
        assert_eq!(node.rows(sql), vec![vec![expected.to_owned()]], "{sql}");
    }
}

/// A function is resolved by name **and** arity, so the wrong one does not exist.
///
/// The second regression test. `current_schema(false)` answered `public` here where a real server
/// says `42883 function current_schema(boolean) does not exist` — a wrong answer rather than a
/// gap, since the argument was simply ignored.
#[test]
fn a_schema_function_with_the_wrong_arity_does_not_exist() {
    let mut node = parity::Node::new(&[]);
    for (sql, message) in [
        (
            "SELECT current_schema(false)",
            "function current_schema(boolean) does not exist",
        ),
        (
            "SELECT current_schemas()",
            "function current_schemas() does not exist",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42883", "{sql}");
        assert_eq!(error.to_string(), message, "{sql}");
    }
}

/// `current_schema` **without parentheses** is the function, not a column.
///
/// SQL's niladic functions may be written bare. A real server answers `public`; this node answered
/// `42703 column "current_schema" does not exist`, because a bare identifier is a column
/// everywhere else. One name, two spellings, and only the parenthesised one was implemented —
/// which is why a triage reading "`current_schema` fails" and a test asserting
/// `current_schema(false)` is `42883` were both right at the same time, and the report went round
/// three times before the capture was replayed statement by statement instead of paraphrased.
///
/// Quoted, it is a column again: `"current_schema"` names one, the same rule the `DEFAULT`
/// keyword follows.
#[test]
fn a_bare_current_schema_is_the_function() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT current_schema"), vec![vec!["public"]]);
    assert_eq!(node.rows("SELECT current_schema()"), vec![vec!["public"]]);

    // Quoted, it is an ordinary name and there is no such column.
    let error = node.run("SELECT \"current_schema\"").unwrap_err();
    assert_eq!(error.sqlstate(), "42703");
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
