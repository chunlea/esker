//! `INSERT … DEFAULT VALUES` — an `INSERT` with no source, which is what `ActiveRecord` emits for
//! a record saved with no attributes set.
//!
//! `abstract/database_statements.rb`'s `empty_insert_statement_value` returns the literal string
//! `"DEFAULT VALUES"`, so the two statements the suite sends are
//!
//! ```text
//! INSERT INTO "dvs" DEFAULT VALUES RETURNING "id"     -- a model with a primary key
//! INSERT INTO dv_nopk DEFAULT VALUES                  -- exec_insert with no returning
//! ```
//!
//! **It is not "insert a row of NULLs".** Every column default is applied, which is the one thing
//! an implementation reaching for the shortest path gets wrong — and it is what makes the statement
//! useful at all: a `bigserial` draws its number, a `DEFAULT 'dflt'` is stored, and only a column
//! with no default is NULL.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "INSERT INTO dvs DEFAULT VALUES, DEFAULT VALUES",
        "Both refuse with `42601`; the text differs. PostgreSQL's parser stops at the comma and \
         says `syntax error at or near \",\"`; `sqlparser` says `Expected: end of statement, \
         found: ,`. The message on a syntax error is `sqlparser`'s and has never been claimed to \
         be PostgreSQL's (`docs/plans/phase-6a.md` §1); what contract C1 promises is that a \
         statement PostgreSQL *accepts* is never `42601`, and this is one it refuses.",
        "pg19_insert_default_values.txt:79",
    )],
};

#[test]
fn every_insert_default_values_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_insert_default_values.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Every column default is applied, and the sequence draws its number.**
///
/// The shape `ActiveRecord` sends, with the `RETURNING` it sends: a table whose key is a
/// `bigserial` and whose other columns have defaults gets a whole row, not a row of NULLs.
#[test]
fn default_values_applies_every_default_rather_than_writing_nulls() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dvs (id bigserial primary key, n integer, s character varying DEFAULT 'dflt')",
    ]);

    assert_eq!(
        node.rows("INSERT INTO \"dvs\" DEFAULT VALUES RETURNING \"id\""),
        [["1"]]
    );
    assert_eq!(
        node.rows("INSERT INTO \"dvs\" DEFAULT VALUES RETURNING \"id\""),
        [["2"]]
    );
    // `n` has no default and is NULL; `s` has one and is not.
    assert_eq!(
        node.rows("SELECT id, n, s FROM dvs ORDER BY id"),
        [["1", "\\N", "dflt"], ["2", "\\N", "dflt"]]
    );
}

/// **A table with no primary key and no defaults at all still gets a row.**
///
/// The second statement the suite sends, and the one that proves `DEFAULT VALUES` is not a
/// no-op when there is nothing to default: `count(*)` is 1 while `count(n)` is 0, so a row of one
/// NULL is a row.
#[test]
fn a_table_with_no_key_and_no_default_gets_a_row_of_null() {
    let mut node = parity::Node::new(&["CREATE TABLE dv_nopk (n integer)"]);

    node.run("INSERT INTO dv_nopk DEFAULT VALUES").unwrap();
    assert_eq!(
        node.rows("SELECT count(*), count(n) FROM dv_nopk"),
        [["1", "0"]]
    );
    // And `RETURNING` over it answers the NULL rather than no row.
    assert_eq!(
        node.rows("INSERT INTO dv_nopk DEFAULT VALUES RETURNING n"),
        [["\\N"]]
    );
}

/// `RETURNING` composes with it in all three forms the suite and the capture use.
#[test]
fn returning_composes_in_every_form() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dvs (id bigserial primary key, n integer, s character varying DEFAULT 'dflt', t character varying DEFAULT 'fixed')",
    ]);

    assert_eq!(
        node.rows("INSERT INTO \"dvs\" DEFAULT VALUES RETURNING \"id\", \"n\", \"s\""),
        [["1", "\\N", "dflt"]]
    );
    assert_eq!(
        node.rows("INSERT INTO \"dvs\" DEFAULT VALUES RETURNING *"),
        [["2", "\\N", "dflt", "fixed"]]
    );
}

