//! **A column default names its sequence the way `::regclass` prints a relation** — debt #95,
//! held to PostgreSQL 19's answers.
//!
//! `nextval('<sequence>'::regclass)` is a `regclass` constant, and PostgreSQL prints one bare when
//! its schema is on the search path and qualified when it is not — `public` included. This node
//! printed a sequence outside `public` qualified and one in `public` bare whatever the path was, so
//! a schema dump taken with the schema on the path wrote `nextval('s.t_id_seq'::regclass)` where a
//! real server writes `nextval('t_id_seq'::regclass)`.
//!
//! `corpus/pg19_default_sequence_name.txt` is the capture and the replay is the test of record.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_default_sequence_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_default_sequence_name.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Bare on the path, qualified off it, `public` too** — through `pg_get_expr` and through
/// `information_schema.columns`, which read one default.
#[test]
fn a_default_names_its_sequence_as_the_search_path_sees_it() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s",
        "CREATE TABLE s.t (id serial, v text)",
        "CREATE TABLE p (id serial)",
    ]);
    let default_of = |table: &str| {
        format!(
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = '{table}'::regclass"
        )
    };
    assert_eq!(
        node.rows(&default_of("s.t")),
        [["nextval('s.t_id_seq'::regclass)"]]
    );
    node.run("SET search_path = s, public").unwrap();
    assert_eq!(
        node.rows(&default_of("s.t")),
        [["nextval('t_id_seq'::regclass)"]]
    );
    assert_eq!(
        node.rows(
            "SELECT column_default FROM information_schema.columns WHERE table_schema = 's' AND \
             table_name = 't' AND column_name = 'id'"
        ),
        [["nextval('t_id_seq'::regclass)"]]
    );
    node.run("SET search_path = s").unwrap();
    assert_eq!(
        node.rows(&default_of("public.p")),
        [["nextval('public.p_id_seq'::regclass)"]]
    );
}
