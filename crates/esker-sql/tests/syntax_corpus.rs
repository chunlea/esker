//! Contract C1's gate: every statement PostgreSQL 19 accepts must parse.
//!
//! The corpus in `corpus/pg19.sql` is not a list of SQL somebody remembered. Every statement in it
//! was run against a real **PostgreSQL 19beta1** server and kept only if that server's parser
//! accepted it — a statement is in the corpus when PostgreSQL answers with anything other than
//! `42601 syntax_error`, because "no such table" means the grammar was satisfied and only the
//! catalog was not. Two candidates were rejected that way while the corpus was being built (`WITH
//! TIES` without `ORDER BY`, and `EXCLUDE CURRENT ROW` without a frame clause), which is the
//! argument for having an oracle at all: both looked right.
//!
//! What the corpus asserts is narrow on purpose. It says the statement *parses*. Whether Esker
//! executes it is contract C2's business, and what it does when it executes is C3's.
//!
//! # The invariant this file exists to hold
//!
//! **No statement PostgreSQL 19 accepts is ever answered with a syntax error.** Not "few", not
//! "only obscure ones" — none, across all 428. There are exactly two acceptable answers:
//!
//! * the statement parses; or
//! * it comes back `0A000 feature_not_supported` **naming the construct**, which is what
//!   PostgreSQL itself would say about a feature it did not build.
//!
//! `42601 syntax_error` about valid PostgreSQL is the failure this file forbids, because it is both
//! untrue and unactionable: it tells a user to fix a statement that is already correct. Contract C1
//! is bounded by `sqlparser`'s coverage, but that bound is a reason to *classify* the shortfall,
//! not to mis-report it. `crate::parse`'s recognizer table is what does the classifying, and
//! [`KNOWN_GAPS`] records which statements it currently answers for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse;
use esker_sql::sqlstate;

/// The corpus, verified against PostgreSQL 19beta1. See the module docs for what "verified" means.
const CORPUS: &str = include_str!("corpus/pg19.sql");

