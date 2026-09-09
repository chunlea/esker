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
    // The standing catalog trade: `nspname` and `relname` are a `name` on a real server and
    // `text` here, `current_schemas(false)` a `name[]`. Every row agrees, `test_schema` included.
    types: &[
        "SELECT 'r', current_schema",
        "SELECT 'r', current_schemas(false)",
        "SELECT 'r', n.nspname, c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname LIKE 'test_schema%' ORDER BY n.nspname, c.relname",
    ],
    answers: &[
        (
            "DROP SCHEMA test_schema",
            "**Which dependent the `DETAIL` names.** Both refuse, with the same code and the same \
             sentence; PostgreSQL names `things` and this node names `Things`, because the schema \
             holds four relations and the two pick a different one — a real server walks its \
             dependency entries and this walks the name records, which are in byte order and put a \
             capital `T` first. Neither order is a contract, and both name a relation that really \
             is in the way",
            "pg19_create_schema_elements.txt:89",
        ),
        (
            "SELECT 'r', pg_typeof(current_schema), pg_typeof(current_schemas(false))",
            "The standing catalog trade made visible as a **row**, because `pg_typeof` returns the \
             type as a value: `name` and `name[]` there against `text` and `text` here. The values \
             those functions answer are identical, which every other line in this file shows",
            "pg19_schema.txt:114",
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

/// **`CREATE SCHEMA` makes a real catalog object**, and `pg_namespace` reports it beside the three
/// that are properties of the build.
#[test]
fn a_created_schema_is_a_row_in_pg_namespace() {
    let mut node = parity::Node::new(&[]);
    // None of these three is a record. `public` is where a user's relations go; `pg_catalog` and
    // `information_schema` are where the catalog's own live, and a real server reports all three
    // before anybody has created anything.
    assert_eq!(
        node.rows("SELECT nspname FROM pg_namespace ORDER BY nspname"),
        vec![
            vec!["information_schema"],
            vec!["pg_catalog"],
            vec!["public"]
        ]
    );
    node.run("CREATE SCHEMA test_schema").unwrap();
    assert_eq!(
        node.rows("SELECT COUNT(*) FROM pg_namespace WHERE nspname = 'test_schema'"),
        [["1"]]
    );
    // `schema_names`, the statement that gates the whole adapter — and it is **unchanged** by the
    // two arriving, which is the point of them: `ActiveRecord` filters out exactly `pg_%` and
    // `information_schema`, so a node that reports them answers this the same as one that hides
    // them, and answers `tables()` correctly instead of by accident.
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
        [["4"]]
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

/// **Two tables of one name, one per schema** — the fact a catalog keyed by name alone cannot
/// hold, and the centre of `schema_test.rb`.
#[test]
fn two_schemas_hold_two_tables_of_one_name() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA test_schema",
        "CREATE SCHEMA test_schema2",
        "CREATE TABLE test_schema.things (id integer, name character varying(50))",
        "CREATE TABLE test_schema2.things (id integer, name character varying(50))",
        "INSERT INTO test_schema.things (id, name) VALUES (1, 'one')",
        "INSERT INTO test_schema2.things (id, name) VALUES (2, 'two')",
    ]);
    // Each holds its own row, and neither can see the other's.
    assert_eq!(
        node.rows("SELECT id, name FROM test_schema.things"),
        [["1", "one"]]
    );
    assert_eq!(
        node.rows("SELECT id, name FROM test_schema2.things"),
        [["2", "two"]]
    );
    // `pg_class` has both, told apart by `relnamespace` — which is what joins it to
    // `pg_namespace`, and the only thing that distinguishes them.
    assert_eq!(
        node.rows(
            "SELECT c.relname, n.nspname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = \
             c.relnamespace WHERE c.relname = 'things' AND c.relkind IN ('r','v','m','p','f') \
             ORDER BY n.nspname"
        ),
        vec![
            vec!["things", "test_schema"],
            vec!["things", "test_schema2"],
        ]
    );
    // Two indexes of one name, one per schema — the other half of the same fact.
    node.run("CREATE INDEX a_index_things_on_name ON test_schema.things (name)")
        .unwrap();
    node.run("CREATE INDEX a_index_things_on_name ON test_schema2.things (name)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'a_index_things_on_name'"),
        [["2"]]
    );
}

