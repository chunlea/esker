//! READ COMMITTED: a writer waits for the writer in front of it
//! ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
//!
//! **This file starts with the test that would have caught the ADR's own hole**, and it is the red
//! test for the whole unit: the waiter's locker *commits*, the waiter waits, re-runs — and its own
//! `COMMIT` **succeeds**, with the row at 111. A version of this unit that implements the wait and
//! the statement re-run and nothing else passes every assertion up to that last one, because
//! first-committer-wins is measured against the transaction's `start_ts` and the locker's commit
//! landed after it. The failure would simply move from the `UPDATE` to the `COMMIT`.
//!
//! Measured on PostgreSQL 19 with two interleaved `psql` sessions: B's `UPDATE` returned 1.74 s
//! after it was sent, and `n + 100` over a row A had just moved from 10 to 11 gave **111**.
//!
//! # The barrier rule
//!
//! Every gate here is on a **transaction's edge** — A's write is buffered, A has committed — and
//! never on "the thread started". Every earlier racy test in this family was the second kind
//! (`docs/plans/phase-9-rails.md`, the flakes lane), and the thing under test here is precisely
//! what happens *between* two edges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;

/// How long a test waits for the other session to reach its edge before calling it wedged. Long
/// enough that a loaded container is not a failure, short enough that a genuine hang is one.
const EDGE: Duration = Duration::from_secs(10);

/// Two sessions on one store, and a channel each way to gate on their transactions' edges.
struct Pair {
    store: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
}

impl Pair {
    fn new(fixture: &[&str]) -> Self {
        let store: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let catalog = Arc::new(Catalog::new());
        let mut setup = parity::Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        for statement in fixture {
            setup.run(statement).unwrap();
        }
        Pair { store, catalog }
    }

    fn session(&self) -> parity::Node {
        parity::Node::on(
            Arc::clone(&self.store),
            Arc::clone(&self.catalog),
            1,
            "esker",
            &[],
        )
    }
}

/// Waits for the other session to say it has reached an edge, and fails rather than hanging.
fn edge(from: &Receiver<&'static str>, what: &str) {
    match from.recv_timeout(EDGE) {
        Ok(_) => {}
        Err(error) => panic!("the other session never reached `{what}`: {error}"),
    }
}

fn reached(to: &Sender<&'static str>, what: &'static str) {
    to.send(what).unwrap();
}

/// **The red test for the whole unit.** A holds the row, B blocks, A commits, B proceeds on A's
/// version — and B's own `COMMIT` succeeds.
///
/// The last assertion is the one a wait-and-re-run without a per-key read timestamp fails: B's
/// prewrite finds A's write at a `commit_ts` above B's `start_ts` and answers `40001` at the
/// commit instead of at the update (ADR 0057 §4).
#[test]
fn a_waiter_whose_locker_commits_commits_too_and_sees_the_new_row() {
    let pair = Pair::new(&[
        "CREATE TABLE rc (id bigint primary key, n bigint)",
        "INSERT INTO rc (id, n) VALUES (1, 10)",
    ]);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let waiter = std::thread::spawn(move || {
        edge(&hears_a, "A's write is buffered");
        b.run("BEGIN").unwrap();
        reached(&b_says, "B is about to write");
        // Blocks here on a real server, and must block here: A's lock is live.
        let update = b.run("UPDATE rc SET n = n + 100 WHERE id = 1");
        let commit = b.run("COMMIT");
        (update, commit)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("UPDATE rc SET n = n + 1 WHERE id = 1").unwrap();
    reached(&a_says, "A's write is buffered");
    edge(&hears_b, "B is about to write");
    // B is inside its `UPDATE` now, or about to be. A commits, which is what releases it.
    std::thread::sleep(Duration::from_millis(200));
    a.run("COMMIT")
        .expect("A is the first writer: nothing may stop its commit");

    let (update, commit) = waiter.join().unwrap();
    update.expect("B's UPDATE must wait for A and then proceed, not fail");
    commit.expect("B's COMMIT must succeed: A's commit is B's input, not its conflict");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM rc WHERE id = 1"),
        [["111"]],
        "the arithmetic is on A's committed version, not on the one B first read"
    );
}
