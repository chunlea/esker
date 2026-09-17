//! `DROP DATABASE … WITH (FORCE)`: the parse half of debt #110, and **only** the parse half.
//!
//! The clause reaches `plan::DropDatabase` now. **It is not acted on yet**, deliberately: a forced
//! drop still meets exactly the refusals an unforced one meets, and the executor is untouched. That
//! is a half-built state and these tests name it rather than papering over it, so that a reader who
//! finds `force: true` travelling into an executor that ignores it knows it is scheduled rather
//! than broken.
//!
//! What 19beta1 answers is captured in `esker-coord/s2-h110-force.out`, and two of its rows decide
//! what the executor half will have to do when it lands: `FORCE` does **not** reach past the checks
//! in front of the session count. A template is `42809` with the clause written, and the database
//! the session is connected to is `55006` with it written. So `FORCE` replaces one refusal, not the
//! rest of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::parse::{parse, parse_statements};
use esker_sql::plan;

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
