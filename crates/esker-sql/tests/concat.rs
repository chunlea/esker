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
        // **What the literal ladder did not reach.** An unadorned `1` is declared `integer` here
        // since the `int4` rung — `SELECT 1` describes as `integer` and `SELECT i4 || 2` names
        // `integer || bigint` — but this refusal is raised by the *evaluator*, from the datums it
        // was handed, and the rung narrowed declared types rather than values: a literal's datum
        // is still an `i64`. So the operator that does not exist is named `bigint || bigint` where
        // a real server names `integer || integer`. The sqlstate, the sentence, the `DETAIL` and
        // the `HINT` are identical, and what the statement is *for* — that `||` is not integer
        // concatenation — is answered the same way on both. Closing it means narrowing the datum,
        // which is a change to what a bare integer *is* rather than to what it is called.
        (
            "SELECT 1 || 2",
            "the refusal is raised from the datums, and a literal's datum is still an i64, so the \
             message names bigint where the declared type is already integer",
            "pg19_concat.txt:22",
        ),
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
