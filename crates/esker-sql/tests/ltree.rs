//! A column of `ltree`, against PostgreSQL 19beta1.
//!
//! Run 70's row: 4 tests in `adapters/postgresql/ltree_test.rb`. The file is small — one
//! extension, one column, a round trip and a schema dump — and the type under it is not: an
//! `ltree`'s **order is not its text's**, which is the one thing this corpus is mostly about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus installs its own extension and creates its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // The standing catalog trade: `typname`, `udt_name` and `extname` are `name` on a real
        // server and `data_type` is `information_schema`'s own domain, all `text` here, all
        // comparing identically.
        // `pg_typeof` is a `regtype` there and `text` here; the value beside it agrees, and it is
        // the one this asks — an `ltree[]` literal is an `ltree[]`.
    ],
    answers: &[
        // **`COLLATE` used to be the reason both of these were here**, and it is not any more:
        // `C` and `POSIX` name byte order, which is what a memcomparable key gives, so the clause
        // is honoured ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)).
        // The comparison below it agreed the moment that landed and its entry is gone.
        //
        // What is left is `ltree`'s own ordering: `ORDER BY path::text COLLATE "C"` sorts the
        // *text* of the paths, and this node's `ltree` is stored as its text, so the two orders
        // are the same here and are not on a real server, where an `ltree` sorts by label. The
        // control statement above — `ORDER BY path` — is where that difference is visible.
        (
            "SELECT 'r', path FROM ltrees ORDER BY path::text COLLATE \"C\"",
            "an ltree sorts by label on a real server and by its text here",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_ltree_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_ltree.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
