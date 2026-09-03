//! `oid`, against PostgreSQL 19beta1 — the type every catalog identifier really has.
//!
//! It has no corpus of its own: the statements live in `tests/corpus/pg19_regtype.txt`, which is
//! where `'oid'::regtype::oid` and `'23'::oid` were captured, and this file is what a reader
//! comes to for the type itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Unsigned is the whole of what separates it from `int4`**, and both ends prove it.
///
/// Measured on 19beta1: `(-1)::oid` is `4294967295` — a negative *wraps*, because the cast
/// reinterprets the bits rather than refusing them — and `4294967296` is `22003`. An
/// implementation that stored it as an `i32` would answer `-1` where a real server answers the
/// top of the range, and one that stored it as an `i64` would accept a value no real server does.
#[test]
fn an_oid_is_unsigned_at_both_ends() {
    let mut node = parity::Node::new(&[]);

    assert_eq!(node.rows("SELECT '123'::oid"), vec![vec!["123"]]);
    assert_eq!(node.rows("SELECT '0'::oid"), vec![vec!["0"]]);
    assert_eq!(
        node.rows("SELECT '4294967295'::oid"),
        vec![vec!["4294967295"]]
    );
    // The one a reader guesses wrong.
    assert_eq!(node.rows("SELECT '-1'::oid"), vec![vec!["4294967295"]]);

    let error = node.run("SELECT '4294967296'::oid").unwrap_err();
    assert_eq!(error.sqlstate(), "22003");
    assert_eq!(
        error.to_string(),
        "value \"4294967296\" is out of range for type oid"
    );
    let error = node.run("SELECT 'abc'::oid").unwrap_err();
    assert_eq!(error.sqlstate(), "22P02");
}

/// It is a number: it compares with the integers, and an index over it sorts unsigned.
#[test]
fn it_compares_with_the_integers_and_sorts_unsigned() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE od (id int8 PRIMARY KEY, o oid)",
        "INSERT INTO od VALUES (1, '26')",
        "INSERT INTO od VALUES (2, '4294967295')",
        "INSERT INTO od VALUES (3, '0')",
    ] {
        node.run(statement).unwrap();
    }
    // Unsigned order: the top of the range sorts last, not first.
    assert_eq!(
        node.rows("SELECT id FROM od ORDER BY o"),
        vec![vec!["3"], vec!["1"], vec!["2"]]
    );
    // A number compares with a number, across the widths.
    assert_eq!(node.rows("SELECT id FROM od WHERE o = 26"), vec![vec!["1"]]);
    assert_eq!(
        node.rows("SELECT id FROM od WHERE o > 26 ORDER BY id"),
        vec![vec!["2"]]
    );
}

/// `'oid'::regtype` follows from `ColumnType::ALL`, as `uuid`'s did.
#[test]
fn the_regtype_probe_follows_the_type() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 'oid'::regtype::oid"), vec![vec!["26"]]);
    assert_eq!(node.rows("SELECT 'oid'::regtype"), vec![vec!["oid"]]);
    assert_eq!(node.rows("SELECT format_type(26, -1)"), vec![vec!["oid"]]);
    assert_eq!(
        node.rows("SELECT oid, typname, typlen, typcategory FROM pg_type WHERE typname = 'oid'"),
        vec![vec!["26", "oid", "4", "N"]]
    );
}
