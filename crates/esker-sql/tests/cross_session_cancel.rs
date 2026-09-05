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
            let mut fields = body.split(|byte| *byte == 0);
            while let Some(field) = fields.next() {
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
        let mut node = sessions.session();
        let mut session = Session::new();
        simple(&mut session, &mut node, "BEGIN");
        // Far past the cancellation, so that a cancel that never lands is a `55P03` this test can
        // name rather than a hang the runner has to kill.
        simple(&mut session, &mut node, "SET lock_timeout = '20s'");
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
    edge(&hears_b, "B is about to block");
    let _ = a_says.send("A holds it");
    let _ = hears_a;

    // The Rails hunt, verbatim — no `state` filter and no `pid <> pg_backend_pid()`.
    let mut pid = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let rows = a.rows("SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'");
        if let Some(found) = rows.first().and_then(|row| row.first()) {
            pid = Some(found.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = pid.expect("the blocked waiter must be visible with its statement");

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
