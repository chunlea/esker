//! Implicit `LATERAL` — a set-returning function in a comma `FROM` list that names the entry to its
//! left, against PostgreSQL 19beta1.
//!
//! The second of the two features `tests/catalog_vectors.rs` measured and declared. Its
//! `FROM pg_constraint c, generate_subscripts(c.conkey, 1) AS idx` is the shape `ActiveRecord`'s
//! schema dump writes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **The subquery half of the same comma.** A function is implicitly `LATERAL` and a
        // subquery is not, so both servers refuse this one — PostgreSQL with the more useful
        // sentence: it says there *is* an entry for `l` and that this part of the query cannot see
        // it, and hints at the word that would fix it. This node cannot tell the two apart yet and
        // says the entry is missing. A refusal either way, and the better message is a unit with
        // the explicit `LATERAL` below.
        (
            "SELECT 'r', s.x FROM lt l, (SELECT l.id AS x) s",
            "PostgreSQL distinguishes an entry that exists but is not visible here from one that \
             does not exist; this node reports the second for both",
        ),
        // **Explicit `LATERAL` over a subquery** is the other feature this file measures and does
        // not implement. The function form needs no keyword and is what `ActiveRecord` writes; a
        // lateral *subquery* is a correlated plan per outer row, which is the subquery machinery
        // pointed at a `FROM` entry rather than at an expression.
        (
            "SELECT 'r', s.x FROM lt l, LATERAL (SELECT l.id AS x) s ORDER BY s.x",
            "LATERAL over a subquery is not implemented; a set-returning function needs no keyword \
             and is",
        ),
    ],
};

#[test]
fn every_lateral_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_lateral.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}
