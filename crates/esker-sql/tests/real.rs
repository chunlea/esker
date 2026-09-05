//! Contract C3 for `real` — tier 1's fifth type, and the last one that needs no typmod.
//!
//! [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md). What makes it a type rather
//! than an alias for `double precision` is the **text** and the **range**, and both are in the
//! corpus:
//!
//! * `float4out` prints the shortest digits that round-trip *as an `f32`*, so `1.0/3.0` is
//!   `0.33333334` where a `float8` says `0.3333333333333333`. Stored as a `double` it would print
//!   seventeen digits a real server never wrote.
//! * **underflow is an error, not a zero**: `1e-50` is `22003` here where a `float8` holds it as a
//!   denormal. Both ends raise, and both quote the value expanded to plain decimal.

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
fn every_real_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_real.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 22,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The text is the type: the shortest digits that round-trip at **32** bits.
#[test]
fn the_text_is_f32_wide() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8 PRIMARY KEY, r real, d float8)"]);
    node.run("INSERT INTO t VALUES (1, 0.1, 0.1), (2, 3.4028235e38, 1e308)")
        .unwrap();

    assert_eq!(
        node.rows("SELECT r, d FROM t ORDER BY id"),
        vec![vec!["0.1", "0.1"], vec!["3.4028235e+38", "1e+308"],]
    );
    assert_eq!(ColumnType::Real.oid(), 700);
    assert_eq!(ColumnType::Real.type_len(), 4);
    assert_eq!(ColumnType::Real.name(), "real");
    assert_eq!(
        node.rows("SELECT oid, typname, typinput FROM pg_type WHERE typname = 'float4'"),
        vec![vec!["700", "float4", "float4in"]]
    );
}

/// Both ends of the range raise, and **underflow is one of them**.
///
/// That is the half a reader would not guess: `1e-50` is a perfectly good `double` and is `22003`
/// as a `real`, where rounding it to zero would store a value a real server refused.
#[test]
fn both_ends_of_the_range_raise() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8 PRIMARY KEY, r real)"]);

    for statement in [
        "INSERT INTO t VALUES (1, 1e40)",
        "INSERT INTO t VALUES (1, 1e-50)",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "{statement}"
        );
        assert!(
            error.to_string().ends_with("is out of range for type real"),
            "{statement} -> `{error}`"
        );
    }

    let error = node.run("INSERT INTO t VALUES (1, 'x')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type real: \"x\""
    );
}

/// The out-of-range message quotes two different texts for one value, and `float8` shares the bug
/// this fixed.
///
/// A bare `1e40` is a **`numeric`** before anything casts it, so PostgreSQL quotes `numeric`'s own
/// text — plain decimal, no exponent — where a *string* `'1e40'` is quoted as written. This crate
/// quoted the literal as written in both cases, and `double precision` was wrong the same way:
/// nothing had captured the literal path for it, because `tests/corpus/pg19_values.txt` is the
/// *string* path by construction. `real` is only the type that surfaced it.
#[test]
fn a_numeric_literal_is_quoted_as_numeric_prints_it() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8 PRIMARY KEY, r real, d float8)"]);

    let error = node.run("INSERT INTO t VALUES (1, 1e40, 0)").unwrap_err();
    assert_eq!(
        error.to_string(),
        "\"10000000000000000000000000000000000000000\" is out of range for type real"
    );

    // The same rule one width up, which is the half `real` did not introduce.
    let error = node.run("INSERT INTO t VALUES (1, 0, 1e400)").unwrap_err();
    assert!(
        error.to_string().starts_with("\"1000000000000000000000000"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .ends_with("is out of range for type double precision"),
        "{error}"
    );

    // And a *string* keeps its own text, because the input function got the string.
    let error = node.run("INSERT INTO t VALUES (1, '1e40', 0)").unwrap_err();
    assert_eq!(error.to_string(), "\"1e40\" is out of range for type real");
}

/// The two rules it shares with `float8` and does not re-derive.
#[test]
fn nan_sorts_highest_and_minus_zero_is_zero() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8 PRIMARY KEY, r real)"]);
    node.run("INSERT INTO t VALUES (1, -0.0), (2, 'NaN'), (3, 1.5), (4, 'Infinity')")
        .unwrap();

    // `-0.0` is stored as `0`: the literal goes through `numeric`, which has no signed zero.
    assert_eq!(node.rows("SELECT r FROM t WHERE id = 1"), vec![vec!["0"]]);
    // NaN above Infinity, which is PostgreSQL's order and not IEEE's.
    assert_eq!(
        node.rows("SELECT id FROM t ORDER BY r"),
        vec![vec!["1"], vec!["3"], vec!["4"], vec!["2"]]
    );
    // And so `> 0` returns it, which is the consequence that surprises.
    assert_eq!(
        node.rows("SELECT id FROM t WHERE r > 0 ORDER BY id"),
        vec![vec!["2"], vec!["3"], vec!["4"]]
    );
}
