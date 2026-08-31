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

// --- The four states, and the three anomalies they exist for -----------------------------------
//
// Each repro is written the way the anomaly actually happens: **two transactions at two snapshots**,
// one begun before the state moved and one after. No fake, no injected staleness — a writer's
// schema is the schema at its own `start_ts`, and that is exactly what makes it possible to be one
// step behind (`docs/plans/phase-6e.md` §1).
//
// Each is **mutation-checked**: `assert_anomaly_needs` re-runs the same interleaving with the
// guarding state skipped and requires it to break. A repro that passes either way is testing the
// code rather than the rule.

/// Moves an index one state on, in a transaction of its own — which is what the job will do.
fn advance(node: &mut Node, table: &str, to: esker_sql::catalog::SchemaState) {
    let mut txn = node.backend.begin().unwrap();
    let view = node.catalog.view_uncached(&*txn, TENANT).unwrap();
    let def = view.table(table).unwrap().unwrap();
    let index_id = def.indexes[0].id;
    esker_sql::catalog::advance_index_state(&mut *txn, TENANT, &def, index_id, to).unwrap();
    txn.commit().unwrap();
}

/// The index as the catalog now holds it.
fn index_state(node: &Node, table: &str) -> esker_sql::catalog::SchemaState {
    node.table(table).indexes[0].state
}

/// A table with an index wound back to `absent`, because `CREATE INDEX` is still the
/// one-transaction kind until unit 5. Setting a *starting condition*, not an interleaving, so it
/// writes the state directly rather than stepping through the states backwards.
fn staged(node: &mut Node) {
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    node.run("CREATE INDEX ti ON t (a)").unwrap();
    set_state(node, esker_sql::catalog::SchemaState::Absent);
    assert_eq!(
        index_state(node, "t"),
        esker_sql::catalog::SchemaState::Absent
    );
}

/// Writes an index's state directly, for a test setting up a starting condition.
fn set_state(node: &Node, state: esker_sql::catalog::SchemaState) {
    let mut txn = node.backend.begin().unwrap();
    let view = node.catalog.view_uncached(&*txn, TENANT).unwrap();
    let current = (*view.table("t").unwrap().unwrap()).clone();
    let mut updated = current.clone();
    updated.schema_version += 1;
    updated.indexes[0].state = state;
    updated.indexes[0].state_since = updated.schema_version;
    esker_sql::catalog::replace_table(&mut *txn, TENANT, &current, &updated).unwrap();
    txn.commit().unwrap();
}

/// How many entries the index holds right now.
fn index_entries(node: &Node, table: &str) -> usize {
    let def = node.table(table);
    let txn = node.backend.begin().unwrap();
    let (start, end) = esker_sql::row::index_range(TENANT, def.id, def.indexes[0].id);
    txn.scan(&start, &end, 0).unwrap().len()
}

/// A second session on the same store, so that two transactions can be open at two snapshots.
fn other_session(node: &Node) -> Node {
    Node {
        backend: Arc::clone(&node.backend),
        catalog: Arc::clone(&node.catalog),
        executor: Executor::new(
            Arc::clone(&node.backend) as Arc<dyn Backend>,
            Arc::clone(&node.catalog),
            TENANT,
        ),
    }
}

/// **Skip delete-only**, as a rule about `maintained()`: at delete-only a delete removes the
/// entry, at absent it leaves it — and an entry that outlives its row becomes a phantom the moment
/// the index goes public.
///
/// Staged by winding the catalog back rather than by an interleaving, because a node that is
/// behind is what this models and the executor cannot produce one — see
/// [`the_deleter_is_never_behind_the_inserter_of_a_row_it_can_see`], which is why. This is the
/// rule the **lease** protects: a node whose schema went stale independently of its snapshot is
/// exactly the failure ADR 0020 calls the hard part, and delete-only is what makes one step of it
/// harmless.
#[test]
fn delete_only_is_what_stops_an_entry_outliving_its_row() {
    for (state, expected, what) in [
        (
            esker_sql::catalog::SchemaState::DeleteOnly,
            0,
            "delete-only removes the entry",
        ),
        (
            esker_sql::catalog::SchemaState::Absent,
            1,
            "the anomaly: at absent the entry outlives its row",
        ),
    ] {
        let mut node = Node::new();
        staged(&mut node);

        // A row written while the index was maintained, so there is an entry to leave behind.
        advance(&mut node, "t", esker_sql::catalog::SchemaState::DeleteOnly);
        advance(&mut node, "t", esker_sql::catalog::SchemaState::WriteOnly);
        node.run("INSERT INTO t VALUES (1, 10)").unwrap();
        assert_eq!(index_entries(&node, "t"), 1);

        // Now the node is behind: its schema says `state` while the entry is already there.
        set_state(&node, state);
        node.run("DELETE FROM t WHERE id = 1").unwrap();

        assert_eq!(index_entries(&node, "t"), expected, "{what}");
    }
}

