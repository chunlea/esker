//! `DROP SEQUENCE` — statement 753 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds what it drops.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `relname` is a `name` on a real server and `relkind` a `"char"`; both are `text` here, with
    // identical characters. The standing trade every `pg_catalog` column makes.
    types: &["SELECT relname, relkind FROM pg_class WHERE relname = 'seqco_id_seq'"],
    answers: &[
        // **Both entries here are deleted** (ADR 0031, rule 2). They said a free-standing
        // sequence could not be created, so `DROP SEQUENCE` never met the one shape that needs no
        // `CASCADE`: a sequence with no dependent default. `CREATE SEQUENCE` landed in the next
        // commit and both lines agree, which is what the entries predicted.
    ],
};

#[test]
fn every_drop_sequence_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_drop_sequence.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 16,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The suite's own line: **absent, with `IF EXISTS`, is a success** — and that is statement 753.
#[test]
fn if_exists_on_an_absent_sequence_is_a_success() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "DROP SEQUENCE IF EXISTS companies_nonstd_seq CASCADE",
        "DROP SEQUENCE IF EXISTS companies_nonstd_seq",
    ] {
        node.run(statement).unwrap();
    }
    // Without the clause it is `42P01`, and the message says *sequence* rather than *relation*.
    let error = node.run("DROP SEQUENCE companies_nonstd_seq").unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(
        error.to_string(),
        "sequence \"companies_nonstd_seq\" does not exist"
    );
}

/// **`CASCADE` drops the default, and leaves the column.**
///
/// The consequence is the interesting part and it is what a node that dropped the column, or that
/// dropped nothing, would both get wrong: the `bigserial` column survives, still `NOT NULL`, with
/// no default to fill it — so an ordinary `INSERT` that never mentions it fails on it.
#[test]
fn cascade_drops_the_default_and_leaves_the_column() {
    let mut node = parity::Node::new(&["CREATE TABLE seqco (id bigserial primary key, name text)"]);
    node.run("DROP SEQUENCE seqco_id_seq CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_attrdef WHERE adrelid = 'seqco'::regclass"),
        [["0"]],
        "the default went with the sequence"
    );
    let error = node
        .run("INSERT INTO seqco (name) VALUES ('a')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23502");
    assert!(error.to_string().contains("\"id\""), "{error}");
}

/// Without `CASCADE`, a sequence a column depends on is `2BP01` — and the `DETAIL` names both.
#[test]
fn a_depended_on_sequence_needs_cascade() {
    let mut node = parity::Node::new(&["CREATE TABLE seqco (id bigserial primary key, name text)"]);
    let error = node.run("DROP SEQUENCE seqco_id_seq").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop sequence seqco_id_seq because other objects depend on it"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("default value for column id of table seqco depends on sequence seqco_id_seq")
    );
    assert!(
        error.hint().is_some_and(|hint| hint.contains("CASCADE")),
        "the HINT names the clause that would work: {error}"
    );
    // And the sequence is still there, still filling the column.
    node.run("INSERT INTO seqco (name) VALUES ('a')").unwrap();
    assert_eq!(node.rows("SELECT id FROM seqco"), [["1"]]);
}

/// A table is **`42809`, not `42P01`**: it was found, and it is the wrong kind.
#[test]
fn a_table_is_not_a_sequence() {
    let mut node = parity::Node::new(&["CREATE TABLE seqco (id bigserial primary key, name text)"]);
    let error = node.run("DROP SEQUENCE seqco").unwrap_err();
    assert_eq!(error.sqlstate(), "42809");
    assert_eq!(error.to_string(), "\"seqco\" is not a sequence");
    assert!(
        error.hint().is_some_and(|hint| hint.contains("DROP TABLE")),
        "the HINT names the statement that would work: {error}"
    );
}

/// **A list is all-or-nothing, and existence is checked before dependency.**
///
/// `DROP SEQUENCE a, b` with `b` absent drops neither and reports `b` — not the dependency `a`
/// has. The order is a real server's and is what a loop that dropped as it walked would get twice
/// wrong: it would name `a`, and it would have taken `a` already.
#[test]
fn a_list_is_all_or_nothing_and_reports_the_missing_name() {
    let mut node = parity::Node::new(&["CREATE TABLE seqt2 (id bigserial primary key, name text)"]);
    let error = node
        .run("DROP SEQUENCE seqt2_id_seq, nosuchseq")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(error.to_string(), "sequence \"nosuchseq\" does not exist");
    // Neither was dropped: the sequence still fills the column.
    node.run("INSERT INTO seqt2 (name) VALUES ('a')").unwrap();
    assert_eq!(node.rows("SELECT id FROM seqt2"), [["1"]]);
}
