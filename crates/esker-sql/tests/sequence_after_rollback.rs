//! **A sequence created after a rolled-back one starts at its own start** — debt #94, held to
//! PostgreSQL 19's answers.
//!
//! A relation id is taken inside the transaction that creates the relation, so a rolled-back
//! `CREATE` gives its id back and the next `CREATE` takes the same one. A sequence's stored counter
//! and the block a node reserved from it are both keyed by that id, and both outlived the rollback
//! — the counter is advanced in a transaction of its own, which is what makes `nextval`
//! non-transactional — so a sequence made on the reused id carried on from the rolled-back one's
//! numbers: `3` after two, where PostgreSQL starts again at `1`.
//!
//! `corpus/pg19_sequence_after_rollback.txt` is the capture and the replay is the test of record;
//! the tests after it pin one way of rolling a creation back each, and two guards on what the fix
//! must not reach.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_sequence_after_rollback_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_sequence_after_rollback.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 50,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A serial after a rolled-back one starts at `1`**, where it carried on from the rolled-back
/// table's numbers.
#[test]
fn a_serial_after_a_rolled_back_one_starts_at_one() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE TABLE a (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO a (v) VALUES ('x'), ('y') RETURNING id"),
        [["1"], ["2"]]
    );
    node.run("ROLLBACK").unwrap();
    node.run("CREATE TABLE b (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO b (v) VALUES ('z') RETURNING id"),
        [["1"]]
    );
}

/// **A sequence of the same name, made again, starts again** — and its `currval` is not this
/// session's until it draws from it.
#[test]
fn a_sequence_made_again_after_a_rollback_starts_again() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE SEQUENCE s").unwrap();
    assert_eq!(
        node.rows("SELECT nextval('s'), nextval('s'), nextval('s')"),
        [["1", "2", "3"]]
    );
    node.run("ROLLBACK").unwrap();
    node.run("CREATE SEQUENCE s").unwrap();
    assert_eq!(
        node.answer("SELECT currval('s')").to_string(),
        "!55000 currval of sequence \"s\" is not yet defined in this session"
    );
    assert_eq!(node.rows("SELECT nextval('s'), currval('s')"), [["1", "1"]]);
}

/// **A `ROLLBACK TO` gives the id back as a `ROLLBACK` does**: a serial made inside the savepoint,
/// and one made after it on the same id, start again.
#[test]
fn a_serial_made_inside_a_rolled_back_savepoint_starts_again() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT a").unwrap();
    node.run("CREATE TABLE d (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO d (v) VALUES ('x'), ('y') RETURNING id"),
        [["1"], ["2"]]
    );
    node.run("ROLLBACK TO a").unwrap();
    node.run("CREATE TABLE e (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO e (v) VALUES ('z') RETURNING id"),
        [["1"]]
    );
    node.run("ROLLBACK").unwrap();
}

/// **A statement outside a block has no `ROLLBACK` of its own**, and its implicit transaction takes
/// a sequence back the same way: a `DO` block that creates a table, draws and raises.
#[test]
fn a_do_block_that_raised_takes_its_sequence_back() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer(
            "DO $$ BEGIN CREATE TABLE a (id serial, v text); INSERT INTO a (v) VALUES ('x'), \
             ('y'); RAISE EXCEPTION 'boom'; END $$"
        )
        .to_string(),
        "!P0001 boom"
    );
    node.run("CREATE TABLE b (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO b (v) VALUES ('z') RETURNING id"),
        [["1"]]
    );
}

/// **A node that rolled a sequence back holds no block of it**, so a sequence another node makes
/// on the same id is never handed out twice.
///
/// Two executors over one store, each with the private block allocator a node has. The first draws
/// from a serial and rolls back; the second makes a table on the same id and draws its first
/// block; then the first draws from the second's table. It holds no block of that id, so it
/// reserves one of its own past the second's — the gap two nodes' blocks already leave
/// (`crate::sequence`) — where it used to hand out `3` from the block it had reserved for the
/// rolled-back sequence: a number the second node's block hands out too.
#[test]
fn a_rolled_back_sequence_leaves_no_block_on_its_node() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let mut first = parity::Node::on(
        Arc::clone(&backend),
        Arc::new(Catalog::new()),
        1,
        "esker",
        &[],
    );
    let mut second = parity::Node::on(backend, Arc::new(Catalog::new()), 1, "esker", &[]);
    first.run("BEGIN").unwrap();
    first.run("CREATE TABLE a (id serial, v text)").unwrap();
    assert_eq!(
        first.rows("INSERT INTO a (v) VALUES ('x'), ('y') RETURNING id"),
        [["1"], ["2"]]
    );
    first.run("ROLLBACK").unwrap();
    second.run("CREATE TABLE b (id serial, v text)").unwrap();
    assert_eq!(
        second.rows("INSERT INTO b (v) VALUES ('p') RETURNING id"),
        [["1"]]
    );
    let past_the_second_block = (esker_sql::catalog::SEQUENCE_BATCH + 1).to_string();
    assert_eq!(
        first.rows("INSERT INTO b (v) VALUES ('q') RETURNING id"),
        [[past_the_second_block.as_str()]]
    );
    assert_eq!(
        second.rows("INSERT INTO b (v) VALUES ('r') RETURNING id"),
        [["2"]]
    );
}

