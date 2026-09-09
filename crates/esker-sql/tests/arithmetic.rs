//! Binary arithmetic over the integers and the floats, against PostgreSQL 19beta1.
//!
//! Statement 741 of the specific schema is `DEFAULT random() * 100`, and it is the first statement
//! in either schema file that asks this node to do arithmetic at all — so the unit is every binary
//! operator on every numeric type, not one multiplication.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Every one of these has PostgreSQL's value and this node's constant width.** A bare
    // integer constant is `int4` on a real server and `int8` here (`tests/unknown_literal.rs`),
    // so an operator over two of them answers `bigint` where PostgreSQL answers `integer`. The
    // arithmetic is not what differs — `2 + 3 * 4` is 14 in both, precedence included — and the
    // same statements written with an explicit cast agree on the type as well.
    types: &[],
    answers: &[(
        "SELECT true + 1",
        "**This node's integer constants are `int8` where a real server's are `int4`** — the divergence `tests/unknown_literal.rs` declares — and it is visible here in the type the message names: PostgreSQL resolves the `unknown` beside a constant to `integer` and this node to `bigint`. Same SQLSTATE, same failure, one word apart. The values agree everywhere; only a constant's *width* differs, and closing it means changing what a bare integer is everywhere rather than anything arithmetic does.",
        "UNMEASURED",
    )],
};

#[test]
fn every_arithmetic_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_arithmetic.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 55,
        "only {checked} statements ran; the corpus did not load"
    );
}
