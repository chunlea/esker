//! Online schema change (ADR 0020, `docs/plans/phase-6e.md`), end to end.
//!
//! Unit 1 is here: `ADD COLUMN ... DEFAULT <constant>` the PostgreSQL 11 way — the value is stored
//! on the column as its **missing value** and the decoder pads with it, so a populated table is
//! altered without a row being rewritten. ADR 0019's pad rule generalised: that rule pads with
//! NULL, and NULL is what a column with no default still pads with.
//!
//! # The two fields really do diverge, and PostgreSQL is why
//!
//! A column carries a *default* (what an `INSERT` that omits it writes) and a *missing value* (what
//! a row too narrow to hold it reads as). They start equal and PostgreSQL lets them drift: measured
//! on 19beta1, `ALTER COLUMN c SET DEFAULT 'new'` leaves `attmissingval` at `old`, so rows that
//! predate the column keep reading `old` while new rows get `new`. One field for both would rewrite
//! history the first time somebody changed a default, which is why there are two.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;

const TENANT: u64 = 1;

struct Node {
    backend: Arc<MemoryBackend>,
    catalog: Arc<Catalog>,
    executor: Executor,
}

impl Node {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new());
        let catalog = Arc::new(Catalog::new());
        let executor = Executor::new(
            Arc::clone(&backend) as Arc<dyn Backend>,
            Arc::clone(&catalog),
            TENANT,
        );
        Node {
            backend,
            catalog,
            executor,
        }
    }

    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = match parsed.class() {
                StatementClass::Begin => {
                    self.executor.begin(parsed.begins_read_only())?;
                    Outcome::done("BEGIN")
                }
                StatementClass::Commit => {
                    self.executor.commit()?;
                    Outcome::done("COMMIT")
                }
                StatementClass::Rollback => {
                    self.executor.rollback()?;
                    Outcome::done("ROLLBACK")
                }
                _ => self.executor.execute(&parsed, &Params::NONE)?,
            };
        }
        Ok(last)
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.map(|bytes| String::from_utf8(bytes).unwrap()))
                        .collect()
                })
                .collect(),
            other @ Outcome::Done { .. } => panic!("expected rows, got {other:?}"),
        }
    }

    fn fails(&mut self, sql: &str) -> esker_sql::SqlError {
        self.run(sql).expect_err("expected a refusal")
    }

    /// The table as the catalog holds it, read in a fresh transaction.
    fn table(&self, name: &str) -> esker_sql::catalog::TableDef {
        let txn = self.backend.begin().unwrap();
        let view = self.catalog.view(&*txn, TENANT).unwrap();
        (*view.table(name).unwrap().unwrap()).clone()
    }
}

/// A table with rows that predate the column about to be added — the whole point of the unit.
fn a_populated_table(node: &mut Node) {
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    node.run("INSERT INTO t VALUES (1), (2)").unwrap();
}

// --- The missing value ------------------------------------------------------------------------

/// The measured PostgreSQL behaviour, end to end: rows written before the column read as the
/// default, and **no row was rewritten** to make that true.
#[test]
fn add_column_with_a_constant_default_is_read_by_rows_that_predate_it() {
    let mut node = Node::new();
    a_populated_table(&mut node);

    let before: Vec<_> = {
        let txn = node.backend.begin().unwrap();
        let (start, end) = esker_sql::row::table_row_range(TENANT, node.table("t").id);
        txn.scan(&start, &end, 0).unwrap()
    };

    node.run("ALTER TABLE t ADD COLUMN c text DEFAULT 'old'")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id, c FROM t ORDER BY id"),
        [
            [Some("1".to_owned()), Some("old".to_owned())],
            [Some("2".to_owned()), Some("old".to_owned())],
        ]
    );

    let after: Vec<_> = {
        let txn = node.backend.begin().unwrap();
        let (start, end) = esker_sql::row::table_row_range(TENANT, node.table("t").id);
        txn.scan(&start, &end, 0).unwrap()
    };
    assert_eq!(
        before, after,
        "not one row byte moved: the value lives in the catalog, not in the rows"
    );
}

