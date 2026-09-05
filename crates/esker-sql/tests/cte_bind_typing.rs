//! A `$n` compared with a column that comes from a **CTE**, not from a table.
//!
//! `with_test.rb`, two tests, reported by r1 in run 102 as
//! `operator does not exist: integer > text`. The statement, from PostgreSQL 19's own log:
//!
//! ```sql
//! WITH "posts_with_tags" AS (SELECT * FROM "posts" WHERE "posts"."tags_count" > 0),
//!      "posts_with_tags_and_truthy" AS (SELECT * FROM posts_with_tags WHERE 1=1),
//!      "posts_with_tags_and_comments" AS (SELECT * FROM posts_with_tags_and_truthy
//!                                         WHERE tags_count > $1),
//!      "posts_with_tags_and_multiple_comments" AS (SELECT "posts".* FROM
//!             posts_with_tags_and_comments AS posts WHERE (legacy_comments_count > 1))
//! SELECT "posts"."id" FROM posts_with_tags_and_multiple_comments AS posts
//! ORDER BY "posts"."id" ASC
//! ```
//!
//! The `$1` sits in the **third** CTE and is compared with `tags_count`, whose type is two CTE
//! levels away from `posts.tags_count`. `bind::named_relations` skips a `FROM` entry that has no
//! `TableDef`, and a CTE reference has none — so nothing types the parameter and it keeps the
//! `text` fallback.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

fn node() -> bind::Node {
    let mut node = bind::Node::new();
    for setup in [
        "CREATE TABLE posts (id bigserial primary key, type varchar, tags_count int default 0, \
         legacy_comments_count int default 0)",
        "INSERT INTO posts (type, tags_count, legacy_comments_count) VALUES ('a', 3, 2)",
        "INSERT INTO posts (type, tags_count, legacy_comments_count) VALUES ('b', 0, 0)",
    ] {
        node.bound(setup, &[]).unwrap();
    }
    node
}

/// **One CTE deep**, which is the smallest statement with the defect in it.
#[test]
fn a_parameter_is_typed_from_a_cte_s_column() {
    let mut node = node();
    let through = node.answer(
        "WITH tagged AS (SELECT * FROM posts WHERE tags_count > 0) \
         SELECT id FROM tagged WHERE tags_count > $1",
        &[Some(b"1".to_vec())],
    );
    let direct = node.answer(
        "SELECT id FROM posts WHERE tags_count > $1",
        &[Some(b"1".to_vec())],
    );
    assert_eq!(
        through.to_string(),
        direct.to_string(),
        "a column of a CTE carries the type of the column it came from"
    );
}

/// The statement `with_test.rb` really sends, from the oracle's log.
#[test]
fn the_statement_with_test_sends_is_answered() {
    let mut node = node();
    let answered = node.answer(
        "WITH \"posts_with_tags\" AS (SELECT * FROM \"posts\" WHERE \"posts\".\"tags_count\" > 0), \
         \"posts_with_tags_and_truthy\" AS (SELECT * FROM posts_with_tags WHERE 1=1), \
         \"posts_with_tags_and_comments\" AS (SELECT * FROM posts_with_tags_and_truthy WHERE tags_count > $1), \
         \"posts_with_tags_and_multiple_comments\" AS (SELECT \"posts\".* FROM posts_with_tags_and_comments AS posts WHERE (legacy_comments_count > 1)) \
         SELECT \"posts\".\"id\" FROM posts_with_tags_and_multiple_comments AS posts ORDER BY \"posts\".\"id\" ASC",
        &[Some(b"0".to_vec())],
    );
    assert!(
        !answered.to_string().starts_with('!'),
        "refused: {answered}"
    );
}

/// **Where the fix stops, and that it stops safely.**
///
/// A CTE that *renames* a column publishes a name the relation underneath does not have, so the
/// outer name cannot stand for that relation and the parameter keeps the `text` fallback. This is a
/// divergence from PostgreSQL, which types `n` from `tags_count` and answers — declared here rather
/// than left to be discovered, and asserted so that the day someone widens the rule this test says
/// what changed.
///
/// The property that matters is the *shape* of the failure: it refuses. What must never happen is
/// the third possibility — typing `n` from whichever relation happened to be underneath and
/// answering with a comparison the user did not write.
#[test]
fn a_renaming_cte_is_not_typed_and_does_not_guess() {
    let mut node = node();
    let answered = node.answer(
        "WITH t AS (SELECT tags_count AS n FROM posts) SELECT n FROM t WHERE n > $1",
        &[Some(b"1".to_vec())],
    );
    assert_eq!(
        answered.to_string(),
        "!42883 operator does not exist: integer > text DETAIL: No operator of that name accepts \
         the given argument types. HINT: You might need to add explicit type casts.",
        "a renamed column is not the relation's column: it refuses rather than guessing"
    );
}

/// A derived table over **two** relations is skipped for the same reason: there is no single
/// relation the outer name can stand for, and picking one would type a parameter from a table the
/// column may not have come from.
#[test]
fn a_cte_over_two_relations_is_not_typed() {
    let mut node = node();
    node.bound(
        "CREATE TABLE tags (id bigserial primary key, tag varchar)",
        &[],
    )
    .unwrap();
    let answered = node.answer(
        "WITH t AS (SELECT * FROM posts, tags) SELECT id FROM t WHERE tags_count > $1",
        &[Some(b"1".to_vec())],
    );
    assert!(
        answered.to_string().starts_with("!42"),
        "two relations under one name is not resolvable here: {answered}"
    );
}
