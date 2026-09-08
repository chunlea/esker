//! **A `regclass` written into a `bigint` or `oid` column is a row that reads back.**
//!
//! It was not. `esker_keys::value::one_representation` says a `regclass` is one representation
//! with an `int8` and an `oid` — the comparison rule that keeps `WHERE attrelid = 'x'::regclass`
//! comparing — and `exec::assign::into_column` returned any value that `fits` its column
//! unchanged. The row codec then wrote the datum's own shape, eight bytes of oid **and a
//! length-prefixed name**, into a column the catalog says holds eight bytes, and the next read of
//! that row was refused as `bytes after the last column`. A valid `INSERT` wrote a row nobody
//! could read again.
//!
//! PostgreSQL takes `regclass` into `bigint` through `oid` (an assignment cast and then an
//! implicit one) and stores the number; the name is the output function's business alone. So the
//! number is what is stored here too, at the assignment, and `encode_row` refuses a `reg*` datum
//! that still carries its name into a column of the plain type — the second line, so that no
//! other caller can reach the same row.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE rc_target (n bigint, s text)",
    "CREATE TABLE rc_oid (o oid, s text)",
    "CREATE TABLE rc_int (i integer)",
];

/// **The statement that wrote an unreadable row.** A second column after the `regclass` is what
/// made the damage visible as wrong values rather than a refusal: the name's bytes were decoded
/// as the start of `s`.
#[test]
fn a_regclass_into_a_bigint_column_stores_its_oid() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO rc_target VALUES ('rc_target'::regclass, 'x')")
        .unwrap();
    // The row reads back at all, and the number is the relation's oid.
    assert_eq!(
        node.rows("SELECT n = 'rc_target'::regclass, s FROM rc_target"),
        vec![vec!["t", "x"]]
    );
    assert_eq!(
        node.rows("SELECT n FROM rc_target"),
        node.rows("SELECT 'rc_target'::regclass::bigint")
    );
    // `UPDATE` is the same path.
    node.run("UPDATE rc_target SET n = 'rc_oid'::regclass")
        .unwrap();
    assert_eq!(
        node.rows("SELECT n = 'rc_oid'::regclass, s FROM rc_target"),
        vec![vec!["t", "x"]]
    );
}

/// An `oid` column takes a `regclass` and a `regtype` the same way: the number, and nothing else.
#[test]
fn a_reg_datum_into_an_oid_column_stores_its_oid() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO rc_oid VALUES ('rc_oid'::regclass, 'a'), ('text'::regtype, 'b')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT o = 'rc_oid'::regclass, s FROM rc_oid ORDER BY s"),
        vec![vec!["t", "a"], vec!["f", "b"]]
    );
    assert_eq!(
        node.rows("SELECT o::text, s FROM rc_oid WHERE s = 'b'"),
        vec![vec!["25", "b"]]
    );
}

/// And the narrower integers go through the same cast a `::integer` would, out-of-range included.
#[test]
fn a_regclass_into_an_integer_column_is_the_assignment_cast() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO rc_int VALUES ('rc_int'::regclass)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT i::bigint = 'rc_int'::regclass FROM rc_int"),
        vec![vec!["t"]]
    );
}
