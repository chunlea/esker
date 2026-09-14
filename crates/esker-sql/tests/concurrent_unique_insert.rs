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
//! The first two tests are the deterministic halves of the READ COMMITTED row: the second writer
//! **waits** for the first, and is then the duplicate PostgreSQL says it is, or goes through. What
//! Rails does after the duplicate — a `SELECT … FOR UPDATE` of the row the first writer committed —
//! is the ADR's §2: the eager lock validated at the statement's snapshot, ruled and built on
//! 2026-09-13.
//!
//! **§3's tests** — ruled 2026-09-13, (ii) with the arbiter rule — are SERIALIZABLE's `40001` for a
//! key read before it was inserted (case 09) and the rest of §3's table: the sequences
//! `unique_race` shares with `serializable.rs`, which runs them against `MemoryBackend`.
//!
//! **§2's tests** — ruled 2026-09-13, a `Check` that carries the statement's read timestamp — are a
//! `FOR UPDATE` of a row committed after the transaction began (debt #91), its REPEATABLE READ twin,
//! which §2 must not move, and `relations_test.rb`'s duel itself, which needs §1 and §2 both. The
//! first and the duel were red and `#[ignore]`d while the ruling waited.
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
mod unique_race;

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use cluster::{Cluster, Session};
use esker_sql::pgwire::session::Outcome;
use unique_race::SUBSCRIBERS;

/// How long a barrier may take to be reached, sized for the worst machine this runs on. It is a
/// readiness bound: running out of it is a failure with a message, never a hang.
const BARRIER: Duration = Duration::from_secs(60);

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

/// **§2: a `FOR UPDATE` of a row another transaction committed after this one began takes the lock,
/// at READ COMMITTED.** PostgreSQL 19 (`esker-coord/s1-oracle-2026-09-13/e/`): a row updated
/// after the locker's first statement comes back at its new value, `11`, and a row inserted after it
/// comes back too, `20`.
///
/// Here the lock is ADR 0088's eager one, a `Check` prewritten when the statement runs. A `Check` was
/// validated at the transaction's `start_ts`, so both were refused `40001 could not serialize access
/// due to concurrent update: a commit at … beat this transaction at …` (debt #91); it now carries the
/// statement's read timestamp and is validated there. No unique index is in it, which is why it is a
/// test of its own: this is every `lock!` in a block that began before somebody else's update of the
/// row.
#[test]
fn a_for_update_of_a_row_committed_after_the_transaction_began_takes_the_lock() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 10)").unwrap();

    let mut updater = cluster.session();
    let mut inserter = cluster.session();
    for locker in [&mut updater, &mut inserter] {
        locker.run("BEGIN").unwrap();
        locker.rows("SELECT count(*) FROM lk");
    }
    let mut other = cluster.session();
    other.run("UPDATE lk SET n = 11 WHERE id = 1").unwrap();
    other.run("INSERT INTO lk VALUES (2, 20)").unwrap();

    let after_update = updater.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE");
    let after_insert = inserter.run("SELECT n FROM lk WHERE id = 2 FOR UPDATE");
    assert_eq!(
        (first_cell(&after_update), first_cell(&after_insert)),
        (Some("11".to_owned()), Some("20".to_owned())),
        "the updated row answered {after_update:?} and the inserted one {after_insert:?}"
    );
    for locker in [&mut updater, &mut inserter] {
        let _ = locker.run("ROLLBACK");
    }
}

