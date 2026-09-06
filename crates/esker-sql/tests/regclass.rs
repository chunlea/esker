//! `'x'::regclass` is a `regclass`, and a client is told so.
//!
//! Three `postgresql_adapter_test.rb` failures come from this one field and none of them mentions
//! a type: `ActiveRecord` reloads its type map when a `RowDescription` carries an OID it does not
//! know, warns once, and treats the value as a String. This node described the cast as a `bigint`
//! — oid 20, which it knows perfectly well — so nothing happened, and all three tests observed the
//! absence (`tests/captures/pg19_unknown_oid.txt`).
//!
//! The value model is [ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)'s
//! one letter along: **the oid is the value, the name is the output function**. Measured in
//! `tests/captures/pg19_regclass.txt`, and the two rows that decide it are here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE rc_a (id int8 PRIMARY KEY, t text)",
    "CREATE TABLE rc_b (id int8 PRIMARY KEY)",
];

/// The oid this node puts in the `RowDescription`, which is the whole of what the three tests see.
#[test]
fn a_regclass_cast_is_described_as_2205() {
    let mut node = parity::Node::new(FIXTURE);

    let outcome = node.run("SELECT 'rc_a'::regclass").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("a SELECT answered no rows at all");
    };
    assert_eq!(fields.len(), 1);
    // 2205 is `regclass`. It was 20 — `bigint` — and that is why nothing reloaded.
    assert_eq!(fields[0].type_oid, 2205);
    // **The column is named after the type**, which is what a real server calls an unaliased cast.
    assert_eq!(fields[0].name, "regclass");
    // Four bytes, as every `reg*` type reports, whatever this node's value is wide enough to hold.
    assert_eq!(fields[0].type_size, 4);
}

/// It prints as the relation's name, not as a number.
#[test]
fn a_regclass_prints_as_the_relation() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SELECT 'rc_a'::regclass"), vec![vec!["rc_a"]]);
    assert_eq!(
        node.rows("SELECT pg_typeof('rc_a'::regclass)"),
        vec![vec!["regclass"]]
    );
}

/// **The comparison every catalog query in this repository makes.**
///
/// `WHERE attrelid = 'x'::regclass` worked before this unit because both sides were `bigint`.
/// The cast is a `regclass` now and the column is still a `bigint`, so this is the assertion that
/// says the pair still compares — the temporary `(RegClass, Int8)` entry in
/// `esker_keys::value`'s compatibility list, and the numbers' family in `exec::query`.
#[test]
fn a_regclass_still_compares_against_the_catalog_columns_it_is_written_beside() {
    let mut node = parity::Node::new(FIXTURE);

    let rows = node.rows("SELECT count(*) FROM pg_attribute WHERE attrelid = 'rc_a'::regclass");
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0][0], "0", "the join found no columns for rc_a");

    // And it names the right relation: `rc_b` has one column where `rc_a` has two.
    let a = node.rows("SELECT count(*) FROM pg_attribute WHERE attrelid = 'rc_a'::regclass");
    let b = node.rows("SELECT count(*) FROM pg_attribute WHERE attrelid = 'rc_b'::regclass");
    assert_ne!(a, b, "two different relations counted the same columns");
}

/// A name that names no relation is `42P01`, before any value is read.
#[test]
fn a_name_that_is_not_a_relation_is_42p01() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node.run("SELECT 'nosuchrel'::regclass").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
}

/// **Two spellings of one relation are one value**, which is what makes the oid the value and the
/// name the rendering rather than the other way round.
#[test]
fn two_spellings_of_one_relation_are_equal() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT 'rc_a'::regclass = 'public.rc_a'::regclass"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT 'rc_a'::regclass = 'rc_b'::regclass"),
        vec![vec!["f"]]
    );
}
