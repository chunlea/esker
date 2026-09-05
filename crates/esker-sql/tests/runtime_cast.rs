//! A cast whose operand is not a constant, replayed against what PostgreSQL 19beta1 answered.
//!
//! Run 95's `a cast to INTEGER is not supported` is 2 tests in `connection_test.rb`, and both send
//! **`SELECT $1::integer`** — a cast of a bind parameter. So the row is not about a spelling of
//! `integer`, and reading the statement is what said so. `text` had `Expr::ToText` and every other
//! target had nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 'c8', txt::date FROM ct;",
        // **The input function's classification, not the cast's** — and it predates this unit:
        // a folded `'42'::date` goes through the same `value::date::from_text` and answers the
        // same. PostgreSQL splits the family in two, `22007 invalid_datetime_format` for text
        // that is not a date at all and `22008 datetime_field_overflow` for digits that parse as
        // a field and do not fit; this node answers `22007` for both. The *value* agrees — both
        // refuse — so what differs is which of two neighbouring sqlstates a client is told.
        //
        // Left as its own thing rather than folded in here: getting it right means the datetime
        // input functions telling the two apart, which is a unit about parsing and not about
        // casting.
        "the datetime input functions do not split 22007 from 22008",
        "pg19_runtime_cast.txt:44",
    )],
};

#[test]
fn every_runtime_cast_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_runtime_cast.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}
