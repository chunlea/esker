//! `CREATE SCHEMA … CREATE TABLE …` — the nested form, and where each element lands.
//!
//! r1's capture, which pins the half `pg19_schema.txt` reaches only through this form.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schemas.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "CREATE SCHEMA test_schema CREATE TABLE things (id integer,name character varying(50),email character varying(50),description character varying(100),name_vector tsvector,moment timestamp without time zone default now())",
            "**Not the schema form**: `tsvector` is a type this node does not have, and the column \
             is `schema_test.rb`\u{2019}s full-text one. The split itself worked — the refusal is the \
             type, reached *inside* the element, which is what a named refusal for the right thing \
             looks like",
        ),
        (
            "CREATE SCHEMA se_multi CREATE TABLE a (i int) CREATE TABLE b (j int) CREATE VIEW v AS SELECT 1 AS one",
            "**`CREATE VIEW` is an element PostgreSQL takes and this node has no views at all**, so \
             the split refuses to qualify it and the statement takes the ordinary path — where \
             `sqlparser` cannot read the nested form and answers `42601`. A C1 gap on top of a \
             missing feature. Passing the element through *unqualified* instead would be worse for \
             the elements this node **does** have: a `CREATE SEQUENCE` would land in `public` \
             where a real server puts it in the schema, which is a wrong answer rather than a gap. \
             `TABLE`, `INDEX` and `SEQUENCE` elements are qualified and run",
        ),
    ],
};

#[test]
fn every_create_schema_element_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_create_schema_elements.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A semicolon ends the form**, and what follows is an ordinary statement in the current schema.
#[test]
fn a_semicolon_ends_the_schema_statement() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE SCHEMA se_semi CREATE TABLE t (i int); CREATE TABLE u (j int)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = \
             c.relnamespace WHERE n.nspname = 'se_semi' ORDER BY c.relname"
        ),
        [["se_semi", "t"]]
    );
    // `u` went to `public`, not into the schema.
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
             n.nspname = 'public' AND c.relname = 'u'"
        ),
        [["1"]]
    );
}

/// **An index element follows its table into the schema**, and it is the name after `ON` that
/// moves — not the index's own.
#[test]
fn an_index_element_lands_beside_its_table() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA se_idx CREATE TABLE t (i int) CREATE UNIQUE INDEX t_i_idx ON t (i)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
             n.nspname = 'se_idx' ORDER BY c.relname"
        ),
        vec![vec!["t"], vec!["t_i_idx"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('se_idx.t_i_idx'::regclass)"),
        [["CREATE UNIQUE INDEX t_i_idx ON se_idx.t USING btree (i)"]]
    );
}