/// Why the interleaving above has to be staged: **through the executor, a deleter is never at an
/// earlier state than the inserter of a row it can see.**
///
/// A transaction's schema is the schema at its own snapshot (`docs/plans/phase-6e.md` §1), and
/// states only move forward. So a transaction that can see a row committed at `S_r` reads a state
/// at least `S_r` — and `written` implies `maintained`, so if the insert wrote an entry the delete
/// removes it. The anomaly needs a node whose schema is stale *independently of its snapshot*,
/// which is what the lease bounds and what nothing in this crate currently produces.
///
/// Asserting it is the point: the property is what makes ADR 0020's step arithmetic sufficient,
/// and a future edit that cached a `TableDef` across transactions would break it silently.
#[test]
fn the_deleter_is_never_behind_the_inserter_of_a_row_it_can_see() {
    let mut node = Node::new();
    staged(&mut node);
    advance(&mut node, "t", esker_sql::catalog::SchemaState::DeleteOnly);

    // A second session opens a transaction at delete-only, before the row exists.
    let mut behind = other_session(&node);
    behind.run("BEGIN").unwrap();
    behind.run("SELECT id FROM t").unwrap();

    advance(&mut node, "t", esker_sql::catalog::SchemaState::WriteOnly);
    node.run("INSERT INTO t VALUES (1, 10)").unwrap();
    assert_eq!(index_entries(&node, "t"), 1);

    // It cannot delete the row, because it cannot see it: its snapshot predates the insert. That
    // is the whole argument, and it is why the state it holds cannot matter.
    behind.run("DELETE FROM t WHERE id = 1").unwrap();
    behind.run("COMMIT").unwrap();
    assert_eq!(
        index_entries(&node, "t"),
        1,
        "the row is not visible to it, so neither is the question"
    );
    assert_eq!(
        node.rows("SELECT id FROM t"),
        [[Some("1".to_owned())]],
        "and the row is still there"
    );
}

/// **Skip write-only** (delete-only → public). A node behind inserts a row and writes no entry; a
/// node ahead answers a query from the index and does not find it. The row exists and the query
/// says it does not.
///
/// Same mutation, one state on: with write-only in the sequence the worst a node can be while
/// another is public is write-only, and a node at write-only writes entries.
#[test]
fn write_only_is_what_stops_a_row_being_invisible_to_the_index() {
    for skip_write_only in [false, true] {
        let mut node = Node::new();
        staged(&mut node);
        advance(&mut node, "t", esker_sql::catalog::SchemaState::DeleteOnly);

        if !skip_write_only {
            advance(&mut node, "t", esker_sql::catalog::SchemaState::WriteOnly);
        }
        let mut behind = other_session(&node);
        behind.run("BEGIN").unwrap();
        behind.run("SELECT id FROM t").unwrap();

        // Behind inserts at its own snapshot; ahead has moved on.
        behind.run("INSERT INTO t VALUES (1, 10)").unwrap();
        behind.run("COMMIT").unwrap();

        let entries = index_entries(&node, "t");
        if skip_write_only {
            assert_eq!(
                entries, 0,
                "the anomaly: a node at delete-only writes no entry, and public is next"
            );
        } else {
            assert_eq!(
                entries, 1,
                "write-only is what makes every node maintain the index before any node trusts it"
            );
        }
    }
}

/// **Skip the backfill** (write-only → public with the old rows unindexed). Every row written
/// before write-only is invisible to an index scan — the same wrong answer as above with a wider
/// blast radius.
///
/// Through a **unique** index, because that is the only kind this planner reads: rules 1 and 3 of
/// `crate::plan::query` choose a point read or a unique-index lookup, and a non-unique index has
/// no read path to be wrong through. So the anomaly is observable exactly where the planner can
/// reach it, which is also the only place it can hurt.
#[test]
fn the_backfill_is_what_makes_the_index_complete_before_it_is_trusted() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 10)").unwrap();
    node.run("CREATE UNIQUE INDEX ti ON t (a)").unwrap();

    // What `CREATE INDEX` does today: build inside the statement, then publish. The row that
    // predates it is in the index, and this is the assertion the staged version has to keep.
    assert_eq!(index_entries(&node, "t"), 1);
    assert_eq!(
        node.rows("SELECT id FROM t WHERE a = 10"),
        [[Some("1".to_owned())]]
    );

    // And the anomaly, staged by hand: an index published with the old rows unindexed answers a
    // query the table can answer with nothing at all.
    let def = node.table("t");
    let mut txn = node.backend.begin().unwrap();
    let (start, end) = esker_sql::row::index_range(TENANT, def.id, def.indexes[0].id);
    for (key, _) in txn.scan(&start, &end, 0).unwrap() {
        txn.delete(&key);
    }
    txn.commit().unwrap();

    assert!(
        node.rows("SELECT id FROM t WHERE a = 10").is_empty(),
        "the anomaly: a public index missing an old row answers a correct query with nothing"
    );
    // The row is still there; only the path through the index is wrong.
    assert_eq!(
        node.rows("SELECT id FROM t WHERE id = 1"),
        [[Some("1".to_owned())]]
    );
}

