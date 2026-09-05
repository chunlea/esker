//! What the lowering refuses, and what it must never quietly drop.
//!
//! `crate::parse`'s lowering turns the parser's tree into `crate::plan`, and the lowered types
//! hold far less than the tree does. Every field it does not read is a clause the user wrote, so
//! the rule is that an unhonoured clause is contract C2's `0A000` **naming the clause** — never a
//! statement executed without it.
//!
//! That is the whole point of this file. `CREATE TEMPORARY TABLE t (a int8)` executed as a
//! permanent table is a failure nothing reports, and the next session finds a table it did not
//! expect — which is why it was refused by name for eight phases and why it is *built* now
//! ([ADR 0054]), in a schema the next session cannot see. Each case below is a clause phase 6a
//! does not honour, put through the lowering, with the assertion that its own name comes back.
//!
//! [ADR 0054]: ../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::catalog::{ExprShape, KeyOrder};
use esker_sql::parse::parse_statements;
use esker_sql::plan::{IndexKeyPart, KeyPartName, Statement};
use esker_sql::sqlstate;
use esker_sql::value::ColumnType;

#[path = "parity_harness/mod.rs"]
mod parity;

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
        ("CREATE TABLE t AS SELECT 1", "CREATE TABLE ... AS"),
        // **`OR REPLACE` is a modifier, not the name of anything.** These were refused as
        // "CREATE OR is not supported" — a truncated token pair that names no feature and tells a
        // user nothing about which statement was declined, and the widening that fixed it is in
        // `crate::parse`. The list has no case left: `CREATE OR REPLACE FUNCTION` left it with the
        // define-only unit (it lowers, and the *language* decides — `LANGUAGE sql` is `42704` from
        // the executor), and **`CREATE OR REPLACE VIEW` left it with `tests/view.rs`**, where what
        // decides it is whether the name is free. Every other `OR REPLACE` spelling PostgreSQL has
        // is one `sqlparser` 0.62.0 cannot parse at all, so there is nothing here to assert.
        // **Nothing about a column `DEFAULT` is on this list any more.** It took an arbitrary
        // expression from the `DEFAULT`-is-an-expression unit, and the last thing it could not
        // evaluate — arithmetic — arrived on `main` in the same round. What a default still
        // refuses is PostgreSQL's own three shapes, and those have their own test rather than a
        // line here: they are not clauses this node declines to honour, they are clauses a real
        // server declines too.
        // The identity forms run, and so does the computed column that shares their grammar:
        // `GENERATED ALWAYS AS (expr) STORED` landed with statement 738. **`VIRTUAL` is not on
        // this list and is not implemented either** — `sqlparser` 0.62.0 cannot parse the word, so
        // it is a *syntax error* rather than a `0A000`, which is a contract **C1** gap and is in
        // the plan's register rather than here. The lowering refuses it by name for the day the
        // parser can read it.
        //
        // The sequence options after an identity are still named: a `START WITH` this node
        // ignored would hand out numbers nobody asked for.
        (
            "CREATE TABLE t (a int8 GENERATED ALWAYS AS IDENTITY (START WITH 100))",
            "a sequence option on an identity column",
        ),
        // `ADD COLUMN ... bigserial` left this list: it creates the sequence now, on an empty
        // table (`tests/add_column_primary_key.rs`). What stays refused is the same statement over
        // a table that has rows, which PostgreSQL fills from the sequence.
        ("CREATE TABLE t (a text COLLATE \"C\")", "COLLATE"),
        // **`PARTITION BY LIST` and `RANGE` left this list** with statements 781-786; `HASH` is
        // what is still refused, because nothing captured how it routes and a strategy this node
        // guessed at would put rows in the wrong partition.
        (
            "CREATE TABLE t (a int8) PARTITION BY HASH (a)",
            "PARTITION BY HASH",
        ),
        // **`INHERITS` left this list** with statement 762: the child takes the parent's
        // columns, a scan of the parent returns the child's rows, and an `UPDATE` or `DELETE` on
        // it reaches them. What is still refused is `FROM ONLY`, and that is a *parser* gap
        // rather than a clause this node declines — it is in the plan's C1 register.
        // Tier 1 is complete: every type it names runs, typmods included. What is left here is
        // tier 2, refused by name, and each line is deleted by the unit that lands its type.
        // A length unit is the standard's spelling and PostgreSQL takes neither — named rather
        // than dropped, since a dropped unit changes what a multi-byte value the column holds.
        (
            "CREATE TABLE t (a varchar(5 OCTETS))",
            "a length unit on varchar",
        ),
        // **A schema qualifier left this list** with the namespace unit: `s.t` is now a relation
        // in the schema `s`, and what refuses it is the *schema* not being there (`3F000`), which
        // is the executor's answer rather than a name the lowering declines.
        // `CASCADE` is built and `DROP ... PURGE` is Oracle's, which PostgreSQL does not take
        // either — so it is the one `DROP` clause left to name.
        ("DROP TABLE t PURGE", "DROP ... PURGE"),
        // The **simple** form of `CASE`. The searched form runs; this one prints back as
        // `CASE x WHEN 1 THEN …`, so desugaring it would store a definition nobody wrote.
        (
            "SELECT CASE a WHEN 1 THEN 'x' END FROM t",
            "CASE <expression> WHEN ..., the simple form",
        ),
        ("CREATE INDEX i ON t USING hash (a)", "an index USING"),
        // **`INCLUDE` left this list** with statement 787. What it still refuses is the payload
        // on an access method that cannot carry one, and that message is PostgreSQL's own rather
        // than a name of ours — it has its own test in `tests/include_index.rs`.
        // Refused by the parser's recognizer rather than the lowering: sqlparser 0.62.0 cannot
        // read this spelling, which is gap G31 in the plan's register. Either way the answer is
        // 0A000 naming it, which is all contract C2 asks.
        (
            "CREATE TABLE t (a int8 UNIQUE NULLS NOT DISTINCT)",
            "NULLS NOT DISTINCT",
        ),
        // **The table-constraint spelling left this list** with statement 779: it parses, it runs,
        // and its index carries the flag. The *column-option* spelling is still refused — by the
        // construct recognizer rather than here, because `sqlparser` 0.62.0 cannot parse it at
        // all, which makes it a C1 gap rather than a clause this node declines.
        // `EXPLAIN ANALYZE SELECT` is **executed** since ADR 0022 milestone 4 — it is how the
        // `ScanStats` a columnar answer carries reaches a user. What stays refused is the form
        // that would *write*: `ANALYZE` runs the statement, and an `EXPLAIN` that inserts a row
        // is a surprise a user cannot undo (`tests/routing.rs`,
        // `analyze_of_a_write_is_still_refused_by_name`).
        (
            "EXPLAIN ANALYZE INSERT INTO t VALUES (1)",
            "EXPLAIN ANALYZE",
        ),
        ("EXPLAIN ANALYZE DELETE FROM t", "EXPLAIN ANALYZE"),
        ("EXPLAIN (FORMAT JSON) SELECT 1", "EXPLAIN"),
        // Phase 9 unit 1 runs GROUP BY, HAVING, DISTINCT and the five aggregates. What sits next
        // to each of them does not, and each still names the clause rather than the expression it
        // happens to be spelled as -- `GROUP BY ROLLUP` and not "the expression ROLLUP (a)",
        // because the first is what a user searches the documentation for.
        ("SELECT a FROM t GROUP BY ROLLUP (a)", "GROUP BY ROLLUP"),
        ("SELECT a FROM t GROUP BY CUBE (a)", "GROUP BY CUBE"),
        (
            "SELECT a FROM t GROUP BY GROUPING SETS ((a), ())",
            "GROUP BY GROUPING SETS",
        ),
        ("SELECT DISTINCT ON (a) a FROM t", "SELECT DISTINCT ON"),
        ("SELECT count(*) OVER () FROM t", "a window function"),
        (
            "SELECT count(*) FILTER (WHERE a > 0) FROM t",
            "an aggregate FILTER clause",
        ),
        ("SELECT sum(*) FROM t", "sum(*)"),
    ];

    refuses_by_name(&cases);
}

