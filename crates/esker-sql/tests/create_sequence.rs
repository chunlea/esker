//! `CREATE SEQUENCE` — statement 754 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds what it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `relkind` is a `"char"` on a real server and `relname` a `name`; both are `text` here, with
    // identical characters. The standing trade every `pg_catalog` column makes.
    types: &["SELECT relkind, relname FROM pg_class WHERE relname = 's1'"],
    answers: &[
        (
            "CREATE SEQUENCE s4 AS smallint",
            "`AS <type>` narrows the counter, and narrowing changes when a sequence runs out. \
             This node counts in `i64` whatever it fills; taking the word and counting wider \
             anyway would hand out a number the column cannot hold instead of the `22003` a real \
             server gives at the same point. Refused by name — contract C2.",
        ),
        (
            "CREATE SEQUENCE s5 MINVALUE 5 MAXVALUE 7 CYCLE",
            "`MINVALUE`, `MAXVALUE` and `CYCLE` each change what happens at an end this node's \
             counter does not have — a bound to stop at, or to wrap at. A node that read the \
             words and counted on regardless would run past the limit the user asked for and \
             would never wrap, so all three are refused by name (C2) rather than stored and \
             ignored. The refusal names the first one it meets.",
        ),
        (
            "SELECT currval('s1')",
            "**A capture artifact, not a divergence in the node.** `currval` is *session* state — \
             it is the last value this session drew from that sequence — and the capture script \
             runs one `psql` per statement, so every line meets a server that has never called \
             `nextval`. PostgreSQL therefore answers `55000` where this node, replaying the whole \
             corpus in one session, correctly answers the value it drew two lines earlier. The \
             session rule itself is checked in `tests/sequence.rs`; this line is kept because a \
             corpus that quietly dropped it would hide the limit of how these captures are made.",
        ),
    ],
};

#[test]
fn every_create_sequence_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_create_sequence.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 22,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The suite's own line, and the fact behind it: **`OWNED BY` is not a default.**
///
/// `companies.id` is already a `bigserial`, so after this the column owns two sequences and still
/// takes its value from the first. A node that pointed the column at the new one here would answer
/// the suite's *next* statement — `ALTER COLUMN … SET DEFAULT nextval(…)` — before it was asked,
/// and would look right until something checked which sequence the rows came from.
#[test]
fn owned_by_does_not_change_the_columns_default() {
    let mut node =
        parity::Node::new(&["CREATE TABLE companies (id bigserial primary key, name text)"]);
    node.run("CREATE SEQUENCE companies_nonstd_seq START 101 OWNED BY companies.id")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_attrdef WHERE adrelid = 'companies'::regclass"),
        [["1"]],
        "one default, and it is still the bigserial's"
    );
    // The row comes from the original sequence, which starts at 1 — not from the new one.
    node.run("INSERT INTO companies (name) VALUES ('a')")
        .unwrap();
    assert_eq!(node.rows("SELECT id FROM companies"), [["1"]]);
    // Both exist, and both are relations.
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM pg_class WHERE relname IN ('companies_id_seq', \
             'companies_nonstd_seq')"
        ),
        [["2"]]
    );
}

/// **`START n` is the first value handed out**, not the one before it.
#[test]
fn start_is_the_first_value() {
    let mut node = parity::Node::new(&["CREATE TABLE companies (id bigserial primary key)"]);
    node.run("CREATE SEQUENCE s START 101 OWNED BY companies.id")
        .unwrap();
    assert_eq!(node.rows("SELECT nextval('s')"), [["101"]]);
    assert_eq!(node.rows("SELECT nextval('s')"), [["102"]]);
}

/// A name collides against the **whole** relation namespace, and `IF NOT EXISTS` covers it.
#[test]
fn a_name_collision_is_42p07() {
    let mut node = parity::Node::new(&["CREATE TABLE companies (id bigserial primary key)"]);
    node.run("CREATE SEQUENCE s OWNED BY companies.id").unwrap();
    for written in ["CREATE SEQUENCE s", "CREATE SEQUENCE companies"] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "42P07", "for {written}");
    }
    node.run("CREATE SEQUENCE IF NOT EXISTS s").unwrap();
}

/// `OWNED BY` names a column, and **both halves can be wrong** with different codes.
#[test]
fn owned_by_checks_the_table_and_the_column() {
    let mut node = parity::Node::new(&["CREATE TABLE companies (id bigserial primary key)"]);
    let error = node
        .run("CREATE SEQUENCE s OWNED BY companies.nosuchcol")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42703");
    assert_eq!(
        error.to_string(),
        "column \"nosuchcol\" of relation \"companies\" does not exist"
    );
    let error = node
        .run("CREATE SEQUENCE s OWNED BY nosuchtable.id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(error.to_string(), "relation \"nosuchtable\" does not exist");
}
