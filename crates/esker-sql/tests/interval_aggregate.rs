//! `sum` and `avg` over an interval, and the division an average is, against PostgreSQL 19beta1.
//!
//! `interval_test.rb`'s fourth test stopped at `function avg(interval) does not exist` and took
//! the rest of the file with it — everything after an aborted statement is
//! `current transaction is aborted`. `min` and `max` over an interval already answered here; `sum`
//! and `avg` fell through to a refusal.
//!
//! **The division is the interesting half.** An average is its sum divided by its count, this node
//! already had `interval / n`, and that operator was wrong wherever the division was not exact:
//! `'1 mon' / 3` answered `9 days 23:59:59.999999` where PostgreSQL answers `10 days`, because
//! `(1/3) * 30` is `9.999999999999998` in a double and each field was truncated. The corpus rows
//! that pin the real rule, and the measurement they came from, are in
//! `tests/captures/pg19_interval_aggregate.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One fact, five times**: `pg_typeof` answers a `regtype` on a real server and `text` here,
    // the trade `'x'::regtype` makes everywhere in this crate. The rows are identical.
    types: &[
        "SELECT pg_typeof(sum(v)), pg_typeof(avg(v)) FROM ia",
        "SELECT pg_typeof(min(v)), pg_typeof(max(v)) FROM ia",
        "SELECT pg_typeof(sum(t)), pg_typeof(avg(t)) FROM ia",
        "SELECT pg_typeof(min(t)), pg_typeof(max(t)) FROM ia",
    ],
    answers: &[],
};

#[test]
fn every_interval_aggregate_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_interval_aggregate.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}
