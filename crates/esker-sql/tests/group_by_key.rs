//! `must appear in the GROUP BY clause` — run 57's third row, 6 tests over 2 files.
//!
//! **A functional dependency, and the capture confirms it is the whole row**: PostgreSQL accepts a
//! bare column when the `GROUP BY` contains that table's primary key, because grouping by a key
//! means one row per group of that table and every other column of it therefore has exactly one
//! value. `ActiveRecord` writes it constantly — `group(:id)` on a relation selecting `*`.
//!
//! It is **per table**. `GROUP BY f.id` frees every column of `f` and none of `a`, in the same
//! select list; an implementation that read it as "some key is grouped" would accept a query that
//! really is ambiguous.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_group_by_key_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_group_by_key.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 15, "the corpus shrank: {checked} statements");
}