/// **A rollback gives no number back to a sequence that survives it** — a guard: what makes a new
/// sequence start fresh must not reach one that was made before the savepoint.
#[test]
fn a_savepoint_rollback_gives_no_number_back() {
    let mut node = parity::Node::new(&["CREATE SEQUENCE k", "CREATE TABLE c (id serial, v text)"]);
    node.run("BEGIN").unwrap();
    node.run("SAVEPOINT a").unwrap();
    assert_eq!(node.rows("SELECT nextval('k'), nextval('k')"), [["1", "2"]]);
    node.run("ROLLBACK TO a").unwrap();
    assert_eq!(node.rows("SELECT nextval('k')"), [["3"]]);
    node.run("SAVEPOINT b").unwrap();
    assert_eq!(
        node.rows("INSERT INTO c (v) VALUES ('p') RETURNING id"),
        [["1"]]
    );
    node.run("ROLLBACK TO b").unwrap();
    assert_eq!(
        node.rows("INSERT INTO c (v) VALUES ('q') RETURNING id"),
        [["2"]]
    );
    node.run("ROLLBACK").unwrap();
}

/// **A commit keeps what it made** — a guard: a sequence made and drawn from in a transaction that
/// commits goes on from its numbers in the next one.
#[test]
fn a_committed_serial_keeps_its_numbers() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    node.run("CREATE TABLE f (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO f (v) VALUES ('x'), ('y') RETURNING id"),
        [["1"], ["2"]]
    );
    node.run("COMMIT").unwrap();
    node.run("BEGIN").unwrap();
    node.run("CREATE TABLE g (id serial, v text)").unwrap();
    node.run("ROLLBACK").unwrap();
    assert_eq!(
        node.rows("INSERT INTO f (v) VALUES ('z') RETURNING id"),
        [["3"]]
    );
    // And outside a block, where the statement's own commit is what keeps it: a statement that
    // fails afterwards must not take back what an earlier one committed.
    node.run("CREATE TABLE h (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO h (v) VALUES ('x') RETURNING id"),
        [["1"]]
    );
    assert!(
        node.run("DO $$ BEGIN RAISE EXCEPTION 'no'; END $$")
            .is_err()
    );
    assert_eq!(
        node.rows("INSERT INTO h (v) VALUES ('y') RETURNING id"),
        [["2"]]
    );
}

/// **A session that ends inside its block gives its sequences back**, as a `ROLLBACK` does — the
/// ending a client that goes away mid-transaction takes, through `Executor::drop`.
#[test]
fn a_session_that_ends_inside_its_block_gives_its_sequences_back() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    let mut leaving = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, "esker", &[]);
    leaving.run("BEGIN").unwrap();
    leaving.run("CREATE TABLE a (id serial, v text)").unwrap();
    assert_eq!(
        leaving.rows("INSERT INTO a (v) VALUES ('x'), ('y') RETURNING id"),
        [["1"], ["2"]]
    );
    drop(leaving);
    let mut staying = parity::Node::on(backend, catalog, 1, "esker", &[]);
    staying.run("CREATE TABLE b (id serial, v text)").unwrap();
    assert_eq!(
        staying.rows("INSERT INTO b (v) VALUES ('z') RETURNING id"),
        [["1"]]
    );
}

/// **A statement whose commit fails takes its sequence back** — the other way a statement outside
/// a block does not commit: a `DO` block that makes a table and draws from it, leaving a deferred
/// check that fails when the statement commits.
#[test]
fn a_statement_whose_commit_fails_takes_its_sequence_back() {
    let mut node = parity::Node::new(&["CREATE TABLE p (id int PRIMARY KEY)"]);
    let answer = node.answer(
        "DO $$ BEGIN CREATE TABLE c (id serial, pid int REFERENCES p DEFERRABLE INITIALLY \
         DEFERRED); INSERT INTO c (pid) VALUES (1); END $$",
    );
    assert!(answer.to_string().starts_with("!23503"), "{answer}");
    node.run("CREATE TABLE d (id serial, v text)").unwrap();
    assert_eq!(
        node.rows("INSERT INTO d (v) VALUES ('z') RETURNING id"),
        [["1"]]
    );
}
