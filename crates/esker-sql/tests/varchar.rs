//! Contract C3 for `character varying` — tier 1's second type, and what `t.string` emits.
//!
//! [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md). **No length here**:
//! `varchar(n)` is a typmod, which is a column property this node has nowhere to keep yet, so it is
//! `0A000` naming itself and gets its own corpus with the unit that adds it. Bare
//! `character varying` is what rung 2 of the ladder asks for.
//!
//! It is the strongest case in tier 1 for "not a format change": a `varchar` column's rows are
//! **byte-identical** to a `text` column's, and `Datum` has no variant for it, because there would
//! be nothing in one that a `Datum::Text` does not already hold. `text`, `varchar` and `bpchar` are
//! one varlena told apart by OID — which is PostgreSQL's own model, not a shortcut.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;
use esker_sql::value::{ColumnType, PgType};

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_varchar_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_varchar.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 18,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// What a client is told, which is the whole of the difference from `text`.
#[test]
fn a_client_is_told_character_varying() {
    let mut node = parity::Node::new(&["CREATE TABLE v (id int8 PRIMARY KEY, a varchar, b text)"]);
    node.run("INSERT INTO v VALUES (1, 'same', 'same')")
        .unwrap();

    // The values are one value; the declared types are two.
    assert_eq!(node.rows("SELECT id FROM v WHERE a = b"), vec![vec!["1"]]);
    match node.answer("SELECT a, b FROM v") {
        parity::Answer::Rows { types, .. } => assert_eq!(
            types,
            vec![
                ColumnType::Varchar.name().to_owned(),
                ColumnType::Text.name().to_owned()
            ]
        ),
        other => panic!("SELECT a, b answered {other}"),
    }
    assert_eq!(ColumnType::Varchar.oid(), 1043);
    assert_eq!(ColumnType::Varchar.type_len(), -1);
    assert_eq!(ColumnType::Varchar.name(), "character varying");

    // And the catalog learned it by itself, from `ColumnType::ALL`.
    assert_eq!(
        node.rows("SELECT oid, typname, typinput FROM pg_type WHERE typname = 'varchar'"),
        vec![vec!["1043", "varchar", "varcharin"]]
    );
}

/// Trailing spaces are significant, which is the difference from `character(n)` and the reason
/// they are two types rather than one with a flag.
#[test]
fn trailing_spaces_are_significant() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8 PRIMARY KEY, a varchar)"]);
    node.run("INSERT INTO t VALUES (1, 'zz  '), (2, 'zz')")
        .unwrap();

    assert_eq!(
        node.rows("SELECT id FROM t WHERE a = 'zz'"),
        vec![vec!["2"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM t WHERE a = 'zz  '"),
        vec![vec!["1"]]
    );
}

/// A length is refused by name rather than ignored.
///
/// Ignoring it is the tempting shortcut and it is a **wrong answer**: a `varchar(5)` that stored a
/// six-character value would answer a later `SELECT` with a row a real server never had, where
/// that server raises `22001`. Refusing is the state between units, and the unit that adds the
/// typmod deletes this test.
#[test]
fn a_length_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE t (a varchar(5))",
        "CREATE TABLE t (a character varying(255))",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{statement}"
        );
        assert!(
            error.to_string().to_ascii_lowercase().contains("varchar")
                || error.to_string().to_ascii_lowercase().contains("varying"),
            "{statement} -> `{error}`, which does not name the type"
        );
    }
}
