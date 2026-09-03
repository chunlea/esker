//! Arithmetic over the date and time types, against PostgreSQL 19beta1.
//!
//! The third of the arithmetic families and the one that is a **table** rather than a rule: what
//! `left <op> right` yields depends on the pair and not on a widening order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The **same dropped cast** as the `date + NULL::interval` entry below, showing as a type
    // rather than as an error: `NULL::int4` is an untyped `Literal::Null`, so the rule that gives
    // an untyped NULL the other side's type makes this `date - date`, whose answer is a count of
    // days. The row is right — NULL either way — and the type a client is told is `integer` where
    // PostgreSQL says `date`.
    types: &["SELECT '2020-01-01'::date - NULL::int4"],
    answers: &[
        (
            "SELECT '2020-01-01'::date + 1.5",
            "**A bare decimal constant is `numeric` on a real server and `double precision` here** — the divergence `tests/unknown_literal.rs` declares. Both raise the same `42883` for the same reason: `date` has no arithmetic with either type. One word of the message differs and nothing else does.",
        ),
        (
            "SELECT '2020-01-01'::date + NULL::interval",
            "**A cast on a NULL is dropped at lowering** — `NULL::interval` is a `Literal::Null` with no type, because until arithmetic arrived a NULL's type never mattered: every comparison with one is NULL whatever it was written as. So the untyped-NULL rule takes the other side's type and this resolves as `date + date`, which has no operator, where PostgreSQL resolves `date + interval` and answers a NULL `timestamp`. It is the one place in these three corpora where this node **raises where PostgreSQL returns a row**, and closing it means giving `Literal::Null` a type rather than anything arithmetic can do from inside.",
        ),
        (
            "SELECT '1 day'::interval * 'Infinity'::float8",
            "**An interval has no infinity in this node.** PostgreSQL 17 gave the type one, so `'1 day' * 'Infinity'::float8` is `infinity` there; here an interval is three finite fields. Scaling by an infinite factor is **refused by name** rather than truncated to `00:00:00`, which is what it would otherwise answer — a wrong value where a refusal is available (ADR 0031).",
        ),
    ],
};

#[test]
fn every_temporal_arithmetic_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_temporal_arithmetic.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 55,
        "only {checked} statements ran; the corpus did not load"
    );
}
