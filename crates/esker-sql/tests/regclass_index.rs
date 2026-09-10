//! **An index on a `regclass` column** — `debts-v1.1.md` #39.
//!
//! A real server allows one, and a **primary key** over the same column already worked here: the
//! row key is `encode_row`-shaped and took #35's new eight-byte form for free, while the secondary
//! index has its own encoding and its own `is_index_key` list, which still refused `RegClass`. That
//! asymmetry — one kind of key working and the other refused, over the same column — is what said
//! this was a list entry rather than a design question.
//!
//! **The key is the number, and the order is the number's.** `ORDER BY r` over relations created
//! as `ra` then `rb`, with `ra` renamed to `rz`, is `rz` before `rb` — measured. A key over the
//! printed *name* would sort them the other way and would also have to be rebuilt on every
//! rename, which is the same reason the row does not carry the name (#35).
//!
//! Measured in `tests/captures/pg19_regclass_index.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE ra (id int8)",
        "CREATE TABLE rb (id int8)",
        "CREATE TABLE rk (id int8, r regclass)",
    ])
}

/// **The index is built, and a lookup through it answers.**
#[test]
fn a_regclass_column_takes_an_index() {
    let mut node = node();
    node.run("CREATE INDEX rk_r ON rk (r)").unwrap();
    node.run("INSERT INTO rk VALUES (1, 'ra'::regclass), (2, 'rb'::regclass), (3, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, r FROM rk WHERE r = 'ra'::regclass"),
        vec![vec!["1", "ra"]]
    );
    // The catalog reports it like any other index.
    assert_eq!(
        node.rows("SELECT indisunique FROM pg_index WHERE indrelid = 'rk'::regclass"),
        vec![vec!["f"]]
    );
}

/// **The key is the oid**, which a rename proves twice over: the lookup still finds the row, and
/// the value it returns has the new name.
#[test]
fn a_rename_does_not_move_the_key() {
    let mut node = node();
    node.run("CREATE INDEX rk_r ON rk (r)").unwrap();
    node.run("INSERT INTO rk VALUES (1, 'ra'::regclass), (2, 'rb'::regclass)")
        .unwrap();
    node.run("ALTER TABLE ra RENAME TO rz").unwrap();
    assert_eq!(
        node.rows("SELECT id, r FROM rk WHERE r = 'rz'::regclass"),
        vec![vec!["1", "rz"]],
        "the key never moved, so the row is still found -- under its new name"
    );
    // **The order is the number's and not the name's**: `rz` was created first, so it sorts first.
    assert_eq!(
        node.rows("SELECT id, r FROM rk ORDER BY r"),
        vec![vec!["1", "rz"], vec!["2", "rb"]]
    );
}

/// A unique index over one enforces uniqueness on the **oid**.
#[test]
fn a_unique_index_over_a_regclass_enforces_uniqueness() {
    let mut node = node();
    node.run("CREATE UNIQUE INDEX rk_r_u ON rk (r)").unwrap();
    node.run("INSERT INTO rk VALUES (1, 'ra'::regclass)")
        .unwrap();
    let error = node
        .run("INSERT INTO rk VALUES (2, 'ra'::regclass)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    // A rename does not make the duplicate acceptable: the key is the number either way.
    node.run("ALTER TABLE ra RENAME TO rz").unwrap();
    let error = node
        .run("INSERT INTO rk VALUES (3, 'rz'::regclass)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
}
