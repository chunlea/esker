//! **`table.func` is a function call, not a column.** PostgreSQL's functional notation: `posts.count`
//! means `count(posts)`, and the two spellings are interchangeable for any function whose argument
//! is the row type.
//!
//! `calculations · test_group_by_with_order_by_virtual_count_attribute` sends
//! `… GROUP BY "posts"."type" ORDER BY "posts"."count" ASC`, and `posts.count` is not a column on
//! either server — `information_schema` has no row for it on either. PostgreSQL answers anyway;
//! this node said `42703 column posts.count does not exist`, which was **literally true and still
//! the wrong answer**.
//!
//! Measured on PostgreSQL 19 over a three-row table:
//!
//! ```text
//! SELECT h1p.count FROM h1p                                      3
//! SELECT pg_typeof(h1p.count) FROM h1p                           bigint
//! SELECT type, h1p.count FROM h1p GROUP BY type ORDER BY h1p.count    b|1 ; a|2
//! SELECT h1p.nosuchfn FROM h1p                     ERROR: column h1p.nosuchfn does not exist
//! ```
//!
//! The last line is the rule's other half and is why this is not a blanket rewrite: a qualified
//! name that is neither a column nor a function keeps the column error it always had.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE posts (id bigint primary key, type text)",
        "INSERT INTO posts VALUES (1, 'a'), (2, 'b'), (3, 'a')",
    ])
}

/// The bare form, which counts the whole table.
#[test]
fn a_table_qualified_function_name_is_a_call_on_the_table() {
    let mut node = node();
    assert_eq!(
        node.rows(r#"SELECT "posts"."count" FROM "posts""#),
        vec![vec!["3".to_string()]]
    );
}

/// And under a `GROUP BY` it is the group's aggregate, which is what the Rails test orders by.
#[test]
fn it_aggregates_per_group_the_way_an_ordinary_aggregate_does() {
    let mut node = node();
    assert_eq!(
        node.rows(
            r#"SELECT "type", "posts"."count" FROM "posts" GROUP BY "type" ORDER BY "posts"."count""#
        ),
        vec![
            vec!["b".to_string(), "1".to_string()],
            vec!["a".to_string(), "2".to_string()],
        ]
    );
}

/// A name that is neither a column nor a function is still a missing column.
#[test]
fn a_qualified_name_that_names_nothing_is_still_a_missing_column() {
    let mut node = node();
    let error = node
        .run(r#"SELECT "posts"."nosuchfn" FROM "posts""#)
        .expect_err("nothing of that name exists");
    assert_eq!(error.to_string(), "column posts.nosuchfn does not exist");
}

/// **And the `Describe` too, because that is the path the client takes.** Active Record prepares by
/// default, so the statement in the failing test reaches `Describe` before it reaches `execute` —
/// and a `42703` there fails it before a single bind.
#[test]
fn a_prepared_statement_is_described_without_the_column_error() {
    let mut node = node();
    let described = node
        .describe(r#"SELECT "type", "posts"."count" FROM "posts" GROUP BY "type" ORDER BY "posts"."count""#)
        .expect("the describe answers the shape rather than a missing column");
    let fields = described.fields.expect("it returns rows");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[1].name, "count");
}
