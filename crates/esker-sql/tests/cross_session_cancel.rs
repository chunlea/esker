//! One session cancels another's **blocked** statement, over the extended protocol.
//!
//! `transaction_test.rb`'s `raises QueryCanceled when canceling statement due to user request`,
//! which has been the file's last failure since run 86 and reports
//! `ActiveRecord::QueryCanceled expected but nothing was raised`. The shape is a holder that takes
//! a row, a waiter that blocks behind it, and the **holder itself** — from inside its own open
//! transaction — hunting the waiter in `pg_stat_activity` and issuing one `pg_cancel_backend`.
//!
//! # Why this file exists beside `statement_cancellation.rs`
//!
//! That file already cancels a blocked writer and passes. It differs from the Rails test in three
//! ways that were each checked and each ruled out — the hunt returns the waiter and not the holder,
//! one request is enough, and it does not matter that the canceller is the one holding the lock.
//! What it does *not* do is drive the victim through `Parse`/`Bind`/`Execute`: `parity::Node`
//! mirrors the session's dispatch but never goes through `pgwire::session::Session`. So that is
//! what this file adds, and the victim here is a real message exchange rather than a call.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;
use parity::{Pair, edge, reached};

/// The `SQLSTATE` of the first `ErrorResponse` in a response, if there is one.
///
/// An `ErrorResponse` is a sequence of `field-code ++ NUL-terminated value`, and `C` is the code.
fn sqlstate(out: &[u8]) -> Option<String> {
    let mut at = 0;
    while at + 5 <= out.len() {
        let len = u32::from_be_bytes([out[at + 1], out[at + 2], out[at + 3], out[at + 4]]) as usize;
        if out[at] == b'E' {
            let body = &out[at + 5..at + 1 + len];
            for field in body.split(|byte| *byte == 0) {
                if field.first() == Some(&b'C') {
                    return Some(String::from_utf8_lossy(&field[1..]).into_owned());
                }
            }
        }
        at += 1 + len;
    }
    None
}

