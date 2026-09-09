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
    answers: &[(
        "SELECT 'r', oid, typname, typlen, typtype, typcategory, typdelim, typinput, typelem, \
         typarray FROM pg_type WHERE typname IN ('int2vector','oidvector') ORDER BY oid",
        "**`typarray` is 1006 and 1013 there and 0 here, and that is a decision.** Both vectors \
         are on `array_delimiter.rs::every_base_type_has_an_array_or_is_listed`'s named-gap list: \
         they are catalog types a client reads and never stores an array of, so `_int2vector` and \
         `_oidvector` would be two types nothing writes and nothing reads. Every other column of \
         both rows agrees — including `typcategory` **A** and the `typelem` that says a vector is \
         made of its element, which is the fact this unit's input functions are built on.",
        "pg19_catalog_vectors.txt:44",
    )],
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
