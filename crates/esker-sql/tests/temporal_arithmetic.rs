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
    // `SELECT '2020-01-01'::date - NULL::int4` was here, declared as a type divergence: a dropped
    // cast made it `date - date` and the client was told `integer` where PostgreSQL says `date`.
    // `Literal::TypedNull` keeps the cast, so the operator resolves as `date - int4` and both the
    // row and the type agree.
    types: &[],
    answers: &[
        (
            "SELECT '2020-01-01'::date + 1.5",
            "**A bare decimal constant is `numeric` on a real server and `double precision` here** — the divergence `tests/unknown_literal.rs` declares. Both raise the same `42883` for the same reason: `date` has no arithmetic with either type. One word of the message differs and nothing else does.",
            "pg19_date.txt:97",
        ),
        // `SELECT '2020-01-01'::date + NULL::interval` was here — the one place in these three
        // corpora where this node **raised where PostgreSQL returns a row**, because a dropped
        // cast made `NULL::interval` an untyped NULL that took the other side's type and resolved
        // as `date + date`. `Literal::TypedNull` is the type surface that closed it: the operator
        // resolves as `date + interval` and the answer is a NULL `timestamp`.
        (
            "SELECT '1 day'::interval * 'Infinity'::float8",
            "**An interval has no infinity in this node.** PostgreSQL 17 gave the type one, so `'1 day' * 'Infinity'::float8` is `infinity` there; here an interval is three finite fields. Scaling by an infinite factor is **refused by name** rather than truncated to `00:00:00`, which is what it would otherwise answer — a wrong value where a refusal is available (ADR 0031).",
            "UNMEASURED",
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
