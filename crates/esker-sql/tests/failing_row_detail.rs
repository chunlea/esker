//! **`DETAIL: Failing row contains (…)` prints the columns a user can see, and nothing else.**
//!
//! A row in this node is not the row PostgreSQL prints. Column 0 of a table with no primary key is
//! the internal row id, and a column `DROP COLUMN` tombstoned is still in the row (ADR 0051), so
//! rendering the stored row put values in the message that no client can account for:
//!
//! ```text
//! CREATE TABLE pk_auto (id integer NOT NULL)     PostgreSQL   (null)        this node  (1, null)
//! CREATE TABLE t3 (a int NOT NULL, b text)                    (null, x)                (1, null, x)
//! ```
//!
//! Found from `PersistenceTest#test_model_with_no_auto_populated_fields_still_returns_primary_key_after_insert`,
//! whose table is a single `integer NOT NULL` filled by a trigger — so its failure message quoted
//! two values for a one-column table. All expected values below were measured on 19beta1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The shape that found it: one column, no primary key, so the row id is column 0.
#[test]
fn a_one_column_table_names_one_value() {
    let mut node = parity::Node::new(&["CREATE TABLE pk_auto (id integer NOT NULL)"]);
    assert_eq!(
        node.answer("INSERT INTO pk_auto DEFAULT VALUES")
            .to_string(),
        "!23502 null value in column \"id\" of relation \"pk_auto\" violates not-null constraint \
         DETAIL: Failing row contains (null)."
    );
}

/// Two columns and a value in the second, which is what says the *order* survived the filtering.
#[test]
fn the_other_columns_values_are_still_there_and_in_order() {
    let mut node = parity::Node::new(&["CREATE TABLE t3 (a int NOT NULL, b text)"]);
    assert_eq!(
        node.answer("INSERT INTO t3 (b) VALUES ('x')").to_string(),
        "!23502 null value in column \"a\" of relation \"t3\" violates not-null constraint \
         DETAIL: Failing row contains (null, x)."
    );
}

/// **A tombstoned column is not a column.** `DROP COLUMN` leaves the value in the row, and the
/// message must not mention it — measured: PostgreSQL prints `(null)`, not `(null, null)`.
#[test]
fn a_dropped_column_is_not_named() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t2 (a int NOT NULL, b int)",
        "ALTER TABLE t2 DROP COLUMN b",
    ]);
    assert_eq!(
        node.answer("INSERT INTO t2 VALUES (NULL)").to_string(),
        "!23502 null value in column \"a\" of relation \"t2\" violates not-null constraint \
         DETAIL: Failing row contains (null)."
    );
}

/// **A table with a primary key has no row id**, so it was always right — and stays right, which
/// is the control that says the fix filtered a column rather than a position.
#[test]
fn a_table_with_a_primary_key_was_never_wrong() {
    let mut node =
        parity::Node::new(&["CREATE TABLE keyed (id bigint PRIMARY KEY, a int NOT NULL, b text)"]);
    assert_eq!(
        node.answer("INSERT INTO keyed (id, b) VALUES (7, 'x')")
            .to_string(),
        "!23502 null value in column \"a\" of relation \"keyed\" violates not-null constraint \
         DETAIL: Failing row contains (7, null, x)."
    );
}

/// `CHECK` builds the same sentence from the same row and had the same bug.
#[test]
fn a_check_constraint_names_the_same_columns() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ck (a int, b text, CONSTRAINT ck_positive CHECK (a > 0))",
    ]);
    assert_eq!(
        node.answer("INSERT INTO ck (a, b) VALUES (-1, 'x')")
            .to_string(),
        "!23514 new row for relation \"ck\" violates check constraint \"ck_positive\" \
         DETAIL: Failing row contains (-1, x)."
    );
}
