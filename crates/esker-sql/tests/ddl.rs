//! DDL, end to end: a statement string in, a catalog change out.
//!
//! These drive the real [`Executor`] over the in-memory transactional store, which is the same
//! path a client's bytes take once the session has decoded them — parse, lower, plan, run. What
//! they check is the part a unit test of any one layer cannot: that the whole chain agrees, and
//! that the answers match what a real PostgreSQL 19 gives for the same statements.
//!
//! Every error message and SQLSTATE asserted here was captured from that server, and four of them
//! are shapes a reading would have collapsed: `DROP TABLE` on a missing table says `table "x" does
//! not exist` where a query says `relation "x" does not exist`; `DROP INDEX` on a missing index is
//! `42704`; and either one naming the wrong *kind* of object is `42809 "x" is not a table`, not a
//! "does not exist" at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::sqlstate;
use esker_sql::value::ColumnType;

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
            1,
        );
        Node {
            backend,
            catalog,
            executor,
        }
    }

    /// Runs every statement in the string, stopping at the first failure, as a session would.
    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    }

    fn notices(&mut self) -> Vec<String> {
        self.executor
            .take_notices()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A second session on the same node: its own executor, the same store and the same catalog
    /// cache. That shared cache is the thing worth testing across two of them.
    fn session(&self) -> Executor {
        Executor::new(
            Arc::clone(&self.backend) as Arc<dyn Backend>,
            Arc::clone(&self.catalog),
            1,
        )
    }

    /// The table as the catalog holds it, read in a fresh transaction.
    fn table(&self, name: &str) -> Option<esker_sql::catalog::TableDef> {
        let txn = self.backend.begin().unwrap();
        let view = self.catalog.view(&*txn, 1).unwrap();
        view.table(name).unwrap().map(|table| (*table).clone())
    }
}

#[test]
fn create_table_puts_the_columns_the_key_and_the_unique_indexes_in_the_catalog() {
    let mut node = Node::new();
    let outcome = node
        .run("CREATE TABLE Accounts (id int8 PRIMARY KEY, email text NOT NULL UNIQUE, note text)")
        .unwrap();
    assert_eq!(outcome, Outcome::done("CREATE TABLE"));

    let table = node.table("accounts").expect("committed");
    assert_eq!(table.name, "accounts");
    assert_eq!(
        table.columns.iter().map(|c| c.ty).collect::<Vec<_>>(),
        [ColumnType::Int8, ColumnType::Text, ColumnType::Text]
    );
    assert_eq!(table.primary_key, [0]);
    assert!(table.columns[0].not_null, "a key column is NOT NULL");
    assert!(table.columns[1].not_null);
    assert!(!table.columns[2].not_null);

    assert_eq!(table.indexes.len(), 1, "the UNIQUE constraint is an index");
    assert_eq!(table.indexes[0].name, "accounts_email_key");
    assert!(table.indexes[0].unique);
    assert_eq!(table.indexes[0].columns, [1]);
}

/// The row key *is* the primary key, so a table without one has nowhere to live. PostgreSQL allows
/// it, so contract C2 says the answer is `0A000` naming it -- not a syntax error and not a table
/// that quietly cannot be written to.
#[test]
fn a_table_without_a_primary_key_is_refused_by_name() {
    let mut node = Node::new();
    let error = node.run("CREATE TABLE t (a int8)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(error.to_string().contains("PRIMARY KEY"), "{error}");
}

#[test]
fn a_duplicate_table_is_42p07_and_if_not_exists_is_a_notice() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();

    let error = node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);
    assert_eq!(error.to_string(), "relation \"t\" already exists");

    node.run("CREATE TABLE IF NOT EXISTS t (a int8 PRIMARY KEY)")
        .unwrap();
    assert_eq!(node.notices(), ["relation \"t\" already exists, skipping"]);
}

