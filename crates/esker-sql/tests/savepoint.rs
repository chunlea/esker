//! Contract C3 for `SAVEPOINT`, `ROLLBACK TO` and `RELEASE`.
//!
//! Forty-eight statements put to a real PostgreSQL 19beta1 in one session and replayed in the same
//! order. A savepoint is entirely about what a previous statement left behind, so nothing here can
//! be tested a statement at a time.
//!
//! The fact the whole unit exists for is that **`ROLLBACK TO SAVEPOINT` un-aborts the block** —
//! after an error every statement is `25P02` until the transaction ends, except that one. It is
//! what lets `ActiveRecord` run a test per transaction, and it is asserted twice here: once through
//! the corpus, and once against the real [`Session`], whose `ReadyForQuery` is the only place the
//! status a client sees actually comes from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::pgwire::message::{Backend, Frontend, TransactionStatus};
use esker_sql::pgwire::session::Session;
use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table and opens and closes its own blocks, which is what is
/// under test. A fixture outside the file would be a second thing that has to agree with it.
const CORPUS_FIXTURE: &[&str] = &[];

/// What the bespoke tests below start from — they are not replaying the corpus and each wants a
/// table of its own.
const FIXTURE: &[&str] = &["CREATE TABLE sp (id int8 PRIMARY KEY, n text)"];

#[test]
fn every_savepoint_statement_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_savepoint.txt"),
        CORPUS_FIXTURE,
        &parity::Divergences::default(),
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The status a client sees comes from `ReadyForQuery` and nowhere else, so the recovery rule is
/// asserted against the real [`Session`] rather than against the harness that mirrors it.
///
/// Without this the corpus would be checking the mirror against itself: `tests/parity_harness`
/// implements the same aborted-block rule, and a test that only replays the corpus would pass with
/// both of them wrong in the same way.
#[test]
fn the_session_itself_treats_rollback_to_as_a_recovery() {
    let mut node = parity::Node::new(FIXTURE);
    let mut session = Session::new();

    let status = |sql: &str, session: &mut Session, node: &mut parity::Node| {
        let mut out = Vec::new();
        session.handle(
            &Frontend::Query(sql.to_owned()),
            &mut node.executor,
            &mut out,
        );
        for expected in [
            TransactionStatus::Idle,
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
        ] {
            if out
                .windows(6)
                .any(|window| window == Backend::ReadyForQuery(expected).to_bytes())
            {
                return expected;
            }
        }
        panic!("{sql}: no ReadyForQuery in the answer");
    };

    assert_eq!(
        status("BEGIN", &mut session, &mut node),
        TransactionStatus::InTransaction
    );
    status("INSERT INTO sp VALUES (1, 'kept')", &mut session, &mut node);
    status("SAVEPOINT s", &mut session, &mut node);
    // A duplicate key aborts the block, exactly as it does on a real server.
    assert_eq!(
        status(
            "INSERT INTO sp VALUES (1, 'duplicate')",
            &mut session,
            &mut node
        ),
        TransactionStatus::Failed
    );
    assert_eq!(
        status("SELECT id FROM sp", &mut session, &mut node),
        TransactionStatus::Failed,
        "an aborted block refuses everything"
    );
    // And this is the one that gets it back.
    assert_eq!(
        status("ROLLBACK TO s", &mut session, &mut node),
        TransactionStatus::InTransaction,
        "ROLLBACK TO SAVEPOINT recovers an aborted block"
    );
    status(
        "INSERT INTO sp VALUES (2, 'after recovery')",
        &mut session,
        &mut node,
    );
    assert_eq!(
        status("COMMIT", &mut session, &mut node),
        TransactionStatus::Idle
    );

    assert_eq!(
        node.rows("SELECT id, n FROM sp ORDER BY id"),
        [["1", "kept"], ["2", "after recovery"]]
    );
}

/// `ROLLBACK TO` used to end the whole block, because the classifier read `sqlparser`'s variant
/// and not its `savepoint` field — a statement PostgreSQL accepts, answered with a `ROLLBACK` tag,
/// and the user's other work gone with no error to say so.
///
/// The regression is the *keeping*: a plain `ROLLBACK` would have discarded row 1 as well.
#[test]
fn rollback_to_does_not_end_the_block() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO sp VALUES (1, 'before')").unwrap();
    node.run("SAVEPOINT s").unwrap();
    node.run("INSERT INTO sp VALUES (2, 'after')").unwrap();
    node.run("ROLLBACK TO s").unwrap();
    node.run("INSERT INTO sp VALUES (3, 'later')").unwrap();
    node.run("COMMIT").unwrap();

    assert_eq!(
        node.rows("SELECT id, n FROM sp ORDER BY id"),
        [["1", "before"], ["3", "later"]]
    );
}

