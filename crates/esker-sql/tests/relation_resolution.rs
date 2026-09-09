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
    // The standing `pg_catalog` trade, down to the oids: `pg_type.oid` is an `oid` on a real
    // server and a `bigint` here, which is the whole of what the second entry still declares.
    // PostgreSQL's identifier type is a `name` here (ADR 0084) and its single-byte type a
    // `"char"` (ADR 0095), and the first entry — which names neither any more — stays only
    // because the same statement is an `answers` divergence.
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
         a.attrelid = 'pg_stat_activity'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER \
         BY a.attnum",
        // **`name[]` there and `text` here**, which is the array half of the same trade:
        // `current_schemas` answers an array on a real server and this node prints the `{a,b}`
        // literal it renders as. The rows are byte-identical, which is what a client reads.
    ],
    answers: &[(
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
        "pg19_relation_resolution.txt:54",
    )],
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
