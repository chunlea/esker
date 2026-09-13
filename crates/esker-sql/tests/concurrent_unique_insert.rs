//! **Two sessions inserting one unique key** — debt #90, and
//! [ADR 0114](../../../docs/adr/0114-a-unique-key-being-written-waits-at-read-committed.md) §1.
//!
//! Run 127 attempt 6, `relations_test.rb`: `CreateOrFindByWithinTransactions` races two threads'
//! `Subscriber.transaction { find_or_create_by(nick: "bob") }` on purpose, over a table with **no
//! primary key** and a unique index on `nick`, and both of its tests were refused
//! `40001 could not serialize access due to concurrent update` where PostgreSQL passes them.
//!
//! What PostgreSQL 19 does with the second `INSERT` of one unique key while the first transaction
//! is still live, measured with two `psql` sessions interleaved by `pg_sleep`
//! (`esker-coord/s1-oracle-2026-09-13/d/`):
//!
//! ```text
//!   level            the holder commits                          the holder rolls back
//!   READ COMMITTED   waits, then 23505                           waits, then inserts
//!   REPEATABLE READ  waits, then 23505                           waits, then inserts
//!   SERIALIZABLE     waits, then 40001 if it had read the key,   waits, then inserts
//!                    else 23505
//! ```
//!
//! These are the two deterministic halves of the READ COMMITTED row: the second writer **waits**
//! for the first, and is then the duplicate PostgreSQL says it is, or goes through. What Rails does
//! after the duplicate — a `SELECT … FOR UPDATE` of the row the first writer committed — is the
//! ADR's §2, which is a format question and is not built.
//!
//! Every test here runs against three real stores, and both sessions are on one node: the wait is
//! ADR 0057's node-local row lock, and one node is the Rails suite's shape.
//!
//! # The barrier
//!
//! *"B waited"* is not read off a clock. A test that slept and then looked would pass whenever B
//! was merely slow to reach its `INSERT` — and a slow B finds the holder already committed and
//! answers `23505` with no wait at all, so the mechanism under test would be absent and the test
//! green. The holder ends only after a third session has **seen B's ungranted row in `pg_locks`**,
//! or after B has answered without one, and the second of those is the failure this file is for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use cluster::{Cluster, Session};
use esker_sql::pgwire::session::Outcome;

/// How long a barrier may take to be reached, sized for the worst machine this runs on. It is a
/// readiness bound: running out of it is a failure with a message, never a hang.
const BARRIER: Duration = Duration::from_secs(60);

/// Rails' `subscribers`, as `activerecord/test/schema/schema.rb` declares it: `id: false`, so the
/// row key is an internal row id and the only thing two rows can collide on is the index.
const SUBSCRIBERS: [&str; 2] = [
    "CREATE TABLE subscribers (nick character varying NOT NULL, name character varying, \
     id integer, books_count integer NOT NULL DEFAULT 0, update_count integer NOT NULL DEFAULT 0)",
    "CREATE UNIQUE INDEX index_subscribers_on_nick ON subscribers (nick)",
];

/// **The red test for #90's first half.** A holds `bob`; B's `INSERT` of `bob` must wait for A, and
/// when A commits it is the duplicate PostgreSQL says it is — **from the `INSERT`**, where Rails'
/// rescue can see it. The block is then usable again: `ROLLBACK TO SAVEPOINT`, a read that finds
/// A's row, and B's own `COMMIT`.
///
/// Before ADR 0114 §1, B's `INSERT` answered at once and its `COMMIT` was refused `23505` — the
/// right code in the one place `create_or_find_by` cannot rescue it.
#[test]
fn a_second_insert_of_a_unique_key_waits_and_is_a_duplicate_when_the_first_commits() {
    let cluster = cluster_with_subscribers();
    let second = second_insert_while_held(&cluster, "COMMIT");

    assert!(second.waited, "B never waited for A: {second:#?}");
    let duplicate = second
        .insert
        .as_ref()
        .expect_err("B inserted a key A had committed");
    assert_eq!(duplicate.sqlstate(), "23505", "{second:#?}");
    assert!(
        duplicate.to_string().contains("index_subscribers_on_nick"),
        "{duplicate}"
    );
    for (step, answer) in &second.rest {
        assert!(
            answer.is_ok(),
            "B's {step} after the duplicate: {second:#?}"
        );
    }
    let found = second
        .rest
        .iter()
        .find(|(step, _)| *step == "SELECT")
        .and_then(|(_, answer)| answer.as_ref().ok())
        .map(row_count);
    assert_eq!(found, Some(1), "B reads A's row: {second:#?}");
    assert_eq!(bobs(&cluster), "1");
}

/// **The holder rolls back, and the waiter's insert goes through** — which PostgreSQL 19 does at
/// every level, after the same wait.
///
/// Before ADR 0114 §1 the outcome was already this one, and the wait was missing: B's `INSERT`
/// answered while A still held the key.
#[test]
fn a_second_insert_of_a_unique_key_waits_and_goes_through_when_the_first_rolls_back() {
    let cluster = cluster_with_subscribers();
    let second = second_insert_while_held(&cluster, "ROLLBACK");

    assert!(second.waited, "B never waited for A: {second:#?}");
    assert!(
        second.insert.is_ok(),
        "nothing was there once A rolled back: {second:#?}"
    );
    for (step, answer) in &second.rest {
        assert!(answer.is_ok(), "B's {step}: {second:#?}");
    }
    assert_eq!(bobs(&cluster), "1");
}

