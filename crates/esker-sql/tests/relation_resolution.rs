//! `relation "…" does not exist` — how a name is resolved, and the catalog view behind run 51's row.
//!
//! Two things, and only one of them is a feature. The feature is `pg_stat_activity`, which
//! `migration_test.rb:1108-1114` reads to ask whether the connection that held an advisory lock is
//! gone; this node did not have the view at all, so the question came back `42P01` rather than
//! `false`. The other half is the **message shapes** a failed lookup produces, which are already
//! implemented and were never replayed against the oracle in one file: a query says `relation`, a
//! `DROP TABLE` says `table`, and a qualified name is quoted **whole** — `"public.nosuchtable"`,
//! not `"public"."nosuchtable"`. Those are the strings `ActiveRecord` matches on, so a corpus is
//! what keeps them from drifting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **`name` and `"char"` there, `text` here** — PostgreSQL's identifier and single-byte types,
    // which compare identically and are the standing choice every catalog view in this crate makes.
    types: &[
        "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
         a.attrelid = 'pg_stat_activity'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER \
         BY a.attnum",
        "SELECT 'r', pg_typeof(pid), pg_typeof(datname), pg_typeof(state), pg_typeof(query) FROM \
         pg_stat_activity LIMIT 1",
        "SELECT 'r', relkind, relname FROM pg_class WHERE relname IN \
         ('pg_type','pg_range','pg_class') ORDER BY relname",
        "SELECT 'r', relkind FROM pg_class WHERE relname = 'pg_stat_activity'",
        "SELECT 'r', t.typname, t.typelem, t.typdelim, t.typtype FROM pg_type as t LEFT JOIN \
         pg_range as r ON t.oid = r.rngtypid WHERE t.typname IN ('int2','int4','int8') ORDER BY \
         t.typname",
        "SELECT 'r', relname FROM pg_class WHERE relname IN ('rr','RR')",
        "SELECT 'r', (n.nspname LIKE 'pg_temp%') AS in_a_temp_schema, c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 'rr' ORDER BY \
         in_a_temp_schema",
        // **`name[]` there and `text` here**, which is the array half of the same trade:
        // `current_schemas` answers an array on a real server and this node prints the `{a,b}`
        // literal it renders as. The rows are byte-identical, which is what a client reads.
        "SELECT 'r', current_schemas(false), current_schemas(true)",
    ],
    answers: &[
        (
            "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = 'pg_stat_activity'::regclass AND a.attnum > 0 AND NOT \
             a.attisdropped ORDER BY a.attnum",
            "**A catalog view's columns are not in `pg_attribute` here**, and that is true of all \
             twenty-nine of them rather than of this one: `pg_attribute`'s rows are built from the \
             column lists of the *records* a `CREATE TABLE` wrote, and a catalog view has no \
             record. The view itself answers `SELECT *` with all twenty-two columns in a real \
             server's order — that is what this unit built — so what is missing is the catalog's \
             description of the catalog. Closing it would not close this row: five of the \
             twenty-two type names are types this node does not have (`name`, `inet`, `xid` \
             twice), so the answer would still differ, in five cells instead of all of them.",
        ),
        (
            "SELECT 'r', pg_typeof(pid), pg_typeof(datname), pg_typeof(state), pg_typeof(query) \
             FROM pg_stat_activity LIMIT 1",
            "The same fact as the `types` entry above, in the *rows* rather than the header: \
             `datname` is a `name` there and a `text` here, and `pg_typeof` reports what the \
             column is. The other three agree, which is what says the divergence is the one string \
             type and not the view.",
        ),
        (
            "SELECT 'r', relkind, relname FROM pg_class WHERE relname IN \
             ('pg_type','pg_range','pg_class') ORDER BY relname",
            "**`r` there and `v` here, and the two are load-bearing in opposite directions.** On a \
             real server `pg_class` and `pg_type` are ordinary tables that a client never sees in \
             a table list because they live in `pg_catalog`, which is not in \
             `current_schemas(false)`. This node has no `pg_catalog` namespace: every catalog \
             relation is reported in `public`, so the only thing keeping them out of \
             `ActiveRecord`'s `tables()` — which filters `relkind IN ('r','p')` — is that they \
             answer `v`. Reporting `r` without the namespace first would put `pg_class` in every \
             schema dump. The two changes are one unit and it is a namespace unit, not a `relkind` \
             one. `pg_stat_activity` is `v` on both sides, which is the row this unit needed.",
        ),
    ],
};

#[test]
fn every_relation_resolution_answer_is_postgresql_19_s() {
    let replayed = parity::replay_reporting(
        include_str!("corpus/pg19_relation_resolution.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        replayed.checked >= 66,
        "the corpus shrank: {} statements",
        replayed.checked
    );
    // **Zero, and it was twenty.** `CREATE TEMP TABLE` was `0A000` here and a statement there, so
    // it aborted the transaction and the capture's last third — the temp table shadowing a
    // permanent one, `DROP TABLE pg_temp.rr` uncovering it, and the two savepoint-visibility
    // probes after it — came back `25P02` and was compared by nobody. This assertion is what the
    // temp-table unit ([ADR 0054]) was measured by, and it stays as the thing that catches the
    // next refusal to open a hole in the middle of this file.
    assert_eq!(
        replayed.swallowed, 0,
        "an aborted transaction is swallowing statements this corpus is supposed to compare"
    );
}
