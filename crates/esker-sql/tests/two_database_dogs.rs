//! The `dogs` table, twice — the 103 tests that need a second database, and why they are not a
//! work item for any other lane.
//!
//! `ActiveRecord`'s `schema.rb` creates `dogs` on `arunit` with four association columns and then,
//! on its **last line**, creates a bare `dogs` on `arunit2`. Pointed at one namespace, the second
//! `create_table … force: true` drops and recreates the first, and every fixture that names
//! `trainer_id` fails. This capture is that sequence on a real server, in one database, showing
//! the column list go 5 → 1 — the measurement behind [ADR 0052](../../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`name` there, `text` here** — PostgreSQL's 63-byte identifier type, which compares
    // identically and is the standing choice every catalog view in this crate makes
    // (`tests/coalesce.rs` declares the same fact about the same function).
    types: &[
        "SELECT 'r', current_database()",
        "SELECT 'r', datname FROM pg_database WHERE datname = current_database()",
    ],
    answers: &[(
        "SELECT 'r', 'a'::name = 'a'::name",
        "**A cast to `name`**, PostgreSQL's 63-byte identifier type, which is not one of the \
             stored types (ADR 0033) and has no spelling here at all — the standing choice is that \
             every catalog column and every server function answering a `name` answers a `text`, \
             declared in the `types` list above and in `tests/coalesce.rs`. So the two halves are \
             one fact: a `name` cannot be *reported* and cannot be *cast to*. It compares \
             identically, which is why the row this line asks for is right on both sides and only \
             the cast is refused. A type-surface item, not a database one.",
    )],
};

#[test]
fn every_two_database_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_two_database_dogs.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}
