//! `DISCARD ALL` — **what a pooled connection is reset with**, 26 tests over 8 files.
//!
//! Not a statement any test writes: `postgresql_adapter.rb:392` sends it when the adapter returns
//! a connection to the pool, so it lands on every file that does. The four targets are captured
//! one at a time against what each must *not* touch, which is the half an implementation that
//! treats `DISCARD` as one word gets wrong.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own state.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Nothing.** The one entry that stood here was `SELECT 'r', 1 AS in_a_transaction`, declared
    // `bigint` against a real server's `integer` — the standing width trade every untyped number
    // used to make. The literal ladder gained its `int4` rung and the row agrees, so it is gone.
    types: &[],
    answers: &[
        // **Every `DISCARD` in this file agrees.** What is listed below is the state a real server
        // has and this node does not, so there is nothing for the statement to reset — the
        // refusals are all older than this unit and each names its own missing feature.
        //
        // The temp-table entries that used to stand here are **gone**: `CREATE TEMPORARY TABLE`
        // was a named refusal, so `DISCARD TEMP` had nothing to drop and this file said so out
        // loud rather than assuming it in the code. ADR 0054 built the tables, and the harness
        // failed this test on three entries that had started agreeing — which is the whole point
        // of writing an absence down. `DISCARD TEMP` now drops what it names.
        //
        // **Eight more went the same way**, and they are the reason to write an absence down
        // rather than assume it: the four SQL prepared-statement statements and the four reads of
        // `pg_prepared_statements`. SQL `PREPARE` was a named refusal and the view did not exist,
        // so this file recorded both — and when the statements landed, rule 2 failed this test and
        // named all eight rather than letting a stale list quietly under-claim the node. What the
        // corpus proves now is the whole sequence: `DISCARD PLANS` and `DISCARD TEMP` leave a
        // prepared statement alone, `DEALLOCATE ALL` and `DISCARD ALL` clear it, and the view
        // counts it either way.
        // **`pg_locks` reports advisory locks now**, so the four lines that counted them — two
        // expecting zero and two expecting one while the lock was held — all agree and have been
        // deleted from this list. The wall this comment used to describe is gone: `Settings`
        // carries the node's advisory table down to the view the same way it already carried the
        // session's prepared statements, which is exactly the way through that was predicted here.
        // **The reset itself is right and one boot value is spelled differently.** `DISCARD ALL`
        // put `statement_timeout` back to `0`; the container boots `TimeZone` at `Etc/UTC` and
        // this node at `UTC`, which is the same instant and an older difference.
        (
            "SELECT 'r', current_setting('statement_timeout'), current_setting('timezone')",
            "The timeout reset agrees at `0`. The zone is `UTC` here and `Etc/UTC` on the \
             oracle's container — the same offset under another name, and a boot value rather \
             than anything `DISCARD` did.",
            "pg19_discard_all.txt:48",
        ),
        // **`SET statement_timeout = '31s'` used to be listed here as a refusal and now agrees**:
        // the statement carries a deadline (`crate::exec::cancel`), so the value is honoured
        // rather than reported and ignored. What the corpus proves either way is that
        // `statement_timeout` reads its boot value after a `DISCARD`, and now it proves the
        // stronger version of it — from `31s` back to `0` rather than from `0` to `0`.
        // A syntax error's *message* has never been claimed to be PostgreSQL's (`phase-6a.md` §1);
        // what C1 promises is that a statement a real server accepts is never `42601`. Both refuse
        // this one and both say `42601`.
        (
            "DISCARD EVERYTHING",
            "`42601` on both, with `sqlparser`'s sentence rather than PostgreSQL's — the standing \
             trade for syntax errors, and this one even lists the four targets.",
            "pg19_discard_all.txt:87",
        ),
    ],
};

#[test]
fn every_discard_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_discard_all.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **What the corpus cannot reach: the state that lives on the *session* rather than the
/// executor.**
///
/// A **protocol-level** prepared statement — one a `Parse` message named — is what no corpus
/// statement can reach: `psql` sends `Query`, so a capture has no `Parse` in it. SQL `PREPARE` and
/// `pg_prepared_statements` are both real now and the corpus above covers them; this is the other
/// door to the same store. `DISCARD ALL` has to clear it, because that is the whole point of the
/// statement: `postgresql_adapter.rb:392` sends it when a connection goes back to the pool, and a
/// pooled connection that kept the last borrower's named statements would answer the next one's
/// `Bind` with the wrong SQL.
///
/// The advisory lock is asserted here for the same reason — `pg_locks` is a declared divergence,
/// so the release is invisible to the corpus and provable only by taking the lock again.
#[test]
fn discarding_all_clears_the_session() {
    use std::sync::Arc;

    use esker_sql::backend::{Backend, MemoryBackend};
    use esker_sql::catalog::Catalog;
    use esker_sql::exec::Executor;
    use esker_sql::pgwire::message::Frontend;
    use esker_sql::pgwire::session::Session;

    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn Backend>;
    let mut executor = Executor::new(
        backend,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    );
    let mut session = Session::new();
    let mut out = Vec::new();

    // A named statement, the way the extended protocol makes one.
    session.handle(
        &Frontend::Parse {
            statement: "s1".to_owned(),
            sql: "SELECT 1".to_owned(),
            param_types: Vec::new(),
        },
        &mut executor,
        &mut out,
    );
    out.clear();
    session.simple_query("SELECT pg_try_advisory_lock(4242)", &mut executor, &mut out);
    assert!(
        String::from_utf8_lossy(&out).contains('t'),
        "the lock was taken"
    );
    out.clear();

    session.simple_query("DISCARD ALL", &mut executor, &mut out);
    assert!(
        String::from_utf8_lossy(&out).contains("DISCARD ALL"),
        "the tag names the target: {:?}",
        String::from_utf8_lossy(&out)
    );
    out.clear();

    // **The named statement is gone**, so binding it is the protocol's "does not exist" rather
    // than a stale plan quietly answering.
    session.handle(
        &Frontend::Bind {
            portal: String::new(),
            statement: "s1".to_owned(),
            param_formats: Vec::new(),
            params: Vec::new(),
            result_formats: Vec::new(),
        },
        &mut executor,
        &mut out,
    );
    let answer = String::from_utf8_lossy(&out);
    assert!(
        answer.contains("26000") || answer.to_lowercase().contains("does not exist"),
        "binding a discarded statement must fail: {answer}"
    );
    out.clear();

    // And the lock went with it: taking it again succeeds, which it could not if the session still
    // held it — the one observation left once `pg_locks` is a divergence.
    session.simple_query("SELECT pg_advisory_unlock(4242)", &mut executor, &mut out);
    assert!(
        String::from_utf8_lossy(&out).contains('f'),
        "DISCARD ALL released it, so there is nothing left to unlock: {:?}",
        String::from_utf8_lossy(&out)
    );
}