/// **§2's REPEATABLE READ twin stays what it was.** PostgreSQL 19 (`e3` and `e4` in
/// `esker-coord/s1-oracle-2026-09-13/e/`): a transaction snapshot sees neither commit, so a `FOR
/// UPDATE` of the row updated after it is `40001 could not serialize access due to concurrent update`,
/// and one of the row inserted after it finds no row and commits. REPEATABLE READ sets no statement
/// timestamp, so its lock is still tag 5 at `start_ts`: this is the guard that §2 did not reach it.
#[test]
fn a_for_update_at_repeatable_read_keeps_the_transaction_s_snapshot() {
    let cluster = Cluster::start();
    let mut setup = cluster.session();
    setup
        .run("CREATE TABLE lk (id bigint primary key, n bigint)")
        .unwrap();
    setup.run("INSERT INTO lk VALUES (1, 10)").unwrap();

    let mut updater = cluster.session();
    let mut inserter = cluster.session();
    for locker in [&mut updater, &mut inserter] {
        locker.run("BEGIN").unwrap();
        locker
            .run("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .unwrap();
        locker.rows("SELECT count(*) FROM lk");
    }
    let mut other = cluster.session();
    other.run("UPDATE lk SET n = 11 WHERE id = 1").unwrap();
    other.run("INSERT INTO lk VALUES (2, 20)").unwrap();

    let refused = updater
        .run("SELECT n FROM lk WHERE id = 1 FOR UPDATE")
        .expect_err("e3: the row changed after the snapshot, so it cannot be locked");
    assert_eq!(refused.sqlstate(), "40001", "e3: {refused}");
    let _ = updater.run("ROLLBACK");

    let inserted = inserter
        .run("SELECT n FROM lk WHERE id = 2 FOR UPDATE")
        .expect("e4: a row the snapshot does not see is not an error");
    assert_eq!(row_count(&inserted), 0, "e4: {inserted:?}");
    inserter.run("COMMIT").expect("e4 commits");
}

/// **ADR 0114 §3: SERIALIZABLE refuses a key it had read with `40001`.** B reads that there is no
/// `bob`, A inserts `bob` and commits, and then B inserts `bob`. PostgreSQL 19 answers `40001 could not
/// serialize access due to read/write dependencies among transactions` at the `INSERT` (case 09),
/// because what B read has moved under it. This node is to answer the same code, at `COMMIT`, where
/// it answered `23505` before §3.
#[test]
fn serializable_refuses_a_unique_key_committed_after_it_was_read_with_40001() {
    let cluster = cluster_with_subscribers();
    let mut b = cluster.session();
    b.run("BEGIN").unwrap();
    b.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    assert!(!find_by(&mut b, "bob").unwrap(), "there is no bob yet");

    let mut a = cluster.session();
    a.run("BEGIN").unwrap();
    a.run("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    a.run("INSERT INTO subscribers (nick) VALUES ('bob')")
        .unwrap();
    a.run("COMMIT").unwrap();

    let refused = b
        .run("INSERT INTO subscribers (nick) VALUES ('bob')")
        .and_then(|_| b.run("COMMIT"))
        .expect_err("B read the key A then committed, so B may not commit");
    assert_eq!(refused.sqlstate(), "40001", "{refused}");
    let _ = b.run("ROLLBACK");
    assert_eq!(bobs(&cluster), "1");
}

impl unique_race::Sql for Session {
    fn sql(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        self.run(sql)
    }
}

/// One of ADR 0114 §3's races on a fresh cluster: three sessions of one node, over three real stores.
fn run_race(race: unique_race::Race) {
    let cluster = cluster_with_subscribers();
    unique_race::assert_refused_as_postgres(
        &mut cluster.session(),
        &mut cluster.session(),
        &mut cluster.session(),
        race,
    );
}

/// **Case 07: SERIALIZABLE, and B never read `bob`** — `23505`. Nothing B read has moved; it only
/// collided. The control for case 09.
#[test]
fn serializable_after_no_read_is_a_duplicate_key() {
    run_race(unique_race::CASE_07);
}

/// **Case 13: SERIALIZABLE `ON CONFLICT DO NOTHING`, after reading `bob`** — `40001`, which
/// PostgreSQL raises from `ExecCheckTupleVisible`: the arbiter found a row B's snapshot cannot see.
#[test]
fn serializable_on_conflict_do_nothing_after_a_count_is_refused_with_40001() {
    run_race(unique_race::CASE_13);
}

/// **Case 13b: the same, with Rails' `find_by` as the read** — case 13's answer, because what decides
/// it is the arbiter's read and not B's (case 14).
#[test]
fn serializable_on_conflict_do_nothing_after_a_find_by_is_refused_with_40001() {
    run_race(unique_race::CASE_13B);
}

/// **Case 14: SERIALIZABLE `ON CONFLICT DO NOTHING`, and B never read `bob`** — still `40001`: the
/// arbiter read it.
#[test]
fn serializable_on_conflict_do_nothing_after_no_read_is_refused_with_40001() {
    run_race(unique_race::CASE_14);
}

/// **Case 15: REPEATABLE READ `ON CONFLICT DO NOTHING`, after reading `bob`** — `40001`: the arbiter
/// rule, at the level that keeps no read set.
#[test]
fn repeatable_read_on_conflict_do_nothing_after_a_count_is_refused_with_40001() {
    run_race(unique_race::CASE_15);
}

/// **Case 16: REPEATABLE READ `ON CONFLICT DO NOTHING`, and B never read `bob`** — `40001`.
#[test]
fn repeatable_read_on_conflict_do_nothing_after_no_read_is_refused_with_40001() {
    run_race(unique_race::CASE_16);
}

/// **Case 04: REPEATABLE READ, a plain `INSERT` after reading `bob`, while A is live** — `23505`. The
/// guard from the other side: at this level a read is not a read set, and only an arbiter's counts.
#[test]
fn repeatable_read_insert_after_a_read_is_a_duplicate_key_while_the_holder_is_live() {
    run_race(unique_race::CASE_04);
}

/// **Case 10: the same, with A committed before B's `INSERT`** — `23505`.
#[test]
fn repeatable_read_insert_after_a_read_is_a_duplicate_key_when_the_holder_committed_first() {
    run_race(unique_race::CASE_10);
}

/// **The acceptance: `relations_test.rb`'s two tests, statement for statement.**
/// `test_multiple_find_or_create_by_within_transactions` and its `_bang_` twin send the same SQL —
/// PostgreSQL 19's own log of the file shows both — so the duel runs twice on one cluster with the
/// file's `teardown` between, as the file runs them. Both sessions commit both times, and there is
/// one `bob`.
///
/// The race is Rails' and is not steered: B wakes as soon as A has inserted, while A's `COMMIT` is
/// still on its way, so whether B meets A's lock or A's commit is the machine's choice. It needs
/// both halves of ADR 0114: §1's wait and §2's lock. It was red and `#[ignore]`d while §2 waited for
/// its ruling, and it is #90's acceptance.
#[test]
fn relations_test_s_find_or_create_by_duel_commits_both_sessions() {
    let cluster = cluster_with_subscribers();
    for round in ["find_or_create_by", "find_or_create_by!"] {
        let (first, second) = duel(&cluster);
        assert!(
            first.is_ok() && second.is_ok(),
            "{round}: A answered {first:?} and B answered {second:?}"
        );
        assert_eq!(bobs(&cluster), "1", "{round}");
        cluster.session().run("DELETE FROM subscribers").unwrap();
    }
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

/// The duel from `relations_test.rb`, with Rails' two `Concurrent::Event`s as channels.
///
/// Each thread wakes the other **whatever its own statements answered**: a thread that returned
/// early would otherwise leave the other waiting out the whole barrier, and a failure that costs a
/// minute to report is a failure nobody waits for.
fn duel(cluster: &Cluster) -> (esker_sql::Result<()>, esker_sql::Result<()>) {
    let (a_wakeup, a_hears) = channel::<()>();
    let (b_wakeup, b_hears) = channel::<()>();
    let mut a = cluster.session();
    let mut b = cluster.session();
    let first = std::thread::spawn(move || {
        a_hears.recv_timeout(BARRIER).unwrap();
        let created = begin_then_find_or_create_by(&mut a);
        b_wakeup.send(()).unwrap();
        created.and_then(|()| a.run("COMMIT").map(|_| ()))
    });
    let second = std::thread::spawn(move || {
        // "Read the record prematurely for MySQL REPEATABLE READ to kick in" — the test's own words.
        let read = b.run("BEGIN").and_then(|_| find_by(&mut b, "bob"));
        a_wakeup.send(()).unwrap();
        b_hears.recv_timeout(BARRIER).unwrap();
        read.and_then(|_| find_or_create_by(&mut b, "bob"))
            .and_then(|()| b.run("COMMIT").map(|_| ()))
    });
    (first.join().unwrap(), second.join().unwrap())
}

/// The first thread's block up to the point where it wakes the second.
fn begin_then_find_or_create_by(a: &mut Session) -> esker_sql::Result<()> {
    a.run("BEGIN")?;
    find_or_create_by(a, "bob")
}

/// `find_by(attributes) || create_or_find_by(attributes)`, which is Rails 8.1's
/// `find_or_create_by`, with `create_or_find_by`'s rescue of `RecordNotUnique`.
fn find_or_create_by(session: &mut Session, nick: &str) -> esker_sql::Result<()> {
    if find_by(session, nick)? {
        return Ok(());
    }
    session.run("SAVEPOINT active_record_1")?;
    match session.run(&format!(
        "INSERT INTO subscribers (nick) VALUES ('{nick}') RETURNING nick"
    )) {
        Ok(_) => session.run("RELEASE SAVEPOINT active_record_1").map(|_| ()),
        Err(error) if error.sqlstate() == "23505" => {
            session.run("ROLLBACK TO SAVEPOINT active_record_1")?;
            let found = session.run(&format!(
                "SELECT nick FROM subscribers WHERE nick = '{nick}' AND nick = '{nick}' LIMIT 1 \
                 FOR UPDATE"
            ))?;
            assert_eq!(
                row_count(&found),
                1,
                "find_by! found nothing after a duplicate"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// The first column of the first row an answer carries, as text.
fn first_cell(answer: &esker_sql::Result<Outcome>) -> Option<String> {
    if let Ok(Outcome::Rows { rows, .. }) = answer {
        rows.first()
            .and_then(|row| row.first())
            .and_then(Option::as_ref)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    } else {
        None
    }
}
