//! `VALUES` as a **relation**, against PostgreSQL 19beta1.
//!
//! A table made of constant rows, in the two places one can stand: `(VALUES …) AS t(a,b)` where a
//! table goes, and `VALUES …` on its own as a statement. It is what `ARRAY(VALUES (1),(2))` needs
//! and what `array_agg` over a literal list is written with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is constant.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`pg_typeof` answers a `regtype` on both now** (ADR 0093): it is resolved at plan
    // time from the argument's declared type, so what this list recorded has no difference
    // left in it.
    types: &[],
    answers: &[],
};

#[test]
fn every_values_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_values_relation.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
