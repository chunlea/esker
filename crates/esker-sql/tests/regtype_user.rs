//! `'<name>'::regtype` over a type a `CREATE TYPE` made, against PostgreSQL 19beta1.
//!
//! Run 70's `type "…" does not exist` row: `color` 3 tests and `mood` 2, all of them
//! `ActiveRecord`'s `lookup_cast_type` writing `SELECT '<sql_type>'::regtype::oid`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus makes its own types.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **A `regtype` is an oid that prints as a name and this node has no type that is both.**
        // `'mood'::regtype` answers the name, as text, which is what a projection wants and what
        // `::regtype::text` wants; a *comparison* wants the oid, and the position it sits in
        // cannot say so. `ActiveRecord` writes the half that can be answered —
        // `'<sql_type>'::regtype::oid` — and that agrees, oid for oid, with `pg_type`.
        //
        // The standing trade one level up: `pg_typeof` of a `regtype` says `text` here.
        (
            "SELECT 'r', pg_typeof('mood'::regtype), pg_typeof('mood'::regtype::oid)",
            "a regtype is text here; the ::oid half is a real oid and agrees",
        ),
        // The comparison half of the same fact. It is also `pg19_do_create_enum.txt`'s swallowed
        // statement, which is on `SWALLOWING_DEBT` as g1's.
        (
            "SELECT 'r', enumlabel FROM pg_enum WHERE enumtypid = 'mood'::regtype ORDER BY \
             enumsortorder",
            "a regtype in a comparison wants the oid and the position cannot say so",
        ),
    ],
};

#[test]
fn every_user_regtype_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regtype_user.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}
