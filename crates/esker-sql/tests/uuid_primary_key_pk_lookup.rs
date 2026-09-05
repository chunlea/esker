//! **A UUID primary key has a name and no sequence**, and `pk_and_sequence_for` must say so.
//!
//! `uuid_test.rb`'s `test_pk_and_sequence_for_uuid_primary_key` asks for `["id", nil]` and got
//! `[nil, nil]` here. The pk half is what fails: `ActiveRecord`'s dependency query finds nothing —
//! correctly, there is no sequence — and its **fallback** is what carries the primary key's name
//! out, because that query returns `attname` whether or not it finds a sequence.
//!
//! The fallback's filter is the part that matters for a UUID column:
//!
//! ```text
//! AND pg_get_expr(def.adbin, def.adrelid) ~* 'nextval|uuid_generate|gen_random_uuid'
//! ```
//!
//! and its `CASE` then answers NULL for the sequence, because the default does not match
//! `nextval`. Measured on 19beta1 against `id uuid PRIMARY KEY DEFAULT uuid_generate_v1()`:
//! `id | public | <null>`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    // `schema.rb` says `enable_extension "uuid-ossp"`, and both functions below are its.
    "CREATE EXTENSION \"uuid-ossp\"",
    "CREATE TABLE g1_uuids (id uuid PRIMARY KEY DEFAULT uuid_generate_v1(), \
     name character varying, other_uuid uuid DEFAULT uuid_generate_v4())",
];

/// `ActiveRecord`'s fallback query, verbatim.
const FALLBACK: &str = "SELECT attr.attname, nsp.nspname, \
     CASE \
       WHEN pg_get_expr(def.adbin, def.adrelid) !~* 'nextval' THEN NULL \
       WHEN split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2) ~ '.' THEN \
         substr(split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2), \
                strpos(split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2), '.')+1) \
       ELSE split_part(pg_get_expr(def.adbin, def.adrelid), '''', 2) \
     END \
     FROM pg_class t \
     JOIN pg_attribute attr ON (t.oid = attrelid) \
     JOIN pg_attrdef def ON (adrelid = attrelid AND adnum = attnum) \
     JOIN pg_constraint cons ON (conrelid = adrelid AND adnum = conkey[1]) \
     JOIN pg_namespace nsp ON (t.relnamespace = nsp.oid) \
     WHERE t.oid = '\"g1_uuids\"'::regclass AND cons.contype = 'p' \
     AND pg_get_expr(def.adbin, def.adrelid) ~* 'nextval|uuid_generate|gen_random_uuid'";

/// The row the test's `["id", nil]` is built from.
#[test]
fn the_fallback_names_the_uuid_primary_key_and_no_sequence() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(FALLBACK),
        [["id".to_owned(), "public".to_owned(), "\\N".to_owned(),]],
        "the column, its schema, and no sequence"
    );
}

/// **The filter on its own**, so a failure says which half broke. Three alternatives, and the one
/// that matches here is the second.
#[test]
fn the_alternation_matches_a_uuid_default() {
    let mut node = parity::Node::new(FIXTURE);
    for (default, matches) in [
        ("uuid_generate_v1()", "t"),
        ("uuid_generate_v4()", "t"),
        ("gen_random_uuid()", "t"),
        ("nextval(''s''::regclass)", "t"),
        ("now()", "f"),
    ] {
        assert_eq!(
            node.rows(&format!(
                "SELECT '{default}' ~* 'nextval|uuid_generate|gen_random_uuid'"
            )),
            [[matches.to_owned()]],
            "{default}"
        );
    }
}

/// And the `CASE`'s own answer: a default that is not a `nextval` has no sequence in it.
#[test]
fn a_uuid_default_yields_no_sequence_name() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT CASE WHEN 'uuid_generate_v1()' !~* 'nextval' THEN NULL \
             ELSE split_part('uuid_generate_v1()', '''', 2) END"
        ),
        [["\\N".to_owned()]]
    );
}
