//! `ADD CONSTRAINT … CHECK … NOT VALID`, and the scan the plain form owes.
//!
//! Run 70's `ADD CONSTRAINT ... NOT VALID is not supported` — 6 tests over two files. `NOT VALID`
//! landed for a **foreign key** in an earlier unit and was refused by name on a `CHECK`, on the
//! argument that refusing was honest while nothing could skip the scan.
//!
//! **Measuring the family found that nothing was doing the scan either.** A plain
//! `ADD CONSTRAINT … CHECK` over a row that already violates it was accepted *silently*, where
//! PostgreSQL answers `23514 check constraint "…" of relation "…" is violated by some row`. So the
//! table ended up advertising a validated constraint its own rows contradict — reachable from one
//! statement, with nothing later to blame. The two halves are one unit: `NOT VALID` is the clause
//! that *skips* a scan, and the scan had to exist before skipping it could mean anything.
//!
//! The other measured fact is the one a reader would get backwards: **a `NOT VALID` constraint is
//! still enforced for new rows.** It says what was skipped, not what is enforced.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `conname` is a `name` on a real server and `text` here, with identical characters — the
    // standing trade every `pg_catalog` column makes.
    types: &[],
    answers: &[],
};

#[test]
fn every_check_not_valid_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_check_not_valid.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
