//! **`format_type` qualifies a user type exactly when it is off the `search_path`.**
//!
//! Schema-qualified types landed, and `enum_test` went `3F 0E → 3F 1E`: the schema dump started
//! writing the schema into the *column attribute* as well as into the `create_enum` line.
//!
//! ```text
//! t.enum "current_mood", enum_type: "test_schema.mood_in_test_schema"   <- what the node emitted
//! t.enum "current_mood", enum_type: "mood_in_test_schema"               <- what the test asserts
//! ```
//!
//! The two strings come from two different queries, which is why one had to change and the other
//! had to not. `create_enum` is built from `ActiveRecord#enum_types`, which selects `nspname`
//! beside the name and joins them in Ruby — it stays qualified, and
//! `test_schema_dump_scoped_to_schemas` wants it that way. The column attribute is
//! `column.sql_type`, which is `format_type(atttypid, atttypmod)`, and **PostgreSQL prints that
//! bare whenever the type is visible on the `search_path`** — the same rule `::regclass` follows,
//! and the one `exec::cursor::qualified_for` already implemented for relations.
//!
//! # Measured on 19beta1, one table and two enums, in one session and then a narrower one
//!
//! ```text
//! SET search_path = g1f_a, public    a g1f_a.mood    -> mood
//!                                    b g1f_b.hidden  -> g1f_b.hidden
//! SET search_path = public           a g1f_a.mood    -> g1f_a.mood
//!                                    b g1f_b.hidden  -> g1f_b.hidden
//! ```
//!
//! So the same column prints two ways in two sessions, which is what makes this a *session* rule
//! rather than a property of the type — and what a fix that simply stopped qualifying would have
//! got wrong in the other direction, for the column whose type is genuinely elsewhere.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &["CREATE SCHEMA g1f_a", "CREATE SCHEMA g1f_b"];

/// Both columns, in one session and then a narrower one.
fn sql_types(node: &mut parity::Node, relation: &str) -> Vec<String> {
    node.rows(&format!(
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
         WHERE a.attrelid = '{relation}'::regclass AND a.attnum > 0 ORDER BY a.attnum"
    ))
    .into_iter()
    .map(|row| row.join("="))
    .collect()
}

fn node() -> parity::Node {
    let mut node = parity::Node::new(FIXTURE);
    node.run("SET search_path TO g1f_a, public").unwrap();
    node.run("CREATE TYPE g1f_a.mood AS ENUM ('sad','ok')")
        .unwrap();
    node.run("CREATE TYPE g1f_b.hidden AS ENUM ('h')").unwrap();
    node.run("CREATE TABLE g1f_t (a g1f_a.mood, b g1f_b.hidden)")
        .unwrap();
    node
}

/// **On the path it is bare; off the path it is qualified** — in the same statement, so a fix that
/// answered one way for both would fail here whichever way it chose.
#[test]
fn a_type_on_the_path_prints_bare_and_one_off_it_does_not() {
    let mut node = node();
    assert_eq!(
        sql_types(&mut node, "g1f_t"),
        ["a=mood".to_owned(), "b=g1f_b.hidden".to_owned()]
    );
}

/// And the same column prints the other way once the path no longer reaches its schema.
#[test]
fn narrowing_the_path_qualifies_what_it_no_longer_reaches() {
    let mut node = node();
    node.run("SET search_path TO public").unwrap();
    assert_eq!(
        sql_types(&mut node, "g1f_a.g1f_t"),
        ["a=g1f_a.mood".to_owned(), "b=g1f_b.hidden".to_owned()]
    );
}

/// **The `create_enum` line keeps its schema**, which is the half this fix must not take with it:
/// it comes from `enum_types`, not from `format_type`, and `test_schema_dump_scoped_to_schemas`
/// asserts the qualified form.
#[test]
fn the_enum_types_query_still_reports_the_schema() {
    let mut node = node();
    assert_eq!(
        node.rows(
            "SELECT type.typname, n.nspname FROM pg_enum AS enum \
             JOIN pg_type AS type ON (type.oid = enum.enumtypid) \
             JOIN pg_namespace n ON type.typnamespace = n.oid \
             WHERE type.typname = 'mood' GROUP BY type.oid, n.nspname, type.typname"
        ),
        [["mood".to_owned(), "g1f_a".to_owned()]]
    );
}

/// A type in `public` is bare under the default path, which is every statement that came before
/// schemas existed and must not have moved.
#[test]
fn a_type_in_public_is_bare_under_the_default_path() {
    let mut node = parity::Node::new(&[
        "CREATE TYPE g1f_plain AS ENUM ('a')",
        "CREATE TABLE g1f_p (c g1f_plain)",
    ]);
    assert_eq!(sql_types(&mut node, "g1f_p"), ["c=g1f_plain".to_owned()]);
}
