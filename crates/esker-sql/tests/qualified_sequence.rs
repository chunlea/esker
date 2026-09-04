//! A schema-qualified sequence name — run 50's top regression, 4,873 tests in 93 files.
//!
//! `reset_pk_sequence!` runs on every fixture load, and once `pk_and_sequence_for` started coming
//! back with a namespace the adapter began passing `'"public"."accounts_id_seq"'` to `::regclass`
//! and to `setval`. Nothing was wrong with that name; what was missing here was that a **quoted**
//! qualified name resolves at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table, schema and sequences.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_qualified_sequence_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_qualified_sequence.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
