//! `SELECT *` with `GROUP BY` — the functional dependency, the star form.
//!
//! The named-column form landed with run 57's third shape: PostgreSQL accepts a bare column when
//! the `GROUP BY` contains that table's primary key, and this node honours it by **widening the
//! grouping** rather than relaxing the check. This file is the form where the target list is `*`,
//! which run 59 ranks at 10 tests over 3 files — `ActiveRecord` writes `Model.group(:id)` on a
//! relation selecting everything, constantly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_group_by_star_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_group_by_star.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
