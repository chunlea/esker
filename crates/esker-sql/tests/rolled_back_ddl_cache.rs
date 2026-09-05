//! **A transaction's uncommitted DDL must never reach the node's shared catalog cache.**
//!
//! The cache is keyed by the catalog *version*, and a version is not a transaction identity: two
//! different transactions can both arrive at version 3, and if one of them rolled back, the other
//! is handed its state. That is only possible because something uncommitted got in.
//!
//! # The reproducer, from `r1-harness`'s bisect of `persistence_test.rb`
//!
//! ```text
//! CREATE TABLE t (id serial primary key);
//! BEGIN; ALTER TABLE t ADD "c1" varchar; ALTER TABLE t ADD "c2" varchar; ROLLBACK;
//! BEGIN;
//!   ALTER TABLE t ADD "foo" varchar;   -- reports ALTER TABLE, and pg_attribute shows foo
//!   ALTER TABLE t DROP COLUMN "foo";   -- ERROR: column "foo" of relation "t" does not exist
//! ```
//!
//! **Exactly two** DDL statements in the rolled-back transaction. Not one, not three — because the
//! rolled-back transaction had to leave the cache at the version the *second* statement of the
//! next transaction would ask for. Traced:
//!
//! ```text
//! rolled back txn:  fills the cache at version 2, then at version 3   (uncommitted)
//! ROLLBACK      :  the committed version returns to 1
//! next txn ADD  :  reads version 2, cache is at 3, so the cache is refused -- correct
//! next txn DROP :  reads version 3, cache is at 3, so the cache ANSWERS -- with the
//!                  rolled-back transaction's table, which has no `foo`
//! ```
//!
//! # Why it got in
//!
//! `Statement::writes_catalog` is what puts a transaction onto the uncached view, and it listed
//! five statements. `ALTER TABLE` was not one of them, so every `ALTER` published its uncommitted
//! table to a cache the whole node reads. Rails reaches it because
//! `test_becomes_default_sti_subclass` runs exactly two `ALTER COLUMN … DEFAULT` statements inside
//! the transaction each test is wrapped in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The reproducer, at every count from none to four — because the count is the whole shape.
#[test]
fn a_rolled_back_ddl_transaction_does_not_change_the_next_one() {
    for rolled_back in 0..=4 {
        let mut node = parity::Node::new(&["CREATE TABLE t (id serial primary key)"]);
        node.run("BEGIN").unwrap();
        for i in 0..rolled_back {
            node.run(&format!("ALTER TABLE t ADD \"c{i}\" character varying"))
                .unwrap();
        }
        node.run("ROLLBACK").unwrap();

        node.run("BEGIN").unwrap();
        node.run("ALTER TABLE t ADD \"foo\" character varying")
            .unwrap();
        assert_eq!(
            node.rows(
                "SELECT attname FROM pg_attribute \
                 WHERE attrelid = 't'::regclass AND attname = 'foo'"
            ),
            [["foo".to_owned()]],
            "{rolled_back} rolled-back statements: the ADD is visible"
        );
        assert_eq!(
            node.answer("ALTER TABLE t DROP COLUMN \"foo\"").to_string(),
            "(a command, no result set)",
            "{rolled_back} rolled-back statements: and the column it just added can be dropped"
        );
        node.run("ROLLBACK").unwrap();
    }
}

/// The rolled-back statements need not touch the table that then breaks.
///
/// **A guard, not a reproducer**: it is green before the fix as well as after, because the cache
/// is filled per relation on demand and lining up a *second* table's entry with the version the
/// next transaction asks for needs more than this. It is here so that the shape has a test at all
/// — the harness saw it fire against a real node, and in-process it does not.
#[test]
fn the_rolled_back_statements_need_not_touch_the_same_table() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id serial primary key)",
        "CREATE TABLE other (id serial primary key)",
    ]);
    node.run("BEGIN").unwrap();
    node.run("ALTER TABLE other ADD \"c1\" character varying")
        .unwrap();
    node.run("ALTER TABLE other ADD \"c2\" character varying")
        .unwrap();
    // **Reading `t` here is what makes this discriminating.** The cache is filled per relation on
    // demand, so `t` has to be *looked at* inside the doomed transaction for its definition to be
    // sitting there under that version afterwards. `ActiveRecord` does exactly this: the test that
    // runs the two `ALTER`s is not the test that then adds a column, but they share a connection
    // and the schema reads in between are what put the table in the cache.
    node.run("SELECT id FROM t").unwrap();
    node.run("ROLLBACK").unwrap();

    node.run("BEGIN").unwrap();
    node.run("ALTER TABLE t ADD \"foo\" character varying")
        .unwrap();
    assert_eq!(
        node.answer("ALTER TABLE t DROP COLUMN \"foo\"").to_string(),
        "(a command, no result set)"
    );
}

/// The rollback itself was never in doubt and is asserted anyway, because the bug looked like a
/// leak: after it the table has only the columns it started with, and the same names re-add.
#[test]
fn the_rollback_itself_is_honoured() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id serial primary key)"]);
    node.run("BEGIN").unwrap();
    node.run("ALTER TABLE t ADD \"c1\" character varying")
        .unwrap();
    node.run("ALTER TABLE t ADD \"c2\" character varying")
        .unwrap();
    node.run("ROLLBACK").unwrap();
    assert_eq!(
        node.rows(
            "SELECT attname FROM pg_attribute WHERE attrelid = 't'::regclass \
             AND attnum > 0 AND NOT attisdropped ORDER BY attnum"
        ),
        [["id".to_owned()]]
    );
    node.run("ALTER TABLE t ADD \"c1\" character varying")
        .expect("the name is free again");
}

/// **The control, and it is the diagnosis.** `CREATE INDEX` was already one of the five statements
/// `writes_catalog` named, so a pair of them never published anything and this shape was always
/// safe. It passes before the fix as well as after — which is the point: the bug was not "DDL in a
/// rolled-back transaction", it was *the statements missing from that list*, and `ALTER TABLE` was
/// the one Rails runs twice.
#[test]
fn a_pair_of_already_listed_statements_was_always_safe() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id serial primary key, a int, b int)"]);
    node.run("BEGIN").unwrap();
    node.run("CREATE INDEX t_a ON t (a)").unwrap();
    node.run("CREATE INDEX t_b ON t (b)").unwrap();
    node.run("ROLLBACK").unwrap();

    node.run("BEGIN").unwrap();
    node.run("ALTER TABLE t ADD \"foo\" character varying")
        .unwrap();
    assert_eq!(
        node.answer("ALTER TABLE t DROP COLUMN \"foo\"").to_string(),
        "(a command, no result set)"
    );
}
