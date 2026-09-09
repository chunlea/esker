//! A set-returning function in the **target list**, against PostgreSQL 19beta1.
//!
//! `generate_series` and `generate_subscripts` already stand where a table does; this is the other
//! half, `SELECT generate_series(1,3)`, and it is what `tests/catalog_vectors.rs` named as a gap.
//! `unnest` arrives with it, because a target list is where the suite writes it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One entry, and there were thirteen.** Every one of the other twelve said that a bare
    // integer constant was an `int8` here where a real server's is an `int4`, so a
    // `generate_series` or an `unnest` over one declared `bigint`; the literal ladder's `int4` rung
    // (ADR 0087) closed all of them. What is left is `pg_typeof`, which answers a `regtype` there
    // and `text` here (ADR 0077), with the row identical.
    types: &["SELECT 'r', pg_typeof(unnest(ARRAY['a','b']::text[]))"],
    answers: &[
        // `generate_series` takes its arguments' type and this node's integer constants are `int8`
        // where a real server's are `int4` — the standing constant-width divergence, showing
        // through the one function that reports a type as a value. The three rows and their values
        // are identical; `pg_typeof` itself also answers `text` here rather than `regtype`.
        (
            "SELECT 'r', pg_typeof(generate_series(1, 3))",
            "a bare integer constant is int8 here and int4 there, and pg_typeof answers text",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_set_returning_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_srf_target_list.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
