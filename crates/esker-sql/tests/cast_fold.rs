//! **The same conversions, over a constant** — as PostgreSQL 19beta1 answers them.
//!
//! The twin of `cast_matrix.rs`, which probes every `pg_cast` pair with a **column** so the answer
//! comes from `exec::cursor`'s `Expr::Cast` arm. This file writes the same pairs as *literals*, so
//! the answer comes from `parse::lower`'s fold instead, and the two files together are the reason
//! a conversion cannot be right in one caller and wrong in the other.
//!
//! It has happened, in both directions, which is why this file exists rather than being trusted:
//!
//!   * `'((0,0),(1,1))'::box::polygon` folded to the two-point polygon `((1,1),(0,0))` where a
//!     real server gives the four corners, and an **open** `'[(0,0),(1,1)]'::path::polygon`
//!     folded silently where a real server refuses it — the fold answering where the evaluator
//!     refused.
//!   * `'r'::"char"::int4` folded to `114` and the same cast over a *column* was
//!     `22P02 invalid input syntax for type integer: "r"` — the evaluator refusing where the fold
//!     answered.
//!   * `200::int4::"char"` folded to a byte where a real server says `22003 "char" out of range`,
//!     because the fold wrapped the value round with a `rem_euclid` and nothing compared it to
//!     anything.
//!
//! **A literal is not a weaker probe than a column, it is a different one.** The fold runs at
//! parse time and its result is what `pg_get_expr` prints for a DEFAULT, so a wrong fold is wrong
//! twice: in the answer and in the catalog.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement here is a bare `SELECT` over constants.
const FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_folded_cast_is_the_cast_postgresql_19_folds() {
    let replayed = parity::replay_reporting(
        include_str!("corpus/pg19_cast_fold.txt"),
        FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        replayed.checked > 20,
        "only {} statements ran; the corpus did not load",
        replayed.checked
    );
    // Every refusal in this file is guarded by a `SAVEPOINT`, so nothing may be swallowed — the
    // lesson `pg19_geometric.txt` cost forty-four statements to learn, where a refusal *both*
    // servers give aborted the block and half the file stopped being compared while the test
    // stayed green.
    assert_eq!(
        replayed.swallowed, 0,
        "an aborted transaction is swallowing statements this corpus is supposed to compare"
    );
}
