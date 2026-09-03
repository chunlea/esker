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
        (
            "SELECT 'r', COUNT(*) FROM pg_namespace WHERE nspname = 'test_schema'",
            "A consequence of the CREATE SCHEMA gap above, not a divergence of its own: with no second namespace, a schema-qualified name is refused by name and the relations the setup would have created are not there. Every one of these closes with that unit.",
        ),
        (
            "SELECT 'r', nspname FROM pg_namespace WHERE nspname !~ '^pg_.*' AND nspname NOT IN ('information_schema') ORDER by nspname",
            "A consequence of the CREATE SCHEMA gap above, not a divergence of its own: with no second namespace, a schema-qualified name is refused by name and the relations the setup would have created are not there. Every one of these closes with that unit.",
        ),
        (
            "CREATE TABLE test_schema.\"things.table\" (id integer, name character varying(50))",
            "A consequence of the CREATE SCHEMA gap above, not a divergence of its own: with no second namespace, a schema-qualified name is refused by name and the relations the setup would have created are not there. Every one of these closes with that unit.",
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

/// **`CREATE SCHEMA` makes a real catalog object**, and `pg_namespace` reports it beside `public`.
#[test]
fn a_created_schema_is_a_row_in_pg_namespace() {
    let mut node = parity::Node::new(&[]);
    // `public` is not a record — it is a property of the build — and is there before anything is
    // created.
    assert_eq!(node.rows("SELECT nspname FROM pg_namespace"), [["public"]]);
    node.run("CREATE SCHEMA test_schema").unwrap();
    assert_eq!(
        node.rows("SELECT COUNT(*) FROM pg_namespace WHERE nspname = 'test_schema'"),
        [["1"]]
    );
    // `schema_names`, the statement that gates the whole adapter — and the `!~` in it now runs.
    assert_eq!(
        node.rows(
            "SELECT nspname FROM pg_namespace WHERE nspname !~ '^pg_.*' AND nspname NOT IN \
             ('information_schema') ORDER by nspname"
        ),
        vec![vec!["public"], vec!["test_schema"]]
    );
    // Every schema has its own oid, and they are distinct.
    assert_eq!(
        node.rows("SELECT count(DISTINCT oid) FROM pg_namespace"),
        [["2"]]
    );
}

/// **Three codes for three ways of naming a schema wrong**, and `3F000` is its own class.
#[test]
fn each_way_of_naming_a_schema_wrong_has_its_own_code() {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema"]);
    let error = node.run("CREATE SCHEMA test_schema").unwrap_err();
    assert_eq!(error.sqlstate(), "42P06");
    assert_eq!(error.to_string(), "schema \"test_schema\" already exists");
    // `IF NOT EXISTS` over one that is there is a **success**, not an error.
    node.run("CREATE SCHEMA IF NOT EXISTS test_schema").unwrap();
    let error = node.run("DROP SCHEMA nosuchschema").unwrap_err();
    assert_eq!(error.sqlstate(), "3F000");
    assert_eq!(error.to_string(), "schema \"nosuchschema\" does not exist");
    // `IF EXISTS` covers absence, and `CASCADE` beside it changes nothing about that.
    node.run("DROP SCHEMA IF EXISTS nosuchschema CASCADE")
        .unwrap();
}

/// `DROP SCHEMA` removes it, and `ALTER SCHEMA … RENAME TO` moves the name.
#[test]
fn a_schema_can_be_dropped_and_renamed() {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema", "CREATE SCHEMA test_schema2"]);
    node.run("ALTER SCHEMA test_schema2 RENAME TO test_schema3")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE 'test_schema%' ORDER BY nspname"
        ),
        vec![vec!["test_schema"], vec!["test_schema3"]]
    );
    // A rename onto a name that is taken is the same `42P06` a create gets.
    let error = node
        .run("ALTER SCHEMA test_schema3 RENAME TO test_schema")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P06");
    node.run("DROP SCHEMA test_schema CASCADE").unwrap();
    node.run("DROP SCHEMA test_schema3").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'test_schema%'"),
        [["0"]]
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
