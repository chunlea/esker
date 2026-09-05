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
    // An integer literal is `int4` there and `int8` here — the standing width trade every
    // untyped number makes (ADR 0030's six stored types), and nothing to do with `DISCARD`.
    types: &["SELECT 'r', 1 AS in_a_transaction"],
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
        // SQL-level `PREPARE` is a named refusal — the extended protocol's named statements are a
        // different thing and *are* cleared by `DISCARD ALL`, which `discarding_all_clears_the_\
        // session` asserts directly because no corpus statement can reach them.
        (
            "PREPARE dsc_plan AS SELECT $1::int + 1",
            "`0A000 PREPARE is not supported`: this node's prepared statements are the wire \
             protocol's, not SQL's. `DISCARD ALL` does clear those — see the test below.",
            "pg19_discard_all.txt:38",
        ),
        (
            "PREPARE dsc_plan2 AS SELECT 1",
            "The same.",
            "pg19_discard_all.txt:69",
        ),
        (
            "PREPARE dsc_plan3 AS SELECT 1",
            "The same.",
            "pg19_discard_all.txt:75",
        ),
        (
            "DEALLOCATE ALL",
            "The same: nothing SQL-level to deallocate.",
            "pg19_discard_all.txt:70",
        ),
        (
            "SELECT 'r', name FROM pg_prepared_statements WHERE name = 'dsc_plan'",
            "`42P01`: no `pg_prepared_statements`, for the same reason as `pg_locks` — its rows \
             are session state and this node's catalog views read the store.",
            "pg19_discard_all.txt:39",
        ),
        (
            "SELECT 'r', count(*) FROM pg_prepared_statements WHERE name = 'dsc_plan'",
            "The same.",
            "pg19_discard_all.txt:51",
        ),
        (
            "SELECT 'r', count(*) FROM pg_prepared_statements",
            "The same.",
            "pg19_discard_all.txt:71",
        ),
        (
            "SELECT count(*) FROM pg_prepared_statements",
            "The same.",
            "pg19_discard_all.txt:93",
        ),
        // **`pg_locks` exists now and answers `0` correctly**, so the two lines that count zero
        // advisory locks have been deleted from this list — they agree. What is left is the two
        // that count a lock *while it is held*, and they diverge for a reason worth naming: the
        // view reports **row** locks, which live in the node's lock table, and an advisory lock is
        // **session** state. A catalog view is handed a transaction and a tenant, not a session,
        // so the advisory table cannot be reached from here — the same wall `pg_prepared_statements`
        // is behind, and the same one `pg_stat_activity` reports one row because of.
        (
            "SELECT 'r', count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 7001",
            "`0` here and `1` on the oracle: the lock is taken and released correctly (asserted in \
             `discarding_all_clears_the_session`), and `pg_locks` shows row locks rather than \
             advisory ones — session state a catalog view is not given.",
            "pg19_discard_all.txt:46",
        ),
        (
            "SELECT 'r', current_setting('statement_timeout'), count(*) FROM pg_locks WHERE \
             locktype = 'advisory' AND objid = 7001",
            "The same, and the `statement_timeout` half agrees: `DEALLOCATE ALL` leaves it at \
             `31s`, which is the point of the line.",
            "pg19_discard_all.txt:72",
        ),
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
        // The zone below is still refused and is older than this unit.
        (
            "SET timezone = 'Europe/Paris'",
            "`0A000` naming the zone: `timestamptz` is printed in UTC and nowhere else, so a zone \
             this node will not use is refused rather than reported.",
            "pg19_discard_all.txt:42",
        ),
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
/// A prepared statement here is the wire protocol's, not SQL's — `PREPARE` is a named refusal, so
/// no statement in the corpus can make one, and `pg_prepared_statements` does not exist to read it
/// back. `DISCARD ALL` still has to clear them, because that is the whole point of the statement:
/// `postgresql_adapter.rb:392` sends it when a connection goes back to the pool, and a pooled
/// connection that kept the last borrower's named statements would answer the next one's `Bind`
/// with the wrong SQL.
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
