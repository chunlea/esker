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
    // **Empty, and it was five entries long.** Each said the same thing and none was about the
    // parameter: a bare integer constant was an `int8` here against a real server's `int4`, so the
    // projected `1` beside every bind was described as `bigint`. The literal ladder gained its
    // `int4` rung and the first column agrees too, which leaves nothing here — the parameters
    // always resolved the same way on both.
    types: &[],
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
