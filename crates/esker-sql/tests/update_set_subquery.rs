//! A scalar subquery on the **right of a `SET`** — `UPDATE a SET n = (SELECT …)`.
//!
//! `docs/plans/phase-12-subquery.md` §4 deferred the write path as a whole ("unless a triage line
//! pulls it forward"); the `WHERE` half landed with `tests/write_in_subquery.rs` and this is the
//! other half. `exec::dml::value_for_column` refused `Expr::Subquery` by name rather than letting
//! it reach the row evaluator, whose answer would have been the internal error of a planner bug.
//!
//! Measured on PostgreSQL 19beta1, in one rolled-back session:
//!
//! ```text
//! UPDATE h1s_a SET n = (SELECT max(n) FROM h1s_b);                       -> UPDATE 2, both 20
//! UPDATE h1s_a SET n = (SELECT n FROM h1s_b WHERE h1s_b.id = h1s_a.id);  -> UPDATE 2, 10 and 20
//! UPDATE h1s_a SET n = (SELECT n FROM h1s_b WHERE id = 99);              -> UPDATE 2, both NULL
//! UPDATE h1s_a SET n = (SELECT n FROM h1s_b);
//!     ERROR:  21000: more than one row returned by a subquery used as an expression
//! ```
//!
//! The third is the one a guess gets wrong: no rows is a **NULL assigned**, not a row skipped and
//! not an error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE h1s_a (id bigint primary key, n bigint)",
        "CREATE TABLE h1s_b (id bigint primary key, n bigint)",
        "INSERT INTO h1s_a VALUES (1, 1), (2, 2)",
        "INSERT INTO h1s_b VALUES (1, 10), (2, 20)",
    ])
}

/// **One value, computed once, for every row.**
#[test]
fn an_uncorrelated_scalar_subquery_is_the_value_for_every_row() {
    let mut node = node();
    assert_eq!(
        node.answer("UPDATE h1s_a SET n = (SELECT max(n) FROM h1s_b)"),
        parity::Answer::Done
    );
    assert_eq!(
        node.rows("SELECT id, n FROM h1s_a ORDER BY id"),
        vec![
            vec!["1".to_owned(), "20".to_owned()],
            vec!["2".to_owned(), "20".to_owned()]
        ]
    );
}

/// **A correlated one is evaluated per row**, against the row being written.
#[test]
fn a_correlated_scalar_subquery_is_evaluated_for_each_row() {
    let mut node = node();
    assert_eq!(
        node.answer("UPDATE h1s_a SET n = (SELECT n FROM h1s_b WHERE h1s_b.id = h1s_a.id)"),
        parity::Answer::Done
    );
    assert_eq!(
        node.rows("SELECT id, n FROM h1s_a ORDER BY id"),
        vec![
            vec!["1".to_owned(), "10".to_owned()],
            vec!["2".to_owned(), "20".to_owned()]
        ]
    );
}

/// **No rows is a NULL that is written**, not a row left alone.
///
/// The shape a guess gets wrong, and the reason `Expr::Subquery` refuses to evaluate with an empty
/// `run` rather than answering "no rows": a NULL looks like an answer.
#[test]
fn a_subquery_that_matches_nothing_assigns_null() {
    let mut node = node();
    assert_eq!(
        node.answer("UPDATE h1s_a SET n = (SELECT n FROM h1s_b WHERE id = 99)"),
        parity::Answer::Done
    );
    assert_eq!(
        node.rows("SELECT id, n FROM h1s_a ORDER BY id"),
        vec![
            vec!["1".to_owned(), "\\N".to_owned()],
            vec!["2".to_owned(), "\\N".to_owned()]
        ],
        "both rows are set to NULL — `\\N` is how this harness renders one"
    );
}

/// **More than one row is `21000`**, with PostgreSQL's own sentence.
#[test]
fn a_subquery_returning_more_than_one_row_is_refused() {
    let mut node = node();
    let answered = node.answer("UPDATE h1s_a SET n = (SELECT n FROM h1s_b)");
    assert_eq!(
        answered,
        parity::Answer::Refused(
            "21000 more than one row returned by a subquery used as an expression".to_owned()
        ),
        "measured on 19beta1: {answered:?}"
    );
    assert_eq!(
        node.rows("SELECT id, n FROM h1s_a ORDER BY id"),
        vec![
            vec!["1".to_owned(), "1".to_owned()],
            vec!["2".to_owned(), "2".to_owned()]
        ],
        "a refused statement writes nothing"
    );
}
