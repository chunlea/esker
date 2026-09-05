//! `ADD COLUMN … NOT NULL` with no `DEFAULT`, and `TRUNCATE`.
//!
//! **Run 66: 5 tests over 4 files, and 4 more over 2.** Both were refusals with written reasons,
//! and the first one's reason was half a rule: "`NOT NULL` needs a value for every row already
//! stored … the alternative is a rewrite this `ALTER` is defined not to do." An *empty* table has
//! no row to hold a NULL, so there is nothing to refuse — and every one of the five tests adds a
//! column to an empty table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **`last_value` reports the reserved block's end, not the last value handed out** — 32 where
    // PostgreSQL says 3. A session takes `SEQUENCE_BATCH` values at a time and hands them out from
    // memory, which is the documented trade that makes `nextval` one write per batch instead of
    // one per row; `last_value` is the one place it is visible. Pre-existing, and not what this
    // unit changed: `TRUNCATE … RESTART IDENTITY` now resets both the counter and the block, which
    // is what the `SELECT id` lines after it measure and they agree.
    answers: &[(
        "SELECT last_value FROM acnn_t_id_seq;",
        "The reserved block's end against the last value handed out: a session takes a batch \
             and serves from it, so the stored counter runs ahead of what any row has seen. The \
             values `nextval` produces agree, which is what every other line here checks.",
        "pg19_add_column_not_null.txt:56",
    )],
};

#[test]
fn every_add_column_not_null_and_truncate_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_add_column_not_null.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}
