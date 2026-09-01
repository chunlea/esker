//! Contract C3 for `nextval`, `currval`, `setval`, `lastval` and `DEFAULT`.
//!
//! Twenty-three statements put to a real PostgreSQL 19beta1 **in one session** and replayed
//! against one node in the same order, which is the only shape that can test them: `currval` and
//! `lastval` are session state, and a corpus captured a connection per statement would have
//! recorded `55000` for every one of them.
//!
//! No divergences. The four rules a reader would have got wrong are at the top of the corpus file;
//! the one that shaped the code most is that the argument is a **name, not a string**, so the
//! folding an identifier gets happens inside the quotes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus's first statement is the table, because what it creates is under test.
const FIXTURE: &[&str] = &[];

#[test]
fn every_sequence_function_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_sequence_functions.txt"),
        FIXTURE,
        &parity::Divergences::default(),
    );
    assert!(
        checked > 21,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `currval` is about the **session**, not about the sequence: a sequence that has been advanced
/// by somebody else still answers `55000` here, because this connection has not taken a value.
///
/// A second node over its own executor is the only way to say that, and the corpus cannot: it is
/// one session by construction.
#[test]
fn currval_is_session_state_and_not_sequence_state() {
    let mut first = parity::Node::new(&["CREATE TABLE c (id bigserial PRIMARY KEY)"]);
    first.run("SELECT nextval('c_id_seq')").unwrap();
    assert_eq!(first.rows("SELECT currval('c_id_seq')"), [["1"]]);

    // A fresh session over a fresh node: the sequence exists and this session has not used it.
    let mut second = parity::Node::new(&["CREATE TABLE c (id bigserial PRIMARY KEY)"]);
    let error = second.run("SELECT currval('c_id_seq')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
    assert_eq!(
        error.to_string(),
        "currval of sequence \"c_id_seq\" is not yet defined in this session"
    );
    let error = second.run("SELECT lastval()").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
    assert_eq!(
        error.to_string(),
        "lastval is not yet defined in this session"
    );
}

/// `lastval()` is the last value from **any** sequence, which is what makes it different from
/// `currval` and is the whole reason it exists.
#[test]
fn lastval_follows_whichever_sequence_moved_last() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE a (id bigserial PRIMARY KEY)",
        "CREATE TABLE b (id bigserial PRIMARY KEY)",
    ]);
    node.run("SELECT nextval('a_id_seq')").unwrap();
    node.run("SELECT setval('b_id_seq', 500)").unwrap();
    assert_eq!(node.rows("SELECT lastval()"), [["500"]]);
    assert_eq!(node.rows("SELECT nextval('a_id_seq')"), [["2"]]);
    assert_eq!(node.rows("SELECT lastval()"), [["2"]]);
    // And each sequence's own `currval` is untouched by the other's.
    assert_eq!(node.rows("SELECT currval('b_id_seq')"), [["500"]]);
}

/// A `setval` has to **discard this session's reserved block**, or the next `nextval` would keep
/// handing out numbers from before it and the statement would have done nothing visible.
///
/// This is the one place the batch that makes `nextval` cheap could have been silently wrong, and
/// it cannot be seen from a single `nextval`: the block is 32 values wide, so the bug would only
/// appear as the *second* value after a `setval`.
#[test]
fn a_setval_discards_the_sessions_reserved_block() {
    let mut node = parity::Node::new(&["CREATE TABLE s (id bigserial PRIMARY KEY)"]);
    // Two values, so a block is definitely reserved and partly used.
    assert_eq!(node.rows("SELECT nextval('s_id_seq')"), [["1"]]);
    assert_eq!(node.rows("SELECT nextval('s_id_seq')"), [["2"]]);

    node.run("SELECT setval('s_id_seq', 900)").unwrap();
    assert_eq!(node.rows("SELECT nextval('s_id_seq')"), [["901"]]);
    // The second one is what a kept block would have got wrong.
    assert_eq!(node.rows("SELECT nextval('s_id_seq')"), [["902"]]);
}

/// A sequence function belongs in a `SELECT` with no `FROM`, which is where every client writes
/// one. Over a table it is a side effect **per row** on a real server, and running it once instead
/// would hand a client one number where it expected four — so it is refused by name.
#[test]
fn a_sequence_function_over_a_table_is_refused_by_name() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id bigserial PRIMARY KEY, n int8)",
        "INSERT INTO t (n) VALUES (1), (2)",
    ]);
    for sql in [
        "SELECT nextval('t_id_seq') FROM t",
        "SELECT currval('t_id_seq') FROM t",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{sql} -> {error}"
        );
        assert!(
            error.to_string().contains("FROM"),
            "{sql} -> `{error}`, which does not say where it is not supported"
        );
    }
    // And the sequence did not move while being refused.
    assert_eq!(node.rows("SELECT nextval('t_id_seq')"), [["3"]]);
}

/// `DEFAULT` is not an explicit value, so a `GENERATED ALWAYS` column takes it where it refuses a
/// number — measured on a real server, and the one place the two clauses have to be told apart.
#[test]
fn default_is_accepted_by_a_generated_always_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE al (id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY, n text)",
    ]);
    assert_eq!(
        node.rows("INSERT INTO al (id, n) VALUES (DEFAULT, 'a') RETURNING id"),
        [["1"]]
    );
    let error = node
        .run("INSERT INTO al (id, n) VALUES (7, 'b')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::GENERATED_ALWAYS);
}
