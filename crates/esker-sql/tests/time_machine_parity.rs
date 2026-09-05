//! Contract C3 for the time machine: the half of it PostgreSQL has, replayed against what
//! PostgreSQL did.
//!
//! `tests/corpus/pg19_time_machine.txt` is what a real PostgreSQL 19beta1 answered for every
//! statement in this feature's surface — the custom GUC, `SET TRANSACTION SNAPSHOT`'s five
//! preconditions in their precedence order, `25006` for a write in a read-only transaction, and
//! the `42601`s that the three invented spellings get from PostgreSQL itself. Every line was
//! captured off the server rather than recalled, which is how the ADR's single `42704` for a bad
//! snapshot id turned out to be two conditions.
//!
//! # Where PostgreSQL parity ends, held from both sides
//!
//! The other half of this feature has **no oracle**. PostgreSQL 19 has no time travel, so what a
//! read at a past snapshot *returns* cannot be checked against it, and neither can a checkpoint or
//! a diff — the server answers `42883 function does not exist` for both, which is a fact about
//! PostgreSQL and not a judgement about us.
//!
//! So every statement this node answers differently is in [`DIVERGENCES`] with its reason, and the
//! list is checked in **both** directions: an unlisted divergence fails the build, and so does a
//! listed one that has started agreeing. A gap cannot be absorbed silently and neither can closing
//! one. The semantic reference for the half with no oracle is `CockroachDB`'s `AS OF SYSTEM TIME` —
//! the rounding rule, the read-only rule, the retention bound — cited in
//! `docs/adr/0021-time-machine.md` and deliberately not copied as syntax.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

/// Statements a real PostgreSQL answers differently, each with the reason.
///
/// Every one is a divergence in the **safe** direction — this node executes something PostgreSQL
/// merely parses, or refuses something PostgreSQL never had to consider. None of them is this node
/// answering a question PostgreSQL answers, differently.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "BEGIN ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION SNAPSHOT 'nope'",
        "`nope` is a legal checkpoint name here, so it is `42704` (a snapshot that is not there) \
         rather than `22023` (a string that could not be one). Both codes are PostgreSQL's and \
         this node uses both; which one a given string gets depends on what a snapshot id may be, \
         and ours may be a name.",
    ),
    (
        "BEGIN; SET TRANSACTION SNAPSHOT 'nope'",
        "A plain `BEGIN` is `READ COMMITTED` on PostgreSQL, which cannot import a snapshot — the \
         `0A000`. Percolator gives snapshot isolation, which is PostgreSQL's `REPEATABLE READ`, so \
         **every** block here satisfies that precondition by construction and the check is one \
         this node can never fail (ADR 0021 said so before it was built). What it answers instead \
         is the next question in PostgreSQL's own order: whether the snapshot is there.",
    ),
    // **`SET nonamespace_thing = '1'` was here and is not any more.** It said this node cannot
    // tell a parameter a real server *has* from a name nobody has, so it refused both by name with
    // `0A000`. The `SET`-parameters unit ruled the other way: `SHOW` and `RESET` had been answering
    // `42704` for the same name since phase 8, so `SET` was the one entry point out of step, and
    // the capture settles the commoner shape — `SET nosuchparameter` is `42704` on a real server
    // (`corpus/pg19_set_parameters.txt`). The row is deleted rather than reworded, which is
    // ADR 0031's rule 2, and `SET work_mem` is the line that pays for it (`tests/time_machine.rs`).
    (
        "CHECKPOINT nightly",
        "`CHECKPOINT` is refused by name here (the plan's gap register, G25), which is right for \
         the bare word PostgreSQL *accepts* and carries over to the named form PostgreSQL rejects. \
         The answer is `0A000` with a `HINT` naming `esker_checkpoint('<name>')`, which is more \
         useful than a syntax error and never claims the word is ours: ADR 0021 is explicit that \
         PostgreSQL owns `CHECKPOINT` for forcing a WAL checkpoint.",
    ),
    (
        "ALTER TABLE ro_probe SET (retention = '7d')",
        "`retention` is a storage parameter this node has and PostgreSQL does not (ADR 0021 \
         Decision 2). PostgreSQL parses the statement and refuses the parameter; here it sets the \
         travel window.",
    ),
    (
        "SELECT esker_checkpoint('nightly')",
        "A function this node has and PostgreSQL does not, which is why it is spelled as a \
         function: `42883` there is the honest answer for a name a server does not know.",
    ),
    (
        "SELECT * FROM esker_diff('ro_probe', 'a')",
        "The same. Here it is a diff; there it is a function that does not exist.",
    ),
    (
        "SELECT esker_flashback('ro_probe', 'a')",
        "The same again, and the reason `FLASHBACK TABLE` is not the spelling: Oracle's word is \
         `42601` on PostgreSQL, so taking it would make this node accept syntax the oracle \
         rejects. It gets a `HINT` naming this verb instead (ADR 0021 Decision 3).",
    ),
];

