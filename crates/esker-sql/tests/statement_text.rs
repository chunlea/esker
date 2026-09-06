//! **What a view shows is what the client sent** — not what the parser was handed.
//!
//! Several statements are rewritten before `sqlparser` sees them, because it cannot read them:
//! a `DO … RAISE` block, `REFRESH MATERIALIZED VIEW`, `ALTER TABLE … RESET`, and every cursor
//! statement all become `SELECT 1` and travel as facts on `Parsed` (`crate::parse`). The rewritten
//! text was what `Parsed::source` returned, so `pg_stat_activity.query` reported **`SELECT 1`** for
//! a session that had run something else entirely — a view whose whole job is to say what a
//! session is doing.
//!
//! And one statement's text is not the whole input. Measured on PostgreSQL 19, two `PREPARE`s in
//! one query string:
//!
//! ```text
//! PREPARE h1_a AS SELECT 1; PREPARE h1_b AS SELECT 2;
//!
//!  name |         statement
//! ------+---------------------------
//!  h1_a | PREPARE h1_a AS SELECT 1;
//!  h1_b | PREPARE h1_b AS SELECT 2;
//! ```
//!
//! **Two views, two answers, one input.** `pg_stat_activity.query` is the whole string — a real
//! server shows all three statements of a three-statement `Query` while any of them runs — and
//! `pg_prepared_statements.statement` is one statement's own slice, semicolon included. Neither is
//! the rewritten text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The rows of a query, tab-joined per row.
fn rows(node: &mut parity::Node, sql: &str) -> Vec<Vec<String>> {
    node.rows(sql)
}

#[test]
fn pg_stat_activity_reports_the_statement_the_client_sent() {
    // Two sessions on one store, because a session reading the view reports *itself* as running
    // the `SELECT` that reads it — the writer's row is only visible from somewhere else.
    let pair = parity::Pair::new(&[]);
    let mut writer = pair.session();
    let mut reader = pair.session();
    // `CLOSE ALL` is read by hand and the parser is handed `SELECT 1` instead: one of the
    // statements this whole file is about, and the one that needs no setup.
    assert!(writer.run("CLOSE ALL").is_ok());
    let seen = rows(
        &mut reader,
        "SELECT query FROM pg_stat_activity WHERE query LIKE 'CLOSE%'",
    );
    assert_eq!(seen, vec![vec!["CLOSE ALL".to_owned()]]);
}

#[test]
fn a_prepared_statement_reports_its_own_slice_of_the_query() {
    let mut node = parity::Node::new(&[]);
    node.run("PREPARE h1_a AS SELECT 1; PREPARE h1_b AS SELECT 2;")
        .unwrap();
    // Each row is the statement that made it, semicolon and all — not both of them, and not the
    // `SELECT 1` the parser was handed for anything rewritten.
    assert_eq!(
        rows(
            &mut node,
            "SELECT name, statement FROM pg_prepared_statements ORDER BY name"
        ),
        vec![
            vec!["h1_a".to_owned(), "PREPARE h1_a AS SELECT 1;".to_owned()],
            vec!["h1_b".to_owned(), "PREPARE h1_b AS SELECT 2;".to_owned()],
        ]
    );
}

#[test]
fn a_semicolon_inside_a_string_is_not_a_statement_boundary() {
    let mut node = parity::Node::new(&[]);
    // The reason splitting is a walk and not a `split(';')`: two of these semicolons are inside
    // something the grammar reads as one token.
    node.run("PREPARE h1_s AS SELECT 'a;b' AS \"c;d\"; PREPARE h1_t AS SELECT 2;")
        .unwrap();
    assert_eq!(
        rows(
            &mut node,
            "SELECT statement FROM pg_prepared_statements WHERE name = 'h1_s'"
        ),
        vec![vec!["PREPARE h1_s AS SELECT 'a;b' AS \"c;d\";".to_owned()]]
    );
}

#[test]
fn one_statement_with_no_semicolon_is_reported_as_it_was_written() {
    let mut node = parity::Node::new(&[]);
    node.run("PREPARE h1_n AS SELECT 1").unwrap();
    assert_eq!(
        rows(
            &mut node,
            "SELECT statement FROM pg_prepared_statements WHERE name = 'h1_n'"
        ),
        vec![vec!["PREPARE h1_n AS SELECT 1".to_owned()]]
    );
}