/// Runs one statement through the extended protocol, the way a prepared client does.
fn extended(
    session: &mut Session,
    node: &mut parity::Node,
    sql: &str,
    params: Vec<Option<Vec<u8>>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for message in [
        Frontend::Parse {
            statement: "s".to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Bind {
            portal: String::new(),
            statement: "s".to_owned(),
            param_formats: Vec::new(),
            params,
            result_formats: Vec::new(),
        },
        Frontend::Describe {
            target: Target::Portal,
            name: String::new(),
        },
        Frontend::Execute {
            portal: String::new(),
            max_rows: 0,
        },
        Frontend::Sync,
        Frontend::Close {
            target: Target::Statement,
            name: "s".to_owned(),
        },
    ] {
        session.handle(&message, &mut node.executor, &mut out);
    }
    out
}

/// A simple-protocol statement through the same `Session`.
fn simple(session: &mut Session, node: &mut parity::Node, sql: &str) -> Vec<u8> {
    let mut out = Vec::new();
    session.handle(
        &Frontend::Query(sql.to_owned()),
        &mut node.executor,
        &mut out,
    );
    out
}

/// **The waiter's statement dies with `57014`, and the holder is what kills it.**
#[test]
fn a_blocked_statement_is_cancelled_by_the_session_holding_its_row() {
    let pair = Pair::new(&[
        "CREATE TABLE samples (id bigint primary key, value bigint)",
        "INSERT INTO samples VALUES (1, 1)",
    ]);
    let (b_says, hears_b) = channel();
    let (a_says, hears_a) = channel();

    // B: the victim, on its own session, through `Parse`/`Bind`/`Execute`.
    let sessions = pair.sessions();
    let victim = std::thread::spawn(move || {
        let hears_a = hears_a;
        let mut node = sessions.session();
        let mut session = Session::new();
        simple(&mut session, &mut node, "BEGIN");
        // Far past the cancellation, so that a cancel that never lands is a `55P03` this test can
        // name rather than a hang the runner has to kill.
        simple(&mut session, &mut node, "SET lock_timeout = '20s'");
        // **A must hold the row before B asks for it.** Without this B is free to win the race,
        // take the lock itself and finish — which is not a cancellation that missed, it is a test
        // that never set up the situation it names. It failed exactly that way twice under a full
        // gate and never once alone: `left: None` in seven milliseconds, a victim that simply
        // committed. The channel for it existed and was dropped instead of waited on.
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to block");
        let out = extended(
            &mut session,
            &mut node,
            "SELECT value FROM samples WHERE id = $1 FOR UPDATE",
            vec![Some(b"1".to_vec())],
        );
        sqlstate(&out)
    });

    // A: the holder, which is also the canceller — from inside its own open transaction.
    let mut a = pair.session();
    let mut a_session = Session::new();
    simple(&mut a_session, &mut a, "BEGIN");
    extended(
        &mut a_session,
        &mut a,
        "SELECT value FROM samples WHERE id = $1 FOR UPDATE",
        vec![Some(b"1".to_vec())],
    );
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to block");

    // The Rails hunt, verbatim — no `state` filter and no `pid <> pg_backend_pid()` — and its
    // **first row**, which is the whole of what this asserts.
    //
    // **Waiting until that row is somebody else is not a weakening of it.** A holds the row and
    // is idle inside its transaction, so its own retained `FOR UPDATE` matches this predicate
    // too; until B's statement is actually running there is only one row to return and it is A's.
    // The Rails test spends a `sleep(0.5)` on exactly that window and a gate under load can
    // outlast it — which is how this failed once at 3,571 tests: the hunt found A, A cancelled
    // *itself*, its block ended, the row was freed and B simply succeeded (`left: None`). So the
    // loop waits for the waiter to be visible and then asserts what the ordering decides: that
    // the first row is the running session and not the idle holder.
    let a_pid = a.rows("SELECT pg_backend_pid()")[0][0].clone();
    let mut pid = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let rows = a.rows("SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'");
        match rows.first().and_then(|row| row.first()) {
            Some(found) if *found != a_pid => {
                pid = Some(found.clone());
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let pid = pid
        .expect("the hunt's first row must become the blocked waiter rather than the idle holder");

    // **One request, which is what the Rails test issues.**
    assert_eq!(
        a.rows(&format!("SELECT pg_cancel_backend({pid})")),
        [["t".to_owned()]],
        "the pid must be one this node holds"
    );

    let answered = victim.join().unwrap();
    simple(&mut a_session, &mut a, "ROLLBACK");
    assert_eq!(
        answered.as_deref(),
        Some("57014"),
        "the blocked statement must be cancelled, not left to its lock_timeout"
    );
}

/// **A connection that finished with `FOR UPDATE` and committed must not still answer the hunt.**
///
/// This is the Rails failure, and it is not in the cancelling at all — it is in who gets cancelled.
/// `Running::drop` keeps the statement text so an idle session can report its last query, which is
/// right; what was missing is that **`COMMIT` is itself a statement and replaces it**. Measured on
/// PostgreSQL 19, a session that ran `SELECT … FOR UPDATE` and committed reports:
///
/// ```text
/// 46238|idle|COMMIT;
/// ```
///
/// The node reported the `SELECT`, for ever. So every connection an earlier `Sample.lock.find`
/// touched stayed in the pool matching `query LIKE '% FOR UPDATE'`, with a **lower pid** than the
/// live waiter — and `pg_stat_activity` is ordered by pid, so `query_value` took the first row and
/// Rails cancelled a session that was doing nothing. `pg_cancel_backend` answered `true` because
/// that session exists, the waiter ran to completion, and the test reported
/// `QueryCanceled expected but nothing was raised`.
#[test]
fn a_committed_session_no_longer_answers_a_hunt_for_its_last_statement() {
    let pair = Pair::new(&[
        "CREATE TABLE samples (id bigint primary key, value bigint)",
        "INSERT INTO samples VALUES (1, 1)",
    ]);

    // An earlier test's connection, back in the pool: it locked a row and committed.
    let mut pooled = pair.session();
    pooled.run("BEGIN").unwrap();
    pooled
        .run("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
        .unwrap();
    pooled.run("COMMIT").unwrap();

    let pid = pooled.rows("SELECT pg_backend_pid()")[0][0].clone();

    // **Scoped to this session's own row**, not to the whole view. `session::register` is a
    // process-global map, so every session any other test in this binary holds is in
    // `pg_stat_activity` too — an unscoped `is_empty()` here failed on the sessions of the test
    // running beside it, which is a fact about the harness rather than about the node.
    let mut watcher = pair.session();
    let hunt = watcher.rows(&format!(
        "SELECT query FROM pg_stat_activity WHERE pid = {pid} AND query LIKE '% FOR UPDATE'"
    ));
    assert!(
        hunt.is_empty(),
        "an idle, committed session still answers the hunt: {:?}",
        watcher.rows(&format!(
            "SELECT pid, state, query FROM pg_stat_activity WHERE pid = {pid}"
        ))
    );

    // And the same for the other two ways a block ends.
    for ending in ["ROLLBACK", "COMMIT"] {
        let mut other = pair.session();
        let other_pid = other.rows("SELECT pg_backend_pid()")[0][0].clone();
        other.run("BEGIN").unwrap();
        other
            .run("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
            .unwrap();
        other.run(ending).unwrap();
        assert_eq!(
            watcher.rows(&format!(
                "SELECT query FROM pg_stat_activity WHERE pid = {other_pid}"
            )),
            [[ending.to_owned()]],
            "a block reports how it ended, whichever way it ended"
        );
    }
}

/// **The hunt with no `ORDER BY` finds the session that is *running* the statement.**
///
/// `transaction_test.rb` picks the first row of
/// `SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'` and cancels it. Both the
/// holder and the waiter match — the holder because its query text is retained while it sits idle
/// in its transaction, which is a real server's behaviour and this node's — so which one comes
/// first decides whether the test cancels the blocked thread it means to or its own.
///
/// **PostgreSQL's order here is not reproducible and this does not try to reproduce it.**
/// `pg_stat_activity` is read out of the `PGPROC` slot array and slots are reused, so its order is
/// neither pid nor connection order. Measured on PG19, three sessions opened a second apart:
///
/// ```text
/// connected  46822, then 46830, then 46838
/// returned   46830 | 46822 | 46838
/// ```
///
/// So this asserts the node's own rule — running sessions first — which answers the question the
/// hunt is actually asking.
#[test]
fn the_running_session_is_listed_before_the_one_idle_in_its_transaction() {
    let pair = Pair::new(&[
        "CREATE TABLE samples (id bigint primary key, value bigint)",
        "INSERT INTO samples VALUES (1, 1)",
    ]);
    let (b_pid, hears_b_pid) = channel();
    let (b_says, hears_b) = channel();
    let (a_says, a_holds) = channel();

    let sessions = pair.sessions();
    let victim = std::thread::spawn(move || {
        let mut node = sessions.session();
        let _ = b_pid.send(node.rows("SELECT pg_backend_pid()")[0][0].clone());
        node.run("BEGIN").unwrap();
        node.run("SET lock_timeout = '20s'").unwrap();
        // The same handshake the test above needs, and for the same reason: without it B may take
        // the row first, and then it is the holder and A is the waiter — the opposite of what this
        // asserts, reached without any assertion failing to say so.
        edge(&a_holds, "A holds the row");
        reached(&b_says, "B is about to block");
        let _ = node.run("SELECT value FROM samples WHERE id = 1 FOR UPDATE");
    });

    // A takes the row first and then sits idle inside its transaction, which is the state that
    // retains its query text and makes it match the hunt.
    let mut a = pair.session();
    let a_pid = a.rows("SELECT pg_backend_pid()")[0][0].clone();
    a.run("BEGIN").unwrap();
    a.run("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
        .unwrap();
    let waiter = hears_b_pid.recv().expect("B says who it is");
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to block");
    std::thread::sleep(Duration::from_millis(300));

    let mut watcher = pair.session();
    let hunt = watcher.rows(&format!(
        "SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE' \
         AND pid IN ({a_pid}, {waiter})"
    ));
    assert_eq!(
        hunt.len(),
        2,
        "both the holder and the waiter match, as they do on a real server: {hunt:?}"
    );
    assert_eq!(
        hunt[0][0], waiter,
        "the blocked session comes first; the holder {a_pid} is idle in its transaction"
    );

    a.run("ROLLBACK").unwrap();
    let _ = victim.join();
}

/// **The cancel lands, and then the lock frees — and the statement must still die.**
///
/// This is the sequence the Rails test actually produces, and the one neither test above reaches.
/// r1 tapped both servers on the same seed (`triage/querycanceled-divergence.md`):
///
/// ```text
/// [869] SELECT … FOR UPDATE      B blocks behind A
/// [870] SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'
/// [871] SELECT pg_cancel_backend(50)
/// [872] COMMIT                   A releases the row
///  node [869] -> [513.3 ms] 1 row(s)          pg19 [869] -> [513.2 ms] ERROR 57014
/// ```
///
/// Identical SQL, identical binds, **513.3 ms against 513.2 ms** — so the node was not failing to
/// receive the cancel and was not ignoring it: both servers stop waiting at the same instant,
/// which is A's `COMMIT`. What differed is what happens next. The wait loop asked `cancel::check`
/// only in the arm it takes **while the lock is still held**; when the lock came free it took the
/// other arm, restarted the statement, and the restart ran to completion with the flag still set.
///
/// So the outcome depended on which of two things B saw first when it woke, two milliseconds
/// apart — and the cancel and the commit arrive microseconds apart, so it lost nearly every time.
/// The sweeps read 12, 11 and 10 FAIL of 12 across three builds, which is what a race that is
/// almost always lost looks like from the outside.
#[test]
fn a_cancel_survives_the_lock_coming_free_underneath_it() {
    let pair = Pair::new(&[
        "CREATE TABLE samples (id bigint primary key, value bigint)",
        "INSERT INTO samples VALUES (1, 1)",
    ]);
    let (b_says, hears_b) = channel();
    let (b_pid_says, hears_b_pid) = channel();
    let (a_says, hears_a) = channel();

    let sessions = pair.sessions();
    let victim = std::thread::spawn(move || {
        let hears_a = hears_a;
        let mut node = sessions.session();
        let mut session = Session::new();
        simple(&mut session, &mut node, "BEGIN");
        // Far past anything this test does, so a missed cancel is a `55P03` with a name rather
        // than a hang the runner has to kill.
        simple(&mut session, &mut node, "SET lock_timeout = '20s'");
        b_pid_says
            .send(node.rows("SELECT pg_backend_pid()")[0][0].clone())
            .unwrap();
        edge(&hears_a, "A holds the row");
        reached(&b_says, "B is about to block");
        let out = extended(
            &mut session,
            &mut node,
            "SELECT value FROM samples WHERE id = $1 FOR UPDATE",
            vec![Some(b"1".to_vec())],
        );
        sqlstate(&out)
    });

    let mut a = pair.session();
    let mut a_session = Session::new();
    simple(&mut a_session, &mut a, "BEGIN");
    extended(
        &mut a_session,
        &mut a,
        "SELECT value FROM samples WHERE id = $1 FOR UPDATE",
        vec![Some(b"1".to_vec())],
    );
    // **B's own pid, over a channel, rather than the Rails hunt.** The hunt is what the test above
    // asserts; here it would only be a second way for this test to be about something else.
    let waiter = hears_b_pid.recv().expect("B says who it is");
    reached(&a_says, "A holds the row");
    edge(&hears_b, "B is about to block");
    // B is *about* to block; wait until it actually is, by asking the view that can see it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut watcher = pair.session();
    while Instant::now() < deadline {
        // **Not filtered by pid**: `pg_locks.pid` is `std::process::id()` for every row here,
        // the same number for every session, so it cannot name one — unlike
        // `pg_stat_activity.pid`, which is the session's. (A real server's are the same number;
        // this node's are not, which is its own divergence and not this test's subject.) One
        // waiter exists in this test, so an ungranted row is that one.
        let waiting = watcher.rows("SELECT count(*) FROM pg_locks WHERE granted = false");
        if waiting[0][0] != "0" {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    // The two statements the Rails test issues, in its order and with nothing between them: the
    // cancel, and then the commit that frees the row the victim is waiting for.
    assert_eq!(
        a.rows(&format!("SELECT pg_cancel_backend({waiter})")),
        [["t".to_owned()]]
    );
    simple(&mut a_session, &mut a, "COMMIT");

    assert_eq!(
        victim.join().unwrap().as_deref(),
        Some("57014"),
        "the cancelled statement must not go on to answer because the lock came free"
    );
}
