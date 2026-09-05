//! **`::regclass::text` qualifies a name only when its schema is off the `search_path`.**
//!
//! `DumpSchemasTest` turns on exactly this: from one database, `dump_schemas(:all)` wants
//! `add_foreign_key "test_schema.test_table2", "test_schema.test_table"` and
//! `dump_schemas("test_schema")` wants `add_foreign_key "test_table2", "test_table"`. The
//! difference is the session's `search_path`, and `ActiveRecord` reads the target through
//! `t2.oid::regclass::text`.
//!
//! Measured on 19beta1:
//!
//! ```text
//! default path            'g1_rc.t'::regclass::text  ->  g1_rc.t     'g1_rc_pub'  ->  g1_rc_pub
//! SET search_path = g1_rc, public                    ->  t                        ->  g1_rc_pub
//! ```
//!
//! `RelationRow::name` is the **bare** name with the schema beside it, so printing `name` alone
//! dropped the qualifier for every relation outside `public` — one of the two answers above, all
//! the time.
//!
//! The path is a property of the *session*, not of the catalog snapshot, so it travels with the
//! cursor rather than being read where the name is printed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA g1_rc",
    "CREATE TABLE g1_rc.t (id int8)",
    "CREATE TABLE g1_rc_pub (id int8)",
];

/// Under the default path, a schema-qualified relation prints qualified and a public one bare.
#[test]
fn the_default_path_qualifies_what_it_cannot_see() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT 'g1_rc.t'::regclass::text, 'g1_rc_pub'::regclass::text"),
        [["g1_rc.t".to_owned(), "g1_rc_pub".to_owned()]]
    );
}

/// **And putting the schema on the path makes the same oid print bare** — which is the half that
/// says this is the session's answer and not the catalog's.
#[test]
fn a_schema_on_the_path_prints_bare() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("SET search_path = g1_rc, public").unwrap();
    assert_eq!(
        node.rows("SELECT 'g1_rc.t'::regclass::text, 'g1_rc_pub'::regclass::text"),
        [["t".to_owned(), "g1_rc_pub".to_owned()]]
    );
}

/// The query `ActiveRecord` reads a foreign key's target with, which is where this reaches the
/// schema dump.
#[test]
fn a_foreign_keys_target_is_named_for_the_session() {
    const TARGET: &str = "SELECT t2.oid::regclass::text FROM pg_constraint c \
                          JOIN pg_class t2 ON c.confrelid = t2.oid WHERE c.contype = 'f'";
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA test_schema",
        "CREATE TABLE test_schema.test_table (id int8 PRIMARY KEY)",
        "CREATE TABLE test_schema.test_table2 (id int8 PRIMARY KEY, test_table_id int8)",
        "ALTER TABLE test_schema.test_table2 ADD CONSTRAINT fk FOREIGN KEY (test_table_id) \
         REFERENCES test_schema.test_table (id)",
    ]);
    assert_eq!(
        node.rows(TARGET),
        [["test_schema.test_table".to_owned()]],
        "off the path, the dump needs the qualifier"
    );
    node.run("SET search_path = test_schema, public").unwrap();
    assert_eq!(
        node.rows(TARGET),
        [["test_table".to_owned()]],
        "on the path, the dump needs it bare"
    );
}
