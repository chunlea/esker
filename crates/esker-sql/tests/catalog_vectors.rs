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
    // **Empty.** The one entry read `indkey[0]` and `indkey[1]` and said they were `text` here
    // where a real server says `smallint`. They are `smallint` now, and the fix was not a type
    // but a **second reader**: `output_columns` types a projection *before* resolution and asked
    // the `element` the node carries, while `pg_typeof` folds *after* it and asked
    // `attnum_vector_element`. The two agreed wherever resolution had already run, so only a
    // `Describe` could see the difference.
    types: &[],
    // **The `typarray` row is gone from this list** — it said *"1006 and 1013 there and 0 here,
    // and that is a decision"*, and the decision was reversed by ADR 0107 step 2 once the reason
    // was measured rather than reasoned. The reason had been *"two types nothing writes and
    // nothing reads"*; what the measurement said is that a `typarray` of 0 makes `ARRAY[vec]` a
    // `text[]`, which a client decodes by oid and gets wrong. `_int2vector` (1006) and
    // `_oidvector` (1013) are types now, and the whole row agrees.
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
