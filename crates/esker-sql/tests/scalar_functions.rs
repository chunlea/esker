//! `lower` and `upper`, against PostgreSQL 19beta1 — what statement 78 indexes on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT lower(1)",
        "`function lower(bigint) does not exist` where a real server says `lower(integer)`. Both \
         refuse, with the same SQLSTATE, the same DETAIL and the same HINT; the type named is the \
         one this node gives a bare integer constant, which is `int8` where PostgreSQL's is \
         `int4`. Already declared in `tests/unknown_literal.rs` and closing with the same unit.",
    )],
};

#[test]
fn every_scalar_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_lower.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 6,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The case mapping is **full Unicode**, which is what lets these exist without a collation.
///
/// A byte-wise implementation passes every ASCII case and fails this one. It is the whole reason
/// `lower`/`upper` could be added to a project that has decided not to link an ICU.
#[test]
fn the_case_mapping_is_unicode_and_not_ascii() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT lower('ÀÉÎ'), upper('àéî')"),
        vec![vec!["àéî", "ÀÉÎ"]]
    );
    assert_eq!(
        node.rows("SELECT lower('MiXeD'), upper('MiXeD')"),
        vec![vec!["mixed", "MIXED"]]
    );
}

/// NULL in, NULL out — and the empty string is not NULL.
#[test]
fn null_passes_through_and_empty_is_not_null() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE lw (id int8 PRIMARY KEY, b text)",
        "INSERT INTO lw VALUES (1, ''), (2, NULL)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id, lower(b) FROM lw ORDER BY id"),
        vec![vec!["1", ""], vec!["2", "\\N"]]
    );
}

/// `42883` has **two** explanations, and they are different conditions.
///
/// The wrong argument *type* gets "argument types" and a `HINT`; the wrong *count* gets "number of
/// arguments" and none. One SQLSTATE, one message, two DETAILs — measured, because an
/// implementation with one of them looks right on half the cases.
#[test]
fn the_wrong_type_and_the_wrong_count_are_different_conditions() {
    let mut node = parity::Node::new(&[]);
    for (sql, detail) in [
        (
            "SELECT lower(1)",
            "No function of that name accepts the given argument types.",
        ),
        (
            "SELECT lower('a','b')",
            "No function of that name accepts the given number of arguments.",
        ),
        (
            "SELECT lower()",
            "No function of that name accepts the given number of arguments.",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42883", "{sql}");
        assert_eq!(error.detail().as_deref(), Some(detail), "{sql}");
    }

    // And only the type mismatch carries a hint.
    assert!(
        node.run("SELECT lower(1)").unwrap_err().hint().is_some(),
        "a type mismatch should hint at a cast"
    );
    assert!(
        node.run("SELECT lower()").unwrap_err().hint().is_none(),
        "an arity mismatch has nothing to cast"
    );
}
