//! `pg_catalog.f(…)` — boot statements 29, 31 and 32.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Empty, and it was not.** This held one entry saying `length` "is a scalar function this
    // node does not have". That stopped being true when `length` was implemented, and the row kept
    // diverging for a *different* reason than the one written down — the declared type was `text`
    // where a real server says `integer`. Typing the counting functions correctly closed the gap
    // and the harness demanded the entry go, which is ADR 0031 rule 2 doing its job: a declared
    // divergence that starts agreeing is deleted, not kept.
    //
    // What the entry was really for is tested directly below: a missing function is refused under
    // its **bare** name, with the qualifier stripped first.
    answers: &[],
};

#[test]
fn every_qualified_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_qualified_function.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 9,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The qualifier is **checked**, and a different one refuses.
///
/// The failure this prevents: stripping whatever schema was written. These functions live in
/// `pg_catalog` and nowhere else, so `public.obj_description(…)` is `42883` on a real server —
/// and a node that accepted it would answer where a real server raises.
#[test]
fn only_pg_catalog_qualifies_and_the_refusal_names_what_was_written() {
    let mut node = parity::Node::new(&["CREATE TABLE qf (id int8 PRIMARY KEY)"]);
    assert_eq!(
        node.rows("SELECT pg_catalog.obj_description('qf'::regclass)"),
        [["\\N"]]
    );
    let error = node
        .run("SELECT public.obj_description('qf'::regclass)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert!(
        error.to_string().contains("public.obj_description"),
        "the refusal names the spelling the user wrote: {error}"
    );
}

/// Unquoted it folds case and quoted it still matches, because the schema is `pg_catalog` in
/// lower case either way.
#[test]
fn the_qualifier_matches_the_way_a_schema_name_does() {
    let mut node = parity::Node::new(&["CREATE TABLE qf (id int8 PRIMARY KEY)"]);
    for written in [
        "pg_catalog.obj_description",
        "PG_CATALOG.obj_description",
        "\"pg_catalog\".obj_description",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT {written}('qf'::regclass)")),
            [["\\N"]],
            "for {written}"
        );
    }
}

/// The strip happens **before** the name is resolved, so a function this node does not have is
/// named bare — which is the name a reader can search for.
#[test]
fn a_missing_function_is_refused_under_its_bare_name() {
    let mut node = parity::Node::new(&[]);
    // `length` used to be the example here and is implemented now, so the test uses one that is
    // still missing — the property under test is the *naming*, not which function is absent.
    assert_eq!(node.rows("SELECT pg_catalog.length('abc')"), [["3"]]);
    let error = node.run("SELECT pg_catalog.soundex('abc')").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(
        error.to_string().contains("the function soundex"),
        "the refusal names `soundex`, not the qualified spelling: {error}"
    );
}
