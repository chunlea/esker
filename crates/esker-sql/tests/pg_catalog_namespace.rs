//! The catalog lives in `pg_catalog` — run 56's two rows, and the divergence that argued for it.
//!
//! Every catalog relation was reported in `public` with `relkind` `v`, and the `v` was doing the
//! namespace's job: it is what kept `pg_class` out of `ActiveRecord`'s `tables()`, which filters
//! `relkind IN ('r','p')`. That worked and was two wrong answers at once — the catalog's tables
//! are `r` on a real server, and they are excluded by their *schema* rather than by their kind.
//!
//! Two statements from `ActiveRecord`'s own adapter are the measure, both verbatim in the corpus:
//! `primary_keys` sends `i.indrelid = '"pg_type"'::regclass` — the name **quoted**, which resolved
//! to nothing — and `table_comment` sends `FROM pg_catalog.pg_class c LEFT JOIN pg_namespace n ON
//! n.oid = c.relnamespace`, which needs the qualifier and the join to agree about where the
//! catalog is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The declared types this entry named all agree now** — `name` (ADR 0084) and `"char"`
    // (ADR 0095) — and it stays because the same statement is an `answers` divergence too: the
    // harness reads a `types` entry only once the *rows* agree, so this one is not read at all
    // and will be deleted with the answer it shadows.
    // Every row below has the right *rows*.
    types: &[
        "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
         a.attrelid = 'pg_namespace'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY \
         a.attnum",
        "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE \
         a.attrelid = 'pg_sequence'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY \
         a.attnum",
        "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND \
         a.attnum = ANY(i.indkey) WHERE i.indrelid = '\"pg_type\"'::regclass AND i.indisprimary \
         ORDER BY array_position(i.indkey, a.attnum)",
    ],
    answers: &[
        (
            "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = 'pg_namespace'::regclass AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            "**This node's `pg_namespace` has two of the four columns**, and the wide catalog \
             relations are where its declared divergences live: a client reads `oid` and \
             `nspname`, and `nspowner` and `nspacl` are an owner and an ACL on a server that has \
             no roles at all. The rows that *are* here are right, in the right order and with the \
             right `attnum`, which is what the catalog describing itself buys. The second half of \
             the difference is the oid column: an `oid` there and a `bigint` here, the same trade \
             `pg_class.oid` already declares everywhere.",
            "pg19_catalog_namespace.txt:59",
        ),
        (
            "SELECT 'r', a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = 'pg_sequence'::regclass AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            "**All eight columns, in order, and two type names apart** — which is the good half of \
             the same fact: `seqrelid` and `seqtypid` are `oid` there and `bigint` here, and the \
             six that are not oids agree exactly. This node has no `oid` *type* for a catalog \
             column to be declared as; `ColumnType::Oid` exists and is what `pg_stat_activity` \
             uses, so closing this is a pass over the catalog's own column lists rather than a \
             type-surface change — a unit of its own, and one that would move eleven views at once.",
            "pg19_catalog_namespace.txt:61",
        ),
        (
            "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND \
             a.attnum = ANY(i.indkey) WHERE i.indrelid = '\"pg_type\"'::regclass AND \
             i.indisprimary ORDER BY array_position(i.indkey, a.attnum)",
            "**`ActiveRecord`'s own `primary_keys`, and the statement this unit exists for.** It \
             answered `42P01 relation \"pg_type\" does not exist` — the name arrives quoted from \
             `quote_table_name`, and a catalog relation was resolved by its bare spelling only — \
             which stopped seven tests in `schema_cache_test.rb` at the first thing they did. It \
             runs now and answers **no rows** where PostgreSQL answers `oid`: a real server's \
             catalog is tables with real indexes on them, and this node's is computed, so \
             `pg_index` has nothing for it. `ActiveRecord` reads that as \"no primary key\", which \
             is what it reads for a view — true of a computed relation, and the answer it can act \
             on.",
            "pg19_catalog_namespace.txt:73",
        ),
        (
            "SELECT 'r', count(*) AS catalog_tables_listed FROM information_schema.tables WHERE \
             table_name IN ('pg_class','pg_type')",
            "**`information_schema.tables` is every schema's relations, not the search path's**, \
             so a real server lists the catalog's tables in it — which is exactly why \
             `ActiveRecord` reads `pg_class` joined to `pg_namespace` for `tables()` and not this \
             view. This node's version is built from the stored relations of one schema and \
             hardcodes `table_schema` to `public`, so adding the catalog to it would produce a \
             *wrong* row (`public|pg_class`) rather than a missing one. The `information_schema` \
             views are their own surface with their own rules — `table_type`, \
             `is_insertable_into`, a row per view as well as per table — and closing this one \
             means capturing that surface, not extending this unit.",
            "pg19_catalog_namespace.txt:76",
        ),
        (
            "SELECT 'r', count(*) AS catalog_columns_listed FROM information_schema.columns WHERE \
             table_name = 'pg_class'",
            "The other half of the same view's gap, and the number is the second half of the \
             finding: **34**, PostgreSQL's own column count for `pg_class`, where this node models \
             ten. So even a version of this view that listed the catalog would not answer 34 — \
             which is what says the two questions are separate, and that this one is about how \
             wide the catalog is rather than about which schema it is in.",
            "pg19_catalog_namespace.txt:86",
        ),
    ],
};

#[test]
fn every_pg_catalog_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_namespace.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked >= 61, "the corpus shrank: {checked} statements");
}
