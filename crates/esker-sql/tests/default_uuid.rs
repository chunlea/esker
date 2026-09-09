//! `DEFAULT gen_random_uuid()` — statement 710, and what the schema stopped on after the functions
//! landed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade: `attname` and `column_name` are `name` on a real server and
    // `column_default` is a `character varying`; all three are `text` here. **Every row agrees**,
    // `gen_random_uuid()` and `uuid_generate_v4()` printed byte for byte.
    types: &[],
    answers: &[],
};

#[test]
fn every_default_uuid_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_default_uuid.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 11,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Per row, not per statement and not per table** — which is the whole point of a volatile
/// default and the failure a folded one produces.
///
/// A default evaluated once at `CREATE TABLE` gives every row the same UUID, so the second insert
/// into statement 710's table would be a primary-key collision. Sixteen rows, sixteen values.
#[test]
fn a_volatile_default_is_evaluated_once_per_row() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE du (id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY, n int8)",
    ]);
    for n in 0..16 {
        node.run(&format!("INSERT INTO du (n) VALUES ({n})"))
            .unwrap();
    }
    assert_eq!(
        node.rows("SELECT count(DISTINCT id), count(*) FROM du"),
        [["16", "16"]]
    );
    // And in one multi-row statement, where a per-statement default would collide.
    node.run("INSERT INTO du (n) VALUES (100), (101), (102)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(DISTINCT id), count(*) FROM du"),
        [["19", "19"]]
    );
}

/// An explicit value wins, and the word `DEFAULT` takes a fresh one.
#[test]
fn an_explicit_value_overrides_and_default_does_not() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE du (id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY, n int8)",
    ]);
    node.run("INSERT INTO du (id, n) VALUES ('11111111-1111-4111-8111-111111111111', 1)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT n FROM du WHERE id = '11111111-1111-4111-8111-111111111111'"),
        [["1"]]
    );
    node.run("INSERT INTO du (id, n) VALUES (DEFAULT, 2)")
        .unwrap();
    assert_eq!(node.rows("SELECT count(DISTINCT id) FROM du"), [["2"]]);
}

/// The default of a column whose function needs an extension is refused **when the table is
/// created**, not when a row is inserted.
///
/// A real server resolves the expression at `CREATE TABLE`, so a table defaulting to
/// `uuid_generate_v4()` cannot exist before `uuid-ossp` does — and a node that deferred the check
/// would accept the table and fail every insert into it.
#[test]
fn the_default_is_resolved_when_the_table_is_created() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("CREATE TABLE du (id uuid DEFAULT uuid_generate_v4())")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    // `gen_random_uuid` is in core, so it needs no extension at all.
    node.run("CREATE TABLE ok (id uuid DEFAULT gen_random_uuid())")
        .unwrap();
    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    node.run("CREATE TABLE du (id uuid DEFAULT uuid_generate_v4())")
        .unwrap();
}
