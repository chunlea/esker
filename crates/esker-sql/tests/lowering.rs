//! What the lowering refuses, and what it must never quietly drop.
//!
//! `crate::parse`'s lowering turns the parser's tree into `crate::plan`, and the lowered types
//! hold far less than the tree does. Every field it does not read is a clause the user wrote, so
//! the rule is that an unhonoured clause is contract C2's `0A000` **naming the clause** — never a
//! statement executed without it.
//!
//! That is the whole point of this file: `CREATE TEMPORARY TABLE t (a int8)` executed as a
//! permanent table is a failure nothing reports, and the next session finds a table it did not
//! expect. Each case below is a clause phase 6a does not honour, put through the lowering, with
//! the assertion that its own name comes back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::plan::Statement;
use esker_sql::sqlstate;
use esker_sql::value::ColumnType;

/// Parse and lower. A construct can be refused at either step — some are already named by the
/// parser's own recognizer (`docs/plans/phase-6a.md` §9) — and contract C2 does not care which,
/// only that the answer is `0A000` naming the construct.
fn lower(sql: &str) -> esker_sql::Result<Statement> {
    parse_statements(sql)?[0].lower()
}

/// Every clause here parses — contract C1 — and every one of them changes what the statement
/// means, so every one of them has to be refused by name.
#[test]
fn a_clause_we_do_not_honour_is_refused_by_name() {
    let cases = [
        (
            "CREATE TEMPORARY TABLE t (a int8)",
            "CREATE TEMPORARY TABLE",
        ),
        ("CREATE TABLE t AS SELECT 1", "CREATE TABLE ... AS"),
        ("CREATE UNLOGGED TABLE t (a int8)", "UNLOGGED"),
        ("CREATE TABLE t (a int8 DEFAULT 1)", "DEFAULT"),
        ("CREATE TABLE t (a int8 REFERENCES u (b))", "REFERENCES"),
        ("CREATE TABLE t (a int8 CHECK (a > 0))", "CHECK"),
        (
            "CREATE TABLE t (a int8 GENERATED ALWAYS AS IDENTITY)",
            "GENERATED",
        ),
        ("CREATE TABLE t (a text COLLATE \"C\")", "COLLATE"),
        (
            "CREATE TABLE t (a int8, FOREIGN KEY (a) REFERENCES u (b))",
            "FOREIGN KEY",
        ),
        ("CREATE TABLE t (a int8, CHECK (a > 0))", "CHECK"),
        (
            "CREATE TABLE t (a int8) PARTITION BY RANGE (a)",
            "PARTITION BY",
        ),
        ("CREATE TABLE t (a int8) INHERITS (u)", "INHERITS"),
        ("CREATE TABLE t (a int4)", "the type INT"),
        ("CREATE TABLE t (a varchar(10))", "the type VARCHAR"),
        ("CREATE TABLE t (a numeric)", "the type NUMERIC"),
        ("CREATE TABLE t (a timestamp)", "the type TIMESTAMP"),
        ("CREATE TABLE t (a int8[])", "the type"),
        ("CREATE TABLE s.t (a int8)", "the qualified name"),
        ("DROP TABLE t CASCADE", "DROP ... CASCADE"),
        (
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "CREATE INDEX CONCURRENTLY",
        ),
        ("CREATE INDEX i ON t USING hash (a)", "an index USING"),
        ("CREATE INDEX i ON t (a) WHERE a > 0", "a partial index"),
        ("CREATE INDEX i ON t (lower(a))", "the index expression"),
        ("CREATE INDEX i ON t (a DESC)", "a DESC index column"),
        (
            "CREATE INDEX i ON t (a) INCLUDE (b)",
            "CREATE INDEX ... INCLUDE",
        ),
        // Refused by the parser's recognizer rather than the lowering: sqlparser 0.62.0 cannot
        // read this spelling, which is gap G31 in the plan's register. Either way the answer is
        // 0A000 naming it, which is all contract C2 asks.
        (
            "CREATE TABLE t (a int8 UNIQUE NULLS NOT DISTINCT)",
            "NULLS NOT DISTINCT",
        ),
        (
            "CREATE TABLE t (a int8, UNIQUE NULLS NOT DISTINCT (a))",
            "NULLS [NOT] DISTINCT",
        ),
        ("EXPLAIN ANALYZE SELECT 1", "EXPLAIN ANALYZE"),
        ("EXPLAIN (FORMAT JSON) SELECT 1", "EXPLAIN"),
    ];

    refuses_by_name(&cases);
}

/// Contract C2 for one statement: it must not lower, the answer must be `0A000`, and the message
/// must name the clause rather than say "not supported" and leave the user guessing.
fn refuses_by_name(cases: &[(&str, &str)]) {
    for &(sql, expected) in cases {
        let error = match lower(sql) {
            Err(error) => error,
            Ok(lowered) => panic!("{sql} lowered to {lowered:?}, dropping `{expected}` silently"),
        };
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{sql} -> `{error}`, which is not contract C2's answer"
        );
        assert!(
            error.to_string().contains(expected),
            "{sql} -> `{error}`, which does not name `{expected}`"
        );
    }
}