/// The planner refuses a non-public index at every state before public — the read half of the
/// rule, and the one that keeps the two anomalies above from ever reaching a user.
#[test]
fn a_non_public_index_is_never_chosen_by_the_planner() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8 UNIQUE)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 10)").unwrap();

    let plan_uses_index = |node: &mut Node| {
        node.rows("EXPLAIN SELECT id FROM t WHERE a = 10")
            .iter()
            .any(|row| row[0].as_deref().is_some_and(|line| line.contains("Index")))
    };
    assert!(plan_uses_index(&mut node), "public: the index is chosen");

    for state in [
        esker_sql::catalog::SchemaState::WriteOnly,
        esker_sql::catalog::SchemaState::DeleteOnly,
        esker_sql::catalog::SchemaState::Absent,
    ] {
        let mut txn = node.backend.begin().unwrap();
        let view = node.catalog.view_uncached(&*txn, TENANT).unwrap();
        let current = (*view.table("t").unwrap().unwrap()).clone();
        let mut updated = current.clone();
        updated.schema_version += 1;
        updated.indexes[0].state = state;
        updated.indexes[0].state_since = updated.schema_version;
        esker_sql::catalog::replace_table(&mut *txn, TENANT, &current, &updated).unwrap();
        txn.commit().unwrap();

        assert!(
            !plan_uses_index(&mut node),
            "{}: an index that is not public must not be read",
            state.name()
        );
        // And the query still answers, out of the table.
        assert_eq!(
            node.rows("SELECT id FROM t WHERE a = 10"),
            [[Some("1".to_owned())]],
            "{}: the answer is the same, the path is not",
            state.name()
        );
    }
}

/// Two states at once is the invariant; two states in one write is how it would be broken. The
/// primitive refuses it, so no caller can.
#[test]
fn an_index_cannot_skip_a_state() {
    let mut node = Node::new();
    staged(&mut node);
    let def = node.table("t");
    let mut txn = node.backend.begin().unwrap();
    let error = esker_sql::catalog::advance_index_state(
        &mut *txn,
        TENANT,
        &def,
        def.indexes[0].id,
        esker_sql::catalog::SchemaState::WriteOnly,
    )
    .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INTERNAL_ERROR);
    assert!(error.to_string().contains("in one step"), "{error}");
}

// --- The lease ---------------------------------------------------------------------------------
//
// ADR 0028. The lease is about **writers**: a node past it may be acting on a schema the cluster
// has moved two states beyond, which is the one thing the four states do not make safe. A reader's
// snapshot already agrees with the rows it can see, so gating reads would add stalls, close no
// hole, and turn a node that has lost PD from degraded into useless.

/// A backend whose schema lease a test can take away.
#[derive(Debug)]
struct Leased {
    inner: MemoryBackend,
    held: std::sync::atomic::AtomicBool,
}

impl Backend for Leased {
    fn begin(&self) -> esker_sql::Result<Box<dyn esker_sql::backend::Txn>> {
        self.inner.begin()
    }
    fn begin_at(&self, start_ts: u64) -> esker_sql::Result<Box<dyn esker_sql::backend::Txn>> {
        self.inner.begin_at(start_ts)
    }
    fn now(&self) -> esker_sql::Result<u64> {
        self.inner.now()
    }
    fn schema_lease_remaining(&self) -> Option<std::time::Duration> {
        self.held
            .load(std::sync::atomic::Ordering::Relaxed)
            .then_some(std::time::Duration::from_secs(5))
    }
}

/// A node whose lease can be taken away mid-session.
fn leased_node() -> (Arc<Leased>, Executor) {
    let backend = Arc::new(Leased {
        inner: MemoryBackend::new(),
        held: std::sync::atomic::AtomicBool::new(true),
    });
    let executor = Executor::new(
        Arc::clone(&backend) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        TENANT,
    );
    (backend, executor)
}

