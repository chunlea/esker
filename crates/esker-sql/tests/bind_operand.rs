//! A bind parameter as an operand of `=`, against PostgreSQL 19beta1.
//!
//! Run 57's `operator does not exist: <type> = text` — 34 raises over six association files, in
//! three spellings. Every one is a parameter this node resolved to `text` where a real server takes
//! the type from the other side of the operator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    // **The standing constant-width trade, and it is the projected `1` rather than the
    // parameter.** A bare integer constant is `int4` on a real server and `int8` here
    // (`tests/unknown_literal.rs`), so `SELECT 1` is described as `integer` there and `bigint`
    // here — in these five it is the *first* column that differs and the parameter beside it that
    // is the point. Every row agrees, and the parameter resolves to a number on both.
    types: &[
        "SELECT 'r', 1 WHERE (1 = $1)",
        "SELECT 'r', 1 WHERE ($1 = 1)",
        "SELECT 'r', 1 WHERE ('a' = $1)",
        "SELECT 'r', 1 WHERE (true = $1)",
    ],
    answers: &[],
};

#[test]
fn every_bind_operand_answer_is_postgresql_19_s() {
    let checked = bind::replay(include_str!("corpus/pg19_bind_operand.txt"), &DIVERGENCES);
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
