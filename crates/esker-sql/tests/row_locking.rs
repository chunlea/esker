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
//! `NOWAIT` and `SKIP LOCKED` are **refused by name**, and the line between them and the bare
//! clause is the one this crate draws everywhere: each of those two promises something a client
//! can check. `NOWAIT` must raise `55P03` when another session holds the row and `SKIP LOCKED`
//! must leave that row out of the answer; a node with no row locks to see would answer rows for
//! both, which is a wrong answer rather than a missing feature.

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
            "SELECT 'r', id FROM lk WHERE id = 1 FOR UPDATE NOWAIT",
            "**`NOWAIT` promises a `55P03` this node cannot raise.** Its whole content is what \
             happens when another session holds the row — measured on the oracle: \
             `55P03 could not obtain lock on row in relation \"lk\"` — and a Percolator \
             transaction has no row lock for a reader to find, because it does not block a \
             conflicting writer at all; it loses to one at commit with `40001` (ADR 0031). \
             Answering the row here would be right in this capture, where nothing is locked, and \
             wrong in the only case anybody writes `NOWAIT` for. Refused by name.",
        ),
        (
            "SELECT 'r', id FROM lk ORDER BY id FOR UPDATE SKIP LOCKED",
            "**`SKIP LOCKED` promises a row will be missing.** Measured against a held lock, the \
             oracle answers `2, 3` where the unlocked answer is `1, 2, 3`. The same reasoning as \
             `NOWAIT` above and the opposite failure: this node would return the row a caller \
             asked it to skip, so a queue built on `SKIP LOCKED` would hand one job to every \
             worker. Refused by name.",
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
