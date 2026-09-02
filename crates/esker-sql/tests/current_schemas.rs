//! `current_schema`, `current_schemas` and their neighbours, against PostgreSQL 19beta1.
//!
//! Imported from `r1-harness`'s capture and replayed rather than paraphrased. The reason is in the
//! corpus header: one function with two spellings, only one implemented, and three rounds of
//! "still broken" / "already fixed" before anybody ran the file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression or a catalog read.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `name` is the 64-byte identifier type, which this node does not have; it answers `text`,
        // whose values are identical and which every comparison against one already assumes.
        "SELECT current_schema",
        "SELECT current_schema()",
        "SELECT n.nspname FROM pg_namespace n WHERE n.nspname = ANY(current_schemas(false))",
    ],
    answers: &[
        (
            "SELECT current_schemas(false)",
            "Selecting it returns an **array**, and this node has array expressions only — no \
             array `Datum`, nothing a `RowDescription` could type. Inside an `= ANY` it answers \
             exactly. ADR 0033's roadmap puts stored arrays in tier 2.",
        ),
        (
            "SELECT current_schemas(true)",
            "The same, for the form that also lists `pg_catalog`.",
        ),
        (
            "SELECT current_schemas(false)::text",
            "The same again: the cast is fine and the value it would cast is the array.",
        ),
        (
            "SELECT current_schemas(NULL)",
            "`NULL` on a real server — the function is strict, so a NULL argument makes a NULL \
             array. This node names it instead, because the argument is neither `true` nor \
             `false` and an array it cannot represent is not worth a third answer. It closes with \
             stored arrays.",
        ),
        (
            "SELECT current_schemas(false)[1]",
            "Subscripting is `42601 syntax error at or near \"[\"` on a real server — its *parser* \
             refuses it because `current_schemas(false)` is not a subscriptable expression there \
             either. This node answers `0A000` naming the expression: both refuse, with different \
             codes.",
        ),
        (
            "SELECT pg_typeof(current_schemas(false)), pg_typeof(current_schema)",
            "`pg_typeof` is not implemented, `0A000` naming it.",
        ),
        (
            "SELECT current_catalog, current_database()",
            "The database's own name, which this node does not model: it serves one database and \
             has no name for it. A session-identity unit, not this one.",
        ),
        (
            "SELECT current_user, session_user, user",
            "The same — there are no roles here, which is already the declared divergence behind \
             `pg_catalog`'s `42501`.",
        ),
        (
            "SELECT version() IS NOT NULL",
            "`version()` is not implemented. It is the one line here a client actually reads, and \
             it belongs with the session-identity unit beside `current_user`.",
        ),
        (
            "SELECT array_length(current_schemas(false), 1), array_length(current_schemas(true), 1)",
            "`array_length` is an array function, queued with `array_agg`.",
        ),
        (
            "SELECT unnest(current_schemas(true))",
            "`unnest` likewise, and it is set-returning, which is a second feature again.",
        ),
        (
            "SET search_path TO public, pg_catalog",
            "A `search_path` this node cannot honour is refused by name rather than accepted and \
             ignored — accepting it would make `current_schemas` answer for a path nobody set. \
             Three lines here are that refusal and the two `current_schemas` reads after it.",
        ),
        (
            "SET search_path TO nosuchschema, public",
            "The same refusal. A real server takes a path naming a schema that does not exist, \
             which is worth knowing and is not something to imitate before there are schemas.",
        ),
    ],
};

#[test]
fn every_schema_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_current_schemas.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
