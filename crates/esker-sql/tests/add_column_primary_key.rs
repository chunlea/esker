//! **`ALTER TABLE … ADD COLUMN … PRIMARY KEY`, and the `serial` that comes with it.**
//!
//! `compatibility_test.rb` builds a table with no primary key and then adds one:
//!
//! ```ruby
//! create_table :legacy_primary_keys, id: false do |t| t.integer :dummy end
//! add_column :legacy_primary_keys, :id, :primary_key
//! ```
//!
//! which the adapter sends as `ALTER TABLE "legacy_primary_keys" ADD "id" serial primary key`
//! (`primary_key: "bigserial primary key"` in the type map, `serial` under a legacy migration).
//! Both halves were refused by name, and the refused statement left the table behind for the next
//! test — 4 failures plus 4 `relation "legacy_primary_keys" already exists` in its wake.
//!
//! # What a real server does, measured
//!
//! ```text
//! ALTER TABLE lpk ADD "id" serial primary key
//!   attname  format_type  attnotnull     id  integer  t
//!   conname                contype       lpk_pkey    p
//!   pg_get_expr                          nextval('lpk_id_seq'::regclass)
//! ```
//!
//! `bigserial` gives `bigint`. On a table that already holds rows PostgreSQL fills the column from
//! the sequence — `1`, `2` for two rows — which is the table rewrite this `ALTER` does not do here;
//! that case stays refused and says so.
//!
//! Every row above is `tests/captures/pg19_add_column_primary_key.txt`, taken in one rolled-back
//! transaction. **The row that decides the scope is the last group**: on a table that already holds
//! rows PostgreSQL fills the new column from the sequence (two rows get `1` and `2`), which is a
//! table rewrite this `ALTER` does not do.
//!
//! **The empty-table case is not a special case, it is the rule with its question asked.** The
//! `NOT NULL` refusal on this same statement was once justified as "needs a value for every row
//! already stored", and that was half the rule: an empty table has no row to hold a NULL. The
//! executor can see the rows and the parser cannot, so the question belongs there — the same move,
//! now for `serial` and for `PRIMARY KEY`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The suite's own statement, on the empty table it is sent to.
#[test]
fn a_serial_primary_key_can_be_added_to_an_empty_table() {
    let mut node = parity::Node::new(&["CREATE TABLE legacy_primary_keys (dummy integer)"]);
    node.run("ALTER TABLE \"legacy_primary_keys\" ADD \"id\" serial primary key")
        .unwrap();

    // The column is `integer`, not null, and filled from a sequence — all three measured on 19beta1.
    assert_eq!(
        node.rows(
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull FROM pg_attribute a WHERE a.attrelid = 'legacy_primary_keys'::regclass AND a.attnum > 0 ORDER BY a.attnum"
        ),
        [
            ["dummy".to_owned(), "integer".to_owned(), "f".to_owned()],
            ["id".to_owned(), "integer".to_owned(), "t".to_owned()],
        ]
    );
    // And it behaves as a primary key: the sequence fills it and a duplicate is refused.
    node.run("INSERT INTO legacy_primary_keys (dummy) VALUES (7)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT dummy, id FROM legacy_primary_keys"),
        [["7".to_owned(), "1".to_owned()]]
    );
    assert!(
        node.run("INSERT INTO legacy_primary_keys (dummy, id) VALUES (8, 1)")
            .is_err(),
        "the added column must really be a primary key"
    );
}

/// `bigserial` is the non-legacy spelling and gives `bigint`, measured.
#[test]
fn bigserial_is_the_other_spelling() {
    let mut node = parity::Node::new(&["CREATE TABLE t (dummy integer)"]);
    node.run("ALTER TABLE t ADD \"id\" bigserial primary key")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE a.attrelid = 't'::regclass AND a.attname = 'id'"
        ),
        [["bigint".to_owned()]]
    );
}

/// A plain `PRIMARY KEY` with no sequence behind it — the same statement without the `serial`.
#[test]
fn a_plain_primary_key_column_can_be_added() {
    let mut node = parity::Node::new(&["CREATE TABLE t (dummy integer)"]);
    node.run("ALTER TABLE t ADD COLUMN id int8 PRIMARY KEY")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.contype FROM pg_constraint c WHERE c.conrelid = 't'::regclass AND c.contype = 'p'"
        ),
        [["p".to_owned()]]
    );
}

/// **A table that already holds rows keeps the refusal**, and says which half is missing.
///
/// PostgreSQL fills the column from the sequence; this `ALTER` does not rewrite rows, so it is
/// named rather than half-done. The empty case above is what the suite sends.
#[test]
fn a_table_with_rows_is_still_refused_by_name() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (dummy integer)",
        "INSERT INTO t VALUES (1), (2)",
    ]);
    let answer = node
        .answer("ALTER TABLE t ADD \"id\" serial primary key")
        .to_string();
    assert!(
        answer.starts_with("!0A000") && answer.contains("row"),
        "expected a named refusal mentioning the rows, got {answer}"
    );
}

/// **The constraint form**, which is what `change_table`'s `t.primary_key :id` sends when the
/// column is already there. Captured: `ALTER TABLE g1_apk3 ADD CONSTRAINT g1_apk3_pkey PRIMARY KEY
/// (id)` is accepted and the constraint is named as written.
#[test]
fn the_constraint_form_adds_a_primary_key_too() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8, dummy integer)"]);
    node.run("ALTER TABLE t ADD CONSTRAINT t_pkey PRIMARY KEY (id)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.conname FROM pg_constraint c WHERE c.conrelid = 't'::regclass AND c.contype = 'p'"
        ),
        [["t_pkey".to_owned()]]
    );
    // It is a key, not a note: the column is NOT NULL and a duplicate is refused.
    node.run("INSERT INTO t VALUES (1, 7)").unwrap();
    assert!(node.run("INSERT INTO t VALUES (1, 8)").is_err());
}

/// **A second primary key is refused with PostgreSQL's own sentence**, on both spellings.
///
/// Captured: `42P16 multiple primary keys for table "g1_apk5" are not allowed`. This is the guard
/// that keeps "declare a key" from becoming "replace the key", and it reads
/// `primary_key_name` — a keyless table's `primary_key` points at its hidden row-id column and is
/// never empty, so the *name* is the only honest test.
#[test]
fn a_second_primary_key_is_refused() {
    let mut node = parity::Node::new(&["CREATE TABLE t (a int4 PRIMARY KEY)"]);
    assert_eq!(
        node.answer("ALTER TABLE t ADD COLUMN b int4 PRIMARY KEY")
            .to_string(),
        "!42P16 multiple primary keys for table \"t\" are not allowed"
    );
    assert_eq!(
        node.answer("ALTER TABLE t ADD CONSTRAINT t_pkey2 PRIMARY KEY (a)")
            .to_string(),
        "!42P16 multiple primary keys for table \"t\" are not allowed"
    );
}
