//! Contract C3 for `smallint` — tier 1's fourth type, and `smallserial` with it.
//!
//! [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md). The shape is `int4`'s exactly,
//! one width down, which is the argument for capturing it rather than deriving it: both ends of the
//! range are in the corpus and so is every direction past them, so a mapping onto `int4` or `int8`
//! answers every `22003` line with a stored row.
//!
//! Two bytes on disk, appended tags in three vocabularies, and nothing that existed before it
//! changes — the same "addition, not a format change" that ADR 0033 argued once and this is the
//! fourth type to inherit.

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
    answers: &[],
};

#[test]
fn every_int2_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_int2.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 24,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The range is the type, and `22003` has the same two messages `integer` has.
#[test]
fn the_range_is_the_type() {
    let mut node = parity::Node::new(&["CREATE TABLE r (id int8 PRIMARY KEY, n smallint)"]);

    node.run("INSERT INTO r VALUES (1, 32767), (2, -32768)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT n FROM r ORDER BY n"),
        vec![vec!["-32768"], vec!["32767"]]
    );

    // A constant one past either end: refused, where an `int4` alias would have stored it.
    for statement in [
        "INSERT INTO r VALUES (3, 32768)",
        "INSERT INTO r VALUES (3, -32769)",
        "UPDATE r SET n = 32768 WHERE id = 1",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(error.to_string(), "smallint out of range", "{statement}");
    }

    // The string path is the input function, and it quotes what it could not read.
    let error = node.run("SELECT n FROM r WHERE n = '32768'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
    assert_eq!(
        error.to_string(),
        "value \"32768\" is out of range for type smallint"
    );
    let error = node.run("SELECT n FROM r WHERE n = 'x'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type smallint: \"x\""
    );
}

/// All three integer widths are one type to a comparison, which is PostgreSQL's own operator set.
#[test]
fn the_three_integer_widths_compare() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE w (id int8 PRIMARY KEY, s smallint, i integer, b bigint)",
    ]);
    node.run("INSERT INTO w VALUES (1, 10, 10, 10), (2, 20, 30, 40)")
        .unwrap();

    assert_eq!(node.rows("SELECT id FROM w WHERE s = i"), vec![vec!["1"]]);
    assert_eq!(node.rows("SELECT id FROM w WHERE s = b"), vec![vec!["1"]]);
    assert_eq!(
        node.rows("SELECT id FROM w WHERE s < i ORDER BY id"),
        vec![vec!["2"]]
    );
    // Sorted by value across the widths, not by which `Datum` variant a row holds.
    assert_eq!(
        node.rows("SELECT s FROM w WHERE s IN (10, 20) ORDER BY s"),
        vec![vec!["10"], vec!["20"]]
    );
}

/// `smallserial` is a `smallint` that fills itself, and it stopped being `0A000` here.
#[test]
fn smallserial_is_a_smallint_with_a_sequence() {
    let mut node = parity::Node::new(&["CREATE TABLE s (id smallserial PRIMARY KEY, name text)"]);
    node.run("INSERT INTO s (name) VALUES ('a'), ('b')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id, name FROM s ORDER BY id"),
        vec![vec!["1", "a"], vec!["2", "b"]]
    );
    match node.answer("SELECT id FROM s ORDER BY id") {
        parity::Answer::Rows { types, .. } => assert_eq!(types, vec![ColumnType::Int2.name()]),
        other => panic!("SELECT id answered {other}"),
    }
}

/// The catalog grew a row without `pg_catalog` being touched.
#[test]
fn the_catalog_learned_the_type_by_itself() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT oid, typname, typinput FROM pg_type WHERE typname = 'int2'"),
        vec![vec!["21", "int2", "int2in"]]
    );
    assert_eq!(ColumnType::Int2.oid(), 21);
    assert_eq!(ColumnType::Int2.type_len(), 2);
    assert_eq!(ColumnType::Int2.name(), "smallint");
}
