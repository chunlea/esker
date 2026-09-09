//! A set-returning function in the **target list**, against PostgreSQL 19beta1.
//!
//! `generate_series` and `generate_subscripts` already stand where a table does; this is the other
//! half, `SELECT generate_series(1,3)`, and it is what `tests/catalog_vectors.rs` named as a gap.
//! `unnest` arrives with it, because a target list is where the suite writes it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
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
fn every_set_returning_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_srf_target_list.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
