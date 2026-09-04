//! `SELECT … FOR UPDATE` / `FOR SHARE` — run 53's row-locking row, 21 tests over 4 files.
//!
//! **What the clause does here is what this node's isolation already does.** A Percolator
//! transaction is snapshot-isolated: it does not block a conflicting writer, it loses to one at
//! commit with `40001`, which is the permanent caveat
//! [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md) already records and the
//! reason the scoreboard carries two numbers. So `FOR UPDATE` and `FOR SHARE` are **accepted and
//! answer their rows**, and the ordering they buy is the one the transaction was going to enforce
//! anyway — a difference no single session can observe.
//!
//! **That is no longer what happens, and this file is where the change shows.** ADR 0057 §5 gave
//! the clause a real row lock: a `SELECT … FOR UPDATE` takes the same lock a write takes, so a
//! writer behind it waits, `NOWAIT` raises `55P03` and `SKIP LOCKED` leaves the row out. The three
//! tests at the bottom are the half of that no single session can observe, and they are why the
//! two entries this file used to list as divergences are gone.
//!
//! `FOR SHARE` is served as `FOR UPDATE`, declared: stricter than the standard asks for, which
//! costs concurrency and never correctness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', 1 FOR UPDATE",
            "**A locking clause with no `FROM` is a `sqlparser` 0.62.0 grammar gap**, so this is a \
             `42601` where PostgreSQL answers the row — the same shape as the `CREATE DATABASE` \
             option list and the `EXCLUDE` constraint before it, and a contract C1 break rather \
             than a feature. It is left declared rather than rewritten around: the statement locks \
             nothing — there is no relation for the clause to hold — so what a source rewrite \
             would buy is one row nobody asks for, where every other C1 rewrite in this crate \
             bought a statement `ActiveRecord` actually sends.",
        ),
        (
            "SELECT id FROM lk UNION SELECT id FROM lk FOR UPDATE",
            "**`UNION` is refused before the locking clause is looked at**, so the sentence is \
             about the set operation rather than about `FOR UPDATE`. PostgreSQL runs `UNION` and \
             refuses the combination — `FOR UPDATE is not allowed with UNION/INTERSECT/EXCEPT`, \
             the same `0A000` — and this node has no `UNION` at all, so the more specific message \
             names a rule it can never reach. It becomes reachable the day `UNION` lands, and this \
             line is what will say so.",
        ),
    ],
};

#[test]
fn every_row_locking_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_row_locking.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

// The half of this feature that only exists between two sessions, measured against the oracle on
// 2026-09-04 with two `psql` connections and recorded in the corpus header. Every expectation
// below is one of those measurements.

use std::sync::mpsc::channel;
use std::time::Duration;

use parity::{Pair, edge, reached};

/// The fixture both sessions work on: three rows, of which A holds the first.
const ROWS: &[&str] = &[
    "CREATE TABLE lk (id bigint primary key, n text)",
    "INSERT INTO lk (id, n) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
];

/// **The red test for the unit: a `SELECT … FOR UPDATE` must lock, so a writer behind it waits.**
///
/// A never writes. That is the whole design of this test and the second attempt at it: the first
/// had A do a `SELECT … FOR UPDATE` *and* an `UPDATE`, and it passed against a node where the
/// locking clause did nothing at all — because the `UPDATE` took the lock unit 1 already takes, so
/// the assertion was about the write and not about the clause under test.
///
/// What only `Op::Lock` can produce is B **still blocked** while A holds a lock it took by reading.
/// So the assertion is on the timing edge rather than on the final value: with A holding and B
/// inside its `UPDATE`, B must not have finished. `lock!`-then-write is the whole of
/// `ActiveRecord`'s pessimistic API, and without an eager lock two sessions both proceed.
#[test]
fn a_select_for_update_holds_the_row_against_a_writer() {
    let pair = Pair::new(ROWS);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();
    let (done_says, hears_done) = channel();

    let mut b = pair.session();
    let writer = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to write");
        // No `BEGIN`: `update_attribute` is one statement, and it must wait exactly as a block does.
        let update = b.run("UPDATE lk SET n = 'B' WHERE id = 1");
        reached(&done_says, "B has written");
        update
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    // A **reads** and locks, and does nothing else. Nothing here writes, so nothing here takes a
    // lock unless `FOR UPDATE` does.
    assert_eq!(a.rows("SELECT n FROM lk WHERE id = 1 FOR UPDATE"), [["a"]]);
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to write");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        hears_done.try_recv().is_err(),
        "B finished its UPDATE while A held the row: the FOR UPDATE locked nothing"
    );
    a.run("COMMIT").unwrap();

    writer
        .join()
        .unwrap()
        .expect("B must wait for the lock A's SELECT took, then write");

    let mut reader = pair.session();
    assert_eq!(
        reader.rows("SELECT n FROM lk WHERE id = 1"),
        [["B"]],
        "B's write is the last one, and it happened after A's commit"
    );
}

