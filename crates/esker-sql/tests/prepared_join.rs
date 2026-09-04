//! A join through the **extended protocol** — run 51's `relation "…" does not exist`, 47 tests in
//! 16 files.
//!
//! `Describe` resolved a statement's relations by reading the list `bind::table_names` built —
//! **positionally**, first entry as the outer table and the rest as the joins. That list is built
//! for parameter *typing*, where a name appearing twice is one name and a name the catalog does
//! not have is nothing to type against, so it de-duplicates and it drops. Both are right for
//! typing and wrong for a position: a self-join's two entries collapsed to one, and `Describe`
//! then planned a one-join `SELECT` with nothing to join to.
//!
//! `Reply < Topic` in `ActiveRecord`'s own schema, so `Topic.joins(:replies)` is a self-join on
//! `topics` — which is why seventeen of the forty-seven failures name that one table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Params};

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE topics (id bigserial primary key, parent_id bigint, title varchar, type varchar)",
    "CREATE TABLE replies (id bigserial primary key, topic_id bigint, body varchar)",
    "INSERT INTO topics (parent_id, title, type) VALUES (NULL,'first',NULL), (1,'second','Reply')",
    "INSERT INTO replies (topic_id, body) VALUES (1,'r1')",
];

/// What `Describe` says the rows look like, or the error it refused with.
fn described(node: &mut parity::Node, sql: &str) -> esker_sql::Result<Vec<String>> {
    let parsed = parse_statements(sql).unwrap();
    Ok(node
        .executor
        .describe(&parsed[0], &[])?
        .fields
        .expect("a SELECT returns rows")
        .iter()
        .map(|field| field.name.clone())
        .collect())
}

/// **A self-join describes**, and it is the shape `Topic.joins(:replies)` sends on every one of
/// the seventeen `topics` failures.
///
/// The same statement through the simple protocol always worked, which is what made this look like
/// a missing table rather than a `Describe` that had lost one.
#[test]
fn a_self_join_describes_and_runs_through_the_extended_protocol() {
    let mut node = parity::Node::new(FIXTURE);
    let sql = "SELECT \"topics\".\"id\", \"replies\".\"title\" FROM \"topics\" \
               INNER JOIN \"topics\" \"replies\" ON \"replies\".\"parent_id\" = \"topics\".\"id\"";

    assert_eq!(described(&mut node, sql).unwrap(), vec!["id", "title"]);
    // And the answer is the same one the simple protocol gives.
    assert_eq!(
        node.rows(sql),
        vec![vec!["1".to_owned(), "second".to_owned()]]
    );
}

/// The parameterised form, which is what `IN ($1, $2, $3, $4)` on an STI type column makes of it.
#[test]
fn a_self_join_carrying_parameters_types_them_against_both_sides() {
    let mut node = parity::Node::new(FIXTURE);
    let parsed = parse_statements(
        "SELECT \"topics\".\"id\" FROM \"topics\" INNER JOIN \"topics\" \"replies\" \
         ON \"replies\".\"parent_id\" = \"topics\".\"id\" AND \"replies\".\"type\" = $1",
    )
    .unwrap();

    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    assert_eq!(described.parameters.len(), 1);
    let outcome = node
        .executor
        .execute(
            &parsed[0],
            &Params {
                values: &[Some(b"Reply".to_vec())],
                formats: &[],
                declared: &[],
                bound: true,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, esker_sql::pgwire::session::Outcome::Rows { ref rows, .. } if rows.len() == 1),
        "{outcome:?}"
    );
}

/// **A join whose inner side is a derived table** — the other way the positional read broke, and
/// the one that would have survived deduplication: a derived table contributes a name the catalog
/// does not have, so it was *dropped* from the list and every entry after it moved up one.
#[test]
fn a_join_onto_a_derived_table_describes() {
    let mut node = parity::Node::new(FIXTURE);
    let sql = "SELECT \"topics\".\"id\", \"r\".\"n\" FROM \"topics\" \
               INNER JOIN (SELECT parent_id AS n FROM topics) AS \"r\" ON \"r\".\"n\" = \"topics\".\"id\"";

    assert_eq!(described(&mut node, sql).unwrap(), vec!["id", "n"]);
    assert_eq!(node.rows(sql), vec![vec!["1".to_owned(), "1".to_owned()]]);
}

/// **Run 51's second shape, and it is this same bug**: `missing FROM-clause entry` — 30 tests in
/// 15 files.
///
/// `Rating.joins(:comment).includes(comment: :post).where(...)` sends a three-join chain in which
/// `comments` appears twice, once plain and once as `comments_ratings`. De-duplicating the name
/// list dropped the second, so the third join — `posts`, which is right there in the `FROM` — was
/// paired with the *second* entry's table, and the qualifier `posts` resolved against `comments`.
/// Hence a `42P01` whose `HINT` offers the alias of a different table entirely, which is what made
/// the two shapes look like two bugs.
///
/// Verbatim from the run's `sql.active_record` log, with the fixture cut to what it names.
#[test]
fn a_chain_of_joins_repeating_a_table_keeps_every_entry() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE posts (id bigserial primary key, title varchar)",
        "CREATE TABLE comments (id bigserial primary key, post_id bigint, developer_id bigint, body varchar)",
        "CREATE TABLE ratings (id bigserial primary key, comment_id bigint)",
        "INSERT INTO posts (title) VALUES ('p')",
        "INSERT INTO comments (post_id, developer_id, body) VALUES (1, 7, 'b')",
        "INSERT INTO ratings (comment_id) VALUES (1)",
    ]);
    let sql = "SELECT COUNT(DISTINCT \"ratings\".\"id\") FROM \"ratings\" \
               INNER JOIN \"comments\" ON \"ratings\".\"comment_id\" = \"comments\".\"id\" \
               LEFT OUTER JOIN \"comments\" \"comments_ratings\" ON \"comments_ratings\".\"id\" = \"ratings\".\"comment_id\" \
               LEFT OUTER JOIN \"posts\" ON \"posts\".\"id\" = \"comments_ratings\".\"post_id\" \
               WHERE \"comments\".\"developer_id\" = $1 AND \"comments\".\"body\" = $2 AND \"posts\".\"id\" = $3";

    let parsed = parse_statements(sql).unwrap();
    // It was `42P01 invalid reference to FROM-clause entry for table "posts"`, hinting at
    // `comments_ratings` — an alias of a table `posts` has nothing to do with.
    node.executor.describe(&parsed[0], &[]).unwrap();
    let outcome = node
        .executor
        .execute(
            &parsed[0],
            &Params {
                values: &[
                    Some(b"7".to_vec()),
                    Some(b"b".to_vec()),
                    Some(b"1".to_vec()),
                ],
                formats: &[],
                declared: &[],
                bound: true,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, esker_sql::pgwire::session::Outcome::Rows { ref rows, .. } if rows == &[vec![Some(b"1".to_vec())]]),
        "{outcome:?}"
    );
}
