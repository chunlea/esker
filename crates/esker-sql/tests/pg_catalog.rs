//! Contract C3 for `pg_catalog`, and the first slice of it: `pg_type` and `pg_range`.
//!
//! The scoreboard ranked these two first and gave the reason: four of `ActiveRecord`'s 36 boot
//! statements read them, they are the *first* thing it asks for, and rung 2 of the ladder stopped
//! on `relation "pg_type" does not exist` — so they are the only item that could move it
//! (`docs/bench/rails-scoreboard.md`).
//!
//! The corpus is 46 statements put to a real PostgreSQL 19beta1 in one session and replayed the
//! same way against one node, with no fixture: both relations are built in. What it pins is that a
//! **computed** relation is a relation like any other — an alias, a qualifier, `WHERE`, `IN`,
//! `ORDER BY`, `DISTINCT`, `GROUP BY`, `count(*)` and both kinds of join all work over rows that
//! came from nowhere, because the last three units built them over rows and not over a scan.
//!
//! **41 of the 46 agreed on the first run.** The five that did not are the two decisions this unit
//! had to make, and both are declared below rather than worked around.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: `pg_type` and `pg_range` are built in.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why. Two decisions, and 28 statements of one fact.
///
/// * **`pg_type` lists the types this server has** — six, with PostgreSQL's own OIDs. A real
///   server has 669 in this database. The decision and its evidence are at the top of
///   `tests/corpus/pg19_pg_catalog.txt`; the short version is that it was taken from
///   `ActiveRecord`'s own source rather than from taste, and that a short map costs a client
///   nothing it can see because **this node never sends an OID that is not in it**.
/// * **Every write is `42501`**, DML included. A real server refuses the DDL exactly this way and
///   lets a *superuser* run the DML — which is how a capture probe took `int8` out of the
///   container and broke the database. There are no roles here and a computed relation has
///   nothing to write to, so all four verbs get the answer a real server gives everyone who is not
///   a superuser.
/// * And the `types` list, which is 28 statements of the same sentence: a real server's
///   `pg_type.oid` is an `oid`, `typname` is a `name`, `typdelim` and `typtype` are `"char"` and
///   `typinput` is a `regproc`. This node has none of those four types, so an `oid` is a `bigint`
///   and the other three are `text`. **Every value is identical** — the harness only reaches this
///   list when the rows already agree — and what differs is the OID in `RowDescription`.
///   `ActiveRecord` reads all five columns with `.to_i` or a string comparison, so nothing it does
///   can see it.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'int8'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'text'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'bool'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'bytea'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'float8'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'timestamptz'",
        "SELECT typname FROM pg_type WHERE typname IN ('int8', 'text', 'bool', 'bytea', 'float8', 'timestamptz') ORDER BY oid",
        "SELECT oid FROM pg_type WHERE typname IN ('int8', 'text', 'bool', 'bytea', 'float8', 'timestamptz') ORDER BY oid",
        "SELECT typname FROM pg_type WHERE typname = 'nosuchtype'",
        "SELECT t.oid FROM pg_type AS t WHERE t.typname = 'int8'",
        "SELECT t.typname FROM pg_type t WHERE t.oid = 20",
        "SELECT typname FROM pg_type WHERE oid = 20",
        "SELECT typtype FROM pg_type WHERE typname = 'int8'",
        "SELECT typdelim FROM pg_type WHERE typname = 'int8'",
        "SELECT typinput FROM pg_type WHERE typname = 'bool'",
        "SELECT typinput FROM pg_type WHERE typname = 'timestamptz'",
        "SELECT typelem, typbasetype FROM pg_type WHERE typname = 'text'",
        "SELECT oid FROM pg_type WHERE typname = 'int8' AND typtype = 'b'",
        "SELECT oid FROM pg_type WHERE typtype IN ('r', 'e', 'd') AND typname IN ('int8', 'text')",
        "SELECT oid FROM pg_type WHERE typelem IN (16, 17) AND typname IN ('int8', 'text')",
        "SELECT typname FROM pg_type WHERE typname IN ('int8', 'text') ORDER BY typname DESC",
        "SELECT DISTINCT typtype FROM pg_type WHERE typname IN ('int8', 'text', 'bool')",
        "SELECT typtype, count(*) FROM pg_type WHERE typname IN ('int8', 'text', 'bool') GROUP BY typtype",
        "SELECT rngsubtype FROM pg_range WHERE rngtypid = 20",
        "SELECT t.typname, r.rngsubtype FROM pg_type AS t LEFT JOIN pg_range AS r ON t.oid = r.rngtypid WHERE t.typname = 'int8'",
        "SELECT t.typname, r.rngsubtype FROM pg_type AS t LEFT JOIN pg_range AS r ON oid = rngtypid WHERE t.typname IN ('int8', 'text') ORDER BY t.oid",
        "SELECT t.typname FROM pg_type AS t JOIN pg_range AS r ON oid = rngtypid WHERE t.typname = 'int8'",
        "SELECT typname FROM pg_type WHERE typname = 'int8'",
    ],
    answers: &[
        (
            "SELECT typname FROM pg_type WHERE typname IN ('int8', 'numeric') ORDER BY typname",
            "**this server's `pg_type` lists this server's types.** A real server answers `int8` \
             and `numeric`; this one answers `int8`, because `numeric` is a type it refuses by \
             name. Listing PostgreSQL's standard OIDs instead would tell a client this node has \
             `numeric` and `int4`, which is a wrong answer rather than a short one. It closes one \
             type at a time as types arrive.",
        ),
        (
            "SELECT typname FROM pg_type WHERE typname = 'numeric'",
            "the same, on its own and in the shape a client actually asks it: one row there, no \
             rows here.",
        ),
        (
            "INSERT INTO pg_type (oid, typname) VALUES (99, 'nope')",
            "**every write to a catalog relation is `42501` here.** A real server refuses the \
             *DDL* exactly that way and permits this — it is a superuser session, and PostgreSQL \
             lets a superuser DML a system catalog; the row is refused only by a `NOT NULL` on a \
             column this node's `pg_type` does not have. There are no roles here and a computed \
             relation has nothing to write to, so the answer is the one a real server gives \
             everyone who is not a superuser. `docs/plans/phase-9-rails.md` §5.",
        ),
        (
            "UPDATE pg_type SET typname = 'nope' WHERE typname = 'int8'",
            "the same, and here a real server **succeeds**. This is not a hypothetical: the \
             capture that produced this corpus ran the `DELETE` below without a transaction the \
             first time, `int8` left the database, and every later statement answered `XX000 \
             cache lookup failed for type 20`. The three write probes are inside rolled-back \
             blocks for that reason.",
        ),
        (
            "DELETE FROM pg_type WHERE typname = 'int8'",
            "the same, and the one that did the damage.",
        ),
    ],
};

