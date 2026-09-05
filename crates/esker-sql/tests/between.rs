//! `BETWEEN` — run 57's top row, 15 tests over 5 files.
//!
//! **The whole feature is a rewrite**: `a BETWEEN x AND y` is `a >= x AND a <= y`, and every rule
//! a corpus could ask about falls out of that rather than needing one of its own — the inclusive
//! ends, the reversed bounds matching nothing, the three-valued NULL, and a type mismatch whose
//! `42883` names `>=` rather than `BETWEEN`. That last one is the rewrite showing through a real
//! server's own message, which is the argument that this is what PostgreSQL does too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `regtype` there and `text` here, which is what `'x'::regtype` is everywhere in this crate:
    // the printed name is identical and the declared type is not.
    types: &["SELECT 'r', pg_typeof(1 BETWEEN 1 AND 2)"],
    answers: &[
        (
            "SELECT 'r', id FROM bt WHERE id BETWEEN SYMMETRIC 3 AND 2 ORDER BY id",
            "**`SYMMETRIC` is a contract C1 gap, not a clause declined here.** `sqlparser` 0.62.0 \
             has no flag for it on its `Between` node and cannot read the keyword at all, so it \
             is refused by name before the parser sees the statement — which is why the message \
             says `BETWEEN SYMMETRIC` rather than `syntax error at or near \"SYMMETRIC\"`. The \
             rewrite it would need is written down and is two lines — `(a >= x AND a <= y) OR (a \
             >= y AND a <= x)` — so this is a parser item and not an executor one, the same family \
             as the `CREATE DATABASE` option list and the `EXCLUDE` constraint. Nothing \
             `ActiveRecord` sends uses it: all fifteen failing statements are the plain form.",
            "pg19_between.txt:35",
        ),
        (
            "SELECT 'r', id FROM bt WHERE id NOT BETWEEN SYMMETRIC 3 AND 2 ORDER BY id",
            "The same gap in its negated spelling.",
            "pg19_between.txt:36",
        ),
        (
            "SELECT 'r', 5 BETWEEN 1 AND 10 AS plain, 5 BETWEEN 10 AND 1 AS reversed, 5 BETWEEN \
             SYMMETRIC 10 AND 1 AS symmetric",
            "The same gap, and this line is listed for a reason worth naming: its first two \
             columns are the plain and reversed forms, which this node answers correctly — the \
             refusal is the whole *statement's*, because one unreadable keyword in it stops the \
             parse. The two facts it would have proved are proved by the rows above it instead.",
            "pg19_between.txt:37",
        ),
    ],
};

#[test]
fn every_between_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_between.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 25, "the corpus shrank: {checked} statements");
}
