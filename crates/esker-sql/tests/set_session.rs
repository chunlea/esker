//! `SET SESSION` — 12 refusals behind run 57's aborted-transaction row.
//!
//! **`SESSION` is the default scope spelled out.** `SET SESSION x TO v`, `SET x TO v` and
//! `SET x = v` are one statement, and `ActiveRecord` writes the long form in four places — the
//! `variables:` config, the timezone it sets on every connection, and `SET SESSION AUTHORIZATION`.
//!
//! What the corpus measures rather than assumes is the *other* scope: `SET LOCAL` belongs to the
//! transaction, so a `ROLLBACK TO` undoes it back to the session's value while a `SET SESSION`
//! survives. Two words, two lifetimes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is the session's own.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SET LOCAL search_path TO 'ss_two'",
            "**`SET LOCAL` is refused by name and `SET SESSION` is not**, which is the pair this \
             file exists to keep apart: `LOCAL` is undone when the transaction ends *whichever way \
             it ends*, and that needs a per-block undo this node keeps only for \
             `esker.read_as_of`. Silently promoting it to a session-wide `SET` would honour a \
             setting past the block that asked for it, which is a wrong answer rather than a \
             missing feature. Nothing in run 57's row sends it — the twelve refusals are all \
             `SET SESSION AUTHORIZATION`.",
        ),
        (
            "RESET SESSION search_path",
            "**Both refuse it and both say `42601`**: `RESET` has no scope keyword on a real \
             server either, so the two statements are deliberately not symmetrical. Only the \
             sentence differs — PostgreSQL's parser says `syntax error at or near \"search_path\"` \
             and `sqlparser` 0.62.0 lists the tokens it expected.",
        ),
        (
            "SELECT 'r', current_user, session_user",
            "`current_user` and `session_user` are `0A000` by name: **this node has no roles at \
             all**, so there is no user for them to answer with and inventing one would be a name \
             nobody created. It is the same absence `SET SESSION AUTHORIZATION` reports below and \
             the same one `CREATE DATABASE … OWNER` reports, and the feature that closes all three \
             is `CREATE USER` — which is what `schema_authorization_test.rb` actually needs.",
        ),
        (
            "SET SESSION AUTHORIZATION esker",
            "**The whole of this row's twelve refusals, and the answer moved rather than closed.** \
             It was `0A000 SET SESSION` — a statement refused wholesale, which aborted the \
             transaction and took the rest of the file with it. It is `22023 role \"esker\" does \
             not exist` now, which is what a real server says about a role that is not there and \
             is true of *every* name here. On the oracle `esker` is the connected superuser, so \
             the same statement succeeds there. `DEFAULT` — which is what `set_session_auth` sends \
             between each named user — agrees on both sides.",
        ),
        (
            "SET LOCAL SESSION AUTHORIZATION esker",
            "A contract **C1** gap rather than a clause declined: `sqlparser` 0.62.0 reads a scope \
             keyword before a parameter assignment and not before `SESSION AUTHORIZATION`, so the \
             statement does not parse at all. The two words after it are the ones this node has an \
             answer for, and the answer is the one above.",
        ),
    ],
};

#[test]
fn every_set_session_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_set_session.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 25, "the corpus shrank: {checked} statements");
}
