//! A `DEFAULT` on an **enum** column, which is a label and not the ordinal it is stored as.
//!
//! Run 95's `invalid input syntax for type smallint: "blue"` — 2 tests in
//! `adapters/postgresql/invertible_migration_test.rb`, `test_migrate_revert_create_enum` and
//! `test_migrate_revert_rename_enum_value`. Both come from one line of a migration:
//! `t.enum :best_color, enum_type: "color", default: "blue", null: false`.
//!
//! An enum's stored value is its ordinal ([ADR 0050](../../../docs/adr/0050-an-enum-is-its-ordinal.md)),
//! so a column's `ty` at lowering is an `int2` placeholder until the catalog has been read —
//! folding `'blue'` against it hands a label to the `int2` input function. `ALTER TABLE … ADD
//! COLUMN` had the guard for this and `CREATE TABLE` did not, which is the whole of the bug: two
//! statements that build the same column, one of them reading the default with the wrong type.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

#[test]
fn an_enum_default_is_a_label_in_both_statements_that_can_declare_one() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TYPE color AS ENUM ('blue', 'green')")
        .unwrap();

    // The migration's own line, and the one that was `22P02`.
    session
        .run("CREATE TABLE enums (id int8, best_color color DEFAULT 'blue' NOT NULL)")
        .unwrap();
    // The same column reached the other way, which already worked — kept beside it so the two
    // cannot drift apart again.
    session.run("CREATE TABLE later (id int8)").unwrap();
    session
        .run("ALTER TABLE later ADD COLUMN best_color color DEFAULT 'green'")
        .unwrap();

    session.run("INSERT INTO enums (id) VALUES (1)").unwrap();
    session.run("INSERT INTO later (id) VALUES (1)").unwrap();

    // **The label, not the ordinal.** The default has to survive as a label all the way to the
    // row: storing `blue` as text or `1` as a number would both read back wrong.
    assert_eq!(
        session.rows("SELECT best_color FROM enums"),
        [[Some("blue".to_owned())]],
        "a CREATE TABLE default is the label it was written as"
    );
    assert_eq!(
        session.rows("SELECT best_color FROM later"),
        [[Some("green".to_owned())]],
        "and so is an ADD COLUMN one"
    );

    // And it is stored as the ordinal underneath, which is what makes the enum's order its
    // declaration order rather than its labels' spelling.
    assert_eq!(
        session.rows("SELECT best_color FROM enums WHERE best_color = 'blue'"),
        [[Some("blue".to_owned())]],
        "the stored default compares as the label it is"
    );
}

/// A label the type does not have is the error PostgreSQL gives, not the `int2` one.
#[test]
fn a_default_that_is_not_a_label_names_the_enum() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TYPE color AS ENUM ('blue', 'green')")
        .unwrap();
    let error = session
        .run("CREATE TABLE bad (c color DEFAULT 'purple')")
        .expect_err("a label the type does not have must be refused");
    assert_eq!(
        error.sqlstate(),
        "22P02",
        "{error} is not the sqlstate a real server gives"
    );
    assert!(
        error.to_string().contains("color"),
        "{error} must name the enum and not its storage type"
    );
}
