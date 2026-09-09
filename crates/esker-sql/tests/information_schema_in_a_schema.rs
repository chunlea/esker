//! **`information_schema` reports the schema a table is in, and a bare name.**
//!
//! `referential_integrity_test#test_all_foreign_keys_valid_having_foreign_keys_in_multiple_schemas`
//! creates a table in its own schema and counts its foreign keys:
//!
//! ```sql
//! SELECT count(*) FROM information_schema.table_constraints
//!  WHERE constraint_schema = 'referential_integrity_test_schema'
//!    AND constraint_type = 'FOREIGN KEY'
//! ```
//!
//! It found **none**, and the reason was not the foreign key: three views wrote the constant
//! `public` into their schema columns and the *stored* name into their name columns. A stored name
//! is `schema ++ NUL ++ name`, so a client reading `information_schema.columns` for a table in a
//! schema got `public` | `rits\0nodes` — a NUL on the wire, and a schema that was never asked for.
//!
//! # Measured on 19beta1
//!
//! ```text
//! table_constraints    rits | fk_parent_node           | FOREIGN KEY | rits | nodes
//!                      rits | nodes_id_not_null        | CHECK       | rits | nodes
//!                      rits | nodes_parent_id_not_null | CHECK       | rits | nodes
//!                      rits | nodes_pkey               | PRIMARY KEY | rits | nodes
//! columns              rits | nodes | parent_id
//! key_column_usage     rits | nodes_pkey | rits | nodes | id
//! ```
//!
//! Four rows and not one: the count the Rails test makes is over the `FOREIGN KEY` of those, and
//! the other three are what says the view is right rather than merely non-empty.
//!
//! **A derived constraint name is stored qualified** — `plan::make_object_name` spends its byte
//! budget on the identifier and re-qualifies — so the name column needed the same split as the
//! table's, which is why `nodes_pkey` and not `rits\0nodes_pkey`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA rits",
    "CREATE TABLE rits.nodes (id bigserial, parent_id int NOT NULL, PRIMARY KEY(id), \
     CONSTRAINT fk_parent_node FOREIGN KEY(parent_id) REFERENCES rits.nodes(id))",
];

/// **The Rails count, and the whole listing behind it.**
#[test]
fn table_constraints_reports_the_tables_own_schema() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM information_schema.table_constraints \
             WHERE constraint_schema = 'rits' AND constraint_type = 'FOREIGN KEY'"
        ),
        [["1"]]
    );
    assert_eq!(
        node.rows(
            "SELECT constraint_schema, constraint_name, constraint_type, table_schema, table_name \
             FROM information_schema.table_constraints WHERE table_schema = 'rits' ORDER BY 2"
        ),
        [
            ["rits", "fk_parent_node", "FOREIGN KEY", "rits", "nodes"],
            ["rits", "nodes_id_not_null", "CHECK", "rits", "nodes"],
            ["rits", "nodes_parent_id_not_null", "CHECK", "rits", "nodes"],
            ["rits", "nodes_pkey", "PRIMARY KEY", "rits", "nodes"],
        ]
    );
}

/// `information_schema.columns` had the same two mistakes, and it is read by every schema dump.
#[test]
fn columns_reports_the_schema_and_a_bare_table_name() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT table_schema, table_name, column_name FROM information_schema.columns \
             WHERE column_name = 'parent_id'"
        ),
        [["rits", "nodes", "parent_id"]]
    );
}

/// And `key_column_usage`, where the constraint's own name needed the split too.
#[test]
fn key_column_usage_reports_both() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT constraint_schema, constraint_name, table_schema, table_name, column_name \
             FROM information_schema.key_column_usage WHERE table_schema = 'rits'"
        ),
        [["rits", "nodes_pkey", "rits", "nodes", "id"]]
    );
}

/// A table in `public` is unchanged, which is every statement that came before schemas.
#[test]
fn a_table_in_public_is_unchanged() {
    let mut node =
        parity::Node::new(&["CREATE TABLE g1is_p (id bigserial primary key, a integer)"]);
    assert_eq!(
        node.rows(
            "SELECT table_schema, table_name FROM information_schema.columns \
             WHERE table_name = 'g1is_p' AND column_name = 'a'"
        ),
        [["public", "g1is_p"]]
    );
    assert_eq!(
        node.rows(
            "SELECT constraint_schema, constraint_name FROM information_schema.table_constraints \
             WHERE table_name = 'g1is_p' AND constraint_type = 'PRIMARY KEY'"
        ),
        [["public", "g1is_p_pkey"]]
    );
}
