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

/// A table `CHECK`'s derived name — a question about derivation, not about schemas.
const TABLE_CHECK_NAME: &str = "a table `CHECK` is derived `<table>_check` here whatever it reads, \
                               where PostgreSQL names one that reads exactly one column for that \
                               column — `k_a_check`. A derivation gap older than this file and in \
                               `public` too; the column constraint's name, `c_id_check`, agrees";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', conname FROM pg_constraint WHERE conrelid = 's2cn.k'::regclass ORDER BY \
             conname",
            TABLE_CHECK_NAME,
            "pg19_constraint_name_in_a_schema.txt:40",
        ),
        (
            "ALTER TABLE s2cn.c ADD CONSTRAINT c_p_fkey CHECK (id > 1)",
            "a `CHECK` is checked for a colliding name against the table's other checks only, so \
             one named like a foreign key is added where PostgreSQL answers `42710` for the one \
             namespace a table's constraints share. Older than this file and in `public` too; a \
             collision between two checks, three lines on, agrees",
            "pg19_constraint_name_in_a_schema.txt:49",
        ),
        (
            "ALTER TABLE s2cn.c ADD CONSTRAINT c_u_key UNIQUE (q)",
            "a `UNIQUE` meets a name already taken as a constraint (`42710`) where PostgreSQL \
             meets it first as the index's relation (`42P07`) — the order of two checks, and in \
             `public` too. The sentence this node sends names the constraint and the relation \
             bare, which is this file's rule",
            "pg19_constraint_name_in_a_schema.txt:55",
        ),
        (
            "SELECT 'r', conname, contype FROM pg_constraint WHERE connamespace = \
             's2cn'::regnamespace ORDER BY conname",
            TABLE_CHECK_NAME,
            "pg19_constraint_name_in_a_schema.txt:73",
        ),
    ],
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
