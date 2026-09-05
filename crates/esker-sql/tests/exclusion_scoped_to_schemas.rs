//! **A constraint belongs to its table's schema, and `connamespace` is what says so.**
//!
//! `test_exclusion_constraints_scoped_to_schemas` creates `test_schema.invoices` beside
//! `public.invoices` and asserts the count for `invoices` does not change. It counted **2**.
//!
//! `ActiveRecord`'s `exclusion_constraints` joins on the *constraint's* namespace, not the table's:
//!
//! ```sql
//! FROM pg_constraint c
//! JOIN pg_class t ON c.conrelid = t.oid
//! JOIN pg_namespace n ON n.oid = c.connamespace
//! WHERE c.contype = 'x' AND t.relname = 'invoices' AND n.nspname = 'public'
//! ```
//!
//! `connamespace` was the constant `public` for every constraint, so the one on
//! `test_schema.invoices` answered the public lookup too. `captures/pg19_exclusion_constraint.txt`
//! had said it before the code did: *"the lookup has to be schema-aware rather than by relname"*.
//!
//! **The same shape as the `INCLUDE` bug**: a catalog column answering by the wrong key, invisible
//! until a second schema exists.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE invoices (id int8, start_date date, end_date date)",
    "ALTER TABLE invoices ADD CONSTRAINT c_public EXCLUDE USING gist \
     (daterange(start_date, end_date) WITH &&)",
    "CREATE SCHEMA test_schema",
    "CREATE TABLE test_schema.invoices (id int8, start_date date, end_date date)",
    "ALTER TABLE test_schema.invoices ADD CONSTRAINT c_schema EXCLUDE USING gist \
     (daterange(start_date, end_date) WITH &&)",
];

/// The adapter's own query, run for each schema in turn.
fn count_for(node: &mut parity::Node, schema: &str) -> Vec<Vec<String>> {
    node.rows(&format!(
        "SELECT c.conname FROM pg_constraint c \
         JOIN pg_class t ON c.conrelid = t.oid \
         JOIN pg_namespace n ON n.oid = c.connamespace \
         WHERE c.contype = 'x' AND t.relname = 'invoices' AND n.nspname = '{schema}' \
         ORDER BY c.conname"
    ))
}

/// **One each**, which is the whole assertion: a same-named table in another schema must not
/// appear in the first one's answer.
#[test]
fn a_constraint_is_scoped_to_its_tables_schema() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(count_for(&mut node, "public"), [["c_public".to_owned()]]);
    assert_eq!(
        count_for(&mut node, "test_schema"),
        [["c_schema".to_owned()]]
    );
}

/// And `pg_constraint` itself reports each constraint under its own table's namespace, which is
/// the column the join above rests on.
#[test]
fn connamespace_names_the_tables_schema() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT c.conname, n.nspname FROM pg_constraint c \
             JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE c.contype = 'x' ORDER BY c.conname"
        ),
        [
            ["c_public".to_owned(), "public".to_owned()],
            ["c_schema".to_owned(), "test_schema".to_owned()],
        ]
    );
}
