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
        "SELECT 'r', lanname, lanpltrusted FROM pg_language ORDER BY lanname",
        "SELECT 'r', p.proname, l.lanname FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang WHERE p.proname = 'fl_gen'",
        "SELECT 'r', proname FROM pg_proc WHERE proname = 'fl_pl'",
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
        ),
        (
            "CREATE OR REPLACE FUNCTION fl_plain(a integer) RETURNS integer AS $$ SELECT a + 1 $$ LANGUAGE SQL",
            "**A function with arguments is refused by name**, and that is older than this unit: \
             a stored body is never run, so a parameter would be a name nothing ever binds. \
             `uuid_test.rb`'s function takes none. Its own unit, and the same one that would give \
             a body an evaluator — a parameter list is only worth reading by something that can \
             use it.",
        ),
        (
            "INSERT INTO fl_t (name) VALUES ('a')",
            "**The default calls a function this node stores and cannot run**, so the row cannot \
             be written: `0A000` naming the function, at the point of use, which is where a real \
             server evaluates it.\n\nIt answered `XX000 internal error` until this unit — the \
             stored-default path treats a text it cannot parse as a broken catalog, which is right \
             for every other cause and wrong for this one. `XX000` says *this server has a bug* \
             about a statement a user can write.",
        ),
        (
            "SELECT 'r', fl_gen()::text AS called",
            "**A stored function is not a callable one.** The body is kept and never run — this \
             node has no evaluator for a `LANGUAGE SQL` body and none for `plpgsql` either, and \
             `CREATE TRIGGER` has registered functions it never fires since the trigger unit. So \
             a call is `0A000` naming the function, which is the same answer it gets for any name \
             the vocabulary lacks.\n\nMeasured before it was declared: `uuid_test.rb`'s eight \
             tests define a function and **never call it** — seven of the eight read the schema — \
             so what the file needs is that the name survive into a column default and out through \
             the dumper, which it now does. An interpreter for function bodies is its own unit and \
             this one deliberately is not it.",
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
