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
//! # Gaps
//!
//! Contract C1 is bounded by `sqlparser`'s own coverage, so its enforceable form is that no gap is
//! silent. A statement PostgreSQL accepts and `sqlparser` cannot parse goes in [`KNOWN_GAPS`] with
//! the register entry in `docs/plans/phase-6a.md` §9, and stays in the corpus. Two tests hold that
//! line from both sides: an unlisted failure fails the build, and so does a *listed* one that has
//! started passing — an upstream release that closes a gap must be noticed, not silently absorbed.

use esker_sql::parse::parse;

/// The corpus, verified against PostgreSQL 19beta1. See the module docs for what "verified" means.
const CORPUS: &str = include_str!("corpus/pg19.sql");

/// Statements PostgreSQL 19 accepts that `sqlparser` 0.62.0 cannot parse.
///
/// Each is a row in `docs/plans/phase-6a.md` §9. Removing one here without the parser actually
/// having gained the syntax makes `every_statement_postgresql_accepts_parses` fail; leaving one
/// here after the parser gains it makes `every_known_gap_is_still_a_gap` fail. Neither direction
/// is quiet.
const KNOWN_GAPS: &[(&str, &str)] = &[
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
    ("DROP INDEX CONCURRENTLY IF EXISTS i;", "G05"),
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
    // G13 -- TABLE as a query
    ("TABLE t;", "G13"),
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
    ("ABORT;", "G22"),
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

/// Contract C1. Every failure is collected before anything is asserted, because the first one is
/// rarely the interesting one — a parser missing a construct usually misses several.
#[test]
fn every_statement_postgresql_accepts_parses() {
    let mut failures = Vec::new();
    for entry in corpus() {
        if is_known_gap(entry.sql).is_some() {
            continue;
        }
        if let Err(error) = parse(entry.sql) {
            failures.push(format!("  [{}] {}\n      {error}", entry.class, entry.sql));
        }
    }
    assert!(
        failures.is_empty(),
        "{} statements that PostgreSQL 19 accepts did not parse.\n\
         Each is either a bug here or an upstream gap: if it is a gap, add it to KNOWN_GAPS and to \
         docs/plans/phase-6a.md §9. Never leave one unlisted -- an unlisted gap is contract C1 \
         broken silently, which is the one thing the contract forbids.\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The other direction. A gap that has started parsing means the dependency grew the syntax, and
/// the register is now wrong — which matters, because the register is what tells a reader what
/// Esker cannot yet be asked.
#[test]
fn every_known_gap_is_still_a_gap() {
    let mut fixed = Vec::new();
    for (sql, id) in KNOWN_GAPS {
        if parse(sql).is_ok() {
            fixed.push(format!("  {id}: {sql}"));
        }
    }
    assert!(
        fixed.is_empty(),
        "{} known gaps now parse. Delete them from KNOWN_GAPS and mark the rows closed in \
         docs/plans/phase-6a.md §9:\n{}",
        fixed.len(),
        fixed.join("\n")
    );
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
