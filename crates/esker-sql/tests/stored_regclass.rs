//! **A `regclass` is a column, and the row holds its number** — `debts-v1.1.md` #35.
//!
//! The row said a `regclass` could not be a stored column here, and gave the reason: this node's
//! `Datum::RegClass` carries the printed name beside the oid, so storing one would write the name
//! into the row — and a name in a row goes stale the moment its relation is renamed, where a real
//! server resolves it at output time from the number alone.
//!
//! So the row holds **eight bytes and no name** (`esker_keys::row`), and the name is put back by
//! `crate::row::decode_row`, which takes the rule as an argument. Every caller in `esker-sql`
//! passes one explicitly; the ones that pass `None` are the paths where no value is printed — a
//! foreign-key check compares oids, a table rewrite re-encodes what it decoded, and a differential
//! diff wants the stable form. **Passing `None` where a client would see the value is not a crash
//! and not corruption**: it leaves the unresolved form, which is a real server's own answer for an
//! oid that names nothing, measured. That is the failure mode this design chose.
//!
//! **What this unit does not do**, both named rather than approximated: a `regclass[]` *column*
//! (the array's write path rebuilds its elements through the element type's input function, and
//! `regclassin` needs a catalog it cannot reach there — the expression type is unaffected and
//! still answers 2210), and a **secondary index** on such a column, which a real server allows. A
//! primary key over one works, which is what says the stored form is a key-shaped value already.
//!
//! Measured in `tests/captures/pg19_stored_regclass.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_stored_regclass_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_stored_regclass.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The name is not in the row**, which is what renaming the relation proves.
///
/// Nothing rewrites the row and nothing reconciles it: the same eight bytes print one name before
/// the rename and another after. A node that stored the name would answer `rc_a` here forever, and
/// no other statement in this file could tell the difference.
#[test]
fn a_rename_changes_what_an_already_stored_regclass_prints() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rc_a (id int8)",
        "CREATE TABLE rc_h (id int8, r regclass)",
    ]);
    node.run("INSERT INTO rc_h VALUES (1, 'rc_a'::regclass), (2, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, r FROM rc_h ORDER BY id"),
        vec![vec!["1", "rc_a"], vec!["2", "\\N"]]
    );
    node.run("ALTER TABLE rc_a RENAME TO rc_b").unwrap();
    assert_eq!(
        node.rows("SELECT id, r FROM rc_h ORDER BY id"),
        vec![vec!["1", "rc_b"], vec!["2", "\\N"]],
        "the stored value must follow the relation's name, not remember it"
    );
    // The number never moved, which is the other half of the same fact.
    assert_eq!(
        node.rows("SELECT r::text, r::oid = 'rc_b'::regclass::oid FROM rc_h WHERE id = 1"),
        vec![vec!["rc_b", "t"]]
    );
    // And the wire says `regclass`, not the `text` it prints as.
    let outcome = node.run("SELECT r FROM rc_h WHERE id = 1").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 2205);
}

/// **A dropped relation leaves the number**: not NULL, not an error, the digits.
///
/// This is the case that makes the unresolved form safe to fall back to anywhere — it is a real
/// server's own rendering for an oid that names nothing, so a reader with no catalog to ask is
/// wrong the way a dangling oid is wrong rather than in a way no server would produce.
#[test]
fn a_dropped_relation_leaves_the_oid_behind() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rc_a (id int8)",
        "CREATE TABLE rc_h (id int8, r regclass)",
    ]);
    node.run("INSERT INTO rc_h VALUES (1, 'rc_a'::regclass), (2, NULL)")
        .unwrap();
    node.run("DROP TABLE rc_a CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT id, r IS NULL, r::text = r::oid::text FROM rc_h ORDER BY id"),
        vec![vec!["1", "f", "t"], vec!["2", "t", "\\N"]]
    );
}

/// **`'nosuchrel'::regclass` fails at the cast**, before any row is written.
#[test]
fn a_name_that_answers_to_nothing_never_reaches_the_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rc_a (id int8)",
        "CREATE TABLE rc_h (id int8, r regclass)",
    ]);
    let error = node
        .run("INSERT INTO rc_h VALUES (3, 'nosuchrel'::regclass)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "relation \"nosuchrel\" does not exist");
    // Nothing was written.
    assert_eq!(node.rows("SELECT count(*) FROM rc_h"), vec![vec!["0"]]);
}

/// **A `regclass` is a primary key**, which says the stored form is already a key-shaped value.
#[test]
fn a_regclass_is_a_primary_key() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rc_a (id int8)",
        "CREATE TABLE rc_i (r regclass PRIMARY KEY)",
    ]);
    node.run("INSERT INTO rc_i VALUES ('rc_a'::regclass)")
        .unwrap();
    assert_eq!(node.rows("SELECT r FROM rc_i"), vec![vec!["rc_a"]]);
    // The key resolves its name on the way out too, which is the same rule reached by the other
    // decode path — a point get rather than a scan.
    assert_eq!(
        node.rows("SELECT r FROM rc_i WHERE r = 'rc_a'::regclass"),
        vec![vec!["rc_a"]]
    );
}

/// **Nothing is a named gap here any more.**
///
/// This unit left two, and both closed within the hour: the `regclass[]` column with #38 — which
/// was this unit's own resolution not walking into an array — and the secondary index with #39,
/// which was two list entries and an `i64` arm, the row key having taken the new form for free.
/// What is left is `tests/stored_regclass_array.rs` and `tests/regclass_index.rs`.
#[test]
fn the_two_shapes_this_unit_named_are_closed() {
    let mut node = parity::Node::new(&["CREATE TABLE rc_a (id int8)"]);
    node.run("CREATE TABLE rc_j (id int8, rs regclass[])")
        .unwrap();
    node.run("CREATE TABLE rc_k (id int8, r regclass)").unwrap();
    node.run("CREATE INDEX rc_k_r ON rc_k (r)").unwrap();
    // The expression type was never in doubt and still answers 2210.
    let outcome = node.run("SELECT ARRAY['rc_a'::regclass]").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 2210);
}
