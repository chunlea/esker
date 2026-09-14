//! **A constraint's name is its identifier, whatever schema its table is in** — held to
//! PostgreSQL 19's answers.
//!
//! A stored relation name is `schema ++ NUL ++ name` outside `public`
//! ([ADR 0071](../../../docs/adr/0071-a-relation-name-is-keyed-by-its-schema.md)); a *derived*
//! constraint name is built from it and re-qualified (`plan::make_object_name`), while a given one
//! is stored as written. So every place that prints a constraint's name, or finds a constraint by
//! it, meets two stored forms — and a place that met only one was invisible in `public`, where the
//! two are one string: `pg_constraint.conname` read `s2nst_pkey`, `VALIDATE CONSTRAINT c_p_fkey` was
//! `42704` for a key the catalog listed, and that `42704` put the NUL inside the relation's quotes.
//! Found by `corpus/pg19_regnamespace.txt`, whose census `UPDATE` selects a foreign key by `conname`
//! in a schema.
//!
//! `corpus/pg19_constraint_name_in_a_schema.txt` is the capture, and the replay is the test of
//! record.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_constraint_name_in_a_schema_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_constraint_name_in_a_schema.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
