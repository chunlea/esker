//! **A sequence outside `public` prints its name with a dot, not with the stored separator.**
//!
//! `DumpSchemasTest` showed the raw separator byte reaching a client, inside the schema dump's
//! `nextval(...)` default. A relation outside `public` is stored `schema NUL name`
//! (`esker_sql::catalog::qualify`) and the `pg_attrdef` text interpolated that stored form
//! directly. Measured on 19beta1, a real server prints a dot:
//!
//! ```text
//! CREATE TABLE g1_ns.t (id bigserial primary key, a int);
//! pg_get_expr(adbin, adrelid)                ->  nextval('g1_ns.t_id_seq'::regclass)
//! information_schema.columns.column_default  ->  the same
//! ```
//!
//! **It is invisible in `public`**, where the stored name is bare and the two forms are the same
//! string — which is why the test lives in a schema and keeps the public case beside it as the
//! control.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA g1_ns",
    "CREATE TABLE g1_ns.t (id bigserial primary key, a int4)",
    "CREATE TABLE pub_t (id bigserial primary key, a int4)",
];

/// The qualified one prints a dot, and carries no separator byte at all.
#[test]
fn a_sequence_default_outside_public_prints_a_dot() {
    let mut node = parity::Node::new(FIXTURE);
    let rows = node.rows(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
         WHERE d.adrelid = 'g1_ns.t'::regclass",
    );
    assert_eq!(rows, [["nextval('g1_ns.t_id_seq'::regclass)".to_owned()]]);
    assert!(
        !rows[0][0].contains(esker_sql::catalog::SCHEMA_SEPARATOR),
        "the stored separator must not reach a client: {:?}",
        rows[0][0]
    );
}

/// **The public control**, which is why the bug was invisible: the stored name is already bare.
#[test]
fn a_public_sequence_default_is_unchanged() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
             WHERE d.adrelid = 'pub_t'::regclass"
        ),
        [["nextval('pub_t_id_seq'::regclass)".to_owned()]]
    );
}