fn run_on(executor: &mut Executor, sql: &str) -> esker_sql::Result<Outcome> {
    let mut last = Outcome::done("");
    for parsed in parse_statements(sql)? {
        last = executor.execute(&parsed, &Params::NONE)?;
    }
    Ok(last)
}

/// A node past its lease refuses **writes** and still serves **reads** — both halves, because
/// either one alone would be the wrong design. Refusing neither ships the missing index entry the
/// whole ADR exists to prevent; refusing both adds stalls that protect nothing.
#[test]
fn a_node_past_its_lease_refuses_writes_and_still_reads() {
    let (backend, mut executor) = leased_node();
    run_on(
        &mut executor,
        "CREATE TABLE t (id int8 PRIMARY KEY, a int8)",
    )
    .unwrap();
    run_on(&mut executor, "INSERT INTO t VALUES (1, 10)").unwrap();

    // The lease runs out and cannot be renewed.
    backend
        .held
        .store(false, std::sync::atomic::Ordering::Relaxed);

    for (sql, command) in [
        ("INSERT INTO t VALUES (2, 20)", "INSERT"),
        ("UPDATE t SET a = 1 WHERE id = 1", "UPDATE"),
        ("DELETE FROM t WHERE id = 1", "DELETE"),
        ("CREATE TABLE u (a int8)", "CREATE TABLE"),
        ("CREATE INDEX ti ON t (a)", "CREATE INDEX"),
    ] {
        let error = run_on(&mut executor, sql).expect_err("a write past the lease must be refused");
        assert_eq!(
            error.sqlstate(),
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            "{sql}"
        );
        assert!(
            error.to_string().contains("schema lease has expired"),
            "{sql}: {error}"
        );
        assert!(
            error.to_string().contains(command),
            "{sql}: the message must name the command, got {error}"
        );
    }

    // And the read still answers, which is the other half of the rule.
    let Outcome::Rows { rows, .. } = run_on(&mut executor, "SELECT a FROM t WHERE id = 1").unwrap()
    else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"10".to_vec())]]);

    // Renewing puts writes back, with nothing else to do.
    backend
        .held
        .store(true, std::sync::atomic::Ordering::Relaxed);
    run_on(&mut executor, "INSERT INTO t VALUES (2, 20)").unwrap();
}

/// A node with **no lease source at all** writes freely: a cluster with no placement driver has no
/// staged schema change to be behind on. The distinction is between "nobody is coordinating" and
/// "I have lost the thing that coordinates", and only the second is a reason to stop.
#[test]
fn a_node_with_no_lease_source_is_not_treated_as_expired() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    node.run("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT id FROM t"), [[Some("1".to_owned())]]);
}

// --- The job: a staged CREATE INDEX ------------------------------------------------------------

/// Drives a job to completion, one step at a time, and says how many steps it took.
fn drive(node: &mut Node, index: &str) -> usize {
    for step in 1..500 {
        let said = node.rows(&format!("SELECT esker_schema_step('{index}')"))[0][0]
            .clone()
            .unwrap();
        if said == "public" {
            return step;
        }
    }
    panic!("the job did not finish");
}

/// `CREATE INDEX CONCURRENTLY` on a populated table: the states advance, the backfill runs in
/// batches, and the index is complete and readable at the end.
#[test]
fn a_concurrent_create_index_walks_the_states_and_ends_complete() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for id in 1..=5 {
        node.run(&format!("INSERT INTO t VALUES ({id}, {})", id * 10))
            .unwrap();
    }

    node.run("CREATE UNIQUE INDEX CONCURRENTLY ti ON t (a)")
        .unwrap();

    // The statement returns immediately, with the index at `absent` and nothing readable through
    // it: that is what CONCURRENTLY means.
    assert_eq!(
        index_state(&node, "t"),
        esker_sql::catalog::SchemaState::Absent
    );
    let jobs = node.rows("SELECT * FROM esker_schema_jobs()");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0][1], Some("ti".to_owned()));
    assert_eq!(jobs[0][2], Some("absent".to_owned()));

    // Step by step, in the ADR's order.
    for expected in ["delete-only", "write-only"] {
        let said = node.rows("SELECT esker_schema_step('ti')")[0][0]
            .clone()
            .unwrap();
        assert_eq!(said, expected);
        assert_eq!(index_state(&node, "t").name(), expected);
    }

    drive(&mut node, "ti");
    assert_eq!(
        index_state(&node, "t"),
        esker_sql::catalog::SchemaState::Public
    );
    assert_eq!(index_entries(&node, "t"), 5, "every row that predated it");
    assert!(
        node.rows("SELECT * FROM esker_schema_jobs()").is_empty(),
        "a finished job is forgotten"
    );

    // And the index answers, which is the point of building it.
    assert_eq!(
        node.rows("SELECT id FROM t WHERE a = 30"),
        [[Some("3".to_owned())]]
    );
}