/// A function name nobody declared is still `0A000` naming it — **one pass later**.
///
/// `soundex` used to be refused where the statement was lowered. It is now carried out as a call
/// the catalog might hold, because lowering cannot tell a name its vocabulary lacks from a user's
/// `CREATE FUNCTION` — so the refusal moved to `Executor`'s resolve pass, which still runs before
/// any row is touched, and the sentence is unchanged. This test moved with it rather than being
/// deleted: what contract C2 promises is the named refusal, not the pass that raises it.
#[test]
fn a_function_nobody_declared_is_still_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE t (a int8, b text)").unwrap();
    let error = node.run("SELECT soundex(b) FROM t").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(error.to_string(), "the function soundex is not supported");
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
fn every_unimplemented_alter_table_action_is_refused_by_name() {
    let cases = [
        // `ADD COLUMN … NOT NULL` was here. It is not a lowering question at all: whether it can
        // be done depends on whether the table has a row, which only the executor can see
        // (`tests/schema_change.rs`, `tests/add_column_not_null.rs`).
        // Still refused, and for the rewrite rather than for volatility: PostgreSQL gives every
        // row already stored its own value, and this `ALTER` is defined not to touch them.
        (
            "ALTER TABLE t ADD COLUMN c int8 DEFAULT random()",
            "ALTER TABLE ... ADD COLUMN ... DEFAULT random(), which would rewrite every row",
        ),
        (
            "ALTER TABLE t ADD COLUMN c int8 UNIQUE",
            "ADD COLUMN ... UNIQUE",
        ),
        // `ADD COLUMN ... PRIMARY KEY` left this list with the `bigserial` above it, and so did
        // `ADD CONSTRAINT ... PRIMARY KEY`.
        ("ALTER TABLE t ADD COLUMN c text COLLATE \"C\"", "COLLATE"),
        // `ALTER COLUMN a TYPE text` was here. It converts now, so what stays refused is the
        // `USING` that asks for a **computation** rather than a conversion — the boundary the unit
        // draws, and the one worth guarding (ADR 0031 rule 2).
        (
            "ALTER TABLE t ALTER COLUMN a TYPE text[] USING string_to_array(a, ',')",
            "ALTER TABLE ... ALTER COLUMN ... TYPE ... USING",
        ),
        // Still refused, and this is the one that keeps the `ADD CONSTRAINT` arm honest now that
        // `UNIQUE`, `FOREIGN KEY`, `CHECK` and `PRIMARY KEY` are all through it: a kind it cannot
        // build must still name itself rather than fall through.
        //
        // **It used to be the plain `PRIMARY KEY (a)`**, which this node builds now — so the guard
        // moved to `USING INDEX`, a real PostgreSQL statement that adopts an existing index as the
        // constraint's and that this node has no way to honour. A guard whose example became
        // supported is a guard that stopped guarding.
        (
            "ALTER TABLE t ADD CONSTRAINT c PRIMARY KEY USING INDEX i",
            "ALTER TABLE ... ADD CONSTRAINT",
        ),
        ("ALTER TABLE ONLY t ADD COLUMN c int8", "ALTER TABLE ONLY"),
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
    assert_eq!(
        create.keys,
        [IndexKeyPart::column("email"), IndexKeyPart::column("id")]
    );
    assert!(create.unique);

    let Statement::CreateIndex(unnamed) = lower("CREATE INDEX ON t (a)").unwrap() else {
        panic!("not a CREATE INDEX")
    };
    assert_eq!(unnamed.name, None, "PostgreSQL derives it");
}

/// An index key part is a **column** when it is a bare name however many parentheses are around
/// it, and an expression otherwise — with the shape PostgreSQL's deparser would give it.
///
/// The parentheses in `((lower(b)))` are the column list's and the expression's, and a real
/// server takes `(lower(b))` for the same index: measured, both print `lower(b)`.
#[test]
fn an_index_key_is_a_column_or_an_expression() {
    let keys = |sql: &str| {
        let Statement::CreateIndex(create) = lower(sql).unwrap() else {
            panic!("not a CREATE INDEX")
        };
        create.keys
    };
    assert_eq!(keys("CREATE INDEX ON t ((b))"), [IndexKeyPart::column("b")]);
    let expression = |expr: &str, shape| IndexKeyPart {
        part: KeyPartName::Expression {
            expr: expr.to_owned(),
            shape,
        },
        order: KeyOrder::ASCENDING,
        opclass: None,
    };
    assert_eq!(
        keys("CREATE INDEX ON t ((lower(b)))"),
        [expression("lower(b)", ExprShape::Call)]
    );
    assert_eq!(
        keys("CREATE INDEX ON t (lower(b))"),
        [expression("lower(b)", ExprShape::Call)]
    );
    assert_eq!(
        keys("CREATE INDEX ON t (a, (lower(b)))"),
        [
            IndexKeyPart::column("a"),
            expression("lower(b)", ExprShape::Call)
        ]
    );
    assert_eq!(
        keys("CREATE INDEX ON t ((b IS NULL))"),
        [expression("b IS NULL", ExprShape::Operator)]
    );
    assert_eq!(
        keys("CREATE INDEX ON t ((1))"),
        [expression("1", ExprShape::Value)]
    );
}

/// A key part's order is resolved against **its own direction's default**, not against a single
/// one: an unwritten `NULLS …` means LAST under `ASC` and FIRST under `DESC`.
#[test]
fn an_index_key_resolves_its_null_placement_from_its_direction() {
    let order = |sql: &str| {
        let Statement::CreateIndex(create) = lower(sql).unwrap() else {
            panic!("not a CREATE INDEX")
        };
        create.keys[0].order
    };
    assert_eq!(order("CREATE INDEX ON t (a)"), KeyOrder::ASCENDING);
    assert_eq!(order("CREATE INDEX ON t (a ASC)"), KeyOrder::ASCENDING);
    assert_eq!(order("CREATE INDEX ON t (a DESC)"), KeyOrder::of(true));
    assert!(order("CREATE INDEX ON t (a DESC)").nulls_first);
    assert!(!order("CREATE INDEX ON t (a DESC NULLS LAST)").nulls_first);
    assert!(order("CREATE INDEX ON t (a NULLS FIRST)").nulls_first);
    assert!(!order("CREATE INDEX ON t (a NULLS FIRST)").descending);
}

/// A stored predicate keeps **one** pair of parentheses however it was written, because that is
/// how `pg_get_indexdef` prints it back: `WHERE a > 1` and `WHERE (a > 1)` are the same index.
#[test]
fn an_index_predicate_is_stored_without_its_own_parentheses() {
    let predicate = |sql: &str| {
        let Statement::CreateIndex(create) = lower(sql).unwrap() else {
            panic!("not a CREATE INDEX")
        };
        create.predicate
    };
    assert_eq!(
        predicate("CREATE INDEX ON t (a) WHERE a > 1"),
        Some("a > 1".to_owned())
    );
    assert_eq!(
        predicate("CREATE INDEX ON t (a) WHERE ((a > 1))"),
        Some("a > 1".to_owned())
    );
}

/// `EXPLAIN` wraps the statement it is about; the statement inside is lowered like any other, so a
/// clause it cannot honour is refused before anything is planned.
#[test]
fn explain_wraps_a_lowered_statement() {
    let Statement::Explain(inner, false) = lower("EXPLAIN CREATE TABLE t (a int8)").unwrap() else {
        panic!("not an EXPLAIN")
    };
    assert!(matches!(*inner, Statement::CreateTable(_)));
    // **The refusal inside is still the answer**, which is what this half asserts: `EXPLAIN` does
    // not excuse a clause the lowering will not honour. `CREATE TEMPORARY TABLE` used to be the
    // case here and now lowers (ADR 0054), so the statement that carries the refusal is one that
    // still has one.
    assert_eq!(
        lower("EXPLAIN CREATE TABLE t AS SELECT 1")
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
        ..
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
        ..
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

/// **Contract C2 for `foreign_table_test.rb`'s four statements**, which run 53 found answering a
/// parser's expected-token list instead of naming what they asked for.
///
/// A client that asks for a feature is owed the name of what it asked for. `42601` with
/// `Expected: …, found: SERVER` says the SQL was malformed, and it was not — it is valid
/// PostgreSQL this node does not implement, which is `0A000` naming the construct. The file's
/// setup sends the first three and its teardown the fourth, so every test in it stops on the first
/// one either way; what changes is whether a reader can tell why.
///
/// The four arrive by **two different routes** and the test does not care which, because a client
/// does not: `CREATE SERVER` and `DROP SERVER` parse and are named by the lowering's fallthrough,
/// while `CREATE USER MAPPING` and `CREATE FOREIGN TABLE` are refused by `parse`'s table because
/// `sqlparser` cannot read them at all. Asserting the answer rather than the stage is what keeps
/// that free to change.
#[test]
fn the_foreign_data_statements_are_refused_by_name() {
    let cases = [
        (
            "CREATE SERVER foreign_server FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'x')",
            "CREATE SERVER",
        ),
        (
            "CREATE USER MAPPING FOR CURRENT_USER SERVER foreign_server",
            "CREATE USER MAPPING",
        ),
        (
            "CREATE FOREIGN TABLE foreign_professors (id int, name character varying NOT NULL) \
             SERVER foreign_server OPTIONS (table_name 'professors')",
            "CREATE FOREIGN TABLE",
        ),
        (
            "DROP SERVER IF EXISTS foreign_server CASCADE",
            "DROP SERVER",
        ),
    ];
    refuses_by_name(&cases);
}
