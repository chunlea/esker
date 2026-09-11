//! **`lquery[]` is a type now** — [ADR 0107](../../../docs/adr/0107-a-borrowed-representation-needs-somewhere-to-carry-its-identity.md)
//! step 1, and the half of family F6 that is a type rather than a model.
//!
//! `lquery` is the `ltree` extension's **pattern** type: category `U`, its own I/O
//! (`lquery_in`/`lquery_out`), and a `typarray`. This node had the scalar and not the array, so
//! `ARRAY['a.*'::lquery]` was a `text[]` — a wrong *type* rather than a refusal, which is the
//! direction ADR 0031 prefers and still a wrong answer to a client that decodes by oid.
//!
//! # What the ADR separates, and why only half of it is here
//!
//! `int2vector` and `oidvector` sit beside `lquery` in the same census family and are **not** this
//! step: they are arrays wearing a scalar's name — zero-based, with their own lower bound and an
//! `esker-keys` encoding nothing above the SQL layer can see. `lquery` needs none of that. It is
//! one variant and the places every array type has, which is why the ADR said it could land alone.
//!
//! # The oid is this node's own, and it is compared by name
//!
//! 19beta1 gave `lquery` **120271** and `_lquery` **120274** on the install this was measured
//! against — an extension's oids are assigned per install, so hardcoding one is the fault that
//! produced twelve UNASKED rows in the wire probe list. This node hands out its own in a reserved
//! block, in pairs: `hstore` 16400/16401, `citext` 16402/16403, `ltree` 16404/16405, and now
//! `lquery` 16406/**16407**.
//!
//! # The record format moved with it
//!
//! A `ColumnType` is a catalog record tag, and that map is an exhaustive `match` — so a new type
//! is a new tag (**107**) and a new `CATALOG_FORMAT_VERSION` (**37**). Additive: the version is
//! written per record and readers accept a range from `OLDEST_TABLE_VERSION`, so a v37 reader
//! reads everything v2 wrote. The number was claimed out loud before it was written
//! (`esker-coord/b4-claims-format-37.md`), because `record.rs` says it is a shared resource
//! between branches and it has collided twice.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS ltree"])
}

/// **Every shape that asks an element for its array type**, which is what F6's census asked.
#[test]
fn an_lquery_array_is_an_lquery_array() {
    let mut node = node();
    for written in [
        "ARRAY['a.*'::lquery]",
        "ARRAY['a.*'::lquery, 'b.*'::lquery]",
        "'{a.*}'::lquery[]",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({written})")),
            vec![vec!["lquery[]"]],
            "{written} is lquery[] on 19beta1"
        );
    }
    // The element comes back as itself, from a subscript and from `unnest` — the two shapes F5's
    // sweep asked of every type, and the two that read the *element* rather than the array.
    assert_eq!(
        node.rows("SELECT pg_typeof((ARRAY['a.*'::lquery])[1])"),
        vec![vec!["lquery"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(u) FROM unnest(ARRAY['a.*'::lquery]) u"),
        vec![vec!["lquery"]]
    );
}

/// **The value survives the round trip**, which is the half a type name does not prove.
#[test]
fn an_lquery_array_keeps_its_patterns() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT (ARRAY['a.*'::lquery, '*.b'::lquery])::text"),
        vec![vec!["{a.*,*.b}"]]
    );
    assert_eq!(
        node.rows("SELECT ('{a.*,*.b}'::lquery[])::text"),
        vec![vec!["{a.*,*.b}"]],
        "and the literal form reads back the same"
    );
    // **The element's input function still validates.** An `lquery` is a checked string, and the
    // array must not be a way around that — `array_in` reads each element with the element's
    // reader, which is the whole reason this type needed nothing but its places.
    assert!(
        node.answer("SELECT '{a..b}'::lquery[]")
            .to_string()
            .starts_with('!'),
        "a malformed pattern inside an array is still refused"
    );
}

/// **`pg_type` reports it the way it reports `ltree[]`**, because a client finds an extension type
/// by name and then reads its `oid`, `typelem` and `typcategory` off that row.
#[test]
fn pg_type_carries_the_array_beside_its_element() {
    let mut node = node();
    assert_eq!(
        node.rows(
            "SELECT t.typname, t.typcategory, e.typname \
             FROM pg_type t JOIN pg_type e ON e.oid = t.typelem \
             WHERE t.typname = '_lquery'"
        ),
        vec![vec!["_lquery", "A", "lquery"]],
        "the internal name is `_lquery`, the category is A, and its element is the pattern"
    );
    assert_eq!(
        node.rows("SELECT typarray::text FROM pg_type WHERE typname = 'lquery'"),
        node.rows("SELECT oid::text FROM pg_type WHERE typname = '_lquery'"),
        "**a base type whose `typarray` is 0 is what cost run 53 its 43 tests**, so the pair has \
         to point at each other"
    );
}

/// **A column of it**, which is what made this a record-format change rather than a type name.
///
/// No `ltree_test.rb` declares one — the ADR says so — and the type exists all the same, because
/// `typarray` is read whether or not a column is ever declared. This asserts the storage anyway:
/// a tag that encodes and does not decode is the shape `esker-keys::columnar`'s reverse map has.
#[test]
fn a_column_of_lquery_arrays_stores_and_reads_back() {
    let mut node = node();
    node.run("CREATE TABLE p (id bigint primary key, pats lquery[])")
        .unwrap();
    node.run("INSERT INTO p VALUES (1, '{a.*,*.b}')").unwrap();
    assert_eq!(
        node.rows("SELECT (pats)::text FROM p"),
        vec![vec!["{a.*,*.b}"]]
    );
    assert_eq!(
        node.rows(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = 'p'::regclass AND a.attname = 'pats'"
        ),
        vec![vec!["lquery[]"]],
        "and the catalog reports the declared type, which is what a schema dump writes back"
    );
}
