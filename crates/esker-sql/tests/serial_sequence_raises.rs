//! **`pg_get_serial_sequence` raises for a relation or a column that is not there**, and answers
//! NULL only for a column that owns no sequence.
//!
//! Two `postgresql_adapter_test` failures, one mechanism:
//!
//! ```text
//! test_serial_sequence                 pg_get_serial_sequence('zomg','id') -> NULL on the node
//! test_default_sequence_name_bad_table expects "zomg_id_seq"
//! ```
//!
//! The second explains the first. `ActiveRecord#default_sequence_name` **rescues** the exception
//! and falls back to `<table>_<pk>_seq`, so a node that answers NULL where a real server raises
//! never produces the fallback — and the caller cannot tell "there is no such table" from "that
//! column owns no sequence", which is the answer NULL is reserved for.
//!
//! # Measured on 19beta1
//!
//! ```text
//! pg_get_serial_sequence('g1z','id')        public.g1z_id_seq   -- always qualified
//! pg_get_serial_sequence('g1z','other')     NULL                -- owns no sequence
//! pg_get_serial_sequence('zomg','id')       ERROR  relation "zomg" does not exist
//! pg_get_serial_sequence('g1z','nosuch')    ERROR  column "nosuch" of relation "g1z" does not exist
//! pg_get_serial_sequence('public.g1z','id') public.g1z_id_seq
//! pg_get_serial_sequence('"g1z"','id')      public.g1z_id_seq
//! ```
//!
//! Two errors and not one, with different codes and different sentences — which is why the fix is
//! two guards rather than a single "not found".

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &["CREATE TABLE g1z (id bigserial primary key, other integer)"];

/// The answers that were already right, kept so a fix cannot trade one for the other.
#[test]
fn a_serial_column_answers_its_qualified_sequence_and_a_plain_one_answers_null() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT pg_get_serial_sequence('g1z','id')"),
        [["public.g1z_id_seq"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_serial_sequence('g1z','other')"),
        [["\\N"]]
    );
    // The name may be written qualified or quoted, and answers the same either way.
    assert_eq!(
        node.rows("SELECT pg_get_serial_sequence('public.g1z','id')"),
        [["public.g1z_id_seq"]]
    );
}

/// **A relation that is not there is `42P01`**, which is what `default_sequence_name` rescues.
#[test]
fn a_relation_that_is_not_there_raises() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("SELECT pg_get_serial_sequence('zomg','id')")
            .to_string(),
        "!42P01 relation \"zomg\" does not exist"
    );
}

/// **And a column that is not there is `42703`**, a different code and a different sentence — the
/// distinction a single "not found" would have lost.
#[test]
fn a_column_that_is_not_there_raises_differently() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("SELECT pg_get_serial_sequence('g1z','nosuch')")
            .to_string(),
        "!42703 column \"nosuch\" of relation \"g1z\" does not exist"
    );
}

/// A sequence in a schema answers with that schema, which is the half the qualified form is for.
#[test]
fn a_sequence_in_a_schema_answers_with_its_own() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA g1z_s",
        "CREATE TABLE g1z_s.t (id bigserial primary key)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_serial_sequence('g1z_s.t','id')"),
        [["g1z_s.t_id_seq"]]
    );
}
