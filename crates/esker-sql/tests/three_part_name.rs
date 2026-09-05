//! **`schema.table.column`, and `schema.table.*`.**
//!
//! `schema_test.rb`'s new head after the `DROP SCHEMA` fix: three tests write a relation's schema
//! into a column reference.
//!
//! ```ruby
//! Thing1.where("test_schema.things.name": "thing1")   # SELECT … WHERE "test_schema"."things"."name" = …
//! Thing1.pluck(:"test_schema.things.name")            # SELECT "test_schema"."things"."name" FROM …
//! Song.joins(:albums).pluck("albums.id")              # "music"."albums_songs"."song_id" in the join
//! ```
//!
//! # Two parsers of one grammar, and only one of them was wrong
//!
//! The census found the refusals came from **two** places, and that everything reached through
//! `relation_name` already worked:
//!
//! ```text
//! SELECT "s"."t"."c" FROM "s"."t"     the qualified column   lower_expr's CompoundIdentifier
//! SELECT "s"."t".*   FROM "s"."t"     the qualified name     object_name, via QualifiedWildcard
//! SELECT c FROM "s"."t"               works                  relation_name
//! SELECT * FROM "s"."t"               works
//! UPDATE "s"."t" SET …                works
//! ```
//!
//! # Measured on 19beta1
//!
//! ```text
//! SELECT s1.things.name FROM s1.things            ok
//! SELECT s1.things.*    FROM s1.things            ok
//! SELECT public.pub.name FROM pub                 ok   <- the FROM is bare and it still resolves
//! SELECT pub.name FROM public.pub                 ok
//! SELECT s2.things.name FROM s1.things            42P01 invalid reference … DETAIL: There is an
//!                                                       entry for table "things", but …
//! SELECT s1.things.name FROM s1.things t          42P01 invalid reference … HINT: Perhaps you
//!                                                       meant to reference the table alias "t".
//! ```
//!
//! **The public row is why the qualifier cannot be compared as text.** A `public` table's stored
//! name is bare here, so `public.pub_things` has to resolve to it — the schema is *resolved*, not
//! matched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA s1",
    "CREATE TABLE s1.things (id int8, name text)",
    "CREATE SCHEMA s2",
    "CREATE TABLE s2.things (id int8, name text)",
    "CREATE TABLE pub_things (id int8, name text)",
    "INSERT INTO s1.things VALUES (1, 'a')",
    "INSERT INTO pub_things VALUES (1, 'p')",
];

/// The three shapes the suite writes, in a non-public schema.
#[test]
fn a_three_part_column_resolves_in_a_non_public_schema() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT s1.things.name FROM s1.things"),
        [["a".to_owned()]]
    );
    assert_eq!(
        node.rows(r#"SELECT "s1"."things"."name" FROM "s1"."things""#),
        [["a".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT id FROM s1.things WHERE s1.things.name = 'a'"),
        [["1".to_owned()]]
    );
}

/// **The public control**, which is the half a textual comparison gets wrong: a `public` table's
/// stored name is bare, so `public.pub_things` has to *resolve* rather than match.
#[test]
fn a_three_part_column_resolves_in_public_too() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT public.pub_things.name FROM pub_things"),
        [["p".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT pub_things.name FROM public.pub_things"),
        [["p".to_owned()]]
    );
}

/// `schema.table.*` — the other parser, and the one `object_name` refused.
#[test]
fn a_three_part_wildcard_expands() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT s1.things.* FROM s1.things"),
        [["1".to_owned(), "a".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT public.pub_things.* FROM pub_things"),
        [["1".to_owned(), "p".to_owned()]]
    );
}

/// **The two refusals a real server keeps**, and they are different sentences: a qualifier naming
/// another schema is `invalid reference` with a `DETAIL`, and one naming a table an alias replaced
/// is `invalid reference` with a `HINT`.
#[test]
fn the_qualifier_still_has_to_name_this_from_item() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "SELECT s2.things.name FROM s1.things",
        "SELECT s1.things.name FROM s1.things t",
    ] {
        let answer = node.answer(sql).to_string();
        assert!(
            answer.starts_with("!42P01"),
            "expected a 42P01 about the FROM entry, got {answer} for {sql}"
        );
    }
}