/// Statements PostgreSQL 19 accepts that `sqlparser` 0.62.0 cannot parse.
///
/// These are no longer *failures* — every one of them now comes back as `0A000
/// feature_not_supported` naming the construct, which is the honest rejection contract C2 requires.
/// What the table records is which of the corpus is answered that way rather than executed, so that
/// the two directions stay visible: a statement that starts parsing must be delisted, and one that
/// stops parsing must be added. Each row is a feature in `docs/plans/phase-6a.md` §9.
const KNOWN_GAPS: &[(&str, &str)] = &[
    // G31 -- the column-option spelling of NULLS NOT DISTINCT; the table-constraint and
    // index spellings parse, which is what makes this one row rather than three.
    ("CREATE TABLE t (a int8 UNIQUE NULLS NOT DISTINCT);", "G31"),
    // G01 -- partition maintenance
    (
        "ALTER TABLE t ATTACH PARTITION p FOR VALUES FROM (1) TO (10);",
        "G01",
    ),
    ("ALTER TABLE t DETACH PARTITION p CONCURRENTLY;", "G01"),
    // G02 -- unlogged / logged tables
    ("CREATE UNLOGGED TABLE t (a int8);", "G02"),
    ("ALTER TABLE t SET LOGGED;", "G02"),
    // G03 -- CREATE TABLE LIKE / OF
    ("CREATE TABLE t (LIKE u INCLUDING ALL);", "G03"),
    ("CREATE TABLE t OF person_type;", "G03"),
    // G04 -- exclusion constraints
    (
        "CREATE TABLE t (a int8, EXCLUDE USING gist (a WITH =));",
        "G04",
    ),
    (
        "CREATE TABLE t (a int8, b daterange, EXCLUDE USING gist (a WITH =, b WITH &&));",
        "G04",
    ),
    // G05 -- index maintenance
    ("CREATE INDEX i ON ONLY t (a);", "G05"),
    ("REINDEX INDEX i;", "G05"),
    ("REINDEX TABLE CONCURRENTLY t;", "G05"),
    // G06 -- views: recursive, materialized
    ("CREATE RECURSIVE VIEW v (n) AS SELECT 1;", "G06"),
    (
        "CREATE MATERIALIZED VIEW mv AS SELECT 1 WITH NO DATA;",
        "G06",
    ),
    ("REFRESH MATERIALIZED VIEW CONCURRENTLY mv;", "G06"),
    // G07 -- sequence options
    ("CREATE SEQUENCE s START WITH 1 INCREMENT BY 1;", "G07"),
    ("ALTER SEQUENCE s RESTART WITH 1;", "G07"),
    // G08 -- routine bodies
    (
        "CREATE FUNCTION f() RETURNS int8 BEGIN ATOMIC SELECT 1; END;",
        "G08",
    ),
    (
        "CREATE PROCEDURE p(a int8) LANGUAGE sql AS $$ SELECT 1 $$;",
        "G08",
    ),
    ("DO $$ BEGIN NULL; END $$;", "G08"),
    ("DO LANGUAGE plpgsql $$ BEGIN NULL; END $$;", "G08"),
    (
        "CREATE AGGREGATE agg (int8) (SFUNC = int8pl, STYPE = int8);",
        "G08",
    ),
    // G09 -- INSERT OVERRIDING
    (
        "INSERT INTO t (a) OVERRIDING SYSTEM VALUE VALUES (1);",
        "G09",
    ),
    // G10 -- MERGE ... DO NOTHING
    (
        "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED AND u.a > 0 THEN DO NOTHING;",
        "G10",
    ),
    // G11 -- GROUP BY DISTINCT
    ("SELECT a FROM t GROUP BY DISTINCT a;", "G11"),
    // G12 -- row-level locking clauses
    ("SELECT a FROM t FOR NO KEY UPDATE OF t NOWAIT;", "G12"),
    ("SELECT a FROM t FOR KEY SHARE;", "G12"),
    // G14 -- SELECT with no list
    ("SELECT;", "G14"),
    // G15 -- JOIN USING alias
    ("SELECT * FROM t JOIN u USING (id) AS j;", "G15"),
    // G16 -- ROWS FROM
    (
        "SELECT * FROM ROWS FROM (generate_series(1, 2), generate_series(3, 4)) WITH ORDINALITY;",
        "G16",
    ),
    // G17 -- recursive CTE SEARCH/CYCLE
    (
        "WITH RECURSIVE w (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM w WHERE n < 5) SEARCH DEPTH FIRST BY n SET o SELECT * FROM w;",
        "G17",
    ),
    (
        "WITH RECURSIVE w (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM w WHERE n < 5) CYCLE n SET c USING p SELECT * FROM w;",
        "G17",
    ),
    // G18 -- window frame EXCLUDE
    (
        "SELECT sum(a) OVER (ORDER BY b GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t;",
        "G18",
    ),
    (
        "SELECT first_value(a) OVER (PARTITION BY b ORDER BY a ROWS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW) FROM t;",
        "G18",
    ),
    // G19 -- BETWEEN SYMMETRIC
    (
        "SELECT a BETWEEN 1 AND 10, a NOT BETWEEN SYMMETRIC 10 AND 1 FROM t;",
        "G19",
    ),
    // G20 -- TRIM keyword forms
    (
        "SELECT TRIM(BOTH ' ' FROM b), TRIM(LEADING FROM b), TRIM(TRAILING 'x' FROM b) FROM t;",
        "G20",
    ),
    // G21 -- JSON_QUERY wrapper
    (r#"SELECT JSON_QUERY('{"a":1}', '$' WITH WRAPPER);"#, "G21"),
    // G22 -- transaction modes
    (
        "BEGIN WORK ISOLATION LEVEL SERIALIZABLE READ WRITE DEFERRABLE;",
        "G22",
    ),
    ("SET CONSTRAINTS ALL DEFERRED;", "G22"),
    // G23 -- two-phase commit
    ("PREPARE TRANSACTION 'gid';", "G23"),
    ("COMMIT PREPARED 'gid';", "G23"),
    ("ROLLBACK PREPARED 'gid';", "G23"),
    // G24 -- role grants and privileges
    ("GRANT alice TO bob WITH ADMIN OPTION;", "G24"),
    (
        "REVOKE GRANT OPTION FOR SELECT ON t FROM alice CASCADE;",
        "G24",
    ),
    ("CREATE USER bob SUPERUSER CREATEDB CREATEROLE;", "G24"),
    (
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO alice;",
        "G24",
    ),
    (
        "CREATE USER MAPPING FOR alice SERVER srv OPTIONS (user 'x');",
        "G24",
    ),
    // G25 -- VACUUM / CLUSTER / CHECKPOINT
    ("VACUUM (FULL, ANALYZE, VERBOSE) t;", "G25"),
    ("VACUUM FREEZE ANALYZE t;", "G25"),
    ("CLUSTER t USING i;", "G25"),
    ("CHECKPOINT;", "G25"),
    // G26 -- cursor MOVE
    ("MOVE BACKWARD 1 IN c;", "G26"),
    // G27 -- database and system admin
    ("CREATE DATABASE d WITH OWNER alice ENCODING 'UTF8';", "G27"),
    ("DROP DATABASE IF EXISTS d WITH (FORCE);", "G27"),
    ("ALTER DATABASE d RENAME TO e;", "G27"),
    ("ALTER SYSTEM SET work_mem = '64MB';", "G27"),
    ("ALTER SYSTEM RESET ALL;", "G27"),
    ("REASSIGN OWNED BY alice TO bob;", "G27"),
    ("DROP OWNED BY alice CASCADE;", "G27"),
    ("SECURITY LABEL ON TABLE t IS 'label';", "G27"),
    // G28 -- extended statistics
    ("CREATE STATISTICS st ON a, b FROM t;", "G28"),
    ("DROP STATISTICS st;", "G28"),
    // G29 -- logical replication
    ("CREATE PUBLICATION pub FOR TABLE t;", "G29"),
    ("CREATE PUBLICATION pub FOR ALL TABLES;", "G29"),
    ("ALTER PUBLICATION pub ADD TABLE u;", "G29"),
    ("DROP PUBLICATION pub;", "G29"),
    (
        "CREATE SUBSCRIPTION sub CONNECTION 'host=x' PUBLICATION pub;",
        "G29",
    ),
    ("DROP SUBSCRIPTION IF EXISTS sub;", "G29"),
    // G32 -- the ALTER COLUMN actions beyond the four sqlparser reads
    ("ALTER TABLE t ALTER COLUMN a SET STATISTICS 100;", "G32"),
    ("ALTER TABLE t ALTER COLUMN a SET STORAGE PLAIN;", "G32"),
    ("ALTER TABLE t ALTER COLUMN a SET COMPRESSION lz4;", "G32"),
    ("ALTER TABLE t ALTER COLUMN a SET (n_distinct = 5);", "G32"),
    (
        "ALTER TABLE t ALTER COLUMN a SET (n_distinct_inherited = 5);",
        "G32",
    ),
    ("ALTER TABLE t ALTER COLUMN a RESET (n_distinct);", "G32"),
    (
        "ALTER TABLE t ALTER COLUMN a SET EXPRESSION AS (b * 2);",
        "G32",
    ),
    (
        "ALTER TABLE t ALTER COLUMN a SET GENERATED BY DEFAULT;",
        "G32",
    ),
    ("ALTER TABLE t ALTER COLUMN a DROP IDENTITY;", "G32"),
    (
        "ALTER TABLE t ALTER COLUMN a DROP IDENTITY IF EXISTS;",
        "G32",
    ),
    ("ALTER TABLE t ALTER COLUMN a DROP EXPRESSION;", "G32"),
    // G33 -- moving a table between schemas
    ("ALTER TABLE t SET SCHEMA s;", "G33"),
    // G34 -- tablespaces
    ("ALTER TABLE t SET TABLESPACE ts;", "G34"),
    ("ALTER TABLE ALL IN TABLESPACE a SET TABLESPACE b;", "G34"),
    // G35 -- table access methods
    ("ALTER TABLE t SET ACCESS METHOD heap;", "G35"),
    // G36 -- clustering, and the OID legacy
    ("ALTER TABLE t CLUSTER ON i;", "G36"),
    ("ALTER TABLE t SET WITHOUT CLUSTER;", "G36"),
    ("ALTER TABLE t SET WITHOUT OIDS;", "G36"),
    // G37 -- resetting storage parameters, and a multi-action list whose first action is itself a gap
    ("ALTER TABLE t RESET (fillfactor);", "G37"),
    ("ALTER TABLE t SET LOGGED, SET (fillfactor = 50);", "G37"),
    // G38 -- table inheritance
    ("ALTER TABLE t INHERIT u;", "G38"),
    ("ALTER TABLE t NO INHERIT u;", "G38"),
    // G39 -- a table of a composite type
    ("ALTER TABLE t OF sometype;", "G39"),
    ("ALTER TABLE t NOT OF;", "G39"),
    // G30 -- foreign data wrappers
    ("CREATE FOREIGN TABLE ft (a int8) SERVER srv;", "G30"),
    (
        "IMPORT FOREIGN SCHEMA remote LIMIT TO (t) FROM SERVER srv INTO local;",
        "G30",
    ),
];

/// One corpus entry: the class it was filed under and the statement itself.
struct Entry {
    class: &'static str,
    sql: &'static str,
}

fn corpus() -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut class = "unclassified";
    for line in CORPUS.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("@class") {
            class = rest.trim();
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            entries.push(Entry {
                class,
                sql: trimmed,
            });
        }
    }
    entries
}

