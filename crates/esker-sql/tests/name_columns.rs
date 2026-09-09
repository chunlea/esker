//! **The catalog's identifier columns are `name`, and that is what makes a 64-character lookup
//! find a 63-character table.**
//!
//! `compatibility_test#test_create_table_on_7_0` and `#test_rename_table_on_7_0`:
//!
//! ```ruby
//! long_table_name = "a" * (connection.table_name_length + 1)   # 64 characters
//! create_table long_table_name                                  # or rename_table :more_testings, …
//! assert connection.table_exists?(long_table_name)              # asked with all 64
//! ```
//!
//! and `table_exists?` on PostgreSQL is a `pg_class` lookup, not an `information_schema` one:
//!
//! ```sql
//! SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace
//!  WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = '<64 a's>'
//!    AND c.relkind IN ('r','p')
//! ```
//!
//! # The comparison is what truncates, not the query
//!
//! Measured on 19beta1 inside `BEGIN … ROLLBACK`:
//!
//! ```text
//! CREATE TABLE <64 a's>                   NOTICE: identifier "<64>" will be truncated to "<63>"
//! pg_class.relname                        <63 a's>, length 63
//! …WHERE c.relname = '<64 a's>'           1 row      <- the row the test needs
//! information_schema.tables = '<64 a's>'  1 row
//! length('<64>'::name)                    63
//! length('<64>'::text)                    64
//! length('<64>'::information_schema.sql_identifier)   63
//! ```
//!
//! `relname` is `name`, so the 64-character literal on the other side of the `=` is coerced to
//! `name` and loses its last character **before** the comparison. This node stored the table under
//! 63 characters already — the truncation at DDL was never the gap — and then compared 64 against
//! 63 as `text` and found nothing. One wrong column type, and a schema lookup that answers "no
//! such table" about a table it just made.
//!
//! `information_schema`'s own columns are `sql_identifier`, a **domain over `name`**, so they
//! truncate for the same reason one layer up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Sixty-four `a`s: one more than an identifier can hold.
fn too_long() -> String {
    "a".repeat(64)
}

/// The name it is stored under.
fn truncated() -> String {
    "a".repeat(63)
}

/// The adapter's own `table_exists?` query, asked with all sixty-four characters.
#[test]
fn a_table_created_with_an_over_long_name_is_found_by_that_name() {
    let mut node = parity::Node::new(&[]);
    node.run(&format!("CREATE TABLE {} (id int)", too_long()))
        .unwrap();
    assert_eq!(
        node.rows(&format!(
            "SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = '{}' \
             AND c.relkind IN ('r','p')",
            too_long()
        )),
        [[truncated()]],
        "the comparison truncates the literal, so the lookup finds the table"
    );
    // And `rename_table`'s half of the pair, measured on the oracle the same way: the rename
    // truncates with the same NOTICE and the 64-character name finds the row.
    node.run("CREATE TABLE more_testings (id int)").unwrap();
    node.run(&format!(
        "ALTER TABLE more_testings RENAME TO {}",
        "b".repeat(64)
    ))
    .unwrap();
    assert_eq!(
        node.rows(&format!(
            "SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n \
             ON n.oid = c.relnamespace WHERE n.nspname = ANY (current_schemas(false)) \
             AND c.relname = '{}' AND c.relkind IN ('r','p')",
            "b".repeat(64)
        )),
        [["b".repeat(63)]],
        "a renamed table is stored truncated and found by the name that was asked for"
    );
}

/// `information_schema.tables` truncates too: its columns are a domain over `name`.
#[test]
fn information_schema_finds_it_by_the_over_long_name_as_well() {
    let mut node = parity::Node::new(&[]);
    node.run(&format!("CREATE TABLE {} (id int)", too_long()))
        .unwrap();
    for asked in [too_long(), truncated()] {
        assert_eq!(
            node.rows(&format!(
                "SELECT table_name FROM information_schema.tables WHERE table_name = '{asked}'"
            )),
            [[truncated()]],
            "asked with {} characters",
            asked.len()
        );
    }
    // A column's name is the same kind of column, one view over.
    assert_eq!(
        node.rows(&format!(
            "SELECT column_name FROM information_schema.columns WHERE table_name = '{}'",
            too_long()
        )),
        [["id".to_owned()]]
    );
}

/// **Every identifier column of the catalog reports `name`**, which is the census this rests on.
///
/// Measured from the oracle's own catalog rather than recalled — `SELECT typname FROM pg_type
/// WHERE atttypid = 'name'::regtype` over each view — because a `*_name`-shaped sweep gets two of
/// these wrong: `pg_prepared_statements.name` is `text` on a real server, the one column called
/// `name` that is not one, and `pg_locks` has none at all.
#[test]
fn the_catalogs_identifier_columns_declare_name() {
    let mut node = parity::Node::new(&["CREATE TABLE nc (id int8 PRIMARY KEY)"]);
    for (sql, expected) in [
        ("SELECT relname FROM pg_class LIMIT 1", "name"),
        ("SELECT nspname FROM pg_namespace LIMIT 1", "name"),
        ("SELECT attname FROM pg_attribute LIMIT 1", "name"),
        ("SELECT typname FROM pg_type LIMIT 1", "name"),
        ("SELECT conname FROM pg_constraint LIMIT 1", "name"),
        ("SELECT amname FROM pg_am LIMIT 1", "name"),
        ("SELECT datname FROM pg_database LIMIT 1", "name"),
        ("SELECT rolname FROM pg_roles LIMIT 1", "name"),
        ("SELECT enumlabel FROM pg_enum LIMIT 1", "name"),
        (
            "SELECT table_name FROM information_schema.tables LIMIT 1",
            "name",
        ),
        (
            "SELECT column_name FROM information_schema.columns LIMIT 1",
            "name",
        ),
        (
            "SELECT constraint_name FROM information_schema.table_constraints LIMIT 1",
            "name",
        ),
        // The two a mechanical sweep would get wrong, in the other direction.
        ("SELECT name FROM pg_prepared_statements LIMIT 1", "text"),
        ("SELECT query FROM pg_stat_activity LIMIT 1", "text"),
    ] {
        match node.answer(sql) {
            parity::Answer::Rows { types, .. } => {
                assert_eq!(types.first().map(String::as_str), Some(expected), "{sql}");
            }
            other => panic!("{sql}: {other}"),
        }
    }
}