/// `ALTER TABLE` on its own, because `ADD COLUMN` of a nullable column is the *only* part of that
/// statement this crate runs and everything around it has to be refused by name. The clauses that
/// would need a row rewritten are the ones worth reading twice: each is refused because the
/// `ALTER` is meant to touch no row at all.
#[test]
fn every_alter_table_action_but_add_column_is_refused_by_name() {
    let cases = [
        (
            "ALTER TABLE t ADD COLUMN c int8 NOT NULL",
            "ADD COLUMN ... NOT NULL",
        ),
        (
            "ALTER TABLE t ADD COLUMN c int8 DEFAULT 0",
            "ADD COLUMN ... DEFAULT",
        ),
        (
            "ALTER TABLE t ADD COLUMN c int8 UNIQUE",
            "ADD COLUMN ... UNIQUE",
        ),
        (
            "ALTER TABLE t ADD COLUMN c int8 PRIMARY KEY",
            "ADD COLUMN ... PRIMARY KEY",
        ),
        ("ALTER TABLE t ADD COLUMN c text COLLATE \"C\"", "COLLATE"),
        ("ALTER TABLE t DROP COLUMN a", "ALTER TABLE ... DROP COLUMN"),
        (
            "ALTER TABLE t RENAME COLUMN a TO b",
            "ALTER TABLE ... RENAME COLUMN",
        ),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE text",
            "ALTER TABLE ... ALTER COLUMN",
        ),
        ("ALTER TABLE t RENAME TO u", "ALTER TABLE ... RENAME TO"),
        (
            "ALTER TABLE t ADD CONSTRAINT c UNIQUE (a)",
            "ALTER TABLE ... ADD CONSTRAINT",
        ),
        (
            "ALTER TABLE t DROP CONSTRAINT c",
            "ALTER TABLE ... DROP CONSTRAINT",
        ),
        ("ALTER TABLE ONLY t ADD COLUMN c int8", "ALTER TABLE ONLY"),
        ("ALTER TABLE t ADD COLUMN c int8[]", "the type"),
        ("ALTER TABLE t OWNER TO someone", "ALTER TABLE ..."),
    ];
    refuses_by_name(&cases);
}

/// The statements phase 6a does execute, lowered. Names are folded, the six types are recognised
/// under every spelling, and a primary key column is found whether it was declared inline or as a
/// table constraint.
#[test]
fn the_ddl_we_execute_lowers_to_what_it_says() {
    let Statement::CreateTable(create) = lower(
        "CREATE TABLE IF NOT EXISTS Accounts (
             Id int8 PRIMARY KEY,
             Email text NOT NULL UNIQUE,
             Balance double precision,
             Active bool,
             Blob bytea,
             Seen timestamptz
         )",
    )
    .unwrap() else {
        panic!("not a CREATE TABLE")
    };

    assert_eq!(create.name, "accounts", "an unquoted name folds");
    assert!(create.if_not_exists);
    assert_eq!(
        create.columns.iter().map(|c| c.ty).collect::<Vec<_>>(),
        [
            ColumnType::Int8,
            ColumnType::Text,
            ColumnType::Double,
            ColumnType::Bool,
            ColumnType::Bytea,
            ColumnType::TimestampTz,
        ]
    );
    assert_eq!(create.columns[0].name, "id");
    assert!(create.columns[1].not_null);
    assert!(!create.columns[2].not_null);
    assert_eq!(create.primary_key, ["id"]);
    assert_eq!(create.unique.len(), 1);
    assert_eq!(create.unique[0].columns, ["email"]);
    assert_eq!(create.unique[0].name, None, "PostgreSQL derives it");
}

/// Every spelling of the six types, because a client may write any of them.
#[test]
fn the_six_types_are_recognised_under_every_spelling() {
    let sql = "CREATE TABLE t (a bigint, b int8, c text, d bool, e boolean, f bytea,
                                g float8, h double precision,
                                i timestamptz, j timestamp with time zone)";
    let Statement::CreateTable(create) = lower(sql).unwrap() else {
        panic!("not a CREATE TABLE")
    };
    assert_eq!(
        create.columns.iter().map(|c| c.ty).collect::<Vec<_>>(),
        [
            ColumnType::Int8,
            ColumnType::Int8,
            ColumnType::Text,
            ColumnType::Bool,
            ColumnType::Bool,
            ColumnType::Bytea,
            ColumnType::Double,
            ColumnType::Double,
            ColumnType::TimestampTz,
            ColumnType::TimestampTz,
        ]
    );
}

