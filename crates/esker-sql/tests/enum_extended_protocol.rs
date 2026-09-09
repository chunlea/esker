//! The type an enum column is declared as **on the extended protocol**.
//!
//! `enum_test.rb#test_enum_mapping` and `#test_works_with_activerecord_enum` read a row back and
//! get `nil` where the label should be. The value was never wrong: run 105's capture pack put the
//! same fixture through three protocol paths and found
//!
//! ```text
//!                          value      RowDescription OID
//!   node  simple exec       "sad"      263621   <- the enum's own oid, correct
//!   node  exec_params       "sad"      21       <- int2
//!   node  prepare+exec      "sad"      21       <- int2
//!   pg19  every path        "sad"      283038
//! ```
//!
//! **`ActiveRecord` decodes by the declared OID.** Told `int2`, it parses `"sad"` as an integer and
//! gets nothing, which is where the `0` and the `nil`s come from. An enum's *storage* is its
//! ordinal (ADR 0050) and `OutputColumn::ty` is therefore `int2`; the type a client is told is the
//! enum's, carried beside it — and the extended protocol's field builder was not reading it.
//!
//! **A test of this cannot go in a corpus**, and that is the whole reason the bug survived three
//! runs: a corpus replays the *simple* protocol, which was correct, and `psql` and `pg_typeof`
//! agree with it. Only a `Describe` sees the difference. Same family as run 98's `->` reporting
//! OID 25 for a `json` column.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TABLE postgresql_enums (id int8 PRIMARY KEY, current_mood mood)",
    "INSERT INTO postgresql_enums VALUES (1, 'sad')",
];

/// The enum's own oid, read out of the catalog the way a client would.
fn enum_oid(node: &mut parity::Node) -> u32 {
    node.rows("SELECT 'mood'::regtype::oid")[0][0]
        .parse()
        .expect("the enum has no oid")
}

/// **The three protocol paths agree**, which is the whole assertion.
#[test]
fn every_path_declares_the_enums_own_oid() {
    let mut node = parity::Node::new(FIXTURE);
    let oid = enum_oid(&mut node);
    assert!(oid >= 16_384, "the enum took a built-in oid: {oid}");

    let statement = "SELECT id, current_mood FROM postgresql_enums ORDER BY id";

    // 1. The simple protocol, which was already right.
    let outcome = node.run(statement).unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, rows, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[1].type_oid, oid, "simple protocol");
    assert_eq!(rows[0][1].as_deref(), Some(b"sad".as_slice()));

    // 2. `Describe`, which is what `exec_params` and `prepare`+`execute` both send first — and
    //    where the column was being declared `int2`.
    let parsed = parse_statements(statement).unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described.fields.expect("a SELECT returns rows");
    assert_eq!(
        fields[1].type_oid, oid,
        "the extended protocol declared the enum column as {} rather than the enum's own oid",
        fields[1].type_oid
    );
    // The name and the width travel with it. **4, not -1**: an enum's `typlen` is four on a real
    // server — its own storage there is an oid — measured, and it is what this node reports too.
    // The label a client reads is variable-length all the same; `typlen` and the wire encoding are
    // two different questions and only the first is asked here.
    assert_eq!(fields[1].name, "current_mood");
    assert_eq!(fields[1].type_size, 4);
}

/// The same column reached through `SELECT *`, which is how the suite's query is written.
#[test]
fn a_wildcard_declares_it_too() {
    let mut node = parity::Node::new(FIXTURE);
    let oid = enum_oid(&mut node);
    let parsed =
        parse_statements("SELECT \"postgresql_enums\".* FROM \"postgresql_enums\"").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described.fields.expect("a SELECT returns rows");
    assert_eq!(fields[1].type_oid, oid);
}

/// **The value is unchanged**: an enum is still a label to a client, never its ordinal (ADR 0050).
#[test]
fn the_value_is_still_the_label() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT current_mood FROM postgresql_enums"),
        vec![vec!["sad"]]
    );
}
