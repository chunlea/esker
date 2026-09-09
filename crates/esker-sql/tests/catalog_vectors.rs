//! The catalog's two vector shapes, against PostgreSQL 19beta1.
//!
//! `pg_index.indkey` is an `int2vector` and `pg_constraint.conkey` is a `smallint[]`: they print
//! differently, they are subscripted from different ends, and both were `text` here. The follow-up
//! to `3b1cd1a`, which made the second one visible — a comparison against `attnum` went from
//! answering to `42883` because an `int2` was meeting a `text`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The `int2vector` half.** Every one of these has the right rows and a `text` where a real
    // server says `int2vector` or `smallint`: `indkey` is text here, so its elements are too. The
    // `conkey` half of the same file is not in this list any more, which is the unit.
    types: &[
        "SELECT 'r', indkey[0], indkey[1], array_length(indkey, 1) FROM pg_index WHERE indexrelid = 'vt_ab'::regclass",
    ],
    answers: &[],
};

#[test]
fn every_catalog_vector_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_vectors.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 13,
        "only {checked} statements ran; the corpus did not load"
    );
}