#[test]
fn every_catalog_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_pg_catalog.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 44,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The four statements this unit is for, from `ActiveRecord`'s own boot, answered.
///
/// They are in `tests/corpus/activerecord_8_1_statements.txt` and `tests/activerecord_surface.rs`
/// counts them; this asserts what they *say*, which a count cannot. Numbering is that file's.
#[test]
fn activerecord_s_four_type_map_queries_answer() {
    let mut node = parity::Node::new(&[]);

    // 4 — the first query `AbstractAdapter` ever sends. Four of its ten names are types this
    // node has, and the other six are the declared divergence.
    assert_eq!(
        node.rows(
            "SELECT t.oid, t.typname FROM pg_type as t WHERE t.typname IN ('int2', 'int4', \
             'int8', 'oid', 'float4', 'float8', 'numeric', 'bool', 'timestamp', 'timestamptz')"
        ),
        vec![
            vec!["16", "bool"],
            vec!["20", "int8"],
            // `int4` arrived with ADR 0033 and the catalog grew a row for it without being
            // touched, because `CatalogView::rows` is derived from `ColumnType::ALL`. Five of
            // this query's ten names are answered now where four were.
            vec!["23", "int4"],
            vec!["701", "float8"],
            vec!["1184", "timestamptz"],
        ]
    );

    // 7 — the `LEFT JOIN pg_range` one, with `ON oid = rngtypid` unqualified. All six types are
    // in its list of forty names, and none of them is a range, so every `rngsubtype` is NULL.
    let seven = node.rows(
        "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, t.typtype, \
         t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON oid = rngtypid WHERE \
         t.typname IN ('int2', 'int4', 'int8', 'oid', 'float4', 'float8', 'text', 'varchar', \
         'char', 'name', 'bpchar', 'bool', 'bit', 'varbit', 'date', 'money', 'bytea', 'point', \
         'hstore', 'json', 'jsonb', 'cidr', 'inet', 'uuid', 'xml', 'tsvector', 'macaddr', \
         'citext', 'ltree', 'line', 'lseg', 'box', 'path', 'polygon', 'circle', 'numeric', \
         'interval', 'time', 'timestamp', 'timestamptz')",
    );
    assert_eq!(
        seven,
        vec![
            vec!["16", "bool", "0", ",", "boolin", "\\N", "b", "0"],
            vec!["17", "bytea", "0", ",", "byteain", "\\N", "b", "0"],
            vec!["20", "int8", "0", ",", "int8in", "\\N", "b", "0"],
            vec!["23", "int4", "0", ",", "int4in", "\\N", "b", "0"],
            vec!["25", "text", "0", ",", "textin", "\\N", "b", "0"],
            vec!["701", "float8", "0", ",", "float8in", "\\N", "b", "0"],
            vec![
                "1184",
                "timestamptz",
                "0",
                ",",
                "timestamptz_in",
                "\\N",
                "b",
                "0"
            ],
        ]
    );

    // 8 — ranges, enums and domains. This node has none of the three, and neither the empty
    // answer nor an error is a guess: `TypeMapInitializer#run` partitions what it is given and
    // registers each partition, so an empty one registers nothing.
    assert!(
        node.rows(
            "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, \
             t.typtype, t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON oid = \
             rngtypid WHERE t.typtype IN ('r', 'e', 'd')"
        )
        .is_empty()
    );

    // 9 — array types, found by their element type. This node has no arrays, so every `typelem`
    // is 0 and nothing matches.
    assert!(
        node.rows(
            "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, \
             t.typtype, t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON oid = \
             rngtypid WHERE t.typelem IN (16, 17, 18, 19, 20, 21, 23, 25, 26, 114, 142, 600, \
             601, 602, 603, 604, 628, 700, 701, 718, 790, 829, 869, 650, 1042, 1043, 1082, 1083, \
             1114, 1184, 1186, 1560, 1562, 1700, 2950, 3614, 3802, 13356, 13359, 13361, 13367, \
             13369, 3904, 3906, 3908, 3910, 3912, 3926)"
        )
        .is_empty()
    );
}