/// The default and the missing value are set together and are **not the same field**. This is the
/// half a test can check without `SET DEFAULT`, which is still `0A000`: the catalog holds both.
#[test]
fn add_column_sets_both_the_default_and_the_missing_value() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    node.run("ALTER TABLE t ADD COLUMN c text DEFAULT 'old'")
        .unwrap();

    let table = node.table("t");
    let column = table.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(column.default, Some(esker_sql::Datum::Text("old".into())));
    assert_eq!(column.missing, Some(esker_sql::Datum::Text("old".into())));
}

/// A row written **after** the column exists is written at full width, so nothing about it is
/// missing — it takes the *default*, which is the other field.
#[test]
fn a_row_written_after_the_column_takes_the_default_not_the_pad() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    node.run("ALTER TABLE t ADD COLUMN c text DEFAULT 'old'")
        .unwrap();
    node.run("INSERT INTO t (id) VALUES (3)").unwrap();
    node.run("INSERT INTO t (id, c) VALUES (4, 'given')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id, c FROM t ORDER BY id"),
        [
            [Some("1".to_owned()), Some("old".to_owned())],
            [Some("2".to_owned()), Some("old".to_owned())],
            [Some("3".to_owned()), Some("old".to_owned())],
            [Some("4".to_owned()), Some("given".to_owned())],
        ]
    );
}

/// `CREATE TABLE` sets a default and **no** missing value: nothing predates a column the table was
/// created with, so there is no narrower row for a pad to answer for.
#[test]
fn create_table_sets_a_default_and_no_missing_value() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, c text DEFAULT 'new', n int8 DEFAULT 7)")
        .unwrap();
    node.run("INSERT INTO t (id) VALUES (1)").unwrap();

    assert_eq!(
        node.rows("SELECT id, c, n FROM t"),
        [[
            Some("1".to_owned()),
            Some("new".to_owned()),
            Some("7".to_owned())
        ]]
    );

    let table = node.table("t");
    let column = table.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(column.default, Some(esker_sql::Datum::Text("new".into())));
    assert_eq!(column.missing, None, "nothing can predate it");
}

/// `NOT NULL` with a constant default is **instant** — PostgreSQL does this too, measured: `ADD
/// COLUMN n int8 NOT NULL DEFAULT 7` sets `attmissingval = {7}` and rewrites nothing. This closes
/// half of a divergence `docs/plans/phase-6a.md` §10a records.
#[test]
fn not_null_with_a_constant_default_is_instant() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    node.run("ALTER TABLE t ADD COLUMN n int8 NOT NULL DEFAULT 7")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id, n FROM t ORDER BY id"),
        [
            [Some("1".to_owned()), Some("7".to_owned())],
            [Some("2".to_owned()), Some("7".to_owned())],
        ]
    );
    let table = node.table("t");
    assert!(
        table
            .columns
            .iter()
            .find(|c| c.name == "n")
            .unwrap()
            .not_null
    );

    // And it is a real constraint, not a label: a NULL written into it is still refused.
    assert_eq!(
        node.fails("INSERT INTO t (id, n) VALUES (3, NULL)")
            .sqlstate(),
        sqlstate::NOT_NULL_VIOLATION
    );
}

/// Bare `NOT NULL` stays refused, and the refusal names **the pair** rather than the keyword —
/// because `NOT NULL DEFAULT 7` is accepted, and a message about `NOT NULL` alone would send a
/// user to the wrong half of their statement.
#[test]
fn not_null_without_a_default_is_still_refused_and_names_the_pair() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    let error = node.fails("ALTER TABLE t ADD COLUMN n int8 NOT NULL");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "ALTER TABLE ... ADD COLUMN ... NOT NULL without a DEFAULT is not supported"
    );
}

