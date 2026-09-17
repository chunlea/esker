//! `DROP DATABASE … WITH (FORCE)`: debt #110, both halves.
//!
//! The clause is read off the source, travels on `plan::DropDatabase`, and now **acts**: a forced
//! drop ends the other sessions on the database instead of being refused by them.
//!
//! **`FORCE` replaces one refusal and nothing in front of it**, which is measured rather than
//! chosen (`esker-coord/s2-h110-force.out`, 19beta1): a template is `42809` with the clause
//! written, the database the session is connected to is `55006` with it written, and one that is
//! not there is `3D000`. Only the session count gives way.
//!
//! **What ending a session means here, and what it does not.** `terminate_others_on_database` sets
//! the same flag `pg_terminate_backend` sets, and the victim's own connection loop reads it on its
//! next round — nothing deregisters another session from the thread running the `DROP`. A real
//! server's `FORCE` returns after the backends are gone; this one returns before they have
//! noticed. So **the termination itself is not observable from `parity::Node`**, which is not a
//! connection loop and holds its pid privately.
//!
//! That has a sharp consequence worth stating rather than leaving to be rediscovered: **deleting
//! the terminate call would redden nothing *in this file*.** With it gone the `FORCE` branch is
//! empty, the drop still proceeds, and the tests below still pass — because everything observable
//! about `FORCE` from an in-process `Node` comes from *skipping the refusal*, not from ending
//! anything. The flag has no public reader and it is set on sessions belonging to a database that
//! no longer exists. So what the tests below defend is the skip and the predicate.
//!
//! **The termination itself is defended in `tests/drop_database_force_over_a_socket.rs`** (#113),
//! where the victim is a real connection and genuinely dies: it answers before the drop, is told
//! `57P01` after it, and its socket is then closed rather than merely errored. That file also
//! carries the control — a plain `DROP` in the same state is `55006` and the victim goes on
//! answering — so the ending is attributed to the clause and not to dropping a database at all.
//!
//! The two-session fixture here is **newly built** — `terminate_over_a_socket.rs` does the same
//! thing over real sockets and there was no in-process precedent for one `Node` ending another.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::parse::{parse, parse_statements};
use esker_sql::{plan, sqlstate};

#[path = "parity_harness/mod.rs"]
mod parity;

/// The database every session here is already connected to, as in `tests/database.rs`.
const SERVING: &str = "esker";

/// A cluster, and a second database on it with `count` sessions held open on that database.
///
/// The sessions are **returned to the caller**, never dropped here: a session that ends when this
/// function does would leave nothing for the statement under test to meet, and the test would pass
/// against a node that never learned to count. `tests/database.rs` names the same trap.
fn cluster_with(name: &str, count: usize) -> (parity::Node, Vec<parity::Node>, u64) {
    let backend: Arc<dyn esker_sql::backend::Backend> =
        Arc::new(esker_sql::backend::MemoryBackend::new());
    let catalog = Arc::new(esker_sql::catalog::Catalog::new());
    let mut serving = parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), 1, SERVING, &[]);
    serving.run(&format!("CREATE DATABASE {name}")).unwrap();
    let id: u64 = serving.rows(&format!(
        "SELECT oid FROM pg_database WHERE datname = '{name}'"
    ))[0][0]
        .parse()
        .unwrap();
    let held = (0..count)
        .map(|_| parity::Node::on(Arc::clone(&backend), Arc::clone(&catalog), id, name, &[]))
        .collect();
    (serving, held, id)
}

/// Lower one statement, or panic saying which one would not.
fn lowered(sql: &str) -> plan::Statement {
    parse_statements(sql)
        .unwrap_or_else(|error| panic!("{sql} did not parse: {error}"))
        .pop()
        .expect("one statement")
        .lower()
        .unwrap_or_else(|error| panic!("{sql} did not lower: {error}"))
}

fn drop_database(sql: &str) -> plan::DropDatabase {
    match lowered(sql) {
        plan::Statement::DropDatabase(drop) => drop,
        other => panic!("{sql} lowered to {other:?}"),
    }
}

