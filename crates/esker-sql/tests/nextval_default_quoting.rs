//! **A `nextval` default names its sequence the way a client can re-parse it.**
//!
//! `primary_keys_test#test_serial_with_quoted_sequence_name` asserts the default of a `serial`
//! column called `monkeyID` to the character:
//!
//! ```text
//! nextval('"mixed_case_monkeys_monkeyID_seq"'::regclass)
//! ```
//!
//! The sequence's name is derived from a mixed-case column, so it is mixed-case itself, and inside
//! that string literal it has to be delimited or it reads back as a different name. Its sibling
//! test asserts the other half — `nextval('topics_id_seq'::regclass)`, bare — so a blanket pair of
//! quotes fails one of the two.
//!
//! # Measured on 19beta1
//!
//! ```text
//! "monkeyID" serial   nextval('"g1nv_mixed_monkeyID_seq"'::regclass)
//! id serial           nextval('g1nv_plain_id_seq'::regclass)
//! id serial, in g1nv_s  nextval('g1nv_s.t_id_seq'::regclass)
//! ```
//!
//! So it is `quote_identifier` **per part**: the schema and the name are each delimited only if
//! they would not read back as themselves, and the dot between them is never inside the quotes.
//! `catalog::display_name` joins with a dot and stops there, which is right where a name is prose
//! and wrong where it is SQL a client re-parses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// All three shapes in one statement, which is what makes a blanket rule fail visibly.
#[test]
fn a_sequence_name_is_quoted_inside_the_literal_only_when_it_needs_to_be() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1nv_mixed (\"monkeyID\" serial primary key, flea integer)",
        "CREATE TABLE g1nv_plain (id serial primary key)",
        "CREATE SCHEMA g1nv_s",
        "CREATE TABLE g1nv_s.t (id serial primary key)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT c.relname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
             JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             JOIN pg_class c ON c.oid = d.adrelid \
             WHERE c.relname IN ('g1nv_mixed','g1nv_plain','t') ORDER BY 1"
        ),
        [
            [
                "g1nv_mixed".to_owned(),
                "nextval('\"g1nv_mixed_monkeyID_seq\"'::regclass)".to_owned()
            ],
            [
                "g1nv_plain".to_owned(),
                "nextval('g1nv_plain_id_seq'::regclass)".to_owned()
            ],
            [
                "t".to_owned(),
                "nextval('g1nv_s.t_id_seq'::regclass)".to_owned()
            ],
        ]
    );
}

/// `information_schema.columns.column_default` reads the same string, so the two surfaces agree —
/// which is the half a fix applied in one renderer only would have missed.
#[test]
fn the_information_schema_column_agrees() {
    let mut node =
        parity::Node::new(&["CREATE TABLE g1nv_mixed (\"monkeyID\" serial primary key)"]);
    assert_eq!(
        node.rows(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'g1nv_mixed' AND column_name = 'monkeyID'"
        ),
        [["nextval('\"g1nv_mixed_monkeyID_seq\"'::regclass)"]]
    );
}
