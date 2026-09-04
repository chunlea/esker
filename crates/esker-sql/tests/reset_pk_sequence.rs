//! Resetting a primary-key sequence after fixtures load with explicit ids, against PostgreSQL
//! 19beta1.
//!
//! Run 46's largest row: **199 tests across 29 files**, every one of them
//! `duplicate key value violates unique constraint "…_pkey"`, and the capture's claim is that they
//! are one gap rather than 199. Fixtures are inserted with explicit ids, which does not advance the
//! sequence, so the next `create` asks for `nextval`, gets `1`, and collides. Rails removes the
//! collision by calling `reset_pk_sequence!`, which is `MAX(pk)` and a `setval`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_sequence_reset_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_reset_pk_sequence.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 33,
        "only {checked} statements ran; the corpus did not load"
    );
}
