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
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        //
        // **One left of four.** `current_schemas(false)`, `(true)` and `unnest` of one all answer
        // `name[]` and `name` now, declared type included (ADR 0086). `session_user` and `user`
        // are still `text` here: only `current_user` was given the type, because the other two are
        // a different value on a real server and this node has no roles to tell them apart.
        "SELECT current_user, session_user, user",
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
            "SELECT current_schemas(NULL)",
            "`NULL` on a real server — the function is strict, so a NULL argument makes a NULL \
             array. This node names it instead, because the argument is neither `true` nor \
             `false` and an array it cannot represent is not worth a third answer. It closes with \
             stored arrays.",
            "pg19_current_schemas.txt:65",
        ),
        (
            "SELECT current_catalog, current_database()",
            "The database's own name, which this node does not model: it serves one database and \
             has no name for it. A session-identity unit, not this one.",
            "pg19_current_schemas.txt:55",
        ),
        (
            "SELECT version() IS NOT NULL",
            "`version()` is not implemented. It is the one line here a client actually reads, and \
             it belongs with the session-identity unit beside `current_user`.",
            "pg19_current_schemas.txt:57",
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