/// **`NOWAIT` on a row another session holds is `55P03`**, in PostgreSQL's own words — measured:
/// `55P03 could not obtain lock on row in relation "lk"`, and the relation is the table's name
/// rather than whatever the query called it.
#[test]
fn nowait_on_a_held_row_refuses_at_once() {
    let pair = Pair::new(ROWS);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let asker = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        let held = b.answer("SELECT id FROM lk WHERE id = 1 FOR UPDATE NOWAIT");
        let free = b.answer("SELECT id FROM lk WHERE id = 2 FOR UPDATE NOWAIT");
        let read = b.rows("SELECT count(*) FROM lk");
        reached(&b_says, "B has asked");
        (held, free, read)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B has asked");
    a.run("ROLLBACK").unwrap();

    let (held, free, read) = asker.join().unwrap();
    assert_eq!(
        held,
        parity::Answer::Refused("55P03 could not obtain lock on row in relation \"lk\"".to_owned()),
        "NOWAIT does not wait, and it does not answer the row either"
    );
    assert_eq!(
        free,
        parity::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["2".to_owned()]],
        },
        "a row nobody holds is answered, NOWAIT or not"
    );
    assert_eq!(read, [["3"]], "a plain read never blocks and never refuses");
}

/// **`SKIP LOCKED` leaves the held row out, and `LIMIT` counts what survives the skip.**
///
/// The second half is the one a plan can get wrong while every single-session test passes:
/// PostgreSQL puts `Limit` **above** `LockRows`, so `LIMIT 1 … SKIP LOCKED` against a held first
/// row answers the *second* row — measured, `2` — where a limit applied before the skip would
/// answer nothing at all. That is the whole reason a queue is written this way, so getting the
/// order wrong turns the feature into an empty answer under exactly the contention it exists for.
#[test]
fn skip_locked_drops_the_held_row_and_the_limit_counts_the_rest() {
    let pair = Pair::new(ROWS);
    let (a_says, hears_a) = channel();
    let (b_says, hears_b) = channel();

    let mut b = pair.session();
    let asker = std::thread::spawn(move || {
        edge(&hears_a, "A holds the row");
        let all = b.rows("SELECT id FROM lk ORDER BY id FOR UPDATE SKIP LOCKED");
        let limited = b.rows("SELECT id FROM lk ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED");
        let offset =
            b.rows("SELECT id FROM lk ORDER BY id LIMIT 1 OFFSET 1 FOR UPDATE SKIP LOCKED");
        reached(&b_says, "B has asked");
        (all, limited, offset)
    });

    let mut a = pair.session();
    a.run("BEGIN").unwrap();
    a.run("SELECT n FROM lk WHERE id = 1 FOR UPDATE").unwrap();
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B has asked");
    a.run("ROLLBACK").unwrap();

    let (all, limited, offset) = asker.join().unwrap();
    assert_eq!(all, [["2"], ["3"]], "the held row is not in the answer");
    assert_eq!(
        limited,
        [["2"]],
        "LIMIT 1 answers the first row that could be locked, not nothing"
    );
    assert_eq!(
        offset,
        [["3"]],
        "OFFSET counts from the rows that survived the skip too"
    );
}