/// **A NOT NULL column with no default is `23502`, and the key in the `DETAIL` has already
/// advanced.**
///
/// Both halves are measured. The statement fails, and the `bigserial` it consumed on the way to
/// failing is *not* given back — so the two failures print `(1, null)` and then `(2, null)`, which
/// is what says the sequence moved. A node that validated before drawing the number would print
/// `(1, null)` twice and would be reporting a different database.
#[test]
fn a_not_null_column_is_23502_and_the_sequence_has_already_advanced() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dv_notnull (id bigserial primary key, req integer NOT NULL)",
    ]);

    for expected_key in ["1", "2"] {
        let error = node
            .run("INSERT INTO dv_notnull DEFAULT VALUES")
            .unwrap_err();
        assert_eq!(error.sqlstate(), "23502");
        assert_eq!(
            error.to_string(),
            "null value in column \"req\" of relation \"dv_notnull\" violates not-null constraint"
        );
        assert_eq!(
            error.detail(),
            Some(format!("Failing row contains ({expected_key}, null)."))
        );
    }

    // **`VALUES (DEFAULT)` is the same statement by another spelling** and fails the same way,
    // consuming a third number.
    let error = node
        .run("INSERT INTO dv_notnull (id) VALUES (DEFAULT)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23502");
    assert_eq!(
        error.detail(),
        Some("Failing row contains (3, null).".to_owned())
    );
}

/// **`DEFAULT VALUES` takes no list and cannot be repeated** — it is not a `VALUES` clause with
/// one row, and there is no multi-row form of it. `42601` on both servers, for both spellings.
///
/// The column-list form is the half the capture does not carry, so it was measured on the same
/// oracle before being asserted here: `INSERT INTO probe_dv (n) DEFAULT VALUES` is
/// `42601 syntax error at or near "DEFAULT"` on PostgreSQL 19beta1. Only the **code** is asserted
/// for either, because the sentence is `sqlparser`'s — see [`DIVERGENCES`].
#[test]
fn it_takes_no_column_list_and_cannot_be_repeated() {
    let mut node = parity::Node::new(&["CREATE TABLE dvs (id bigserial primary key, n integer)"]);
    for written in [
        "INSERT INTO dvs DEFAULT VALUES, DEFAULT VALUES",
        "INSERT INTO dvs (n) DEFAULT VALUES",
    ] {
        assert_eq!(
            node.run(written).unwrap_err().sqlstate(),
            "42601",
            "for {written}"
        );
    }
}

/// **`ON CONFLICT` composes with it too**, which is not in the capture and was measured on the
/// oracle for the same reason: `INSERT INTO probe_dv DEFAULT VALUES ON CONFLICT (id) DO NOTHING`
/// is an ordinary success on PostgreSQL 19beta1.
///
/// It is here because the clause is the one part of an `INSERT` that reads the row *before* it is
/// written, and a row of no expressions is the case where every value it arbitrates on came from a
/// default. The second insert draws its own key, so it does not conflict — the `DO NOTHING` is
/// never reached, and both rows land.
#[test]
fn on_conflict_composes_with_it() {
    let mut node = parity::Node::new(&["CREATE TABLE dvc (id bigserial primary key, n integer)"]);

    for _ in 0..2 {
        node.run("INSERT INTO dvc DEFAULT VALUES ON CONFLICT (id) DO NOTHING")
            .unwrap();
    }
    assert_eq!(node.rows("SELECT count(*) FROM dvc"), [["2"]]);
}

/// **A missing table is reported against the table**, not against the missing source — the
/// statement has none by construction, so there is nothing else the error could be about.
#[test]
fn a_missing_table_is_42p01() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("INSERT INTO nosuchtable DEFAULT VALUES")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(error.to_string(), "relation \"nosuchtable\" does not exist");
}

/// The other spelling that reaches the same result, and the reason both are here: `ActiveRecord`
/// emits `DEFAULT VALUES` and the suite's own SQL writes `VALUES (DEFAULT, DEFAULT)`.
#[test]
fn the_default_keyword_inside_a_values_list_reaches_the_same_row() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE dv_alldefault (a integer DEFAULT 1, b integer DEFAULT 2)",
    ]);

    node.run("INSERT INTO dv_alldefault DEFAULT VALUES")
        .unwrap();
    node.run("INSERT INTO dv_alldefault VALUES (DEFAULT, DEFAULT)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT a, b FROM dv_alldefault ORDER BY a, b"),
        [["1", "2"], ["1", "2"]]
    );
}