#[test]
fn two_columns_of_one_name_are_42701() {
    let mut node = Node::new();
    let error = node
        .run("CREATE TABLE t (a int8 PRIMARY KEY, a text)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_COLUMN);
}

#[test]
fn a_key_naming_a_column_that_is_not_there_is_42703() {
    let mut node = Node::new();
    let error = node
        .run("CREATE TABLE t (a int8, PRIMARY KEY (b))")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
}

/// PostgreSQL words this one `table "x" does not exist`, where a query says `relation`. Captured.
#[test]
fn dropping_what_is_not_there_is_an_error_and_if_exists_is_a_notice() {
    let mut node = Node::new();

    let error = node.run("DROP TABLE nope").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "table \"nope\" does not exist");

    node.run("DROP TABLE IF EXISTS nope").unwrap();
    assert_eq!(node.notices(), ["table \"nope\" does not exist, skipping"]);

    let error = node.run("DROP INDEX nope").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_OBJECT);
    assert_eq!(error.to_string(), "index \"nope\" does not exist");

    node.run("DROP INDEX IF EXISTS nope").unwrap();
    assert_eq!(node.notices(), ["index \"nope\" does not exist, skipping"]);
}

/// A name that is there and is the wrong kind of thing is `42809`, and `IF EXISTS` does not excuse
/// it -- the object exists, it is just not what the statement can act on.
#[test]
fn dropping_the_wrong_kind_of_object_is_42809_even_with_if_exists() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, b text UNIQUE)")
        .unwrap();

    let error = node.run("DROP TABLE IF EXISTS t_b_key").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);
    assert_eq!(error.to_string(), "\"t_b_key\" is not a table");

    let error = node.run("DROP INDEX IF EXISTS t").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);
    assert_eq!(error.to_string(), "\"t\" is not an index");
}

#[test]
fn dropping_a_table_takes_its_name_and_its_indexes_names_with_it() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, b text UNIQUE)")
        .unwrap();
    node.run("DROP TABLE t").unwrap();
    assert!(node.table("t").is_none());
    // The index's name is free again, which is only true if dropping the table released it.
    node.run("CREATE TABLE t_b_key (a int8 PRIMARY KEY)")
        .unwrap();
}

#[test]
fn create_index_adds_to_the_table_and_drop_index_removes_it() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, b text)")
        .unwrap();

    node.run("CREATE INDEX ON t (b)").unwrap();
    let table = node.table("t").unwrap();
    assert_eq!(table.indexes.len(), 1);
    assert_eq!(
        table.indexes[0].name, "t_b_idx",
        "PostgreSQL's derived name"
    );
    assert!(!table.indexes[0].unique);

    node.run("CREATE UNIQUE INDEX by_b ON t (b, a)").unwrap();
    let table = node.table("t").unwrap();
    assert_eq!(table.indexes.len(), 2);
    assert_eq!(
        table.indexes[1].columns,
        [1, 0],
        "index order, not column order"
    );
    assert!(table.indexes[1].unique);

    node.run("DROP INDEX t_b_idx").unwrap();
    let table = node.table("t").unwrap();
    assert_eq!(table.indexes.len(), 1);
    assert_eq!(table.indexes[0].name, "by_b");
}

#[test]
fn an_index_on_a_column_that_is_not_there_is_42703() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();
    let error = node.run("CREATE INDEX ON t (nope)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);

    let error = node.run("CREATE INDEX ON nope (a)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "relation \"nope\" does not exist");
}

/// DDL is transactional. A `CREATE TABLE` inside a block that rolls back leaves nothing, which is
/// PostgreSQL's behaviour and not every database's.
#[test]
fn ddl_rolls_back_with_its_transaction() {
    let mut node = Node::new();
    node.executor.begin().unwrap();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();
    node.executor.rollback().unwrap();
    assert!(node.table("t").is_none(), "the CREATE went with the block");

    node.executor.begin().unwrap();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();
    node.executor.commit().unwrap();
    assert!(node.table("t").is_some());
}

/// A statement outside a block gets its own transaction, and a failure in it leaves nothing
/// behind -- including the ids it allocated on the way.
#[test]
fn a_failed_statement_outside_a_block_leaves_nothing() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, PRIMARY KEY (nope))")
        .unwrap_err();
    assert!(node.table("t").is_none());

    // The next successful statement still works, so the failure did not poison the executor.
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();
    assert!(node.table("t").is_some());
}