/// A catalog relation is read-only, through every verb that could write one.
///
/// The corpus carries the three DML verbs; these are the DDL ones plus the two answers that are
/// **not** `42501`, which are the interesting half — a blanket refusal on the name would have got
/// both of them wrong.
#[test]
fn a_catalog_relation_is_read_only() {
    let mut node = parity::Node::new(&[]);

    for statement in [
        "DROP TABLE pg_type",
        // `IF EXISTS` does not excuse it. Measured: a real server answers `42501` for this too.
        "DROP TABLE IF EXISTS pg_type",
        "ALTER TABLE pg_type ADD COLUMN a bigint",
        "CREATE INDEX ix ON pg_type (oid)",
        "DROP TABLE pg_range",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "{statement}"
        );
        assert!(
            error.to_string().ends_with("is a system catalog"),
            "{statement}"
        );
    }

    // And the two that are a different question. `DROP INDEX` is asking about a *kind*, not about
    // a write, and a real server answers `42809 "pg_type" is not an index` — so the catalog
    // relation has to be visible to that check rather than refused before it.
    let error = node.run("DROP INDEX pg_type").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);

    // `CREATE TABLE pg_type` is `42P07`, which is what a real server answers for
    // `CREATE TABLE pg_catalog.pg_type`. The *unqualified* spelling succeeds there, because it
    // makes a `public.pg_type` that `pg_catalog.pg_type` still resolves ahead of; this node has
    // no schemas, so there is one name and it is taken. Not in the corpus: replaying it against a
    // real server leaves a table behind and the file would stop being idempotent.
    let error = node.run("CREATE TABLE pg_type (a bigint)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);
}

/// `SELECT *` expands to the view's columns, and hides none of them.
///
/// **This is not in the corpus, and it is the bug the corpus could not have caught.** A real
/// server's `pg_type` has some thirty columns and 669 rows, so `SELECT *` is not a line two
/// servers can agree on — and the first version of this unit answered it one column short. A
/// `TableDef` with no primary key means a *keyless table*, whose column 0 is an internal row id
/// that `SELECT *` hides; a computed relation has no key **and** no row id, because nothing stores
/// its rows. `pg_type.oid` is column 0, so the framework's own first question came back without
/// the answer in it.
#[test]
fn a_star_expands_to_every_column_of_the_view() {
    let mut node = parity::Node::new(&[]);

    assert_eq!(
        node.rows("SELECT * FROM pg_type WHERE typname = 'int8'"),
        vec![vec!["20", "int8", "0", ",", "int8in", "b", "0"]]
    );
    assert_eq!(
        node.rows("SELECT t.* FROM pg_type AS t WHERE t.oid = 20"),
        vec![vec!["20", "int8", "0", ",", "int8in", "b", "0"]]
    );
    assert_eq!(
        node.rows("SELECT * FROM pg_type").len(),
        esker_sql::value::ColumnType::ALL.len(),
        "one row per type this server has, and the catalog cannot fall behind the enum"
    );

    // `pg_range` is empty, so its columns can only be read off the description.
    match node.answer("SELECT * FROM pg_range") {
        parity::Answer::Rows { types, rows } => {
            assert!(rows.is_empty(), "pg_range has no rows here");
            assert_eq!(types.len(), 2, "rngtypid and rngsubtype, and no `oid`");
        }
        other => panic!("SELECT * FROM pg_range answered {other}"),
    }
}

/// `EXPLAIN` over a computed relation says what it is: one access path, and no cost.
#[test]
fn explain_names_the_catalog_scan() {
    let mut node = parity::Node::new(&[]);
    let plan: Vec<String> = node
        .rows("EXPLAIN SELECT typname FROM pg_type WHERE oid = 20")
        .into_iter()
        .map(|row| row.join(""))
        .collect();
    assert_eq!(
        plan,
        vec![
            "Project (1 columns)".to_owned(),
            "  Filter".to_owned(),
            "    Condition: (oid = 20)".to_owned(),
            "    Catalog Scan on pg_type".to_owned(),
        ]
    );
}