/// The statement parses and the flag arrives on the plan.
///
/// `sqlparser` 0.62.0 has no option list on its `Drop`, so before this the statement was a syntax
/// error about valid PostgreSQL — the one answer contract C1 exists to prevent.
#[test]
fn a_forced_drop_parses_and_carries_the_flag() {
    let drop = drop_database("DROP DATABASE d WITH (FORCE)");
    assert_eq!(drop.names, vec!["d".to_owned()]);
    assert!(drop.force, "WITH (FORCE) must reach the plan");
    assert!(!drop.if_exists);
}

/// **Both clauses at once**, because they are two booleans that have to travel together: `IF
/// EXISTS` covers absence and `FORCE` covers other sessions, and neither implies the other.
#[test]
fn if_exists_and_force_travel_together() {
    let drop = drop_database("DROP DATABASE IF EXISTS d WITH (FORCE)");
    assert!(drop.if_exists);
    assert!(drop.force);
}

/// The control. Without the clause the flag is false — a cutter that fired on every `DROP DATABASE`
/// would satisfy every assertion above and hand a forced drop to somebody who asked for an ordinary
/// one, which is the direction that costs.
#[test]
fn an_unforced_drop_does_not_carry_the_flag() {
    for sql in [
        "DROP DATABASE d",
        "DROP DATABASE IF EXISTS d",
        r#"DROP DATABASE "d""#,
    ] {
        assert!(!drop_database(sql).force, "{sql} must not be forced");
    }
}

