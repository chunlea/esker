//! `argument of AND/OR must be type boolean` — run 57's second row, 7 tests in `insert_all_test.rb`.
//!
//! One statement shape, and it is `upsert_all`'s: the adapter guards the timestamp touch with a
//! `CASE` over the columns that would change, so `updated_at` moves only when something else did.
//! What this node was answering instead was the *value* it found — `not Text("one")` — which is a
//! message that leaks a row into an error and differs per row of one query. A real server names
//! the **type**.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `regtype` there and `text` here, which is what `pg_typeof` is everywhere in this crate:
    // the printed name is identical and the declared type is not.
    types: &["SELECT 'r', pg_typeof('a' IS NOT DISTINCT FROM 'a')"],
    answers: &[(
        "SELECT 'r', 1 FROM bl WHERE name AND true",
        "**`character varying` there and `text` here**, and the difference is where the type is \
         read from rather than what it is. The column is declared `character varying(255)` on both \
         sides; the message is built at *evaluation*, from the value the row held, and a `varchar` \
         value is a `text` datum here — `text`, `varchar` and `bpchar` are one representation told \
         apart by OID (ADR 0033), which is PostgreSQL's own model too. What would close it is the \
         **declared** type reaching the refusal, which means catching a non-boolean condition where \
         the expression is resolved rather than where a row is evaluated — the path `CASE/WHEN` \
         already takes for every shape whose type is known then, and this is the one that is not. \
         The class, the construct's name and the shape of the sentence all agree.",
        "pg19_boolean_argument.txt:49",
    )],
};

#[test]
fn every_boolean_argument_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_boolean_argument.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 20, "the corpus shrank: {checked} statements");
}
