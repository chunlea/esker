//! `ARRAY[…]` over expressions, replayed against what PostgreSQL 19beta1 answered.
//!
//! A constructor over constants folds at lowering, where what was written settles the element
//! type. This is the other one — an element that is a column has no value until there is a row —
//! and it is the whole of `ActiveRecord`'s `can_perform_case_insensitive_comparison_for?`, which
//! joins on `ARRAY[casttarget]::oidvector = proargtypes`.

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
fn every_array_constructor_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_constructor.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}
