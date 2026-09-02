//! Contract C3 for `timestamp` **without** time zone — tier 1's third type, and the third thing
//! rung 2's migration names: `t.timestamps` compiles to `timestamp(6)`.
//!
//! [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md). **`timestamp` and
//! `timestamp(6)` are the same type**, which is why this unit takes the precision `ActiveRecord`
//! writes without a typmod to keep it in: six is PostgreSQL's default *and* its maximum, so the two
//! hold identical values and print identically. `timestamp(0)` through `timestamp(5)` really do
//! round, and those are refused by name until the typmod unit.
//!
//! What makes it a type rather than an alias for `timestamptz` is the text: it prints with **no
//! zone suffix**, and it does no conversion — the value that goes in is the value that comes out,
//! whatever the session's `TimeZone` is. Eight bytes either way, so no format change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;
use esker_sql::value::{ColumnType, PgType};

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // Column `b` is declared `timestamp(6)`, and a real server prints the precision back:
        // `timestamp(6) without time zone`. This node has nowhere to keep a typmod yet, so it says
        // `timestamp without time zone` — the same type, the same values, one string shorter. The
        // typmod unit closes it, and until then the *rows* on every one of these lines agree,
        // which is what the harness reaching this list at all already proves.
        "SELECT id, a, b, c FROM ts ORDER BY id",
        "SELECT id, a, b FROM ts WHERE id = 4",
        "SELECT id, a, b FROM ts WHERE id = 5",
    ],
    answers: &[],
};

#[test]
fn every_timestamp_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_timestamp.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The zone is the difference, and it is visible in exactly one place: the text.
#[test]
fn it_prints_without_an_offset() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE z (id int8 PRIMARY KEY, a timestamp, b timestamptz)",
        "INSERT INTO z VALUES (1, '2020-01-01 12:00:00', '2020-01-01 12:00:00+00')",
    ]);

    assert_eq!(
        node.rows("SELECT a, b FROM z"),
        vec![vec!["2020-01-01 12:00:00", "2020-01-01 12:00:00+00"]]
    );
    assert_eq!(ColumnType::Timestamp.oid(), 1114);
    assert_eq!(ColumnType::Timestamp.type_len(), 8);
    assert_eq!(ColumnType::Timestamp.name(), "timestamp without time zone");
    assert_eq!(
        node.rows("SELECT oid, typname, typinput FROM pg_type WHERE typname = 'timestamp'"),
        vec![vec!["1114", "timestamp", "timestamp_in"]]
    );
}

/// `timestamp` and `timestamp(6)` are one type, and the other precisions are refused by name.
#[test]
fn six_is_the_default_and_the_maximum() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p (id int8 PRIMARY KEY, a timestamp, b timestamp(6))",
        "INSERT INTO p VALUES (1, '2020-06-15 08:30:15.123456', '2020-06-15 08:30:15.123456')",
    ]);
    // The same value, stored twice through two spellings of one type.
    assert_eq!(node.rows("SELECT id FROM p WHERE a = b"), vec![vec!["1"]]);

    // A precision that really rounds is `0A000` naming itself, not silently widened. Storing
    // microseconds in a `timestamp(3)` column would answer a later `SELECT` with digits a real
    // server discarded.
    for statement in [
        "CREATE TABLE q (a timestamp(3))",
        "CREATE TABLE q (a timestamp(0))",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement}"
        );
        assert!(
            error.to_string().to_ascii_uppercase().contains("TIMESTAMP"),
            "{statement} -> `{error}`, which does not name the type"
        );
    }
}

/// A seventh fractional digit rounds, and a tie rounds to **even** — not up.
///
/// Found by the corpus and fixed in the shared parser, so `timestamptz` was wrong the same way
/// before this unit: PostgreSQL reads the fraction as a double and applies `rint`, and this crate
/// added one whenever the seventh digit was five or more. The four halves below land on their even
/// neighbours in two different directions, which is what tells the two rules apart — a single
/// example cannot.
#[test]
fn a_tie_rounds_to_even() {
    let mut node = parity::Node::new(&["CREATE TABLE r (id int8 PRIMARY KEY, a timestamp)"]);

    for (index, (written, expected)) in [
        (".1234565", "2020-01-01 00:00:00.123456"),
        (".1234575", "2020-01-01 00:00:00.123458"),
        (".1234555", "2020-01-01 00:00:00.123456"),
        (".1234545", "2020-01-01 00:00:00.123454"),
        // Not a tie: something follows the five, so it is above the half whatever the parity.
        (".12345650001", "2020-01-01 00:00:00.123457"),
        // And the ordinary cases either side of a tie.
        (".1234564", "2020-01-01 00:00:00.123456"),
        (".1234566", "2020-01-01 00:00:00.123457"),
    ]
    .into_iter()
    .enumerate()
    {
        node.run(&format!(
            "INSERT INTO r VALUES ({index}, '2020-01-01 00:00:00{written}')"
        ))
        .unwrap();
        assert_eq!(
            node.rows(&format!("SELECT a FROM r WHERE id = {index}")),
            vec![vec![expected.to_owned()]],
            "{written}"
        );
    }

    // The same parser, so `timestamptz` was wrong the same way and is right the same way.
    node.run("CREATE TABLE rz (id int8 PRIMARY KEY, a timestamptz)")
        .unwrap();
    node.run("INSERT INTO rz VALUES (1, '2020-01-01 00:00:00.1234565+00')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT a FROM rz"),
        vec![vec!["2020-01-01 00:00:00.123456+00"]]
    );
}

/// The input error names `timestamp`, and the comparison error names the long form.
///
/// Two messages about one type, measured, and neither is the other's — an implementation that
/// routed both through `ColumnType::name()` would say `timestamp without time zone` for the input
/// error, which a real server never does.
#[test]
fn two_messages_about_one_type() {
    let mut node = parity::Node::new(&["CREATE TABLE e (id int8 PRIMARY KEY, a timestamp)"]);

    let error = node
        .run("INSERT INTO e VALUES (1, 'not a date')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_DATETIME_FORMAT);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type timestamp: \"not a date\""
    );

    let error = node.run("SELECT id FROM e WHERE a = 1").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "operator does not exist: timestamp without time zone = integer"
    );
}
