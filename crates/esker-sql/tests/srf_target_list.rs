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
    // **One fact, thirteen times**: a bare integer constant is `int8` here and `int4` on a real
    // server, and `generate_series` takes its arguments' type — so the column it makes is `bigint`
    // where PostgreSQL says `integer`. Every row is identical; only the declared type differs.
    types: &[
        "SELECT 'r', generate_series(1, 3)",
        "SELECT 'r', generate_series(1, 0)",
        "SELECT 'r', generate_series(3, 1, -1)",
        "SELECT 'r', generate_series(1, 2), generate_series(1, 2)",
        "SELECT 'r', generate_series(1, 3), generate_series(1, 2)",
        "SELECT 'r', generate_series(1, 2) + 10",
        "SELECT 'r', abs(generate_series(-1, 1))",
        "SELECT 'r', id, generate_series(1, 2) FROM sr WHERE id = 1",
        "SELECT 'r', unnest(ARRAY[1,2,3])",
        "SELECT 'r', pg_typeof(unnest(ARRAY['a','b']::text[]))",
        "SELECT 'r', g FROM (SELECT generate_series(1, 3) AS g) s WHERE g > 1 ORDER BY g DESC",
        "SELECT 'r', generate_series(1, 3) ORDER BY 2 DESC",
        "SELECT 'r', generate_series(1, 3) LIMIT 2",
    ],
    answers: &[
        // `generate_series` takes its arguments' type and this node's integer constants are `int8`
        // where a real server's are `int4` — the standing constant-width divergence, showing
        // through the one function that reports a type as a value. The three rows and their values
        // are identical; `pg_typeof` itself also answers `text` here rather than `regtype`.
        (
            "SELECT 'r', pg_typeof(generate_series(1, 3))",
            "a bare integer constant is int8 here and int4 there, and pg_typeof answers text",
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
