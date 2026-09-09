//! `ALTER TABLE … DROP CONSTRAINT` — **the one statement four `ActiveRecord` methods end in**,
//! 28 tests over 4 files.
//!
//! `remove_check_constraint`, `remove_foreign_key`, `remove_unique_constraint` and
//! `remove_exclusion_constraint` all render through `schema_creation.rb:101`.
//!
//! The distinction the capture spends four lines on, and the one an implementation gets wrong: a
//! unique **index** is not a unique **constraint**. They build the same index and look identical
//! in `pg_indexes`; `DROP CONSTRAINT` removes only the second, and `DROP INDEX` only the first.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_indexes.indexname` is a `name` on a real server and `text` here, with the same
    // characters — the standing trade every `pg_catalog` column makes. Three occurrences.
    types: &[],
    answers: &[
        // `ALTER COLUMN … SET NOT NULL` was here, refused because it has to check every row.
        // It now runs the scan and is gone from this list (ADR 0031 rule 2) —
        // `pg19_foreign_key_options.txt` is where the whole family is measured.
        (
            "SELECT 'r', conname, contype FROM pg_constraint WHERE conrelid = '\"dcp\"'::regclass \
             AND contype = 'n' ORDER BY conname",
            "Reads back what the `SET NOT NULL` above would have restored, so it is one row short \
             here — a consequence of that refusal and not of anything `DROP CONSTRAINT` did.",
            "pg19_drop_constraint.txt:88",
        ),
    ],
};

#[test]
fn every_drop_constraint_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_drop_constraint.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