/// What the second session answered, step by step, so that a failure names every one of them.
#[derive(Debug)]
struct Second {
    /// Whether a third session saw it waiting before the holder ended.
    waited: bool,
    /// Its `INSERT`.
    insert: esker_sql::Result<Outcome>,
    /// What it did next — `RELEASE` after an insert that went through, `ROLLBACK TO` and a read
    /// after a duplicate — and then `COMMIT`.
    rest: Vec<(&'static str, esker_sql::Result<Outcome>)>,
}

/// A holds `bob`; B runs `create_or_find_by`'s `SAVEPOINT` and `INSERT` against it; A ends with
/// `holder_ends` once B has been seen waiting, or once B has answered without waiting.
fn second_insert_while_held(cluster: &Cluster, holder_ends: &'static str) -> Second {
    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("INSERT INTO subscribers (nick) VALUES ('bob')")
        .unwrap();

    let (about_to_insert, hears_about_to_insert) = channel();
    let (inserted, hears_inserted) = channel();
    let (holder_ended, hears_holder_ended) = channel::<()>();
    let mut b = cluster.session();
    let second = std::thread::spawn(move || {
        b.run("BEGIN").unwrap();
        assert!(!find_by(&mut b, "bob").unwrap(), "there is no bob yet");
        b.run("SAVEPOINT active_record_1").unwrap();
        about_to_insert.send(()).unwrap();
        let insert = b.run("INSERT INTO subscribers (nick) VALUES ('bob') RETURNING nick");
        inserted.send(()).unwrap();
        hears_holder_ended.recv_timeout(BARRIER).unwrap();
        let rest = after_the_insert(&mut b, &insert);
        (insert, rest)
    });

    hears_about_to_insert.recv_timeout(BARRIER).unwrap();
    let waited = seen_waiting(cluster, &hears_inserted);
    a.run(holder_ends).unwrap();
    holder_ended.send(()).unwrap();
    let (insert, rest) = second.join().unwrap();
    Second {
        waited,
        insert,
        rest,
    }
}

/// What B does after its `INSERT`: `RELEASE` on success; after a duplicate, `ROLLBACK TO SAVEPOINT`
/// and a read of the row that is there; and in both cases the block's `COMMIT`.
///
/// Rails' rescue reads that row **`FOR UPDATE`**, and the lock that takes is ADR 0114 §2's subject
/// rather than §1's, so the read here is a plain one: this test is about the wait and the code, and
/// a refusal from the lock would be reported against the wrong half.
fn after_the_insert(
    b: &mut Session,
    insert: &esker_sql::Result<Outcome>,
) -> Vec<(&'static str, esker_sql::Result<Outcome>)> {
    let mut rest = Vec::new();
    match insert {
        Ok(_) => rest.push((
            "RELEASE SAVEPOINT",
            b.run("RELEASE SAVEPOINT active_record_1"),
        )),
        Err(error) if error.sqlstate() == "23505" => {
            rest.push((
                "ROLLBACK TO SAVEPOINT",
                b.run("ROLLBACK TO SAVEPOINT active_record_1"),
            ));
            rest.push((
                "SELECT",
                b.run("SELECT nick FROM subscribers WHERE nick = 'bob' LIMIT 1"),
            ));
        }
        Err(_) => {}
    }
    rest.push(("COMMIT", b.run("COMMIT")));
    rest
}

/// Whether a third session sees an ungranted row in `pg_locks` before B's `INSERT` answers.
fn seen_waiting(cluster: &Cluster, hears_inserted: &Receiver<()>) -> bool {
    let mut watcher = cluster.session();
    let began = Instant::now();
    while began.elapsed() < BARRIER {
        match hears_inserted.try_recv() {
            Ok(()) => return false,
            Err(TryRecvError::Disconnected) => {
                panic!("B's thread ended before its INSERT answered")
            }
            Err(TryRecvError::Empty) => {}
        }
        if !watcher
            .rows("SELECT pid FROM pg_locks WHERE granted = false")
            .is_empty()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "B neither waited nor answered within {BARRIER:?} ({:?} elapsed)",
        began.elapsed()
    );
}

/// `Subscriber.find_by(nick:)`: whether there is one.
fn find_by(session: &mut Session, nick: &str) -> esker_sql::Result<bool> {
    let found = session.run(&format!(
        "SELECT nick FROM subscribers WHERE nick = '{nick}' LIMIT 1"
    ))?;
    Ok(row_count(&found) > 0)
}

/// The rows an outcome carries; a command with no result set carries none.
fn row_count(outcome: &Outcome) -> usize {
    match outcome {
        Outcome::Rows { rows, .. } => rows.len(),
        Outcome::Done { .. } => 0,
    }
}

/// How many `bob`s a fresh session sees.
fn bobs(cluster: &Cluster) -> String {
    cluster
        .session()
        .rows("SELECT count(*) FROM subscribers WHERE nick = 'bob'")[0][0]
        .clone()
        .expect("count(*) is never NULL")
}

/// A cluster with Rails' `subscribers` in it.
fn cluster_with_subscribers() -> Cluster {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    for statement in SUBSCRIBERS {
        setup.run(statement).unwrap();
    }
    cluster
}
