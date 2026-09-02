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
        // **The reason changed under these two and the entry has to say so.** Selecting the
        // function used to be `0A000` — this node had array *expressions* only, nothing a
        // `RowDescription` could type — and boot statement 22's unit gave it the value. The
        // **first** read of each now agrees byte for byte, `{pg_catalog,public}` included, and
        // differs only in the declared type: `name[]` against `text`, the same trade as every
        // entry in `types` above.
        //
        // What keeps them here is the **second** read of each, after
        // `SET search_path TO public, pg_catalog` — a statement this node refuses by name three
        // entries below, so the path never changes and the answer stays `{public}` where a real
        // server then says `{public,pg_catalog}`. A divergence list keyed by statement *text*
        // cannot say "the second one", which is the same limitation `tests/corpus/pg19_date.txt`
        // records for `DateStyle`; listing them here is what that limitation costs, and it will
        // cost nothing the day `search_path` is honoured.
        (
            "SELECT current_schemas(false)",
            "The first read agrees exactly but for `name[]` against `text`. The second follows a \
             `SET search_path` this node refuses, so it answers for the path that is still set.",
        ),
        (
            "SELECT current_schemas(true)",
            "The same, for the form that also lists `pg_catalog`.",
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
