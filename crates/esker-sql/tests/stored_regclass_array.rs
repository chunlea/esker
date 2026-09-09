//! **A `regclass[]` column, element by element** — `debts-v1.1.md` #38.
//!
//! #35 made the scalar a column: the row holds eight bytes and no name, and `crate::row::decode_row`
//! puts the name back on the way out. The array was refused by name in the same commit, because
//! its write path went through the element type's *input function* and `regclassin` is a catalog
//! lookup it cannot be handed there.
//!
//! That turned out to be one line of the story. The refusal was written from a failure whose real
//! cause was a **fold** — `ARRAY['t'::regclass]` reaching `Datum::from_text` — and closing that
//! inside #35 left the write path already correct: `esker_keys::row` encodes an array element by
//! element, so a `Datum::RegClass` element writes its eight bytes exactly as the scalar does. What
//! was left was the other half of #35's own design, applied one level in: **the resolution on the
//! way out had to walk into arrays**, and it only knew about scalars.
//!
//! So the rule is the same rule, and this file is the same three questions asked of an array: it
//! stores, it reads back, and it follows a rename.
//!
//! Measured in `tests/captures/pg19_stored_regclass_array.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE ra (id int8)",
        "CREATE TABLE rb (id int8)",
        "CREATE TABLE rj (id int8, rs regclass[])",
    ])
}

/// **It stores**, and the column is a `regclass[]` to a client.
#[test]
fn a_regclass_array_is_a_column() {
    let mut node = node();
    node.run("INSERT INTO rj VALUES (1, '{ra,rb}'::regclass[])")
        .unwrap();
    node.run("INSERT INTO rj VALUES (2, ARRAY['ra'::regclass]), (3, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT attname, atttypid FROM pg_attribute WHERE attrelid = 'rj'::regclass AND attnum > 0 ORDER BY attnum"),
        vec![vec!["id", "20"], vec!["rs", "2210"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(rs) FROM rj WHERE id = 1"),
        vec![vec!["regclass[]"]]
    );
}

/// **It reads back as names**, which is the half that was missing: the row holds the numbers.
#[test]
fn the_elements_come_back_as_names() {
    let mut node = node();
    node.run(
        "INSERT INTO rj VALUES (1, '{ra,rb}'::regclass[]), (2, ARRAY['ra'::regclass]), (3, NULL)",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, rs FROM rj ORDER BY id"),
        vec![vec!["1", "{ra,rb}"], vec!["2", "{ra}"], vec!["3", "\\N"],]
    );
    // The element and the whole are one value, so both doors give the name.
    assert_eq!(
        node.rows("SELECT rs[1], array_length(rs, 1), rs::text FROM rj WHERE id = 1"),
        vec![vec!["ra", "2", "{ra,rb}"]]
    );
    assert_eq!(
        node.rows("SELECT unnest(rs) FROM rj WHERE id = 1"),
        vec![vec!["ra"], vec!["rb"]]
    );
}

/// **It follows a rename**, element by element — the pin the whole design exists for.
#[test]
fn a_rename_reaches_inside_the_array() {
    let mut node = node();
    node.run("INSERT INTO rj VALUES (1, '{ra,rb}'::regclass[]), (2, ARRAY['ra'::regclass])")
        .unwrap();
    node.run("ALTER TABLE ra RENAME TO rz").unwrap();
    assert_eq!(
        node.rows("SELECT id, rs FROM rj ORDER BY id"),
        vec![vec!["1", "{rz,rb}"], vec!["2", "{rz}"]],
        "the stored elements must follow the relation's name, not remember it"
    );
    // And a dropped relation leaves its number inside the array, exactly as a scalar does.
    node.run("DROP TABLE rz CASCADE").unwrap();
    let printed = node.rows("SELECT rs FROM rj WHERE id = 2");
    assert_eq!(printed.len(), 1);
    assert!(
        printed[0][0].starts_with('{')
            && printed[0][0][1..].starts_with(|c: char| c.is_ascii_digit()),
        "a dropped relation prints its digits inside the array too: {printed:?}"
    );
}

/// A NULL element is not the array being NULL, and neither is a name that answers to nothing.
#[test]
fn a_null_element_and_a_bad_name_are_two_different_things() {
    let mut node = node();
    node.run("INSERT INTO rj VALUES (4, '{ra,NULL}'::regclass[])")
        .unwrap();
    assert_eq!(
        node.rows("SELECT rs FROM rj WHERE id = 4"),
        vec![vec!["{ra,NULL}"]]
    );
    // The bad name fails at the cast, before any row is written -- the element's own refusal.
    let error = node
        .run("INSERT INTO rj VALUES (5, '{nosuchrel}'::regclass[])")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "relation \"nosuchrel\" does not exist");
    assert_eq!(node.rows("SELECT count(*) FROM rj"), vec![vec!["1"]]);
}
