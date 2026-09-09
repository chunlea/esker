//! `CREATE TEMPORARY TABLE` — a relation in a schema that belongs to one session
//! ([ADR 0054](../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md)).
//!
//! The whole of it is the search path. A temp table is an ordinary relation in a schema named
//! `pg_temp_<n>`, and pushing that schema to the front of the session's resolved path is what makes
//! a bare name find it, `public.x` find the permanent one, and a table list — which filters
//! `nspname = ANY (current_schemas(false))` — see neither. There is no rule anywhere that says
//! "hide temporary tables"; PostgreSQL's two spellings of the search path already say it.
//!
//! Run 56 stops one test on this. The measure that matters is next door:
//! `tests/relation_resolution.rs` asserts 20 statements of its corpus are swallowed by the
//! transaction this refusal aborts, and that number falls to zero here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: both corpora build what they need.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** PostgreSQL's identifier type is a `name` here (ADR 0084) and its single-byte
    // type a `"char"` (ADR 0095), which is what these seven entries declared — six of them read
    // `relpersistence`. Every row below has the right rows, `relpersistence` `t` included, and
    // now the right declared types with them.
    types: &[],
    answers: &[(
        "CREATE TEMPORARY UNLOGGED TABLE tt_both (id int)",
        "**Both refuse it and both say `42601`** — the two words cannot be combined on a real \
         server either, which is what `UnloggedTablesTest` is checking `ActiveRecord` never \
         sends. What differs is the sentence: PostgreSQL's parser says `syntax error at or near \
         \"UNLOGGED\"` and `sqlparser` 0.62.0 lists the tokens it expected. It is the same family \
         as every other parser message this node passes through and not a rule of its own, and it \
         arrives here for a reason worth keeping: `TEMPORARY` is a flag the parser reads while \
         `UNLOGGED` is cut out of the source before parsing (`crate::parse::strip_unlogged`), and \
         the strip deliberately does not match the two together — so the combination reaches the \
         parser and fails there, exactly as it does on a real server.",
        "pg19_temp_table.txt:96",
    )],
};

#[test]
fn every_temp_table_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_temp_table.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 40, "the corpus shrank: {checked} statements");
}

/// The `ON COMMIT` half, captured in a session that really commits.
const ON_COMMIT_DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_on_commit_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_temp_on_commit.txt"),
        CORPUS_FIXTURE,
        &ON_COMMIT_DIVERGENCES,
    );
    assert!(checked > 20, "the corpus shrank: {checked} statements");
}

/// **The two rules an oracle in one session cannot show**, and they are the ones a client feels:
/// another session cannot see the table, and it is gone when the session that made it ends.
///
/// Both are about *two* connections, so no `BEGIN … ROLLBACK` capture can reach them — a corpus is
/// one session by construction. They are asserted here against two executors on one store, which
/// is the same arrangement `tests/two_database_dogs.rs` uses for the other thing a single session
/// cannot show.
#[test]
fn a_temp_table_belongs_to_one_session_and_dies_with_it() {
    use std::sync::Arc;

    use esker_sql::backend::{Backend, MemoryBackend};
    use esker_sql::catalog::Catalog;

    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());

    let mut other = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, "esker", &[]);
    {
        let mut mine =
            parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, "esker", &[]);
        mine.run("CREATE TEMPORARY TABLE session_only (id int)")
            .unwrap();
        mine.run("INSERT INTO session_only (id) VALUES (1)")
            .unwrap();
        assert_eq!(mine.rows("SELECT count(*) FROM session_only"), [["1"]]);

        // **The other session cannot name it**, because the schema it is in is in nobody else's
        // search path — which is the whole of the isolation, and needs no rule of its own.
        assert!(
            other.run("SELECT count(*) FROM session_only").is_err(),
            "another session resolved a name only the first one can see"
        );
        // It is in `pg_class` for everyone, as it is on a real server: what is private is the
        // ability to *name* it, not its existence.
        assert_eq!(
            other.rows("SELECT count(*) FROM pg_class WHERE relname = 'session_only'"),
            [["1"]]
        );
    }

    // ...and the session ending takes it, records and rows together.
    assert_eq!(
        other.rows("SELECT count(*) FROM pg_class WHERE relname = 'session_only'"),
        [["0"]]
    );
    assert_eq!(
        other.rows("SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'pg_temp%'"),
        [["0"]]
    );
}
