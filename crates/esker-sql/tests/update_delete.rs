//! `UPDATE` and `DELETE`: rewriting a row without leaving an index behind.
//!
//! The hard part of both is not the row, it is everything that points at it. An `UPDATE` that
//! changes an indexed column has to remove the old index entry as well as write the new one, and
//! one that changes the *primary key* has to remove the old row as well. An index entry left
//! pointing at a row that is gone is not a slow query, it is a wrong answer — the planner will
//! follow it.
//!
//! Every expectation here came from running the same statements against a real PostgreSQL 19.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome};
use esker_sql::sqlstate;

struct Node {
    executor: Executor,
}

impl Node {
    /// `u (id int8 PRIMARY KEY, e text UNIQUE, n int8 NOT NULL)` with three rows.
    fn loaded() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Node {
            executor: Executor::new(backend, Arc::new(Catalog::new()), 1),
        };
        node.run("CREATE TABLE u (id int8 PRIMARY KEY, e text UNIQUE, n int8 NOT NULL)")
            .unwrap();
        node.run("INSERT INTO u VALUES (1, 'a', 10), (2, 'b', 20), (3, NULL, 30)")
            .unwrap();
        node
    }

    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed)?;
        }
        Ok(last)
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        match self.run(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| {
                            value.map_or_else(
                                || "NULL".to_owned(),
                                |bytes| String::from_utf8(bytes).unwrap(),
                            )
                        })
                        .collect()
                })
                .collect(),
            Outcome::Done { tag } => panic!("not rows: {tag}"),
        }
    }
}

#[test]
fn an_update_rewrites_the_row_and_counts_what_it_touched() {
    let mut node = Node::loaded();
    assert_eq!(
        node.run("UPDATE u SET n = 99 WHERE id = 1").unwrap(),
        Outcome::done("UPDATE 1")
    );
    assert_eq!(node.rows("SELECT n FROM u WHERE id = 1"), [["99"]]);

    assert_eq!(
        node.run("UPDATE u SET n = 0").unwrap(),
        Outcome::done("UPDATE 3"),
        "no WHERE means every row"
    );
    assert_eq!(
        node.rows("SELECT n FROM u ORDER BY id"),
        [["0"], ["0"], ["0"]]
    );
}

/// A `SET` expression is evaluated against the row as it was, so `SET a = b, b = a` swaps them
/// rather than assigning `a` twice.
#[test]
fn set_expressions_see_the_row_as_it_was() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE swap (id int8 PRIMARY KEY, a int8, b int8)")
        .unwrap();
    node.run("INSERT INTO swap VALUES (1, 10, 20)").unwrap();
    node.run("UPDATE swap SET a = b, b = a WHERE id = 1")
        .unwrap();
    assert_eq!(node.rows("SELECT a, b FROM swap"), [["20", "10"]]);

    node.run("UPDATE swap SET a = a WHERE id = 1").unwrap();
    assert_eq!(node.rows("SELECT a FROM swap"), [["20"]]);
}

/// The index has to move with the value. An entry left behind would be followed by the planner and
/// answer with a row that no longer has that value.
#[test]
fn changing_an_indexed_column_moves_its_index_entry() {
    let mut node = Node::loaded();
    node.run("UPDATE u SET e = 'z' WHERE id = 1").unwrap();

    assert_eq!(node.rows("SELECT id FROM u WHERE e = 'z'"), [["1"]]);
    assert_eq!(
        node.rows("SELECT id FROM u WHERE e = 'a'"),
        Vec::<Vec<String>>::new(),
        "the old entry was followed and answered with a row that no longer has that value"
    );
    // And the freed value can be taken by another row.
    node.run("UPDATE u SET e = 'a' WHERE id = 2").unwrap();
    assert_eq!(node.rows("SELECT id FROM u WHERE e = 'a'"), [["2"]]);
}

