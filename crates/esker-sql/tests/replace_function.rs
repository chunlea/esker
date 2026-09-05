//! `replace(text, from, to)`, against PostgreSQL 19beta1.
//!
//! Run 89's `the function REPLACE is not supported`: 4 tests, every use inside an `ORDER BY`, and
//! between them a column, a constant and a bind appear in each of the three argument positions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus makes its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_replace_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_replace.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}
