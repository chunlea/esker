//! Inline `UNIQUE` table constraints — statement 779.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `conname` and `relname` are `name` on a real server and `contype` a `"char"`; all are `text`
    // here, with identical characters. The standing trade every `pg_catalog` column makes.
    types: &[
        "SELECT conname, contype, condeferrable, condeferred, pg_get_constraintdef(oid) FROM \
         pg_constraint WHERE conrelid = 'tuc'::regclass AND contype = 'u' ORDER BY conname",
        "SELECT c.relname, i.indisunique, i.indnullsnotdistinct FROM pg_index i JOIN pg_class c ON \
         c.oid = i.indexrelid WHERE i.indrelid = 'tuc'::regclass ORDER BY c.relname",
        "SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = 'tuc2'::regclass AND contype = 'u'",
    ],
    answers: &[
        (
            "CREATE TABLE tuc3 (a integer UNIQUE NULLS NOT DISTINCT)",
            "**The column-option spelling is a contract C1 gap in the parser**, and it already \
             names itself: `sqlparser` 0.62.0 parses `NULLS NOT DISTINCT` after a *table* \
             constraint and after an index's column list, and not after a column's `UNIQUE`, so \
             this one is a syntax error that the construct recognizer turns into `0A000 UNIQUE \
             NULLS NOT DISTINCT on a column`. The table-constraint form — which is what \
             `ActiveRecord` writes, and what statement 779 uses — parses and runs, and the two \
             build the same index.",
        ),
        (
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = \
             'tuc3'::regclass AND contype = 'u'",
            "The consequence of the line above: the table was never created. The rendering it \
             would show — `UNIQUE NULLS NOT DISTINCT (a)`, with the clause **before** the column \
             list — is checked on the table-constraint form instead, which reaches it by the \
             same code.",
        ),
        (
            "CREATE TABLE tucd (a integer, CONSTRAINT d UNIQUE (a) DEFERRABLE INITIALLY DEFERRED)",
            "**`INITIALLY DEFERRED` really defers, and this node cannot** — refused by name \
             (contract C2). Measured on 19beta1: both colliding rows are accepted inside the \
             transaction and the `23505` is raised by `COMMIT`, which rolls the whole transaction \
             back. Every check in this crate is immediate, so taking the clause would refuse a \
             transaction a real server commits — a wrong answer rather than a gap. It is the one \
             of statement 779's four constraints that does not land; the other three do, \
             `DEFERRABLE INITIALLY IMMEDIATE` included, because that one is not deferred at all.",
        ),
        (
            "SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM \
             pg_constraint WHERE conrelid = 'tucd'::regclass AND contype = 'u'",
            "The consequence of the line above: the table was never created. It is in the corpus \
             because it is what `condeferred` being `t` looks like — the only row in this file \
             where it is not `f`, and the fact that makes the refusal above necessary rather than \
             conservative.",
        ),
    ],
};

#[test]
fn every_inline_unique_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_inline_unique.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 22,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`NULLS NOT DISTINCT` lets one NULL into the column and no more.**
///
/// The consequence is what a reader would get wrong: an `INSERT` that never mentions the column
/// still collides, because omitting it writes a NULL and the constraint calls two NULLs equal.
#[test]
fn nulls_not_distinct_admits_one_null() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE tuc (id bigserial primary key, position_4 integer, CONSTRAINT tuc_pos_nnd \
         UNIQUE NULLS NOT DISTINCT (position_4))",
    ]);
    node.run("INSERT INTO tuc (position_4) VALUES (NULL)")
        .unwrap();
    let error = node
        .run("INSERT INTO tuc (position_4) VALUES (NULL)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.detail().as_deref(),
        Some("Key (position_4)=(null) already exists."),
        "lower-case `null`, in parentheses"
    );
    // A row that never mentions the column collides for the same reason.
    assert_eq!(
        node.run("INSERT INTO tuc (id) VALUES (99)")
            .unwrap_err()
            .sqlstate(),
        "23505"
    );
    // Distinct values are fine, and so is the ordinary NULLS DISTINCT default beside it.
    node.run("INSERT INTO tuc (position_4) VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM tuc"), [["2"]]);
}

/// **`DEFERRABLE INITIALLY IMMEDIATE` checks at the statement**, like any other unique constraint.
///
/// Only the catalog and the printed definition differ, and `pg_get_constraintdef` drops the
/// `INITIALLY IMMEDIATE` half — so the text that comes out is not the text that went in.
#[test]
fn deferrable_initially_immediate_is_checked_at_the_statement() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE td (id integer, CONSTRAINT td_imm UNIQUE (id) DEFERRABLE INITIALLY IMMEDIATE)",
    ]);
    node.run("INSERT INTO td VALUES (1)").unwrap();
    let error = node.run("INSERT INTO td VALUES (1)").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        node.rows(
            "SELECT condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'td'::regclass AND contype = 'u'"
        ),
        [["t", "f", "UNIQUE (id) DEFERRABLE"]]
    );
}

/// **`INITIALLY DEFERRED` really waits**, and this node cannot — so it is refused by name.
///
/// Measured: both rows go in inside the transaction and the violation is raised by `COMMIT`, which
/// rolls the whole transaction back. Checking it at the statement instead would refuse a
/// transaction a real server commits — a wrong answer, not a gap — so the clause is named rather
/// than approximated. It is the one of the four constraints in statement 779 that does not land.
#[test]
fn initially_deferred_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("CREATE TABLE td (id integer, CONSTRAINT d UNIQUE (id) DEFERRABLE INITIALLY DEFERRED)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(
        error.to_string(),
        "UNIQUE ... DEFERRABLE INITIALLY DEFERRED is not supported"
    );
}

/// All four of statement 779's constraints in one statement — three land, and the fourth names
/// itself.
#[test]
fn statement_779s_constraints() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE tuc (id bigserial primary key, position_1 integer, position_2 integer, \
         position_4 integer, CONSTRAINT tuc_pos_false UNIQUE (position_1), CONSTRAINT \
         tuc_pos_immediate UNIQUE (position_2) DEFERRABLE INITIALLY IMMEDIATE, CONSTRAINT \
         tuc_pos_nnd UNIQUE NULLS NOT DISTINCT (position_4))",
    )
    .unwrap();
    assert_eq!(
        node.rows(
            "SELECT conname, condeferrable, pg_get_constraintdef(oid) FROM pg_constraint WHERE \
             conrelid = 'tuc'::regclass AND contype = 'u' ORDER BY conname"
        ),
        vec![
            vec!["tuc_pos_false", "f", "UNIQUE (position_1)"],
            vec!["tuc_pos_immediate", "t", "UNIQUE (position_2) DEFERRABLE"],
            vec!["tuc_pos_nnd", "f", "UNIQUE NULLS NOT DISTINCT (position_4)"],
        ]
    );
}