/// A volatile default cannot be one value in the catalog — PostgreSQL rewrites the table for it,
/// measured (`atthasmissing` comes back false). This node has no rewrite until the job exists, so
/// it refuses by name rather than storing one row's answer for every row.
#[test]
fn a_volatile_default_is_refused_by_name() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    let error = node.fails("ALTER TABLE t ADD COLUMN r int8 DEFAULT random()");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "DEFAULT random(), which may be volatile is not supported"
    );
}

/// PostgreSQL folds `(1+1)` to `2` before storing it. There is no folder here, and naming that is
/// the honest answer — a folder is a feature, not an oversight to paper over.
#[test]
fn an_unfolded_expression_default_is_refused_by_name() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    let error = node.fails("ALTER TABLE t ADD COLUMN e int8 DEFAULT (1+1)");
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("is not a constant"), "{error}");
}

/// A default is read as the column's own type, by the same conversion an `INSERT` does — so a bad
/// one is the same error in the same place.
#[test]
fn a_default_of_the_wrong_type_is_the_same_error_a_value_would_be() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    let error = node.fails("ALTER TABLE t ADD COLUMN n int8 DEFAULT 'x'");
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
}

/// `DEFAULT NULL` is the same thing as no default, which is what PostgreSQL makes of it too. Kept
/// as a case because a format that stored it as a *present* NULL would read differently from a
/// column that never had one.
#[test]
fn default_null_is_the_same_as_no_default() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    node.run("ALTER TABLE t ADD COLUMN c text DEFAULT NULL")
        .unwrap();

    let table = node.table("t");
    let column = table.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(column.default, None);
    assert_eq!(column.missing, None);
    assert_eq!(
        node.rows("SELECT c FROM t ORDER BY id"),
        [[None::<String>], [None]]
    );
}

/// Successive `ADD COLUMN`s each with their own default, over rows of three different widths. The
/// pad is per column, so a row written between two of them reads the first column's value and the
/// second's default.
#[test]
fn each_column_pads_with_its_own_value_across_successive_alters() {
    let mut node = Node::new();
    a_populated_table(&mut node);
    node.run("ALTER TABLE t ADD COLUMN a text DEFAULT 'A'")
        .unwrap();
    node.run("INSERT INTO t (id) VALUES (3)").unwrap();
    node.run("ALTER TABLE t ADD COLUMN b text DEFAULT 'B'")
        .unwrap();
    node.run("INSERT INTO t (id) VALUES (4)").unwrap();
    node.run("ALTER TABLE t ADD COLUMN c text").unwrap();

    assert_eq!(
        node.rows("SELECT id, a, b, c FROM t ORDER BY id"),
        [
            // Written at width 1: both defaults are pads, and `c` has none so it is NULL.
            [
                Some("1".to_owned()),
                Some("A".to_owned()),
                Some("B".to_owned()),
                None
            ],
            [
                Some("2".to_owned()),
                Some("A".to_owned()),
                Some("B".to_owned()),
                None
            ],
            // Written at width 2: `a` is a stored value, `b` is a pad.
            [
                Some("3".to_owned()),
                Some("A".to_owned()),
                Some("B".to_owned()),
                None
            ],
            // Written at width 3: both are stored values.
            [
                Some("4".to_owned()),
                Some("A".to_owned()),
                Some("B".to_owned()),
                None
            ],
        ]
    );
}

/// A default is visible through an **index** as well as through a scan, because both decode the
/// row the same way. Worth its own case: the index path builds its own plan node, and a node built
/// with the types but not the pads would answer NULL here.
#[test]
fn a_padded_column_reads_the_same_through_an_index() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, k int8 UNIQUE)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 10)").unwrap();
    node.run("ALTER TABLE t ADD COLUMN c text DEFAULT 'old'")
        .unwrap();

    // A point read through the primary key, and a lookup through the unique index.
    assert_eq!(
        node.rows("SELECT c FROM t WHERE id = 1"),
        [[Some("old".to_owned())]]
    );
    assert_eq!(
        node.rows("SELECT c FROM t WHERE k = 10"),
        [[Some("old".to_owned())]],
        "the index path decodes rows too"
    );
}