/// A table larger than one batch is **completely** indexed — the assertion that the cursor is
/// doing its job rather than the first batch looking like the whole table.
#[test]
fn a_table_larger_than_one_batch_is_completely_indexed() {
    let rows = esker_sql::exec::BATCH_ROWS * 2 + 7;
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for id in 1..=rows {
        node.run(&format!("INSERT INTO t VALUES ({id}, {id})"))
            .unwrap();
    }

    node.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    let steps = drive(&mut node, "ti");
    assert!(steps > 3, "more than one backfill batch ran: {steps} steps");
    assert_eq!(index_entries(&node, "t"), rows);
}

/// **Resume, not restart.** A node that dies mid-backfill leaves a durable cursor, and the next
/// one picks up from it — on a table large enough to need a job, restarting is how a job never
/// finishes.
#[test]
fn a_backfill_resumes_from_its_cursor_rather_than_restarting() {
    let rows = esker_sql::exec::BATCH_ROWS + 5;
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for id in 1..=rows {
        node.run(&format!("INSERT INTO t VALUES ({id}, {id})"))
            .unwrap();
    }
    node.run("CREATE INDEX CONCURRENTLY ti ON t (a)").unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();

    let after_one_batch = index_entries(&node, "t");
    assert_eq!(after_one_batch, esker_sql::exec::BATCH_ROWS);
    let jobs = node.rows("SELECT * FROM esker_schema_jobs()");
    assert_eq!(jobs[0][2], Some("write-only".to_owned()));
    assert_ne!(
        jobs[0][3],
        Some("not started".to_owned()),
        "the cursor moved"
    );

    // The node dies. A **different** session, sharing only the store, finishes the job.
    let mut fresh = other_session(&node);
    drive(&mut fresh, "ti");
    assert_eq!(index_entries(&node, "t"), rows, "resumed, not restarted");
}

/// A `UNIQUE` backfill that meets a duplicate fails the **whole change** with `23505`, and the
/// states unwind so that nothing half-built is left behind. The one place a schema change fails on
/// *data* rather than on a conflict.
#[test]
fn a_unique_backfill_meeting_a_duplicate_fails_and_unwinds() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    node.run("INSERT INTO t VALUES (1, 10), (2, 10)").unwrap();

    node.run("CREATE UNIQUE INDEX CONCURRENTLY ti ON t (a)")
        .unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();

    let error = node.fails("SELECT esker_schema_step('ti')");
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert!(error.to_string().contains("ti"), "{error}");

    assert_eq!(
        index_state(&node, "t"),
        esker_sql::catalog::SchemaState::Absent,
        "the states unwind"
    );
    assert!(
        node.rows("SELECT * FROM esker_schema_jobs()").is_empty(),
        "and the job is forgotten"
    );
    // The table is untouched, which is what "fails the whole change" has to mean.
    assert_eq!(node.rows("SELECT id FROM t ORDER BY id").len(), 2);
}

/// Live traffic during the backfill: a row inserted at **write-only** is maintained by its own
/// writer, and the backfill writing the same entry is a no-op rather than a conflict.
#[test]
fn dml_during_the_backfill_converges_with_it() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (id int8 PRIMARY KEY, a int8)")
        .unwrap();
    for id in 1..=3 {
        node.run(&format!("INSERT INTO t VALUES ({id}, {id})"))
            .unwrap();
    }
    node.run("CREATE UNIQUE INDEX CONCURRENTLY ti ON t (a)")
        .unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();
    node.run("SELECT esker_schema_step('ti')").unwrap();
    assert_eq!(index_state(&node, "t").name(), "write-only");

    // Written while the index is write-only: its writer maintains it, so its entry exists before
    // the backfill ever reaches it.
    node.run("INSERT INTO t VALUES (4, 4)").unwrap();
    assert_eq!(index_entries(&node, "t"), 1, "the writer's own entry");

    drive(&mut node, "ti");
    assert_eq!(
        index_entries(&node, "t"),
        4,
        "one entry per row, no duplicates"
    );
    assert_eq!(
        node.rows("SELECT id FROM t WHERE a = 4"),
        [[Some("4".to_owned())]]
    );
}
