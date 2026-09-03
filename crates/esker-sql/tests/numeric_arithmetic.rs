//! `numeric` arithmetic against PostgreSQL 19beta1 — the exact half of the operators.
//!
//! Split from `tests/arithmetic.rs` because the rules are different in kind: a float's operators
//! have one answer per pair of values, and `numeric`'s have an answer *and a scale*, decided by a
//! different rule for each operator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 2::numeric ^ 3::numeric",
            "**`^` over two exact values has a scale rule of its own** and it is not division's: `2 ^ 3` is `8.0000000000000000` — sixteen places — while `10 ^ 100` is a bare integer, so the rule is about the *result's* weight rather than the operands'. That is `numeric_power` in `numeric.c`, a different function from the `select_div_scale` this node already implements, and it needs a capture round of its own. Refused by name and counted; `^` over the floats answers (`tests/arithmetic.rs`), which is the form every statement in either schema file uses.",
        ),
        (
            "SELECT 2::numeric ^ 0.5::numeric",
            "**`^` over two exact values has a scale rule of its own** and it is not division's: `2 ^ 3` is `8.0000000000000000` — sixteen places — while `10 ^ 100` is a bare integer, so the rule is about the *result's* weight rather than the operands'. That is `numeric_power` in `numeric.c`, a different function from the `select_div_scale` this node already implements, and it needs a capture round of its own. Refused by name and counted; `^` over the floats answers (`tests/arithmetic.rs`), which is the form every statement in either schema file uses.",
        ),
        (
            "SELECT (10::numeric ^ 100)::text",
            "**`^` over two exact values has a scale rule of its own** and it is not division's: `2 ^ 3` is `8.0000000000000000` — sixteen places — while `10 ^ 100` is a bare integer, so the rule is about the *result's* weight rather than the operands'. That is `numeric_power` in `numeric.c`, a different function from the `select_div_scale` this node already implements, and it needs a capture round of its own. Refused by name and counted; `^` over the floats answers (`tests/arithmetic.rs`), which is the form every statement in either schema file uses.",
        ),
        (
            "SELECT (-2)::numeric ^ 0.5::numeric",
            "**`^` over two exact values has a scale rule of its own** and it is not division's: `2 ^ 3` is `8.0000000000000000` — sixteen places — while `10 ^ 100` is a bare integer, so the rule is about the *result's* weight rather than the operands'. That is `numeric_power` in `numeric.c`, a different function from the `select_div_scale` this node already implements, and it needs a capture round of its own. Refused by name and counted; `^` over the floats answers (`tests/arithmetic.rs`), which is the form every statement in either schema file uses.",
        ),
        (
            "SELECT 0::numeric ^ 0::numeric",
            "**`^` over two exact values has a scale rule of its own** and it is not division's: `2 ^ 3` is `8.0000000000000000` — sixteen places — while `10 ^ 100` is a bare integer, so the rule is about the *result's* weight rather than the operands'. That is `numeric_power` in `numeric.c`, a different function from the `select_div_scale` this node already implements, and it needs a capture round of its own. Refused by name and counted; `^` over the floats answers (`tests/arithmetic.rs`), which is the form every statement in either schema file uses.",
        ),
        (
            "SELECT 1.5 + 1.5",
            "**A bare decimal constant is `numeric` on a real server and `double precision` here** — the divergence `tests/unknown_literal.rs` declares, and this is the line that shows it costs a value and not only a type: `0.1 + 0.2` is exactly `0.3` in `numeric` and is not in binary floating point. Written with an explicit `::numeric` the same sum agrees (line 8 of this corpus). Closing it means changing what a decimal constant is everywhere, which is a unit of its own and not one arithmetic can do from inside.",
        ),
        (
            "SELECT 1.5 * 2",
            "**A bare decimal constant is `numeric` on a real server and `double precision` here** — the divergence `tests/unknown_literal.rs` declares, and this is the line that shows it costs a value and not only a type: `0.1 + 0.2` is exactly `0.3` in `numeric` and is not in binary floating point. Written with an explicit `::numeric` the same sum agrees (line 8 of this corpus). Closing it means changing what a decimal constant is everywhere, which is a unit of its own and not one arithmetic can do from inside.",
        ),
        (
            "SELECT 0.1 + 0.2",
            "**A bare decimal constant is `numeric` on a real server and `double precision` here** — the divergence `tests/unknown_literal.rs` declares, and this is the line that shows it costs a value and not only a type: `0.1 + 0.2` is exactly `0.3` in `numeric` and is not in binary floating point. Written with an explicit `::numeric` the same sum agrees (line 8 of this corpus). Closing it means changing what a decimal constant is everywhere, which is a unit of its own and not one arithmetic can do from inside.",
        ),
    ],
};

#[test]
fn every_numeric_arithmetic_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_numeric_arithmetic.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 50,
        "only {checked} statements ran; the corpus did not load"
    );
}
