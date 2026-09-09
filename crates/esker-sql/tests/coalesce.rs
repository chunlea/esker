//! `COALESCE` and `current_database()`, against PostgreSQL 19beta1.
//!
//! Two rows of run 46's ranking — `COALESCE` 143 tests over 20 files, `current_database` 52 over 7
//! — and both are ordinary SQL rather than a type gap. `COALESCE` reaches the server from
//! `counter_cache.rb`, `relation.rb` and Arel's factory methods; `current_database()` is the
//! adapter's own, in four queries it runs against `pg_database` while connecting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`name` there, `text` here** — the 64-byte identifier type, which compares identically and
    // is the standing choice every catalog view in this crate makes. `current_database()` is one
    // of them, and `COALESCE` carries the type through, which is what the last line measures.
    types: &[
        // **The two constant-width divergences this line carried are gone**, and what is left is
        // `pg_typeof`'s own `regtype`/`text` trade (ADR 0077): the `int4` rung (ADR 0087) and the
        // decimal-literal `numeric` closed the widths, and **the promotion itself was always
        // right** — the integer gives way to the wider type rather than the first argument
        // winning, which is what this line was added to measure.
        "SELECT 'r', pg_typeof(COALESCE(NULL::int4, 0)), pg_typeof(COALESCE(NULL::text, 'x')), \
         pg_typeof(COALESCE(1, 2.5))",
        "SELECT 'r', current_database()",
        "SELECT 'r', pg_encoding_to_char(encoding) FROM pg_database WHERE datname = current_database()",
        "SELECT 'r', COALESCE(NULL, current_database())",
    ],
    answers: &[
        // **The locale belongs to the oracle's container, not to PostgreSQL** — the capture's own
        // header says so. This node has no collation support at all (`pg_collation` is empty for
        // the same reason), so the honest locale is the one that sorts by byte value. What the
        // adapter needs from these two queries is that they answer with a `text`, which they do.
        (
            "SELECT 'r', datcollate FROM pg_database WHERE datname = current_database()",
            "no collation support here, so the locale is C rather than the container's en_US.utf8",
            "pg19_coalesce_current_database.txt:53",
        ),
        (
            "SELECT 'r', datctype FROM pg_database WHERE datname = current_database()",
            "no collation support here, so the locale is C rather than the container's en_US.utf8",
            "pg19_coalesce_current_database.txt:54",
        ),
        // `pg_typeof` answers a `regtype` there and `text` here, and `current_database()` is a
        // `name` there and `text` here — the same trade `current_schema()` already makes. The
        // value is the database's name either way.
        (
            "SELECT 'r', pg_typeof(current_database())",
            "pg_typeof answers text here, and current_database is a name there and text here",
            "pg19_coalesce_current_database.txt:55",
        ),
        // Both messages are right in every part except that width: the `22P02` is the unknown
        // literal coerced to the common type, and the `42804` names `COALESCE` and the two types
        // in the order the list is walked.
    ],
};

#[test]
fn every_coalesce_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_coalesce_current_database.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
