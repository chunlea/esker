//! Contract C3 for `integer` — tier 1's first type, and the one the ladder asks for first.
//!
//! [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md) decided it is a **distinct
//! type and not an alias for `int8`**, and the corpus is what makes that a measurement rather than
//! a preference: both ends of the range are in it, and so is every direction past them. A mapping
//! onto `int8` answers every `22003` line with a stored row.
//!
//! It is not an on-disk format change. The row codec carries no per-value type tag, so an `Int4`
//! writes four little-endian bytes where nothing wrote before, and every row written before this
//! type existed decodes exactly as it did — `esker-keys`' own goldens and property tests say so
//! without being regenerated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;
use esker_sql::value::{ColumnType, PgType};

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Empty, and its one entry was `SELECT id FROM i4 WHERE n = 10::int8`.** It recorded a cast
    // refused by name, and the reason it was in the corpus at all was as evidence that the two
    // integer widths compare. They do now — an integer value compares like an integer literal
    // whatever width it arrived as (ADR 0087) — so the statement answers the row a real server
    // answers and the entry is gone.
    answers: &[],
};

#[test]
fn every_int4_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_int4.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 26,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The range is the type, and the two `22003` messages are not interchangeable.
///
/// A real server says a bare `integer out of range` when a *constant* will not fit and
/// `value "…" is out of range for type integer` when `int4in` fails on a *string*. Both are
/// `22003`, and an implementation that routed them through one function would be right about the
/// SQLSTATE and wrong about the sentence.
#[test]
fn the_range_is_the_type() {
    let mut node = parity::Node::new(&["CREATE TABLE r (id int8 PRIMARY KEY, n integer)"]);

    node.run("INSERT INTO r VALUES (1, 2147483647), (2, -2147483648)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT n FROM r ORDER BY n"),
        vec![vec!["-2147483648"], vec!["2147483647"]]
    );

    // A constant one past either end: refused, where an `int8` alias would have stored it.
    for statement in [
        "INSERT INTO r VALUES (3, 2147483648)",
        "INSERT INTO r VALUES (3, -2147483649)",
        "UPDATE r SET n = 2147483648 WHERE id = 1",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(error.to_string(), "integer out of range", "{statement}");
    }

    // The string path is the input function, and it quotes what it could not read.
    let error = node
        .run("SELECT n FROM r WHERE n = '2147483648'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
    assert_eq!(
        error.to_string(),
        "value \"2147483648\" is out of range for type integer"
    );
    let error = node.run("SELECT n FROM r WHERE n = 'x'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type integer: \"x\""
    );
}

/// The two widths are one type to a comparison, which is PostgreSQL's `int4 = int8` operator.
#[test]
fn the_two_integer_widths_compare() {
    let mut node =
        parity::Node::new(&["CREATE TABLE w (id int8 PRIMARY KEY, n integer, b bigint)"]);
    node.run("INSERT INTO w VALUES (1, 10, 10), (2, 20, 30)")
        .unwrap();

    assert_eq!(node.rows("SELECT id FROM w WHERE n = b"), vec![vec!["1"]]);
    assert_eq!(
        node.rows("SELECT id FROM w WHERE n < b ORDER BY id"),
        vec![vec!["2"]]
    );
    // And the ordering is by value across the widths, not by which variant a `Datum` is.
    assert_eq!(
        node.rows("SELECT n FROM w WHERE n IN (10, 20) ORDER BY n"),
        vec![vec!["10"], vec!["20"]]
    );
}

/// `serial` is an `integer` that fills itself, and it stopped being `0A000` when `int4` arrived.
///
/// The refusal it used to get had one argument — that answering it with an `int8` would accept
/// values a real server refuses — and ADR 0033 withdrew it because that argument is now empty.
#[test]
fn serial_is_an_integer_with_a_sequence() {
    let mut node = parity::Node::new(&["CREATE TABLE s (id serial PRIMARY KEY, name text)"]);
    node.run("INSERT INTO s (name) VALUES ('a'), ('b')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id, name FROM s ORDER BY id"),
        vec![vec!["1", "a"], vec!["2", "b"]]
    );
    // An `integer`, not a `bigint`: the column a client is told about is the one it got.
    match node.answer("SELECT id FROM s ORDER BY id") {
        parity::Answer::Rows { types, .. } => assert_eq!(types, vec![ColumnType::Int4.name()]),
        other => panic!("SELECT id answered {other}"),
    }
}

/// `pg_type` grew a row without `pg_catalog` being touched, because it is derived from
/// `ColumnType::ALL`.
#[test]
fn the_catalog_learned_the_type_by_itself() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT oid, typname, typinput FROM pg_type WHERE typname = 'int4'"),
        vec![vec!["23", "int4", "int4in"]]
    );
    assert_eq!(ColumnType::Int4.oid(), 23);
    assert_eq!(ColumnType::Int4.type_len(), 4);
    assert_eq!(ColumnType::Int4.name(), "integer");
}