#[test]
fn explain_describes_a_statement_without_running_it() {
    let mut node = Node::new();
    let outcome = node
        .run("EXPLAIN CREATE TABLE t (a int8 PRIMARY KEY)")
        .unwrap();
    let Outcome::Rows { fields, rows, tag } = outcome else {
        panic!("EXPLAIN returns rows")
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "QUERY PLAN");
    assert_eq!(fields[0].type_oid, ColumnType::Text.oid());
    assert_eq!(tag, "EXPLAIN");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some(&b"Create Table on t"[..]));
    assert!(node.table("t").is_none(), "EXPLAIN ran nothing");
}

/// `CREATE INDEX ON t (a)` twice gives two indexes, not an error: PostgreSQL disambiguates a name
/// it derived itself. Measured -- three of them produced `t_a_idx`, `t_a_idx1` and `t_a_idx2`.
#[test]
fn a_derived_index_name_is_disambiguated_and_a_given_one_is_not() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, b text)")
        .unwrap();
    node.run("CREATE INDEX ON t (b)").unwrap();
    node.run("CREATE INDEX ON t (b)").unwrap();
    node.run("CREATE INDEX ON t (b)").unwrap();
    assert_eq!(
        node.table("t")
            .unwrap()
            .indexes
            .iter()
            .map(|index| index.name.clone())
            .collect::<Vec<_>>(),
        ["t_b_idx", "t_b_idx1", "t_b_idx2"]
    );

    node.run("CREATE INDEX mine ON t (b)").unwrap();
    let error = node.run("CREATE INDEX mine ON t (b)").unwrap_err();
    assert_eq!(
        error.sqlstate(),
        sqlstate::DUPLICATE_TABLE,
        "a name the user chose is theirs, and a collision is an error"
    );
}

/// The primary key's constraint name is a relation name, reserved even though there is no index
/// behind it -- the row key *is* the primary key. PostgreSQL reserves it too.
#[test]
fn the_primary_key_name_is_taken_and_can_be_given() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY)").unwrap();
    assert_eq!(node.table("t").unwrap().primary_key_name, "t_pkey");

    let error = node.run("CREATE INDEX t_pkey ON t (a)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);

    // Dropping it is refused the way PostgreSQL refuses it: the constraint needs it.
    let error = node.run("DROP INDEX t_pkey").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop index t_pkey because constraint t_pkey on table t requires it"
    );

    let mut named = Node::new();
    named
        .run("CREATE TABLE u (a int8, CONSTRAINT my_pk PRIMARY KEY (a))")
        .unwrap();
    assert_eq!(
        named.table("u").unwrap().primary_key_name,
        "my_pk",
        "a constraint the user named keeps its name"
    );
}

/// PostgreSQL words a missing column in a key clause differently from a missing column anywhere
/// else, and the three extra words are what point a user at the constraint.
#[test]
fn a_key_naming_a_missing_column_says_named_in_key() {
    let mut node = Node::new();
    let error = node
        .run("CREATE TABLE t (a int8, PRIMARY KEY (b))")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "column \"b\" named in key does not exist"
    );

    let error = node
        .run("CREATE TABLE t (a int8 PRIMARY KEY, UNIQUE (b))")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "column \"b\" named in key does not exist"
    );
}

/// The `HINT` field is part of what a user reads. PostgreSQL answers "what should I do instead?"
/// in the same message, and both hints here were captured from it.
#[test]
fn the_wrong_object_type_carries_postgresqls_own_hint() {
    let mut node = Node::new();
    node.run("CREATE TABLE t (a int8 PRIMARY KEY, b text UNIQUE)")
        .unwrap();
    assert_eq!(
        node.run("DROP TABLE t_b_key").unwrap_err().hint(),
        Some("Use DROP INDEX to remove an index.")
    );
    assert_eq!(
        node.run("DROP INDEX t").unwrap_err().hint(),
        Some("Use DROP TABLE to remove a table.")
    );
}

