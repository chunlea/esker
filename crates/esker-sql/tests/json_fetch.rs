//! `->` and `->>` over `json` and `jsonb`, replayed against what PostgreSQL 19beta1 answered.
//!
//! Run 92's `the operator ->> is not supported` is 2 tests in `json_test.rb` and one statement —
//! `payload->'a', payload->>'b'` — so the pair is one unit. There was no `->` to put `->>` beside
//! either: it was `0A000 -> over text` for a jsonb literal as well as a column, because `->` means
//! an hstore's fetch *and* a document's and a `jsonb` is a canonical `Datum::Text` here, which a
//! value cannot be told apart from a string. Told apart the way `||` is — by the cast where the
//! lowerer can see one, and by `Expr::Ordinal`'s declared type where the operand is a column.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 'j10b', '[10,20]'::jsonb->>-1;",
        // **A tokenizer gap, not a `->>` one.** `sqlparser` 0.62.0 reads `->>-` as one operator,
        // so the unspaced form is `0A000 the operator ->>- is not supported` where the spaced one
        // beside it in the corpus agrees exactly. PostgreSQL parses both identically — measured,
        // both are `20` — so this is contract C1's shortfall rather than a missing feature, and it
        // is its own row so the ratchet says when `sqlparser` stops needing it.
        "sqlparser 0.62.0 tokenizes `->>-` as one operator",
        "pg19_json_fetch.txt:51",
    )],
};

#[test]
fn every_json_fetch_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_json_fetch.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}
