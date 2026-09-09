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
/// **Nothing, and the two entries that used to be here closed one after the other.**
///
/// `ALTER COLUMN … SET NOT NULL` was the first: refused because it has to check every row, then
/// built, and its entry deleted per [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md)
/// rule 2 — `pg19_foreign_key_options.txt` is where that family is measured now. The second was
/// its *consequence*, a `pg_constraint` read that came back one row short because the constraint
/// the refusal never restored was missing, and it agreed the moment the first was built. Nobody
/// deleted it, because rule 2 could not see it: its corpus line declares no types, and rule 2
/// asked for the whole answer to be equal while this node always answers *some* type. Fixed in
/// `parity_harness/mod.rs`'s `agrees`, which found this row and eleven others across eight files.
///
/// The `types` list emptied the same way, on the catalog's `name` unit; the header comment that
/// described its three occurrences went with it.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
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
