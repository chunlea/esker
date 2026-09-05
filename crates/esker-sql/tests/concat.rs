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
        // **`jsonb || jsonb` is refused, not answered.** PostgreSQL merges the two documents;
        // this node has no `Datum` for a `jsonb` — it is a `Datum::Text`, where `hstore`, `ltree`,
        // `citext` and `tsvector` each have a variant — so by the time `||` has two values in
        // hand a document and a string are the same thing, and concatenating them would produce a
        // string that is not a document. That is a **wrong answer** where refusing is a gap, and
        // [ADR 0031](../../../docs/adr/0031-the-rails-suite-is-the-measure.md) ranks a wrong
        // answer worse, so the operator gives back the `0A000` it gave before `||` over text
        // existed. `tests/concat_refuses_json.rs` pins the refusal itself.
        //
        // `json` is refused beside it and is **not the same type**: it keeps key order,
        // whitespace and duplicate keys, so `'{"a":1}'::json || '{"b":2}'::json` really is the
        // two documents' text run together on a real server. Refusing it too is the conservative
        // half — telling the two apart is part of the unit that gives `jsonb` a representation,
        // `docs/plans/jsonb-representation.md`, and guessing which of them this node's
        // `Datum::Text` is standing for is exactly what got the operator into this.
        (
            "SELECT 'r', '{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb",
            "jsonb has no representation of its own here, so || refuses rather than concatenating",
        ),
        (
            "SELECT 'r', '{\"a\":1}'::json || '{\"b\":2}'::json",
            "json is refused beside jsonb until the two have representations that tell them apart",
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
