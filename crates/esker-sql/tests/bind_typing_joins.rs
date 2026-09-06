//! **A bind inside a derived table, and a bind in a joined `UPDATE`** — two more entry points
//! where `$n` was resolved to `text` and then compared with a `bigint`.
//!
//! `bind_parameter_test.rb` and `has_many_associations_test.rb`, both
//! `PG::UndefinedFunction: operator does not exist: bigint = text` (run 103's triage). The same
//! family as the view-bind matrix and the CTE bind: the parameter is typed from the column beside
//! it, and these are two places the walk could not see the column.
//!
//! Measured on PostgreSQL 19 through `PREPARE` and `pg_prepared_statements.parameter_types`, which
//! is the same instrument this file reads:
//!
//! ```text
//! SELECT COUNT(*) FROM (SELECT "authors".* FROM "authors"
//!   INNER JOIN "posts" ON "posts"."title" = $1 AND "posts"."author_id" = "authors"."id"
//!   WHERE "authors"."name" = $2) authors WHERE "authors"."id" = $3   -> {text,text,bigint}
//!
//! UPDATE "posts" "__active_record_update_alias" SET "author_id" = $1
//!   FROM "posts" INNER JOIN "categorizations" ON "categorizations"."post_id" = "posts"."id"
//!   WHERE "__active_record_update_alias"."id" = "posts"."id"
//!     AND "categorizations"."author_id" = $2                          -> {bigint,bigint}
//! ```
//!
//! **The bug is `$3`**, a `bigint` reached through a derived table, which this node resolved to
//! `text` — that is the `bigint = text` the suite reports, and it is what this file fixes.
//!
//! **The first two are a second divergence and are left alone**, asserted here as this node
//! answers them: `character varying` where PostgreSQL says `text`. The mechanism is not the
//! relation lookup this file is about — it is operator resolution. `varchar` has no equality
//! operator of its own and is binary-coercible to `text`, so `varchar_col = $1` resolves through
//! `texteq` and PostgreSQL types the parameter from the *operator's* signature rather than from
//! the column. Nothing in the suite turns on it: the value travels as text either way and compares
//! the same. It belongs with the type surface, and is reported rather than changed here — a
//! parameter's OID moving from 1043 to 25 is every `varchar` comparison on the wire.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE authors (id bigserial primary key, name varchar(255))",
    "CREATE TABLE posts (id bigserial primary key, author_id bigint, title varchar(255))",
    "CREATE TABLE categorizations (id bigserial primary key, post_id bigint, author_id bigint)",
];

/// The types this node resolved for a statement's parameters, read the way the oracle was read.
fn parameter_types(node: &mut parity::Node, name: &str, sql: &str) -> String {
    node.run(&format!("PREPARE {name} AS {sql}"))
        .unwrap_or_else(|error| panic!("PREPARE {name} was refused: {error}"));
    node.rows(&format!(
        "SELECT parameter_types FROM pg_prepared_statements WHERE name = '{name}'"
    ))
    .first()
    .and_then(|row| row.first())
    .cloned()
    .unwrap_or_else(|| "<no row>".to_owned())
}

#[test]
fn a_bind_inside_a_derived_table_is_typed_by_the_column_it_is_compared_with() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        parameter_types(
            &mut node,
            "d1",
            "SELECT COUNT(*) FROM (SELECT \"authors\".* FROM \"authors\" \
             INNER JOIN \"posts\" ON \"posts\".\"title\" = $1 \
             AND \"posts\".\"author_id\" = \"authors\".\"id\" \
             WHERE \"authors\".\"name\" = $2) authors WHERE \"authors\".\"id\" = $3"
        ),
        "{\"character varying\",\"character varying\",bigint}"
    );
}

#[test]
fn a_bind_in_a_joined_update_is_typed_by_the_column_it_writes() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        parameter_types(
            &mut node,
            "u1",
            "UPDATE \"posts\" \"__active_record_update_alias\" SET \"author_id\" = $1 \
             FROM \"posts\" INNER JOIN \"categorizations\" \
             ON \"categorizations\".\"post_id\" = \"posts\".\"id\" \
             WHERE \"__active_record_update_alias\".\"id\" = \"posts\".\"id\" \
             AND \"categorizations\".\"author_id\" = $2"
        ),
        "{bigint,bigint}"
    );
}

/// The statements run, which is what the suite is actually asserting — a resolved type that is
/// wrong shows up as `operator does not exist: bigint = text` and not as a bad catalog row.
#[test]
fn both_statements_execute_rather_than_refusing_by_operator() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO authors (name) VALUES ('a')").unwrap();
    node.run("INSERT INTO posts (author_id, title) VALUES (1, 't')")
        .unwrap();
    node.run("INSERT INTO categorizations (post_id, author_id) VALUES (1, 1)")
        .unwrap();
    assert_eq!(
        parameter_types(
            &mut node,
            "d2",
            "SELECT COUNT(*) FROM (SELECT \"authors\".* FROM \"authors\" \
             INNER JOIN \"posts\" ON \"posts\".\"title\" = $1 \
             AND \"posts\".\"author_id\" = \"authors\".\"id\" \
             WHERE \"authors\".\"name\" = $2) authors WHERE \"authors\".\"id\" = $3"
        ),
        "{\"character varying\",\"character varying\",bigint}"
    );
    assert_eq!(
        node.rows("EXECUTE d2('t', 'a', 1)"),
        vec![vec!["1".to_owned()]]
    );
}