/// A DDL statement that rolls back must leave nothing of itself on the node.
///
/// Two sessions share one catalog cache, and catalog versions are reused after a rollback -- the
/// bump is `read + 1` inside the transaction, so the abandoned number is the very next one a
/// committing DDL takes. Before the executor started reading through the cache for a transaction
/// that has written the catalog, the second session's `SELECT` here answered from a table the
/// first session had rolled back.
#[test]
fn a_rolled_back_ddl_is_invisible_to_the_next_session() {
    let mut node = Node::new();
    let mut other = node.session();

    // `BEGIN`/`ROLLBACK` are the session's, not the executor's, so a test that drives the
    // executor directly opens the block through the same trait the session uses.
    node.executor.begin().unwrap();
    node.run("CREATE TABLE ghost (id int8 PRIMARY KEY)")
        .unwrap();
    // The writer sees its own DDL, which is what puts the definition in reach of the cache.
    node.run("SELECT id FROM ghost").unwrap();
    node.executor.rollback().unwrap();

    // A different DDL commits, and lands on the catalog version the rolled-back one abandoned.
    for parsed in parse_statements("CREATE TABLE real (id int8 PRIMARY KEY)").unwrap() {
        other.execute(&parsed, &Params::NONE).unwrap();
    }

    let error = node.run("SELECT id FROM ghost").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    let error = {
        let parsed = parse_statements("SELECT id FROM ghost").unwrap();
        other.execute(&parsed[0], &Params::NONE).unwrap_err()
    };
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert!(
        node.table("real").is_some(),
        "the one that committed is there"
    );
}

/// `ALTER TABLE ADD COLUMN`: the catalog gains the column, the table's schema version moves, and
/// no row is touched. Every message and code here was captured from a real PostgreSQL 19beta1.
#[test]
fn add_column_appends_to_the_catalog_and_bumps_the_schema_version() {
    let mut node = Node::new();
    node.run("CREATE TABLE a (id int8 PRIMARY KEY, name text)")
        .unwrap();
    let before = node.table("a").expect("committed");
    assert_eq!(before.schema_version, 1, "CREATE TABLE leaves it at one");

    let outcome = node.run("ALTER TABLE a ADD COLUMN note text").unwrap();
    assert_eq!(outcome, Outcome::done("ALTER TABLE"));

    let after = node.table("a").expect("committed");
    assert_eq!(after.schema_version, 2);
    assert_eq!(
        after
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.ty, column.not_null))
            .collect::<Vec<_>>(),
        [
            ("id", ColumnType::Int8, true),
            ("name", ColumnType::Text, false),
            // Nullable, always: every row already stored is missing it.
            ("note", ColumnType::Text, false),
        ]
    );
    assert_eq!(after.id, before.id, "the same table, not a new one");
    assert_eq!(after.indexes, before.indexes);

    // Several actions in one statement are one shape change and one version bump.
    node.run("ALTER TABLE a ADD COLUMN m1 text, ADD COLUMN m2 bool")
        .unwrap();
    let after = node.table("a").expect("committed");
    assert_eq!(after.schema_version, 3);
    assert_eq!(after.columns.len(), 5);
}

/// The refusals, with the codes and sentences a real server gives.
#[test]
fn add_column_refuses_what_postgresql_refuses() {
    let mut node = Node::new();
    node.run("CREATE TABLE a (id int8 PRIMARY KEY, name text)")
        .unwrap();

    let error = node.run("ALTER TABLE a ADD COLUMN name text").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_COLUMN);
    assert_eq!(
        error.to_string(),
        "column \"name\" of relation \"a\" already exists"
    );

    // `IF NOT EXISTS` is a notice -- and PostgreSQL keeps `42701` on it rather than dropping to
    // `00000` the way its `DROP ... IF EXISTS` notice does. Captured, not assumed.
    node.run("ALTER TABLE a ADD COLUMN IF NOT EXISTS name text")
        .unwrap();
    assert_eq!(
        node.notices(),
        ["column \"name\" of relation \"a\" already exists, skipping"]
    );

    let error = node.run("ALTER TABLE nope ADD COLUMN c text").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "relation \"nope\" does not exist");

    node.run("ALTER TABLE IF EXISTS nope ADD COLUMN c text")
        .unwrap();
    assert_eq!(
        node.notices(),
        ["relation \"nope\" does not exist, skipping"]
    );

    // An index is a relation, so PostgreSQL does not say "is not a table" here.
    let error = node
        .run("ALTER TABLE a_pkey ADD COLUMN c text")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);
    assert_eq!(
        error.to_string(),
        "ALTER action ADD COLUMN cannot be performed on relation \"a_pkey\""
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("This operation is not supported for indexes.")
    );

    // A statement whose every action skips changes no shape, so it must not move the version --
    // a bump would make every node discard its cache and every concurrent DDL conflict for
    // nothing.
    let before = node.table("a").expect("committed").schema_version;
    node.run("ALTER TABLE a ADD COLUMN IF NOT EXISTS name text")
        .unwrap();
    assert_eq!(node.table("a").expect("committed").schema_version, before);
}

