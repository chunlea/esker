//! `CREATE` / `REFRESH` / `DROP MATERIALIZED VIEW` — run 70's row, 9 tests in `view_test.rb`.
//!
//! **A materialized view is a table whose rows are recomputed**
//! ([ADR 0064](../../../docs/adr/0064-a-materialized-view-is-a-table-whose-rows-are-recomputed.md)),
//! which is the one line of the capture that settles the design: an `INSERT` into the base table
//! leaves `count(*)` at 1, and the `REFRESH` after it makes the answer 2. A view in this node is a
//! stored `SELECT` expanded where it is read, and a materialized view is defined by *not* doing
//! that.
//!
//! `view_test.rb` includes the same nine-test module twice — once over `CREATE VIEW`, once over
//! `CREATE MATERIALIZED VIEW` — so the second copy asks the same nine questions of a relation that
//! holds its rows. `test_does_not_assume_id_column_as_primary_key` is the sharp one: a materialized
//! view has **no** primary key, and the capture agrees — `pg_constraint` and `pg_index` are both
//! empty for a fresh one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table and relations.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** PostgreSQL's identifier type is a `name` here (ADR 0084) and its single-byte
    // type a `"char"` (ADR 0095); those two were the whole of what this list declared.
    types: &[],
    answers: &[],
};

#[test]
fn every_materialized_view_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_matview.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The refusals, which are the only thing between a client and a **writable** materialized view:
/// the relation is a table underneath, so every guard here is load-bearing rather than cosmetic.
#[test]
fn every_materialized_view_refusal_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_matview_guards.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