/// A composite primary key, and a quoted name that keeps its case.
#[test]
fn a_composite_primary_key_keeps_its_column_order() {
    let Statement::CreateTable(create) =
        lower("CREATE TABLE \"Mixed\" (a int8, b text, PRIMARY KEY (b, a))").unwrap()
    else {
        panic!("not a CREATE TABLE")
    };
    assert_eq!(create.name, "Mixed", "a quoted name does not fold");
    assert_eq!(
        create.primary_key,
        ["b", "a"],
        "key order, not column order"
    );
}

#[test]
fn drop_and_create_index_lower_to_their_lists() {
    let Statement::DropTable(drop) = lower("DROP TABLE IF EXISTS A, B").unwrap() else {
        panic!("not a DROP TABLE")
    };
    assert_eq!(drop.names, ["a", "b"]);
    assert!(drop.if_exists);

    let Statement::CreateIndex(create) =
        lower("CREATE UNIQUE INDEX By_Email ON Accounts (Email, Id)").unwrap()
    else {
        panic!("not a CREATE INDEX")
    };
    assert_eq!(create.name.as_deref(), Some("by_email"));
    assert_eq!(create.table, "accounts");
    assert_eq!(create.columns, ["email", "id"]);
    assert!(create.unique);

    let Statement::CreateIndex(unnamed) = lower("CREATE INDEX ON t (a)").unwrap() else {
        panic!("not a CREATE INDEX")
    };
    assert_eq!(unnamed.name, None, "PostgreSQL derives it");
}

/// `EXPLAIN` wraps the statement it is about; the statement inside is lowered like any other, so a
/// clause it cannot honour is refused before anything is planned.
#[test]
fn explain_wraps_a_lowered_statement() {
    let Statement::Explain(inner) = lower("EXPLAIN CREATE TABLE t (a int8)").unwrap() else {
        panic!("not an EXPLAIN")
    };
    assert!(matches!(*inner, Statement::CreateTable(_)));
    assert_eq!(
        lower("EXPLAIN CREATE TEMPORARY TABLE t (a int8)")
            .unwrap_err()
            .sqlstate(),
        sqlstate::FEATURE_NOT_SUPPORTED
    );
}

/// `ALTER TABLE`, lowered: several actions in one statement stay one statement, and both the
/// statement's `IF EXISTS` and each action's `IF NOT EXISTS` are carried rather than dropped.
#[test]
fn alter_table_lowers_its_actions_in_order() {
    let Statement::AlterTable(alter) = lower(
        "ALTER TABLE IF EXISTS Accounts
             ADD COLUMN Note text,
             ADD COLUMN IF NOT EXISTS Score int8",
    )
    .unwrap() else {
        panic!("not an ALTER TABLE");
    };
    assert_eq!(alter.name, "accounts", "the name is folded");
    assert!(alter.if_exists);
    assert_eq!(alter.actions.len(), 2);

    let esker_sql::plan::AlterTableAction::AddColumn {
        column,
        if_not_exists,
    } = &alter.actions[0]
    else {
        panic!("not an ADD COLUMN");
    };
    assert_eq!(column.name, "note");
    assert_eq!(column.ty, ColumnType::Text);
    assert!(!column.not_null, "a column added by ALTER is nullable");
    assert!(!if_not_exists);

    let esker_sql::plan::AlterTableAction::AddColumn {
        column,
        if_not_exists,
    } = &alter.actions[1]
    else {
        panic!("not an ADD COLUMN");
    };
    assert_eq!(column.name, "score");
    assert_eq!(column.ty, ColumnType::Int8);
    assert!(if_not_exists);

    // `ADD c text` without the COLUMN keyword is the same statement; PostgreSQL takes both.
    let Statement::AlterTable(alter) = lower("ALTER TABLE t ADD c text").unwrap() else {
        panic!("not an ALTER TABLE");
    };
    assert_eq!(alter.actions.len(), 1);
}

/// The fallback naming for an `ALTER TABLE` action nobody wrote a case for. It has to be a name a
/// user can act on and must not echo an identifier back at them, which is why it takes the
/// leading keywords rather than the action's own rendering — `ALTER TABLE t OWNER TO alice` must
/// not come back as "OWNER TO alice is not supported", which puts a name the user is asking about
/// into the message as though it were the problem.
///
/// The last of the three is answered by the *recognizer* rather than the lowering, because
/// `sqlparser` cannot read it (§9, G33). Both paths produce the same sentence, which is the point
/// of naming them the same way: a user cannot tell, and should not have to, whether the gap is
/// upstream or ours.
#[test]
fn an_unnamed_alter_action_still_gets_a_name_and_no_identifiers() {
    for (sql, expected) in [
        ("ALTER TABLE t OWNER TO someone", "ALTER TABLE ... OWNER TO"),
        (
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE ... ENABLE ROW LEVEL",
        ),
        ("ALTER TABLE t SET SCHEMA s", "ALTER TABLE ... SET SCHEMA"),
    ] {
        let error = lower(sql).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(error.to_string(), format!("{expected} is not supported"));
    }
}