/// The versioned catalog is what makes another node see the change. Two sessions share the cache;
/// the second one must not answer a query with the shape it read before the first one's ALTER.
#[test]
fn a_second_session_sees_the_new_column_through_the_version_check() {
    let mut node = Node::new();
    let mut other = node.session();

    node.run("CREATE TABLE a (id int8 PRIMARY KEY, name text)")
        .unwrap();
    node.run("INSERT INTO a VALUES (1, 'one')").unwrap();

    // The second session reads the table, which is what puts it in the shared cache.
    let run_other = |other: &mut Executor, sql: &str| -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql).unwrap() {
            last = other.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    };
    run_other(&mut other, "SELECT id, name FROM a").unwrap();

    node.run("ALTER TABLE a ADD COLUMN note text").unwrap();

    // The cached definition is from an older catalog version, so it is discarded rather than
    // served. Without the version check this is `42703 column "note" does not exist`.
    let outcome = run_other(&mut other, "SELECT id, note FROM a").unwrap();
    let Outcome::Rows { rows, .. } = outcome else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"1".to_vec()), None]], "the padded NULL");

    // And it can write the new shape.
    run_other(&mut other, "INSERT INTO a VALUES (2, 'two', 'hi')").unwrap();
    let Outcome::Rows { rows, .. } = node.run("SELECT id, note FROM a ORDER BY id").unwrap() else {
        panic!("not rows");
    };
    assert_eq!(
        rows,
        [
            [Some(b"1".to_vec()), None],
            [Some(b"2".to_vec()), Some(b"hi".to_vec())],
        ]
    );
}

/// PostgreSQL's DDL is transactional and so is this: the column is visible to the transaction that
/// added it and to nobody else until it commits, and a rollback takes the rows written against it
/// with it. Captured from a real server, which is where the `25P02` after a failed ALTER comes
/// from too.
#[test]
fn ddl_inside_a_transaction_is_visible_to_itself_and_to_nobody_else() {
    let mut node = Node::new();
    let mut other = node.session();
    node.run("CREATE TABLE a (id int8 PRIMARY KEY)").unwrap();
    node.run("INSERT INTO a VALUES (1)").unwrap();

    node.executor.begin().unwrap();
    node.run("ALTER TABLE a ADD COLUMN x text").unwrap();
    // Its own view sees it, and can write against it.
    node.run("INSERT INTO a VALUES (2, 'two')").unwrap();
    let Outcome::Rows { rows, .. } = node.run("SELECT id, x FROM a ORDER BY id").unwrap() else {
        panic!("not rows");
    };
    assert_eq!(rows.len(), 2);

    // Another session, at its own snapshot, sees neither the column nor the row.
    let parsed = parse_statements("SELECT id, x FROM a").unwrap();
    let error = other.execute(&parsed[0], &Params::NONE).unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);

    node.executor.rollback().unwrap();

    // After the rollback the column is gone, and so is the row that used it.
    let error = node.run("SELECT id, x FROM a").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    let Outcome::Rows { rows, .. } = node.run("SELECT id FROM a").unwrap() else {
        panic!("not rows");
    };
    assert_eq!(rows, [[Some(b"1".to_vec())]]);
    assert_eq!(node.table("a").expect("committed").schema_version, 1);
}
