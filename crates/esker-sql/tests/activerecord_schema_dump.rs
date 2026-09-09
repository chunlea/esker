//! The acceptance for phase 13: `ActiveRecord`'s schema-dump path, statement for statement.
//!
//! Every statement `schema_dumper`, `indexes()`, `foreign_keys()`, `columns()` and
//! `primary_keys()` send — verbatim from `activerecord-8.1.3.1` — over a fixture schema of this
//! node's own types, put to both servers.
//!
//! # Why the real statements rather than their shapes
//!
//! `tests/pg_catalog_*.rs` assert the rows of each relation. This asserts that the rows **join**,
//! three deep in one statement (`pg_class t → pg_index d → pg_class i`, with `pg_namespace` on the
//! side), which is exactly what two views computing an oid independently would get wrong and what
//! no per-relation corpus can see. It is also the only place `format_type` is asked about a
//! *column's* `atttypid` and `atttypmod` eighteen times over fourteen types rather than about
//! literals.
//!
//! Where a statement carries a clause this node has no feature for, the clause is named in the
//! divergence list and the rest of the statement is still here — because what is missing is a
//! clause and not a row, and saying which is the whole job of ADR 0031's category (c).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schema.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `attname`, `relname`, `conname` and `collname` are `name` on a real server, `atttypid` is an
    // `oid` and `indkey` is an `int2vector`; this node has none of those types, so they are `text`
    // and `bigint`. `atttypmod` is an `int4` and `ordinal_position` an `integer` on both. Every
    // value is identical — the harness only reaches this list when the rows already agree.
    types: &[
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod FROM pg_attribute a LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE a.attrelid = '\"dumpy\"'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod, attidentity, attgenerated FROM pg_attribute a LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE a.attrelid = '\"dumpz\"'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "SELECT conname, contype, pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'p' AND t.relname = 'dumpz' AND n.nspname = ANY (current_schemas(false))",
    ],
    answers: &[
        // **Two entries left this list with the `name` columns, and their reasons had gone stale
        // behind the type difference.** They said `= ANY(i.indkey)` over an array value and a
        // per-row `t2.oid::regclass::text` were refused `0A000`; both answer now — `id`, and no
        // rows, which is what the oracle says — and what had kept the *rows* half from being
        // compared was the declared type: `attname` and `conname` were `text` here and `name`
        // there. A type divergence standing in front of an answer divergence hides the day the
        // answer stops diverging, which is the one thing ADR 0031 rule 2 exists to catch.
    ],
};

#[test]
fn every_schema_dump_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_activerecord_schema_dump.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The clauses this path still needs, each refused **naming itself**.
///
/// Contract C2 in the one place it matters most: a client that cannot get an answer must be able
/// to read which feature is missing out of the error. Every one of these is ADR 0031 category (c),
/// and every one of them is a clause wrapped around rows this node already has.
///
/// **Two entries left this list when the catalog-functions unit landed**: the comment columns,
/// `col_description` and `obj_description`, which now answer NULL rather than refusing — and NULL
/// is what a real server answers about a table nobody has commented on.
///
/// **A third left with the `indkey` unit**: `= ANY` over an `int2vector`, which is how
/// `primary_keys()` reads a key, runs now — the array it needed is a value of the row, and
/// `crate::value::vector` says why that is text rather than a `Datum`.
///
/// **The register is empty.** `ARRAY(SELECT …)` over `generate_subscripts` was the last entry and
/// it runs, so what this test asserts has turned around: it used to name what the dump could not
/// do, and now it holds the statement that emptied it and checks that it still answers. The
/// `GAPS` shape stays because the register may fill again — a schema this node has not met yet
/// puts an entry back — and an empty one is a state it was written to reach.
#[test]
fn what_the_schema_dump_still_needs_names_itself() {
    // A **register**, not a list of examples: an entry leaves it when its unit lands.
    // `col_description` and `= ANY` over an `int2vector` left it, then `ARRAY(SELECT …)` did.
    const GAPS: &[(&str, &str)] = &[];

    // The last entry to leave, kept as an assertion rather than deleted: it is the statement
    // `indexes()` builds each index's column list with, and the one boot statement 32 is.
    const CLOSED: &str = "SELECT ARRAY(SELECT pg_get_indexdef(d.indexrelid, k + 1, true) FROM \
                          generate_subscripts(d.indkey, 1) AS k ORDER BY k) AS columns FROM \
                          pg_index d WHERE d.indrelid = 'nd'::regclass ORDER BY columns";

    let mut node = parity::Node::new(&[
        "CREATE TABLE nd (id bigserial PRIMARY KEY, a int4)",
        "CREATE INDEX nd_a_idx ON nd (a)",
    ]);

    for (statement, wanted) in GAPS {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement}"
        );
        let named = error.to_string();
        assert!(
            named.to_lowercase().contains(wanted),
            "the refusal for {statement} does not name {wanted}: {named}"
        );
    }

    // **Two indexes**, the one declared and the primary key's, each a single column — so the
    // answer is one array per index holding that index's column. Which is the whole of what
    // `indexes()` reads this statement for.
    assert_eq!(
        node.rows(CLOSED),
        vec![vec!["{a}".to_owned()], vec!["{id}".to_owned()]]
    );
}

/// The whole of `columns()` over a table of every type this node has, joined as `ActiveRecord`
/// writes it.
///
/// The statement `schema.rb` stops on at line 193, run for real. Not a corpus divergence and not a
/// shape: this is the eighteen rows a schema dump reads to write a migration back out, and every
/// value in them comes from a different part of this phase — `format_type` from the type and the
/// typmod, `pg_get_expr` from a `LEFT JOIN` that matched once out of eighteen, `attnotnull` from
/// the column and from the key it is in.
#[test]
fn columns_reads_a_whole_table_back() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cw (id bigserial PRIMARY KEY, name character varying, \
         settings character varying(1024), tag character(3), precise timestamp(3), \
         note text NOT NULL, code integer DEFAULT 7)",
    ]);

    assert_eq!(
        node.rows(
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod), \
             pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod \
             FROM pg_attribute a \
             LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             WHERE a.attrelid = '\"cw\"'::regclass AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum"
        ),
        vec![
            vec![
                "id",
                "bigint",
                "nextval('cw_id_seq'::regclass)",
                "t",
                "20",
                "-1"
            ],
            vec!["name", "character varying", "\\N", "f", "1043", "-1"],
            vec![
                "settings",
                "character varying(1024)",
                "\\N",
                "f",
                "1043",
                "1028"
            ],
            vec!["tag", "character(3)", "\\N", "f", "1042", "7"],
            vec![
                "precise",
                "timestamp(3) without time zone",
                "\\N",
                "f",
                "1114",
                "3"
            ],
            vec!["note", "text", "\\N", "t", "25", "-1"],
            vec!["code", "integer", "7", "f", "23", "-1"],
        ]
    );
}
