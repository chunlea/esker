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
            "UNMEASURED",
        ),
        (
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = \
             'tuc3'::regclass AND contype = 'u'",
            "The consequence of the line above: the table was never created. The rendering it \
             would show — `UNIQUE NULLS NOT DISTINCT (a)`, with the clause **before** the column \
             list — is checked on the table-constraint form instead, which reaches it by the \
             same code.",
            "UNMEASURED",
        ),
        (
            "SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM \
             pg_constraint WHERE conrelid = 'tucd'::regclass AND contype = 'u'",
            "The consequence of the line above: the table was never created. It is in the corpus \
             because it is what `condeferred` being `t` looks like — the only row in this file \
             where it is not `f`, and the fact that makes the refusal above necessary rather than \
             conservative.",
            "UNMEASURED",
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

/// **`INITIALLY DEFERRED` really waits**, and now it does here too.
///
/// This test asserted the refusal until the transaction could owe a check. What it asserts now is
/// the behaviour that refusal was standing in for, and the case that makes deferral worth having:
/// a transaction breaks the constraint in the middle and **repairs it before the end**, which a
/// real server commits. Checking at the statement would refuse it — a wrong answer, not a gap,
/// which is why the clause was named rather than approximated until the mechanism existed
/// (`crate::exec::deferred`).
#[test]
fn initially_deferred_waits_for_the_commit() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE td (id int8 PRIMARY KEY, a int4, CONSTRAINT td_a UNIQUE (a) DEFERRABLE \
         INITIALLY DEFERRED)",
        "INSERT INTO td VALUES (1, 1)",
    ]);

    // Broken in the middle: the second row goes in, and both are visible inside the block.
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO td VALUES (2, 1)").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM td"), vec![vec!["2"]]);

    // And the `COMMIT` is what refuses it — rolling the whole transaction back, so the row that
    // was visible a moment ago is not there.
    let error = node.run("COMMIT").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"td_a\""
    );
    assert_eq!(node.rows("SELECT count(*) FROM td"), vec![vec!["1"]]);

    // **Repaired before the end**: the same duplicate, with the original deleted, commits. This
    // is the case the whole mechanism exists for and the one an immediate constraint cannot
    // express — there is no order of these two statements that an immediate check admits.
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO td VALUES (3, 1)").unwrap();
    node.run("DELETE FROM td WHERE id = 1").unwrap();
    node.run("COMMIT").unwrap();
    assert_eq!(
        node.rows("SELECT id, a FROM td ORDER BY id"),
        vec![vec!["3", "1"]]
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