/// A quoted name is a name: the cut is taken past it, not past the first bare word.
#[test]
fn a_quoted_name_is_still_a_name() {
    let drop = drop_database(r#"DROP DATABASE "My Db" WITH (FORCE)"#);
    assert!(drop.force);
    assert_eq!(drop.names, vec!["My Db".to_owned()]);
}

/// **The spellings PostgreSQL itself refuses stay refused**, measured on 19beta1 rather than
/// reasoned about (`esker-coord/s2-h110-force.out`). A cutter that swallowed a list it could not
/// read would turn one of these into a silently accepted statement.
#[test]
fn the_spellings_postgresql_refuses_are_still_refused() {
    for sql in [
        // No `WITH`: `42601 syntax error at or near "FORCE"` on 19beta1.
        "DROP DATABASE d FORCE",
        // `42601 syntax error at or near ")"`.
        "DROP DATABASE d WITH ()",
        // `42601 syntax error at or near "NOSUCHOPTION"`, with and without FORCE beside it.
        "DROP DATABASE d WITH (NOSUCHOPTION)",
        "DROP DATABASE d WITH (FORCE, NOSUCHOPTION)",
    ] {
        assert!(
            parse(sql).is_err(),
            "{sql} is a syntax error on 19beta1 and must be one here"
        );
    }
}

/// And the one spelling that looks wrong and is not: 19beta1 takes `WITH (force)`.
#[test]
fn the_clause_is_case_insensitive_because_postgresql_is() {
    assert!(drop_database("drop database d with (force)").force);
}

/// **The two entry points must answer alike**, which nothing tested until this.
///
/// `crate::parse` has two doors — `parse_statements` and `parse` — and every clause cut out of the
/// source has to come off on both. `parse/mod.rs` states the rule in a comment beside the strips
/// that were hung on both: *"a clause that only comes off on the other path makes the same
/// statement parse through one door and not the other."* A rule with no test is a rule nobody will
/// hear break, and hanging a new strip on one door is the easiest way to break it — so this walks
/// every statement the module rewrites, not only #110's.
#[test]
fn both_doors_answer_alike_for_every_rewritten_statement() {
    for sql in [
        // #110's, the one this test was added for.
        "DROP DATABASE d WITH (FORCE)",
        "DROP DATABASE IF EXISTS d WITH (FORCE)",
        // And its neighbours, each a clause some earlier unit had to cut out of the source.
        "DROP INDEX CONCURRENTLY i",
        "CREATE UNLOGGED TABLE t (a int8)",
        "CREATE DATABASE d ENCODING = 'utf8'",
        "CREATE DOMAIN dn AS int8 NOT NULL",
        "CREATE MATERIALIZED VIEW mv AS SELECT 1 WITH NO DATA",
        // A statement neither door rewrites, so a door that answered differently would be
        // reporting something other than the strips.
        "SELECT 1",
    ] {
        assert_eq!(
            parse(sql).is_ok(),
            parse_statements(sql).is_ok(),
            "{sql} parses through one door and not the other"
        );
    }
}

/// **The unit's point, and the control comes first.** An unforced drop is refused by the sessions
/// on the database; the same statement with `WITH (FORCE)` ends them and proceeds.
///
/// The refusal in the middle is not decoration: without it a fixture that failed to attach its
/// sessions would let the forced drop succeed for the wrong reason, and this test would pass
/// against a node that never learned to terminate anything.
#[test]
fn a_forced_drop_ends_the_sessions_an_ordinary_one_is_refused_by() {
    let (mut serving, _held, _id) = cluster_with("arunit2", 2);

    let error = serving.run("DROP DATABASE arunit2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::OBJECT_IN_USE);
    assert_eq!(
        error.detail().as_deref(),
        Some("There are 2 other sessions using the database."),
        "the refusal counts the sessions, and the count is what FORCE is about to act on"
    );

    serving.run("DROP DATABASE arunit2 WITH (FORCE)").unwrap();
    assert_eq!(
        serving.rows("SELECT count(*) FROM pg_database WHERE datname = 'arunit2'")[0][0],
        "0",
        "the database is gone, not merely unrefused"
    );
}

/// **A session somebody has already ended is not a user of the database** — the predicate's own
/// test, and it has nothing to do with `FORCE`.
///
/// `pg_terminate_backend` sets a flag the victim's loop reads later, so the registry row outlives
/// the decision to end it. Counting that row would refuse an ordinary `DROP DATABASE` on behalf of
/// a session that is already finished. The victim is **held across the whole test**, so the drop at
/// the end cannot succeed because the session went away — only because it was terminated.
#[test]
fn a_terminated_session_no_longer_refuses_an_ordinary_drop() {
    let (mut serving, held, _id) = cluster_with("h110_t", 1);

    // The victim is really attached — the evidence, not the assumption.
    assert_eq!(
        serving.rows("SELECT count(*) FROM pg_stat_activity WHERE datname = 'h110_t'")[0][0],
        "1"
    );

    // **The control.** While it is live the ordinary drop is refused, which is what says the
    // success at the end is the termination's doing.
    let error = serving.run("DROP DATABASE h110_t").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::OBJECT_IN_USE);

    let pid =
        serving.rows("SELECT pid FROM pg_stat_activity WHERE datname = 'h110_t'")[0][0].clone();
    assert_eq!(
        serving.rows(&format!("SELECT pg_terminate_backend({pid})"))[0][0],
        "t",
        "the pid has to be one the registry holds, or nothing was terminated"
    );

    serving.run("DROP DATABASE h110_t").unwrap();
    assert_eq!(
        serving.rows("SELECT count(*) FROM pg_database WHERE datname = 'h110_t'")[0][0],
        "0"
    );
    drop(held);
}

/// The refusals in front of the session count stand with the clause written — each one measured on
/// 19beta1 rather than reasoned about (`esker-coord/s2-h110-force.out`).
#[test]
fn force_does_not_reach_past_the_refusals_in_front_of_it() {
    let (mut serving, _held, _id) = cluster_with("h110_u", 0);

    let own = serving
        .run(&format!("DROP DATABASE {SERVING} WITH (FORCE)"))
        .unwrap_err();
    assert_eq!(own.sqlstate(), sqlstate::OBJECT_IN_USE);
    assert_eq!(own.to_string(), "cannot drop the currently open database");

    let template = serving
        .run("DROP DATABASE template1 WITH (FORCE)")
        .unwrap_err();
    assert_eq!(template.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);
    assert_eq!(template.to_string(), "cannot drop a template database");

    let missing = serving
        .run("DROP DATABASE nosuchdb_h110 WITH (FORCE)")
        .unwrap_err();
    assert_eq!(missing.sqlstate(), sqlstate::INVALID_CATALOG_NAME);

    // And `IF EXISTS` still covers absence with the clause written, which is a notice and not an
    // error on both servers.
    serving
        .run("DROP DATABASE IF EXISTS nosuchdb_h110 WITH (FORCE)")
        .unwrap();
}
