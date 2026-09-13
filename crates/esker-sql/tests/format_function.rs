//! `format()` held to PostgreSQL 19 — the function `check_all_foreign_keys_valid!` builds every
//! statement it `EXECUTE`s with ([ADR 0113], `docs/plans/plpgsql-subset.md` §6).
//!
//! [ADR 0113]: ../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing to declare: every row answers as the capture does.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_format_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_format.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The census's own call**, over the constraint `referential_integrity_test.rb` creates.
#[test]
fn the_statement_check_all_foreign_keys_valid_builds() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT FORMAT('UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname \
             = ''%1$I'' AND connamespace::regnamespace = ''%2$I''::regnamespace; ALTER TABLE \
             %2$I.%3$I VALIDATE CONSTRAINT %1$I;', 'fk_parent_node', \
             'referential_integrity_test_schema', 'nodes')"
        ),
        [[
            "UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = \
             'fk_parent_node' AND connamespace::regnamespace = \
             'referential_integrity_test_schema'::regnamespace; ALTER TABLE \
             referential_integrity_test_schema.nodes VALIDATE CONSTRAINT fk_parent_node;"
        ]]
    );
}
