//! **`||` over text**, against PostgreSQL 19beta1.
//!
//! The residual `integer_plus_text.rs` had declared three times over as "`||` over text is not
//! built". It was lowered — always to the hstore concatenation — and the evaluator told an hstore
//! from an ltree from a tsvector by their operands and refused everything else. The type dispatch
//! in `exec::query` had already been answering `text` for the remaining case, so what was missing
//! was only the value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus makes its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **`SELECT 1 || 2` stood here and is gone**, closed by the `||` operator table (wire v3
        // families F3a and F3b). It said the refusal named `bigint || bigint` where a real server
        // names `integer || integer`, because it was raised by the *evaluator* from the datums and
        // a literal's datum is still an `i64` — the `int4` rung narrowed declared types, not
        // values. **The table moved the refusal to resolution**, where the declared type has been
        // `integer` all along, and the whole sentence now matches. Closing it needed no change to
        // what a bare integer *is*: it needed the decision made where the type was still known.
        // **`||` over arrays is built now**, and the entry that stood here for it is gone: three
        // shapes and the two NULL rules, measured in `tests/captures/pg19_array_families.txt`.
        // What was named a family of its own turned out to be the whole operator — `text[] ||
        // text[]` was refused too — and the refusal above is still the refusal above, because
        // `1 || 2` is not an array on either server.
    ],
};

#[test]
fn every_concat_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_concat.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}
