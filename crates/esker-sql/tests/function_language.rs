//! `CREATE FUNCTION … LANGUAGE SQL`, and a column default that calls one.
//!
//! Run 70's `language "…" does not exist` — 8 tests, one file. All eight are the **setup** of
//! `uuid_test.rb`'s `PostgresqlUUIDGenerationTest`, which defines a function and then creates a
//! table defaulting to it:
//!
//! ```sql
//! CREATE OR REPLACE FUNCTION my_uuid_generator() RETURNS uuid
//!   AS $$ SELECT * FROM uuid_generate_v4() $$ LANGUAGE SQL VOLATILE;
//! ```
//!
//! **None of the eight ever inserts a row.** They read the *schema* — seven of them are schema-dump
//! assertions — so what the file needs is for the function to be creatable and for its name to
//! survive into `pg_attrdef` and out through the dumper. Measuring that first is what kept this
//! unit from being an interpreter for SQL-language function bodies.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own function and table.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `lanname` and `proname` are `name` on a real server and `text` here, with identical
    // characters — the standing trade every `pg_catalog` column makes.
    types: &[
        "SELECT 'r', column_name, column_default FROM information_schema.columns WHERE table_name = 'fl_t' AND column_name = 'id'",
    ],
    answers: &[
        (
            "CREATE FUNCTION fl_c() RETURNS integer AS 'x', 'y' LANGUAGE c",
            "**PostgreSQL gets further than this node and fails at the file.** `c` is in its \
             `pg_language` — and now in this node's, because that catalog is what a client reads \
             and the language does exist on the server this answers as — so a real server accepts \
             the language and then tries to open the shared object: `58P01 could not access file \
             \"x\"`. There is no shared object here and no path to look one up in, so the \
             refusal lands one step earlier, at the language, with `42704`.\n\nA node that \
             answered `58P01` would be claiming to have looked for a file, which is a worse lie \
             than refusing the language it cannot load.",
            "pg19_function_language.txt:27",
        ),
        (
            "CREATE OR REPLACE FUNCTION fl_plain(a integer) RETURNS integer AS $$ SELECT a + 1 $$ LANGUAGE SQL",
            "**A function with arguments is refused by name**, and that is older than this unit: \
             a stored body is never run, so a parameter would be a name nothing ever binds. \
             `uuid_test.rb`'s function takes none. Its own unit, and the same one that would give \
             a body an evaluator — a parameter list is only worth reading by something that can \
             use it.",
            "pg19_function_language.txt:19",
        ),
    ],
};

#[test]
fn every_function_language_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_function_language.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
