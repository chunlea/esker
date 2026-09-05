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
        // **The standing integer-literal trade.** An unadorned `1` is an `int8` here and an
        // `integer` there, so the operator that does not exist is named `bigint || bigint` rather
        // than `integer || integer`. The sqlstate, the sentence, the `DETAIL` and the `HINT` are
        // identical, and what the statement is *for* — that `||` is not integer concatenation —
        // is answered the same way on both.
        (
            "SELECT 1 || 2",
            "an unadorned integer literal is int8 here, so the message names bigint",
            "pg19_concat.txt:22",
        ),
        // **`||` over arrays is a family of its own and is not built** — `42883 operator does not
        // exist: bigint[] || bigint[]` where a real server appends. A gap, and named as one: the
        // operator refuses rather than guessing, which is what `text_concat` checks for before it
        // will concatenate anything.
        (
            "SELECT 'r', ARRAY[1,2] || ARRAY[3], ARRAY[1,2] || 3, pg_typeof(ARRAY[1,2] || 3)",
            "|| over arrays is not built; it refuses rather than answering",
            "pg19_concat.txt:25",
        ),
        // **`pg_typeof` of the merge**: `jsonb` on both, and it is the one line here whose
        // *column type* still differs — a `regtype` there and `text` here, the standing catalog
        // trade. The value agrees.
        (
            "SELECT 'r', pg_typeof('{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb)",
            "pg_typeof answers a regtype on a real server and text here; the value is jsonb on both",
            "pg19_concat.txt:33",
        ),
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