#[test]
fn every_captured_statement_answers_the_way_postgresql_19_does() {
    let mut checked = 0;
    let mut diverged = Vec::new();

    for (line_number, script, expected) in corpus() {
        let ours = answer(&script);
        checked += 1;

        // The **whole script** is the key, not its last statement: the same statement diverges
        // inside a transaction block and agrees outside one, and a suffix match would have
        // excused both.
        let listed = DIVERGENCES.iter().find(|(sql, _)| script == *sql);
        if ours == expected || both_are_syntax_errors(&ours, &expected) {
            assert!(
                listed.is_none(),
                "line {line_number}: `{script}` is listed as a divergence and now agrees with \
                 PostgreSQL ({ours}). Delete its row in DIVERGENCES."
            );
            continue;
        }

        match listed {
            Some(_) => diverged.push(script),
            None => panic!(
                "line {line_number}: `{script}`\n  PostgreSQL 19: {expected}\n  here:          \
                 {ours}\nEither this is a bug or it is a divergence; if it is a divergence, it \
                 belongs in DIVERGENCES with its reason."
            ),
        }
    }

    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
    assert_eq!(
        diverged.len(),
        DIVERGENCES.len(),
        "every listed divergence must be exercised by the corpus; these diverged: {diverged:?}"
    );
}

/// Whether both answers are `42601`, in which case only the **code** is compared.
///
/// The message on a syntax error is `sqlparser`'s and has never been claimed to be PostgreSQL's —
/// `docs/plans/phase-6a.md` §1 excludes it explicitly, along with the `LINE n: ... ^` caret. What
/// contract C1 promises is that a statement PostgreSQL 19 *accepts* is never answered with this
/// code at all, and the corpus in `tests/corpus/pg19.sql` is what holds that. Here the code
/// agreeing is the whole assertion: both servers found the same statement malformed.
fn both_are_syntax_errors(ours: &str, expected: &str) -> bool {
    ours.starts_with("!42601 ") && expected.starts_with("!42601 ")
}

/// What this node answers for one `;`-separated script: `ok`, or `!SQLSTATE message`.
///
/// Only the **last** statement's answer is reported; everything before it is setup, and a failure
/// there is a broken case rather than a result. Transaction control goes to the executor's own
/// calls, as `crate::pgwire::session` sends it.
fn answer(script: &str) -> String {
    let backend = Arc::new(MemoryBackend::new());
    let mut executor = Executor::new(
        Arc::clone(&backend) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    );

    // The table PostgreSQL's capture used, so a statement naming it is about a table and not about
    // a missing one.
    run_one(&mut executor, "CREATE TABLE ro_probe (a int8 PRIMARY KEY)")
        .expect("the corpus fixture must exist");

    let statements: Vec<&str> = script.split(';').map(str::trim).collect();
    let (last, setup) = statements.split_last().expect("a script has a statement");
    for statement in setup {
        // A setup step that fails leaves the case meaningless, so it is loud rather than silent.
        if let Err(error) = run_one(&mut executor, statement) {
            return format!("!{} {error} (in setup `{statement}`)", error.sqlstate());
        }
    }
    match run_one(&mut executor, last) {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("!{} {error}", error.sqlstate()),
    }
}

fn run_one(executor: &mut Executor, sql: &str) -> esker_sql::Result<()> {
    for parsed in parse_statements(sql)? {
        match parsed.class() {
            StatementClass::Begin => {
                executor.begin(parsed.begins_read_only())?;
                if let Some(level) = parsed.begins_isolation() {
                    executor.set_isolation(level)?;
                }
            }
            StatementClass::Commit => executor.commit()?,
            StatementClass::Rollback => executor.rollback()?,
            _ => {
                let _: Outcome = executor.execute(&parsed, &Params::NONE)?;
            }
        }
    }
    Ok(())
}

/// `(line number, script, expected answer)` for every case in the corpus.
fn corpus() -> Vec<(usize, String, String)> {
    include_str!("corpus/pg19_time_machine.txt")
        .lines()
        .enumerate()
        .filter_map(|(at, line)| {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (script, expected) = line.split_once('\t')?;
            Some((at + 1, script.to_owned(), expected.to_owned()))
        })
        .collect()
}
