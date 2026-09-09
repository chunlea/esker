//! **An integer literal takes the smallest type that holds it: `int4`, then `int8`, then
//! `numeric`.**
//!
//! `or_test.rb`'s *or with large number* sends `WHERE id = 1 OR id = 9223372036854775808` against a
//! `bigint` primary key. PostgreSQL 19 answers one row: the literal is past `int8`, so it is a
//! `numeric`, the comparison is `numeric = numeric` with the column promoted, and it is simply
//! false for every row. This node answered `22003 bigint out of range` — it read every unadorned
//! integer as an `i64` and refused the ones that did not fit.
//!
//! The ladder, and the sign folded before the choice, is measured on PostgreSQL 19 rather than
//! inferred from the one test: `pg_typeof(2147483647)` is `integer`, `pg_typeof(2147483648)` is
//! `bigint`, `pg_typeof(9223372036854775808)` is `numeric`, and `-2147483648` is `integer` while
//! `-2147483649` is `bigint`, so the minus sign belongs to the literal and not to an operator
//! applied after it.
//!
//! # The `int4` rung is measured and **not** implemented, deliberately
//!
//! `pg_typeof(1)` is `integer` on PostgreSQL 19 and `bigint` here: this node has one integer
//! literal type and it is `int8`. That is a real divergence and it is not this unit — giving small
//! literals `int4` changes the type OID in the `RowDescription` of every `SELECT 1` the suite
//! sends, which is a blast radius that wants its own unit and its own gate. What is here is the
//! rung the Rails test needs, and the one that was an *error* rather than a wrong type: past
//! `int8` is `numeric`, in both signs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Every rung of the ladder, in both signs.
#[test]
fn an_integer_literal_takes_the_smallest_type_that_holds_it() {
    let mut node = parity::Node::new(&[]);
    for (literal, want) in [
        ("2147483648", "bigint"),
        ("9223372036854775807", "bigint"),
        ("9223372036854775808", "numeric"),
        ("-9223372036854775808", "bigint"),
        ("-9223372036854775809", "numeric"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({literal})")),
            vec![vec![want.to_string()]],
            "pg_typeof({literal})"
        );
    }
}

/// And the statement the Rails test sends: a literal past `int8` **compares** rather than raising.
#[test]
fn a_literal_past_bigint_compares_against_a_bigint_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE posts (id bigint primary key)",
        "INSERT INTO posts VALUES (1), (2)",
    ]);
    assert_eq!(
        node.rows("SELECT id FROM posts WHERE id = 1 OR id = 9223372036854775808"),
        vec![vec!["1".to_string()]],
        "no row can equal a number no bigint can hold, and that is an answer and not an error"
    );
    // The comparison on its own, which is where PostgreSQL promotes the column to `numeric`.
    assert_eq!(
        node.rows("SELECT 9223372036854775808 = 1::bigint"),
        vec![vec!["f".to_string()]]
    );
}
