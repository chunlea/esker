//! **The five geometric shapes get their arrays**, against PostgreSQL 19beta1.
//!
//! r1's wire sweep filed six rows: `array_agg` and `ARRAY[…]` over `circle`, `path` and `polygon`
//! all lost their array type — the aggregate came back a **scalar `text`** (OID 25) and the
//! constructor a `text[]` (1009) — where a real server declares `circle[]` (719), `path[]` (1019)
//! and `polygon[]` (1027). The same shape `name[]` was (ADR 0086): an element type with no array
//! to be aggregated into falls back to `text`.
//!
//! **`lseg` and `line` were not among the six and are built anyway.**
//! `tests/array_delimiter.rs` carried "the five remaining geometric shapes" as *one* named gap with
//! one reason — no suite test declares an array of one — and that reason is now false for three of
//! the five. Splitting it 3/2 leaves a worse gap than it closes, and the mechanism is identical.
//!
//! Two refusals are the point of the family: `min(circle)` does not exist, and a `circle[]`
//! compared with a `circle[]` is `42883 could not identify an equality operator for type circle`
//! **while the scalar `=` answers `t`** — array equality needs the element's *btree* equality and a
//! circle's `=` compares areas without one.
//!
//! Measured in `tests/captures/pg19_geometric_array.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **A declared type and nothing else.** `pg_typeof` agrees on both sides now (ADR 0093) and
    // `typdelim`/`typcategory` are `"char"` here since ADR 0095; what is left is the catalog's own
    // `oid` and `regproc`, each its own unit.
    types: &[
        "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, t.typcategory, t.typlen FROM \
         pg_type t WHERE t.typname IN \
         ('circle','_circle','path','_path','polygon','_polygon','lseg','_lseg','line','_line','point','_point','box','_box') \
         ORDER BY t.oid",
        "SELECT typarray FROM pg_type WHERE typname IN \
         ('circle','path','polygon','lseg','line') ORDER BY typname",
    ],
    answers: &[],
};

#[test]
fn every_geometric_array_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_geometric_array.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`** — the path r1's sweep reads.
fn described(node: &mut parity::Node, statement: &str) -> u32 {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")[0]
        .type_oid
}

/// **The six rows r1 filed as group C**, over the wire, plus the two that were not probed.
#[test]
fn an_aggregate_and_a_constructor_declare_the_elements_array() {
    let mut node = parity::Node::new(&[]);
    for (shape, value, oid) in [
        ("circle", "<(0,0),1>", 719),
        ("path", "((0,0),(1,1))", 1019),
        ("polygon", "((0,0),(1,1),(1,0))", 1027),
        // Not in r1's six; the same gap and the same mechanism.
        ("lseg", "[(0,0),(1,1)]", 1018),
        ("line", "{1,2,3}", 629),
    ] {
        let aggregate = format!("SELECT array_agg(c::{shape}) AS v FROM (VALUES ('{value}')) s(c)");
        let constructor = format!("SELECT ARRAY[c::{shape}] AS v FROM (VALUES ('{value}')) s(c)");
        let literal = format!("SELECT '{{\"{value}\"}}'::{shape}[] AS v");
        for statement in [&aggregate, &constructor, &literal] {
            assert_eq!(described(&mut node, statement), oid, "{statement}");
        }
    }
}

/// The values, which quote themselves because each shape prints what the array grammar reserves.
#[test]
fn each_element_quotes_itself_inside_the_array() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT array_agg(c::circle) FROM (VALUES ('<(0,0),1>'),('<(2,2),3>')) s(c)"),
        vec![vec!["{\"<(0,0),1>\",\"<(2,2),3>\"}"]]
    );
    // **The bracket is data**: `[…]` is an open path and `(…)` a closed one, and both survive.
    assert_eq!(
        node.rows(
            "SELECT array_agg(c::path) FROM (VALUES ('((0,0),(1,1))'),('[(2,2),(3,3)]')) s(c)"
        ),
        vec![vec!["{\"((0,0),(1,1))\",\"[(2,2),(3,3)]\"}"]]
    );
    assert_eq!(
        node.rows("SELECT ARRAY['{1,2,3}'::line]"),
        vec![vec!["{\"{1,2,3}\"}"]]
    );
}

/// `unnest` gives the element type back, and `array_length` counts.
#[test]
fn an_array_of_shapes_unnests_to_the_shape() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT unnest('{\"<(0,0),1>\",\"<(2,2),3>\"}'::circle[])"),
        vec![vec!["<(0,0),1>"], vec!["<(2,2),3>"]]
    );
    assert_eq!(
        described(&mut node, "SELECT unnest('{\"<(0,0),1>\"}'::circle[]) AS v"),
        718
    );
    assert_eq!(
        node.rows("SELECT array_length('{\"<(0,0),1>\",\"<(2,2),3>\"}'::circle[], 1)"),
        vec![vec!["2"]]
    );
}

/// A `circle[]` column is a column, not only an expression's type.
#[test]
fn a_shape_array_is_a_column_type() {
    let mut node = parity::Node::new(&["CREATE TABLE g (id int8, c circle[], p polygon[])"]);
    node.run("INSERT INTO g VALUES (1, '{\"<(0,0),1>\"}', '{\"((0,0),(1,1),(1,0))\"}')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT c, p FROM g"),
        vec![vec!["{\"<(0,0),1>\"}", "{\"((0,0),(1,1),(1,0))\"}"]]
    );
    assert_eq!(described(&mut node, "SELECT c FROM g"), 719);
    assert_eq!(described(&mut node, "SELECT p FROM g"), 1027);
}

/// **The scalar `=` answers and the array `=` does not**, which is the family's own rule.
#[test]
fn the_scalar_equality_answers_and_the_array_equality_does_not() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '<(0,0),1>'::circle = '<(0,0),1>'::circle"),
        vec![vec!["t"]]
    );
    let error = node
        .run("SELECT '{\"<(0,0),1>\"}'::circle[] = '{\"<(0,0),1>\"}'::circle[]")
        .unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "could not identify an equality operator for type circle"
    );
    // And no aggregate over one: `min(circle)` does not exist on a real server either.
    let error = node
        .run("SELECT min(c::circle) FROM (VALUES ('<(0,0),1>')) s(c)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
    assert!(
        error.to_string().contains("min(circle)"),
        "the refusal did not name the aggregate: {error}"
    );
}

/// **The catalog carries all five rows**, delimiters included.
#[test]
fn pg_type_has_the_five_array_rows() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typelem, typdelim, typinput FROM pg_type WHERE typname IN \
             ('_circle','_line','_lseg','_path','_polygon') ORDER BY oid"
        ),
        vec![
            vec!["629", "_line", "628", ",", "array_in"],
            vec!["719", "_circle", "718", ",", "array_in"],
            vec!["1018", "_lseg", "601", ",", "array_in"],
            vec!["1019", "_path", "602", ",", "array_in"],
            vec!["1027", "_polygon", "604", ",", "array_in"],
        ]
    );
    // And every one of the five now points at a row that is there.
    assert_eq!(
        node.rows(
            "SELECT typname, typarray FROM pg_type WHERE typname IN \
             ('circle','path','polygon','lseg','line') ORDER BY typname"
        ),
        vec![
            vec!["circle", "719"],
            vec!["line", "629"],
            vec!["lseg", "1018"],
            vec!["path", "1019"],
            vec!["polygon", "1027"],
        ]
    );
}
