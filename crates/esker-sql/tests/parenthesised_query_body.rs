//! `((SELECT …))` — **parentheses around a query are grouping, and the grammar is transparent.**
//!
//! `postgresql_adapter_prevent_writes_test.rb:78` sends
//! `/*action:index*/(\n( SELECT * FROM ex WHERE data = '…' ) )`, and this node answered
//! `0A000 the query body ((SELECT …)) is not supported` — it matched only a bare `SELECT` and had
//! no arm for a query wrapped in its own parentheses.
//!
//! PostgreSQL does not treat this as nesting at all: `gram.y`'s `insertSelectOptions` **merges**
//! the clauses written outside the parentheses into the query inside, and refuses only when both
//! levels supply the same one. Measured on PostgreSQL 19:
//!
//! ```text
//! (((((SELECT 42)))))                                          -> 42
//! /*action:index*/( ( SELECT * FROM ex2 WHERE data = 'a' ) )   -> the row
//! (SELECT id FROM ex2) ORDER BY id DESC LIMIT 1                -> 3      (outer clauses apply)
//! WITH w AS (SELECT 1 AS n) (SELECT n FROM w)                  -> 1      (so does an outer WITH)
//! (SELECT id FROM ex3 FOR UPDATE) FOR UPDATE                   -> 1      (locks merge, silently)
//! ((SELECT id FROM ex2 ORDER BY id LIMIT 2)) ORDER BY id DESC  42601: multiple ORDER BY clauses not allowed
//! (SELECT id FROM ex2 ORDER BY id LIMIT 2) LIMIT 1             42601: multiple LIMIT clauses not allowed
//! (SELECT id FROM ex3 OFFSET 0) OFFSET 0                       42601: multiple OFFSET clauses not allowed
//! WITH a AS (…) (WITH b AS (…) SELECT n FROM b)                42601: multiple WITH clauses not allowed
//! ```
//!
//! **The refusals are the interesting half.** A fix that simply unwrapped the parentheses would
//! answer all four of those instead of refusing them, and would silently drop one of the two
//! clauses while doing it — which is worse than the `0A000` it replaced. Locking is the exception
//! and is measured too: two `FOR UPDATE`s merge rather than collide.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE ex (id int PRIMARY KEY, data text)",
    "INSERT INTO ex VALUES (1, 'a'), (2, 'b'), (3, 'c')",
];

fn node() -> parity::Node {
    parity::Node::new(FIXTURE)
}

/// The rows joined, or the refusal as `!<message>`.
fn ask(node: &mut parity::Node, sql: &str) -> String {
    match node.run(sql) {
        Ok(esker_sql::pgwire::session::Outcome::Rows { rows, .. }) => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        cell.as_ref().map_or_else(
                            || "\\N".to_owned(),
                            |bytes| String::from_utf8_lossy(bytes).into_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("|"),
        Ok(_) => "not rows".to_owned(),
        Err(error) => format!("!{error}"),
    }
}

#[test]
fn a_parenthesised_select_behind_a_leading_comment_is_the_select() {
    let mut node = node();
    // The suite's own statement, newline and all.
    assert_eq!(
        ask(
            &mut node,
            "/*action:index*/(\n( SELECT id, data FROM ex WHERE data = 'a' ) )"
        ),
        "1\ta"
    );
}

#[test]
fn parentheses_nest_as_deep_as_the_parser_allows() {
    let mut node = node();
    assert_eq!(ask(&mut node, "(((((SELECT 42)))))"), "42");
}

#[test]
fn clauses_written_outside_belong_to_the_query_inside() {
    let mut node = node();
    assert_eq!(
        ask(&mut node, "(SELECT id FROM ex) ORDER BY id DESC LIMIT 1"),
        "3"
    );
    assert_eq!(
        ask(&mut node, "(SELECT id FROM ex) ORDER BY id LIMIT 1"),
        "1"
    );
    assert_eq!(
        ask(&mut node, "(SELECT id FROM ex ORDER BY id) LIMIT 1"),
        "1"
    );
}

#[test]
fn a_with_outside_the_parentheses_reaches_the_query_inside() {
    let mut node = node();
    assert_eq!(
        ask(&mut node, "WITH w AS (SELECT 1 AS n) (SELECT n FROM w)"),
        "1"
    );
}

#[test]
fn the_same_clause_on_both_levels_is_a_syntax_error() {
    let mut node = node();
    // Four separate sentences, each PostgreSQL's own — a client that wrote two `ORDER BY`s is told
    // which clause it doubled, not that a query body is unsupported.
    assert_eq!(
        ask(
            &mut node,
            "((SELECT id FROM ex ORDER BY id LIMIT 2)) ORDER BY id DESC"
        ),
        "!multiple ORDER BY clauses not allowed"
    );
    assert_eq!(
        ask(&mut node, "(SELECT id FROM ex ORDER BY id LIMIT 2) LIMIT 1"),
        "!multiple LIMIT clauses not allowed"
    );
    assert_eq!(
        ask(&mut node, "(SELECT id FROM ex OFFSET 0) OFFSET 0"),
        "!multiple OFFSET clauses not allowed"
    );
    assert_eq!(
        ask(
            &mut node,
            "WITH a AS (SELECT 1 AS n) (WITH b AS (SELECT 2 AS n) SELECT n FROM b)"
        ),
        "!multiple WITH clauses not allowed"
    );
}