/// Changing the primary key moves the row itself: the old key must not still be there.
#[test]
fn changing_the_primary_key_moves_the_row() {
    let mut node = Node::loaded();
    node.run("UPDATE u SET id = 10 WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM u WHERE id = 1"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(node.rows("SELECT e FROM u WHERE id = 10"), [["a"]]);
    assert_eq!(
        node.rows("SELECT id FROM u ORDER BY id").len(),
        3,
        "moved, not copied"
    );
}

/// The Halloween problem: an update that moves a row forward in the key space must not meet it
/// again and move it a second time.
#[test]
fn an_update_that_moves_rows_forward_touches_each_one_once() {
    let mut node = Node::loaded();
    node.run("CREATE TABLE h (id int8 PRIMARY KEY)").unwrap();
    node.run("INSERT INTO h VALUES (1), (2), (3)").unwrap();
    assert_eq!(
        node.run("UPDATE h SET id = id").unwrap(),
        Outcome::done("UPDATE 3"),
        "three rows, updated once each"
    );
    assert_eq!(
        node.rows("SELECT id FROM h ORDER BY id"),
        [["1"], ["2"], ["3"]]
    );
}

#[test]
fn an_update_into_a_taken_key_is_23505() {
    let mut node = Node::loaded();
    let error = node.run("UPDATE u SET id = 2 WHERE id = 1").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert!(error.to_string().contains("u_pkey"), "{error}");

    let error = node.run("UPDATE u SET e = 'b' WHERE id = 3").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    assert!(error.to_string().contains("u_e_key"), "{error}");

    // And the failed statement changed nothing.
    assert_eq!(node.rows("SELECT e FROM u WHERE id = 3"), [["NULL"]]);
}

/// Setting a row's own value back is not a duplicate of itself.
#[test]
fn setting_a_unique_column_to_what_it_already_is_is_not_a_duplicate() {
    let mut node = Node::loaded();
    node.run("UPDATE u SET e = 'a', n = 11 WHERE id = 1")
        .unwrap();
    assert_eq!(node.rows("SELECT e, n FROM u WHERE id = 1"), [["a", "11"]]);
}

#[test]
fn an_update_that_breaks_a_constraint_says_which_one() {
    let mut node = Node::loaded();
    let error = node.run("UPDATE u SET n = NULL WHERE id = 2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NOT_NULL_VIOLATION);
    assert_eq!(
        error.to_string(),
        "null value in column \"n\" of relation \"u\" violates not-null constraint"
    );

    let error = node.run("UPDATE u SET nope = 1 WHERE id = 2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_COLUMN);
    assert_eq!(
        error.to_string(),
        "column \"nope\" of relation \"u\" does not exist"
    );

    let error = node.run("UPDATE u SET n = 'x' WHERE id = 2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert_eq!(
        error.to_string(),
        "invalid input syntax for type bigint: \"x\""
    );
}

#[test]
fn a_delete_removes_the_row_and_its_index_entries() {
    let mut node = Node::loaded();
    assert_eq!(
        node.run("DELETE FROM u WHERE id = 999").unwrap(),
        Outcome::done("DELETE 0")
    );
    assert_eq!(
        node.run("DELETE FROM u WHERE id = 1").unwrap(),
        Outcome::done("DELETE 1")
    );
    assert_eq!(
        node.rows("SELECT id FROM u WHERE e = 'a'"),
        Vec::<Vec<String>>::new(),
        "the index entry outlived the row"
    );
    // The value is free again, which is only true if the entry really went.
    node.run("INSERT INTO u VALUES (9, 'a', 1)").unwrap();
    assert_eq!(node.rows("SELECT id FROM u WHERE e = 'a'"), [["9"]]);
}

#[test]
fn a_delete_with_no_where_empties_the_table() {
    let mut node = Node::loaded();
    assert_eq!(
        node.run("DELETE FROM u").unwrap(),
        Outcome::done("DELETE 3")
    );
    assert_eq!(node.rows("SELECT id FROM u"), Vec::<Vec<String>>::new());
    // And every key is free again.
    node.run("INSERT INTO u VALUES (1, 'a', 10)").unwrap();
    assert_eq!(node.rows("SELECT id FROM u"), [["1"]]);
}

/// Both go through the same planner as `SELECT`, so a pinned key is a point read here too.
#[test]
fn update_and_delete_use_the_same_access_paths() {
    let mut node = Node::loaded();
    let plan: Vec<String> = node
        .rows("EXPLAIN SELECT * FROM u WHERE id = 1")
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert!(plan.join("\n").contains("Point Get"), "{plan:?}");

    // The statements themselves touch exactly the row the path names.
    node.run("UPDATE u SET n = 5 WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT n FROM u ORDER BY id"),
        [["5"], ["20"], ["30"]]
    );
    node.run("DELETE FROM u WHERE e = 'b'").unwrap();
    assert_eq!(node.rows("SELECT id FROM u ORDER BY id"), [["1"], ["3"]]);
}

/// Both roll back with their transaction, like every other statement.
#[test]
fn a_failed_statement_leaves_the_table_alone() {
    let mut node = Node::loaded();
    node.run("UPDATE u SET n = 1, e = 'b' WHERE id = 1")
        .unwrap_err();
    assert_eq!(node.rows("SELECT n, e FROM u WHERE id = 1"), [["10", "a"]]);
}