/// **`public` keeps every behaviour it had.** A bare name is stored with no separator at all, so
/// the key bytes, the catalog rows and the messages are the ones they were.
#[test]
fn a_relation_in_public_is_unchanged() {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema"]);
    node.run("CREATE TABLE things (id integer)").unwrap();
    node.run("CREATE TABLE test_schema.things (id integer)")
        .unwrap();
    // The bare name is `public`'s, and the qualified spelling of it is the same relation.
    node.run("INSERT INTO things (id) VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT id FROM public.things"), [["1"]]);
    assert_eq!(
        node.rows("SELECT count(*) FROM test_schema.things"),
        [["0"]]
    );
    assert_eq!(
        node.rows(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
             c.relname = 'things' ORDER BY n.nspname"
        ),
        vec![vec!["public"], vec!["test_schema"]]
    );
}

/// **A missing schema is `3F000` and a missing relation is `42P01`**, with the schema *inside* the
/// quotes.
#[test]
fn a_missing_schema_and_a_missing_relation_are_different_answers() {
    let mut node = parity::Node::new(&["CREATE SCHEMA test_schema"]);
    let error = node
        .run("CREATE TABLE nosuchschema.t (a integer)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "3F000");
    assert_eq!(error.to_string(), "schema \"nosuchschema\" does not exist");
    let error = node.run("SELECT a FROM nosuchschema.t").unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(
        error.to_string(),
        "relation \"nosuchschema.t\" does not exist"
    );
    // A schema that exists and a relation that does not is the ordinary `42P01`, qualified.
    let error = node.run("SELECT a FROM test_schema.nosuch").unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(
        error.to_string(),
        "relation \"test_schema.nosuch\" does not exist"
    );
}

/// **`DROP SCHEMA` without `CASCADE` is `2BP01` naming one dependent**, and with it takes the
/// relations.
#[test]
fn dropping_a_schema_needs_cascade_once_something_is_in_it() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA test_schema",
        "CREATE TABLE test_schema.things (id integer)",
        "INSERT INTO test_schema.things (id) VALUES (1)",
    ]);
    let error = node.run("DROP SCHEMA test_schema").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop schema test_schema because other objects depend on it"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("table test_schema.things depends on schema test_schema")
    );
    // **`IF EXISTS` covers absence and not dependence** — the same `2BP01` with the clause on.
    let error = node.run("DROP SCHEMA IF EXISTS test_schema").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    node.run("DROP SCHEMA test_schema CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'things'"),
        [["0"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_namespace WHERE nspname = 'test_schema'"),
        [["0"]]
    );
    // And nothing in `public` went with it.
    node.run("CREATE TABLE things (id integer)").unwrap();
}

/// A relation in a schema is dropped and altered by its qualified name, and its derived names —
/// the primary key's, an index's — are **its own**, in its own schema.
#[test]
fn a_qualified_relation_is_created_altered_and_dropped() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA test_schema",
        "CREATE TABLE test_schema.table_with_pk (id bigserial primary key)",
    ]);
    // `<table>_pkey` on the bare name, in the table's schema — not `test_schema.things_pkey`.
    assert_eq!(
        node.rows(
            "SELECT c.relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = \
             c.relnamespace WHERE c.relname = 'table_with_pk_pkey'"
        ),
        [["table_with_pk_pkey", "test_schema"]]
    );
    node.run("ALTER TABLE test_schema.table_with_pk ADD COLUMN name character varying")
        .unwrap();
    node.run("INSERT INTO test_schema.table_with_pk (name) VALUES ('a')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name FROM test_schema.table_with_pk"),
        [["a"]]
    );
    node.run("DROP TABLE test_schema.table_with_pk").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname LIKE 'table_with_pk%'"),
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
    // A path is **not validated**: an entry naming no schema is skipped, so the default resolves
    // to `{public}` and one naming nothing at all resolves to `{}`.
    node.run("SET search_path TO \"$user\", public").unwrap();
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{public}"]]);
    node.run("SET search_path TO test_schema").unwrap();
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{}"]]);
    // A schema-qualified relation whose schema is not there is `3F000` — the *schema* is what is
    // missing, and it is a different answer from a missing relation.
    let error = node
        .run("CREATE TABLE nosuchschema.things (id integer)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "3F000");
}