fn is_known_gap(sql: &str) -> Option<&'static str> {
    KNOWN_GAPS
        .iter()
        .find(|(gap, _)| *gap == sql)
        .map(|(_, id)| *id)
}

/// The invariant. Every corpus statement either parses or is honestly refused; nothing PostgreSQL
/// 19 accepts may come back as a syntax error. Failures are collected before anything is asserted,
/// because the first is rarely the interesting one.
#[test]
fn no_statement_postgresql_accepts_is_ever_a_syntax_error() {
    let mut failures = Vec::new();
    for entry in corpus() {
        match parse(entry.sql) {
            Ok(_) => {}
            Err(error) if error.sqlstate() == sqlstate::FEATURE_NOT_SUPPORTED => {}
            Err(error) => failures.push(format!(
                "  [{}] {}\n      {} {error}",
                entry.class,
                entry.sql,
                error.sqlstate()
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} statements PostgreSQL 19 accepts came back as something other than a parse or an \
         honest 0A000.\nEvery one of these is telling a user their correct SQL is malformed. Add \
         a row to the UNSUPPORTED table in src/parse.rs naming the construct, and a line to \
         docs/plans/phase-6a.md §9.\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The refusal has to be *useful*, which means naming the construct. "not supported" on its own
/// tells a user nothing about what to change, so an empty or generic name is a failure here.
#[test]
fn every_refusal_names_the_feature_it_is_refusing() {
    for entry in corpus() {
        if let Err(error) = parse(entry.sql) {
            let message = error.to_string();
            assert!(
                message.ends_with(" is not supported"),
                "[{}] {} produced a message that does not name a feature: {message}",
                entry.class,
                entry.sql
            );
            let feature = message.trim_end_matches(" is not supported");
            assert!(
                feature.len() >= 2,
                "[{}] {} named its feature as {feature:?}",
                entry.class,
                entry.sql
            );
        }
    }
}

/// Both directions of drift. A listed gap that has started parsing means the dependency grew the
/// syntax and the register is stale; an unlisted statement that stopped parsing means we lost
/// ground without noticing. Either way the register in §9 is now wrong, which matters because it
/// is what tells a reader what Esker cannot yet be asked.
#[test]
fn the_register_still_describes_what_the_parser_does() {
    let mut newly_parsing = Vec::new();
    let mut newly_refused = Vec::new();
    for entry in corpus() {
        let listed = is_known_gap(entry.sql).is_some();
        let parses = parse(entry.sql).is_ok();
        if listed && parses {
            newly_parsing.push(format!("  {}", entry.sql));
        } else if !listed && !parses {
            newly_refused.push(format!("  {}", entry.sql));
        }
    }
    assert!(
        newly_parsing.is_empty(),
        "{} statements in KNOWN_GAPS now parse. Delete them here and close their rows in \
         docs/plans/phase-6a.md §9:\n{}",
        newly_parsing.len(),
        newly_parsing.join("\n")
    );
    assert!(
        newly_refused.is_empty(),
        "{} statements outside KNOWN_GAPS stopped parsing:\n{}",
        newly_refused.len(),
        newly_refused.join("\n")
    );
}

/// The two synonyms PostgreSQL documents and `sqlparser` does not know are rewritten rather than
/// refused, so they execute like the statements they are defined to be equal to.
#[test]
fn documented_synonyms_are_rewritten_not_refused() {
    let table = parse("TABLE t").expect("TABLE t is SELECT * FROM t");
    assert_eq!(table[0].to_string(), "SELECT * FROM t");

    let abort = parse("ABORT").expect("ABORT is ROLLBACK");
    assert_eq!(abort[0].to_string(), "ROLLBACK");

    // The rewrite is a leading-keyword substitution, so the rest of the statement survives it.
    let ordered = parse("TABLE t ORDER BY a LIMIT 1").expect("TABLE takes query clauses");
    assert_eq!(ordered[0].to_string(), "SELECT * FROM t ORDER BY a LIMIT 1");
}

/// The converse of the invariant, and the reason the recognizer is a table of constructs rather
/// than a blanket "anything that fails to parse is a missing feature". A typo is a syntax error,
/// and calling it `0A000` would be its own kind of lie.
#[test]
fn malformed_sql_is_still_a_syntax_error() {
    for sql in [
        "SELCT 1",
        "INSERT INTO",
        "SELECT * FROM t WHERE",
        "((((",
        "CREATE TABLE",
        "SELECT 1 +",
        "UPDATE SET",
        "SELECT * FROM t GROUP",
        "INSERT INTO t VALUES",
        "CREATE INDEX ON",
    ] {
        let error = parse(sql).expect_err("this is not valid PostgreSQL");
        assert_eq!(
            error.sqlstate(),
            sqlstate::SYNTAX_ERROR,
            "{sql} should be a syntax error, got {}",
            error.sqlstate()
        );
    }
}

/// A corpus that shrank, or that lost a statement class, would keep passing while testing less.
#[test]
fn the_corpus_covers_every_statement_class() {
    let entries = corpus();
    assert!(
        entries.len() >= 350,
        "the corpus has shrunk to {} statements",
        entries.len()
    );

    let classes: std::collections::BTreeSet<&str> = entries.iter().map(|e| e.class).collect();
    for required in [
        "ddl-table",
        "ddl-index",
        "ddl-other",
        "ddl-routine",
        "dml-insert",
        "dml-update-delete",
        "dml-merge",
        "query-select",
        "query-join",
        "query-cte",
        "query-setop",
        "query-window",
        "expression",
        "expression-json",
        "tcl",
        "dcl",
        "utility",
    ] {
        assert!(
            classes.contains(required),
            "the corpus no longer covers {required}"
        );
    }
    assert!(
        !entries.iter().any(|e| e.class == "unclassified"),
        "a statement was added above the first @class line"
    );
}
