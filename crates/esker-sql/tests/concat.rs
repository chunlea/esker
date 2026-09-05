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
        ),
        // **`||` over arrays is a family of its own and is not built** — `42883 operator does not
        // exist: bigint[] || bigint[]` where a real server appends. A gap, and named as one: the
        // operator refuses rather than guessing, which is what `text_concat` checks for before it
        // will concatenate anything.
        (
            "SELECT 'r', ARRAY[1,2] || ARRAY[3], ARRAY[1,2] || 3, pg_typeof(ARRAY[1,2] || 3)",
            "|| over arrays is not built; it refuses rather than answering",
        ),
        // **`jsonb || jsonb` concatenates as text here, and that is a wrong answer, not a gap.**
        // It is worth writing down precisely because of that. `jsonb` has **no `Datum` of its
        // own** — it is a `Datum::Text`, where `hstore`, `ltree`, `citext` and `tsvector` each
        // have a variant — so at the value layer this operator cannot tell a jsonb from a string
        // and concatenates the two documents instead of merging them. Before `||` over text
        // existed the same call was refused, so this replaces a gap with a wrong answer in one
        // narrow case, which
        // [ADR 0031](../../../docs/adr/0031-the-rails-suite-is-the-measure.md) ranks the other way
        // round.
        //
        // The fix is not in this operator: it is
        // [ADR 0042](../../../docs/adr/0042-a-type-shares-a-representation-only-if-it-shares-a-comparison.md)'s
        // rule applied to `jsonb`, which needs a representation that is not `text`. Owed, and
        // named in the handover — `jsonb` merge semantics are a unit of their own.
        (
            "SELECT 'r', '{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb",
            "jsonb is a Datum::Text here, so || concatenates the documents instead of merging them",
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
