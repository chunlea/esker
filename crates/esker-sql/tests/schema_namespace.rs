//! `CREATE SCHEMA` and the named-schema half of `schema_test.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schemas.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
///
/// **This corpus is evidence before it is a gate.** Three statements diverge and sixty-two more
/// are swallowed behind them, because a named schema is not one feature: it is a second namespace
/// in the catalog, and two of the three things blocking it are not schemas at all. Each is named
/// below, and the harness fails the day any of them starts agreeing — which is what makes the file
/// worth having now rather than when the work is done.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade: `current_schema` is a `name` on a real server and
    // `current_schemas(false)` a `name[]`; both are `text` here, with the same characters in them.
    // The rows agree — `public` and `{public}`.
    types: &[
        "SELECT 'r', current_schema",
        "SELECT 'r', current_schemas(false)",
    ],
    answers: &[
        (
            "SELECT 'r', nspname FROM pg_namespace WHERE nspname !~ '^pg_.*' AND nspname NOT IN ('information_schema') ORDER by nspname",
            "**Not a schema gap at all**: `!~` is PostgreSQL\u{2019}s regex non-match, and this node \
             has neither it nor `~`. It is the first statement `schema_names` sends, so it aborts \
             the block and takes the sixty-two after it — the same shape `LIKE`\u{2019}s absence had \
             before that landed. It belongs in `plan::Expr` beside `LIKE`, which is another \
             lane\u{2019}s file",
        ),
        (
            "CREATE SCHEMA test_schema CREATE TABLE things (id integer, name character varying(50), email character varying(50), description character varying(100), moment timestamp without time zone default now())",
            "**`CREATE SCHEMA … CREATE TABLE …` is one statement**, and `sqlparser` 0.62.0 reads \
             only the first half — `Expected: end of statement, found: CREATE`. A C1 parser gap, \
             and it is the spelling `schema_test.rb`\u{2019}s `setup` uses for both of its schemas, so \
             nothing in that file is reachable without it",
        ),
        (
            "CREATE SCHEMA test_schema2 CREATE TABLE things (id integer, name character varying(50), email character varying(50), description character varying(100), moment timestamp without time zone default now())",
            "The second of the pair, and the same gap. The two together are what make the file\u{2019}s \
             central fact — two tables called `things`, one per schema — even statable",
        ),
    ],
};

#[test]
fn every_schema_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_schema.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 50,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// What this node answers **today** for the three schema statements a real server takes, so that
/// the refusals are pinned rather than merely absent.
///
/// Every one of them is a named refusal (contract C2) rather than a wrong answer, which is the
/// property that matters while the namespace is not built: a client is told what is missing.
#[test]
fn a_named_schema_is_refused_rather_than_answered_wrongly() {
    let mut node = parity::Node::new(&[]);
    // The one schema this node has answers for itself, and both spellings are exact.
    assert_eq!(node.rows("SELECT current_schema"), [["public"]]);
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{public}"]]);
    // A `search_path` that resolves to `public` is honoured; one that names another schema is
    // `0A000` quoting the path back, rather than being accepted and quietly meaning `public`.
    node.run("SET search_path TO \"$user\", public").unwrap();
    let error = node.run("SET search_path TO test_schema").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    // A schema-qualified relation is refused by name, not resolved to a table of the same name in
    // the one schema there is — which would be a wrong answer rather than a gap.
    let error = node
        .run("CREATE TABLE test_schema.things (id integer)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
}
