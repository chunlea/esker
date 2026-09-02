//! Contract C2 over everything `ActiveRecord` 8.1.3.1 actually says, and a count of how much of it
//! this node runs.
//!
//! `tests/corpus/activerecord_8_1_statements.txt` is the 36 distinct statements a real
//! `ActiveRecord` sent a real PostgreSQL 19beta1 — captured from the **server's** log, in issue
//! order, by the out-of-repo harness (`docs/plans/phase-9-rails.md` §0).
//!
//! # What this asserts, and what it deliberately does not
//!
//! It does **not** assert the answers. These queries read PostgreSQL's own catalog, whose contents
//! this node does not and should not reproduce; a corpus that demanded the same rows would be
//! demanding the wrong thing. What it asserts is the contract that does apply to every statement
//! whatever the answer:
//!
//! * **C1** — every one of them parses. A statement a real server accepts must never come back as
//!   a syntax error, and the whole file is a set of statements a real server accepted.
//! * **C2** — anything not executed is `0A000` **naming the construct**, or another condition
//!   PostgreSQL itself would raise. Never a syntax error, never a wrong answer.
//!
//! And it counts what runs. [`RUNS`] is that number, asserted exactly: a unit that makes more of
//! `ActiveRecord`'s surface work moves it up and has to say so here, and one that quietly breaks
//! something moves it down. It is expected to be low, and publishing a low number honestly is the
//! same discipline ADR 0031 puts on the scoreboard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// A schema close enough to what `ActiveRecord`'s own migration would have made, in the six
/// types
/// this node has — so that the statements which *do* run have something to run against.
///
/// It is not what `ActiveRecord` asked for, and that gap is the point: `t.string` compiles to
/// `character varying` and `t.integer` to `integer`, neither of which exists here. See the
/// module's note and `docs/plans/phase-9-rails.md` §5.
const FIXTURE: &[&str] =
    &["CREATE TABLE harness_widgets (id int8 PRIMARY KEY, name text NOT NULL, n int8, live bool)"];

/// How many of the 36 statements this node runs today.
///
/// **Asserted exactly, not as a floor.** A floor would let a regression hide behind an unrelated
/// improvement, and the number is here to be argued with: it is the honest measure of how far a
/// real `ActiveRecord` gets, and it should be uncomfortable until it is not.
///
/// Eleven, and what moved it says as much as the number. Unit 5 landed **two** features and only
/// one of them shows here:
///
/// * the six `SET`s and two `SHOW`s (`tests/session_parameters.rs`) are +8, all of it. They were
///   the cheapest ratio on the board and the handover said so.
/// * a **table alias** (`tests/alias.rs`) is +0, and that is not a disappointment — it is the
///   thing this counter is for. Nineteen statements open `FROM pg_type AS t`, and every one of
///   them now fails on its *second* blocker instead of its first: a catalog relation to alias.
///   A gate is not a feature until what is behind it exists.
///
/// **`IN (list)` is also +0**, and that is worth a line rather than silence: it was the first
/// scoreboard's top recommendation and it moved four statements (4, 7, 8, 9) off the query surface
/// and onto `relation "pg_type" does not exist` — the catalog, which is the destination. This
/// counter measures what is *answered*, and `docs/bench/rails-scoreboard.md`'s per-statement table
/// measures what is in the way; the second is what says where a unit went.
///
/// It was guessed at ten before the first run, which is the reason for measuring: the guess was
/// wrong by more than a factor of three in the flattering direction.
///
/// **Fifteen after `pg_type` + `pg_range`** — statements 4, 7, 8 and 9, the first thing
/// `ActiveRecord` asks a server and what rung 2 of the ladder stopped on. Unlike the alias and
/// `IN`, that one was +4 rather than +0: the two units before it moved those four statements *onto*
/// the catalog, and it was the catalog.
///
/// **Eighteen now, and the three are the type surface** — statement 13, the `CREATE TABLE` a
/// migration emits, and 14 and 20 which failed only because 13 had. `character varying` and
/// `timestamp` are what it names ([ADR
/// 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md) tier 1, with the `integer` that
/// landed before them). That is what the three numbers together are for — this counter says what
/// is answered, the scoreboard's per-statement table says what is in the way, and the ladder says
/// whether a client can get through.
///
/// **Nineteen now, and the nineteenth is statement 12** — `SELECT 'integer'::regtype::oid`, the
/// cast `Quoting#lookup_cast_type` sends once per column type. It is worth one line of its own,
/// because it is the number that did **not** move that made it matter: scoreboard run 3 measured
/// tier 1 complete, this counter at eighteen, and the ladder still at rung 1. One statement, and
/// rung 2 passes. A counter and a ladder disagreeing is the most useful thing the three numbers
/// have done so far.
///
/// **Twenty-two now.** The rung-4 unit answered three at once — the relation-listing statements
/// that need `pg_class`, `pg_namespace` and `= ANY (current_schemas(false))` together. They are
/// one shape asked three ways, which is why no single earlier unit could move any of them: a
/// statement is served or it is not, and this one wanted three features before it was either.
const RUNS: usize = 22;

#[test]
fn every_statement_activerecord_sends_parses_and_is_answered_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    let mut ran = 0;
    let mut syntax_errors = Vec::new();
    let mut refused = Vec::new();

    for (line, statement) in corpus() {
        match node.run(&statement) {
            Ok(_) => ran += 1,
            Err(error) => {
                // Contract C1: a statement a real server accepted is never a syntax error here.
                if error.sqlstate() == sqlstate::SYNTAX_ERROR {
                    syntax_errors.push(format!("line {line}: {statement}\n  {error}"));
                    continue;
                }
                // Contract C2: and a refusal names what it could not do.
                if error.sqlstate() == sqlstate::FEATURE_NOT_SUPPORTED
                    && error.to_string() == "is not supported"
                {
                    refused.push(format!("line {line}: {statement}\n  named nothing"));
                }
            }
        }
    }

    assert!(
        syntax_errors.is_empty(),
        "{} statements a real server accepted came back as syntax errors, which contract C1 \
         forbids:\n\n{}",
        syntax_errors.len(),
        syntax_errors.join("\n\n")
    );
    assert!(
        refused.is_empty(),
        "{} refusals named no construct, which contract C2 forbids:\n\n{}",
        refused.len(),
        refused.join("\n\n")
    );
    assert_eq!(
        ran, RUNS,
        "this node runs {ran} of ActiveRecord's statements and the file says {RUNS}. Moving that \
         number is the point of a unit; update RUNS and say which unit moved it."
    );
}

/// The statements, with their line numbers so a failure names a place in the file.
fn corpus() -> Vec<(usize, String)> {
    include_str!("corpus/activerecord_8_1_statements.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| (index + 1, line.to_owned()))
        .collect()
}
