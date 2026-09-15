//! **A sequence's counter is written only by transactions of its own** — debt #97, held to
//! PostgreSQL 19's answers.
//!
//! `nextval`, `setval` and `TRUNCATE … RESTART IDENTITY` write a sequence's counter in a
//! transaction of their own and commit it at once, which is what makes a sequence
//! non-transactional. `CREATE SEQUENCE` wrote its `START` into the same key in the statement's
//! transaction, and every drop deleted the key there too, so the key had writers on both sides of
//! the statement's snapshot. Two wrong answers followed: a transaction that made a sequence and
//! drew from it drew `1` where it said `START 7` — the draw could not see the uncommitted `START`
//! — and could not commit, because the draw's own commit had moved the key under it (`40001`); and
//! a `REPEATABLE READ` block that drew and then dropped what it drew from was refused at `COMMIT`
//! the same way. PostgreSQL answers `7` and commits, both times.
//!
//! `corpus/pg19_sequence_counter_writers.txt` is the capture and the replay is the test of record;
//! the tests after it pin one writer each, with two guards on what a drop takes and keeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The reserved block's end against the last value handed out — the trade
/// `tests/add_column_not_null.rs` declares, and nothing about who writes the counter.
const BLOCK_END: &str = "the reserved block's end against the last value handed out: a node takes \
                         a batch and serves from it, so the stored counter runs ahead of what \
                         `nextval` has answered. The values `nextval` produces agree, which is \
                         what every other line here checks";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', last_value, is_called FROM s2g_a",
            BLOCK_END,
            "pg19_sequence_counter_writers.txt:45",
        ),
        (
            "SELECT 'r', nextval('s2g_f'), last_value, is_called FROM s2g_f",
            BLOCK_END,
            "pg19_sequence_counter_writers.txt:73",
        ),
    ],
};

#[test]
fn every_sequence_counter_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_sequence_counter_writers.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 50,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`START` holds in the transaction that made the sequence**, before anything is drawn and after.
#[test]
fn a_start_holds_in_the_transaction_that_made_the_sequence() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE SEQUENCE w START 7").unwrap();
    assert_eq!(
        node.rows("SELECT last_value, is_called FROM w"),
        [["7", "f"]]
    );
    assert_eq!(node.rows("SELECT nextval('w'), nextval('w')"), [["7", "8"]]);
    node.run("ROLLBACK").unwrap();
}

/// **A transaction that makes a sequence and draws from it commits**, and the next value carries
/// on from it.
#[test]
fn a_sequence_made_and_drawn_from_in_one_transaction_commits() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE SEQUENCE c START 7").unwrap();
    assert_eq!(node.rows("SELECT nextval('c')"), [["7"]]);
    assert_eq!(node.answer("COMMIT"), parity::Answer::Done);
    assert_eq!(node.rows("SELECT nextval('c')"), [["8"]]);
}

/// **A `REPEATABLE READ` block that draws and then drops what it drew from commits** — whether it
/// drops the sequence, the table that owns it, or the column.
#[test]
fn a_repeatable_read_block_that_draws_and_then_drops_commits() {
    let mut node = parity::Node::new(&[]);
    for (make, draw, drop) in [
        (
            "CREATE SEQUENCE s",
            "SELECT nextval('s')",
            "DROP SEQUENCE s",
        ),
        (
            "CREATE TABLE t (id serial, v text)",
            "INSERT INTO t (v) VALUES ('x') RETURNING id",
            "DROP TABLE t",
        ),
        (
            "CREATE TABLE u (id serial, v text)",
            "INSERT INTO u (v) VALUES ('x') RETURNING id",
            "ALTER TABLE u DROP COLUMN id",
        ),
    ] {
        node.run("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        node.run(make).unwrap();
        assert_eq!(node.rows(draw), [["1"]], "{make}");
        node.run(drop).unwrap();
        assert_eq!(node.answer("COMMIT"), parity::Answer::Done, "{drop}");
    }
}

/// **`TRUNCATE … RESTART IDENTITY` goes back to the sequence's `START`**, which is where a sequence
/// with no counter starts.
#[test]
fn restart_identity_goes_back_to_the_start() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE d (id int, v text)",
        "CREATE SEQUENCE d_seq START 50 OWNED BY d.id",
        "ALTER TABLE d ALTER COLUMN id SET DEFAULT nextval('d_seq')",
    ]);
    assert_eq!(
        node.rows("INSERT INTO d (v) VALUES ('a'), ('b') RETURNING id"),
        [["50"], ["51"]]
    );
    node.run("TRUNCATE d RESTART IDENTITY").unwrap();
    assert_eq!(
        node.rows("INSERT INTO d (v) VALUES ('c') RETURNING id"),
        [["50"]]
    );
}

/// **A sequence nothing has drawn from reports its `START`** — a guard on the reader: with no counter
/// written by `CREATE SEQUENCE`, the record's `START` is what `last_value` has to answer.
#[test]
fn an_undrawn_sequence_reports_its_start() {
    let mut node = parity::Node::new(&["CREATE SEQUENCE n START 7"]);
    assert_eq!(
        node.rows("SELECT last_value, is_called FROM n"),
        [["7", "f"]]
    );
}

/// **A dropped sequence's numbers go with it, and a rolled-back drop keeps them** — two guards: the
/// counter a drop leaves behind is never read again, and one a drop did not take is still read.
#[test]
fn a_drop_takes_its_numbers_and_a_rolled_back_drop_keeps_them() {
    let mut node = parity::Node::new(&["CREATE SEQUENCE g", "CREATE SEQUENCE h"]);
    assert_eq!(node.rows("SELECT nextval('g'), nextval('g')"), [["1", "2"]]);
    node.run("DROP SEQUENCE g").unwrap();
    node.run("CREATE SEQUENCE g").unwrap();
    assert_eq!(node.rows("SELECT nextval('g')"), [["1"]]);
    assert_eq!(node.rows("SELECT nextval('h')"), [["1"]]);
    node.run("BEGIN").unwrap();
    node.run("DROP SEQUENCE h").unwrap();
    node.run("ROLLBACK").unwrap();
    assert_eq!(node.rows("SELECT nextval('h')"), [["2"]]);
}
