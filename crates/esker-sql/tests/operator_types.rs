//! Comparing two columns of different types, against PostgreSQL 19beta1.
//!
//! Run 46's `bigint = text` and `integer = text` rows are that message being **right** — the
//! statements behind them came through `exec_params`, and a bind takes its type from its context
//! (`0badfe7`). What this file pins is the other direction, found while confirming it: a
//! comparison PostgreSQL refuses must not answer here with no rows.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_operator_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_operator_types.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}