/// An `UPDATE` and a `DELETE` are undone as exactly as an `INSERT` is, which is the property the
/// undo log has and a truncated write buffer would also have had — asserted because the two are
/// the shapes where a compensating log can be subtly wrong: restoring a row needs its *old value*,
/// not merely its absence.
#[test]
fn an_update_and_a_delete_are_put_back_as_they_were() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO sp VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .unwrap();
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT s").unwrap();
    node.run("UPDATE sp SET n = 'changed' WHERE id = 1")
        .unwrap();
    node.run("DELETE FROM sp WHERE id = 2").unwrap();
    node.run("INSERT INTO sp VALUES (4, 'new')").unwrap();
    assert_eq!(
        node.rows("SELECT id, n FROM sp ORDER BY id"),
        [["1", "changed"], ["3", "three"], ["4", "new"]]
    );
    node.run("ROLLBACK TO s").unwrap();
    node.run("COMMIT").unwrap();

    assert_eq!(
        node.rows("SELECT id, n FROM sp ORDER BY id"),
        [["1", "one"], ["2", "two"], ["3", "three"]]
    );
}

/// A key written **twice** under one savepoint must come back to what it was at the mark, not to
/// what it was in between — which is why the undo log is replayed backwards and why a key appears
/// in it once per write rather than once.
#[test]
fn a_key_written_twice_comes_back_to_the_mark() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO sp VALUES (1, 'original')").unwrap();
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT s").unwrap();
    node.run("UPDATE sp SET n = 'first' WHERE id = 1").unwrap();
    node.run("UPDATE sp SET n = 'second' WHERE id = 1").unwrap();
    node.run("UPDATE sp SET n = 'third' WHERE id = 1").unwrap();
    node.run("ROLLBACK TO s").unwrap();
    node.run("COMMIT").unwrap();

    assert_eq!(node.rows("SELECT n FROM sp"), [["original"]]);
}

/// Names stack. Two `SAVEPOINT dup` are two marks: `ROLLBACK TO dup` finds the most recent, and
/// after a `RELEASE dup` the *older* one is what it finds.
///
/// A map from name to mark passes every single-savepoint test and fails this one.
#[test]
fn savepoint_names_stack_rather_than_rebind() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT dup").unwrap();
    node.run("INSERT INTO sp VALUES (1, 'outer')").unwrap();
    node.run("SAVEPOINT dup").unwrap();
    node.run("INSERT INTO sp VALUES (2, 'inner')").unwrap();

    node.run("ROLLBACK TO dup").unwrap();
    assert_eq!(node.rows("SELECT id FROM sp ORDER BY id"), [["1"]]);
    // The mark is still there, so the same one can be rolled back to again.
    node.run("ROLLBACK TO dup").unwrap();
    assert_eq!(node.rows("SELECT id FROM sp ORDER BY id"), [["1"]]);

    // Releasing the inner one exposes the outer one, whose mark is before row 1.
    node.run("RELEASE dup").unwrap();
    node.run("ROLLBACK TO dup").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM sp ORDER BY id"),
        Vec::<Vec<String>>::new()
    );
    node.run("COMMIT").unwrap();
}

/// A released savepoint is gone, and rolling back to it is `3B001` — which itself aborts the
/// block, so it is one of the ways *into* `25P02`.
#[test]
fn a_released_savepoint_is_gone_and_saying_so_aborts_the_block() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT s").unwrap();
    node.run("RELEASE s").unwrap();

    let error = node.run("ROLLBACK TO s").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::NO_SUCH_SAVEPOINT, "{error}");
    assert_eq!(error.to_string(), "savepoint \"s\" does not exist");

    let after = node.run("SELECT id FROM sp").unwrap_err();
    assert_eq!(after.sqlstate(), sqlstate::IN_FAILED_SQL_TRANSACTION);
    node.run("ROLLBACK").unwrap();
}

/// Outside a block all three are `25P01`, and each names **PostgreSQL's own verb** rather than the
/// spelling the user typed — so `RELEASE s` says `RELEASE SAVEPOINT`.
#[test]
fn outside_a_block_each_verb_names_its_full_form() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, expected) in [
        (
            "SAVEPOINT s",
            "SAVEPOINT can only be used in transaction blocks",
        ),
        (
            "RELEASE s",
            "RELEASE SAVEPOINT can only be used in transaction blocks",
        ),
        (
            "ROLLBACK TO s",
            "ROLLBACK TO SAVEPOINT can only be used in transaction blocks",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "{sql} -> {error}"
        );
        assert_eq!(error.to_string(), expected, "{sql}");
    }
}

/// A transaction with no savepoint open records nothing, which is what keeps the pre-image read
/// off the path of every other statement in the system.
///
/// Asserted through the behaviour rather than through a counter: a block that never took a
/// savepoint still rolls back whole, and a `RELEASE` of the last mark stops the recording.
#[test]
fn a_block_with_no_savepoint_still_rolls_back_whole() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO sp VALUES (1, 'gone')").unwrap();
    node.run("ROLLBACK").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM sp"),
        Vec::<Vec<String>>::new(),
        "a whole-block rollback is the transaction's, not the undo log's"
    );
}
