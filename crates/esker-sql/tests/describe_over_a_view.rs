//! `Describe` expands a view, the way planning does.
//!
//! **`ActiveRecord` prepares by default**, so this is the path every read of a view takes. Eight of
//! `view_test.rb`'s tests failed with `relation "…" does not exist` for a view that
//! `SELECT * FROM v` answered perfectly through the simple protocol, and the client-side frame said
//! which path it was:
//!
//! ```text
//! PG::Connection#exec_prepared: ERROR:  relation "paperbacks" does not exist (PG::UndefinedTable)
//! ```
//!
//! It reproduced on the **first** `exec_prepared`, with no drop or re-creation in between — so it
//! is not a stale plan surviving a catalog change, it is a view that the describe path could never
//! resolve at all.
//!
//! [`Executor::plan_select`] rewrites `FROM v` into the derived table the view stands for before
//! anything else looks at the statement. `described_in` resolves its relations off the statement's
//! own `FROM`, for reasons its comment records — and did that rewrite nowhere, so the name reached
//! `relation_of` as a table that does not exist. That is the same shape as the derived-table bug
//! the same function already carries a comment about: a step planning does, and describing did not.
//!
//! **What is deliberately not tested here**: a view named inside an *expression* subquery —
//! `SELECT id FROM t WHERE id IN (SELECT id FROM v)`. `expand_views` walks `FROM` and the joins,
//! and `each_relation_name` with it, so neither sees a name that only appears inside a `WHERE`.
//! That is a gap in **both** paths, measured — the simple protocol answers `42P01` for it exactly
//! as the prepared one does, where PostgreSQL 19 returns the row — so it is not a describe bug and
//! a test of it belongs with whatever fixes the expansion, not here. `view_test.rb` does not reach
//! it; it is recorded in `docs/plans/debts-v1.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The names and the types both come back, which is what a client binds against.
#[test]
fn describe_answers_the_shape_of_a_view() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dv_books (id bigserial primary key, name varchar(255), format text)")
        .unwrap();
    node.run("CREATE VIEW dv_paperbacks AS SELECT id, name FROM dv_books WHERE format = 'paper'")
        .unwrap();

    let parsed = parse_statements("SELECT id, name FROM dv_paperbacks").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(
        described
            .fields
            .expect("a SELECT over a view returns rows")
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>(),
        vec!["id", "name"]
    );
}

/// **A `SELECT *` over a view has to widen to the view's columns and not to the base table's.**
/// Describing the base table instead would answer with `format` as well, which is a shape the view
/// never returns — a client told about a third column reads a `DataRow` that has two.
#[test]
fn describe_widens_a_star_to_the_views_own_columns() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dv_all (id bigserial primary key, name text, secret text)")
        .unwrap();
    node.run("CREATE VIEW dv_public AS SELECT id, name FROM dv_all")
        .unwrap();

    let parsed = parse_statements("SELECT * FROM dv_public").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(
        described
            .fields
            .expect("a SELECT over a view returns rows")
            .iter()
            .map(|field| field.name.clone())
            .collect::<Vec<_>>(),
        vec!["id", "name"]
    );
}

/// **Describe and execute agree**, which is the protocol obligation the sibling file exists for:
/// a client told two columns must receive two.
#[test]
fn describe_and_execute_agree_about_a_view() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE dv_rows (id bigserial primary key, name text, format text)")
        .unwrap();
    node.run("INSERT INTO dv_rows (name, format) VALUES ('kept', 'paper')")
        .unwrap();
    node.run("CREATE VIEW dv_kept AS SELECT id, name FROM dv_rows WHERE format = 'paper'")
        .unwrap();

    let parsed = parse_statements("SELECT id, name FROM dv_kept").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let described_width = described.fields.expect("row-returning").len();

    let rows = node.rows("SELECT id, name FROM dv_kept");
    assert_eq!(
        described_width,
        rows.first().expect("one row").len(),
        "describe promised {described_width} columns and execute sent a different number"
    );
}
