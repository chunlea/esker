//! `search_path` — what an unqualified name finds, and what the two `current_schema` functions
//! answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schemas.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade: `current_schema()` is a `name` and `current_schemas` a `name[]`
    // on a real server, `text` here, with the same characters in them.
    types: &[
        "SELECT 'r', current_schema(), current_schemas(false), current_schemas(true)",
        "SELECT 'r', current_schema(), current_schemas(false)",
        "SELECT 'r', current_schemas(false)",
        "SELECT 'r', current_schema()",
        "SELECT 'r', current_schemas(false), current_schemas(true)",
        "SELECT 'r', n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 'made_here'",
    ],
    answers: &[(
        "SELECT 'r', pg_typeof(current_schema()), pg_typeof(current_schemas(false))",
        "The standing catalog trade made visible as a **row**, because `pg_typeof` returns the \
         type as a value: `name` and `name[]` there against `text` and `text` here. This node has \
         neither type, and every other line in this file shows the two functions answering the \
         same characters",
    )],
};

#[test]
fn every_search_path_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_search_path.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Order decides, and it is the order written** — the same query, twice, two answers.
#[test]
fn the_path_is_searched_in_order() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA sp_a CREATE TABLE t (id integer)",
        "CREATE SCHEMA sp_b CREATE TABLE t (id integer)",
        "INSERT INTO sp_a.t (id) VALUES (1)",
        "INSERT INTO sp_b.t (id) VALUES (2)",
    ]);
    node.run("SET search_path TO sp_b, sp_a").unwrap();
    assert_eq!(node.rows("SELECT id FROM t"), [["2"]]);
    node.run("SET search_path TO sp_a, sp_b").unwrap();
    assert_eq!(node.rows("SELECT id FROM t"), [["1"]]);
}

/// **`SHOW` gives the path as set; `current_schemas` gives it as resolved.** Two answers to one
/// question, and both are read.
#[test]
fn show_is_the_path_as_set_and_current_schemas_is_it_resolved() {
    let mut node = parity::Node::new(&["CREATE SCHEMA sp_b"]);
    assert_eq!(node.rows("SHOW search_path"), [["\"$user\", public"]]);
    // `$user` names no schema here, so the default resolves to `{public}` alone.
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{public}"]]);
    node.run("SET search_path TO nosuchschema, sp_b").unwrap();
    assert_eq!(node.rows("SHOW search_path"), [["nosuchschema, sp_b"]]);
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{sp_b}"]]);
    // **`current_schemas(true)` prepends `pg_catalog`**, and only that one.
    node.run("SET search_path TO public").unwrap();
    assert_eq!(
        node.rows("SELECT current_schemas(false), current_schemas(true)"),
        [["{public}", "{pg_catalog,public}"]]
    );
}

/// **`current_schema()` is NULL when nothing resolves** — not `public`, and not an error. A bare
/// name then finds nothing, and the message quotes the **bare** name even though the path failed.
#[test]
fn nothing_resolves_and_current_schema_is_null() {
    let mut node = parity::Node::new(&["CREATE SCHEMA sp_a CREATE TABLE t (id integer)"]);
    node.run("SET search_path TO nosuchschema").unwrap();
    assert_eq!(
        node.rows("SELECT current_schema(), current_schemas(false)"),
        [["\\N", "{}"]]
    );
    let error = node.run("SELECT id FROM t").unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(error.to_string(), "relation \"t\" does not exist");
}

/// **`CREATE` goes to the first schema of the path**, not to `public`.
#[test]
fn a_create_lands_in_the_first_schema_of_the_path() {
    let mut node = parity::Node::new(&["CREATE SCHEMA sp_a", "CREATE SCHEMA sp_b"]);
    node.run("SET search_path TO sp_a, sp_b").unwrap();
    node.run("CREATE TABLE made_here (id integer)").unwrap();
    assert_eq!(
        node.rows(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
             c.relname = 'made_here'"
        ),
        [["sp_a"]]
    );
    // The **first that resolves**: an entry naming nothing is skipped here too.
    node.run("SET search_path TO nosuchschema, sp_b").unwrap();
    node.run("CREATE TABLE also_here (id integer)").unwrap();
    assert_eq!(
        node.rows(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
             c.relname = 'also_here'"
        ),
        [["sp_b"]]
    );
}

/// `RESET` and `SET … TO DEFAULT` are the same statement, and both restore the boot value.
#[test]
fn reset_and_default_restore_the_boot_value() {
    let mut node = parity::Node::new(&["CREATE SCHEMA sp_a"]);
    node.run("SET search_path TO sp_a").unwrap();
    assert_eq!(node.rows("SELECT current_schema()"), [["sp_a"]]);
    node.run("RESET search_path").unwrap();
    assert_eq!(node.rows("SHOW search_path"), [["\"$user\", public"]]);
    assert_eq!(node.rows("SELECT current_schema()"), [["public"]]);
    node.run("SET search_path TO sp_a").unwrap();
    node.run("SET search_path TO DEFAULT").unwrap();
    assert_eq!(node.rows("SHOW search_path"), [["\"$user\", public"]]);
    // **A string literal is taken and the quotes are the literal's**, not part of the value.
    node.run("SET search_path TO 'sp_a'").unwrap();
    assert_eq!(node.rows("SHOW search_path"), [["sp_a"]]);
}

/// **`public` is unchanged.** With the default path, everything answers exactly as it did before
/// there were schemas.
#[test]
fn the_default_path_is_the_behaviour_it_always_was() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer)",
        "INSERT INTO t (id) VALUES (1)",
    ]);
    assert_eq!(node.rows("SELECT current_schema()"), [["public"]]);
    assert_eq!(node.rows("SELECT current_schemas(false)"), [["{public}"]]);
    assert_eq!(node.rows("SELECT id FROM t"), [["1"]]);
    assert_eq!(node.rows("SELECT id FROM public.t"), [["1"]]);
    assert_eq!(
        node.rows("SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = 't'"),
        [["public"]]
    );
}
