//! Contract C3 for aggregates: replay `tests/corpus/pg19_aggregate.txt` and require our answer.
//!
//! The corpus is 145 probes put to a real PostgreSQL 19beta1 over a fixture this test rebuilds,
//! recording the rows, the types `\gdesc` declared for them, or the SQLSTATE and message it
//! refused with. Nothing in it was written from documentation, which is the point: an aggregate is
//! a pile of small rules — what a NULL does, what no rows at all do, which types have a `min` —
//! and every one of them is a place to be confidently wrong from memory.
//!
//! # Divergences are held from both sides
//!
//! Two lists, because there are two kinds. [`TYPE_DIVERGENCES`] is where the **rows agree** and the
//! declared type does not — every one of them is `sum(bigint)`, which PostgreSQL types `numeric`
//! and this node types `bigint`, printing the same characters for every input that does not
//! overflow (ADR 0031). [`DIVERGENCES`] is where the answer itself differs.
//!
//! Both are checked in **both directions**: an unlisted divergence fails, and so does a listed one
//! that has started agreeing. Closing a gap cannot be absorbed silently, and neither can opening
//! one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The fixture the corpus was captured over, and the reason the whole file is one comparison
/// rather than a hundred assertions: both servers see the same five rows.
const FIXTURE: &[&str] = &[
    "CREATE TABLE agg (id int8 PRIMARY KEY, g text, n int8, f float8, b bool, ts timestamptz, \
     y bytea)",
    "INSERT INTO agg VALUES \
     (1, 'a',  10,   1.5,   true,  '2024-01-01 00:00:00+00', '\\x01'), \
     (2, 'a',  20,   2.5,   false, '2024-06-01 12:00:00+00', '\\x02'), \
     (3, 'b',  NULL, NULL,  NULL,  NULL,                     NULL), \
     (4, 'b',  -5,   0.5,   true,  '2023-01-01 00:00:00+00', '\\xff'), \
     (5, NULL, 7,    'NaN', false, '2025-01-01 00:00:00+00', '\\x00')",
    "CREATE TABLE wide (id int8 PRIMARY KEY, n int8, g text)",
    "CREATE TABLE big (id int8 PRIMARY KEY, n int8)",
    "INSERT INTO big VALUES (1, 9223372036854775807), (2, 9223372036854775807)",
];

/// The rows agree; the type in `RowDescription` does not.
///
/// Every entry is `sum(bigint)` and every entry has one reason, ADR 0031: PostgreSQL's `sum` over
/// a `bigint` is `numeric` and this node has no `numeric`. For every input that does not overflow
/// an `int8`, the two print **the same characters** — which is what makes this a type divergence a
/// client sees only in the OID rather than a wrong number. The input that *does* overflow is in
/// [`DIVERGENCES`], where it belongs.
const TYPE_DIVERGENCES: &[&str] = &[
    // A self-join under two aliases, which ran for the first time when unit 5 built them. Its
    // rows agree; what differs is what `sum(int8)` is called, the same as every line above.
];

/// Queries this node answers differently, each with its reason.
///
/// A `0A000` here is contract C2 working — the construct is named rather than approximated — and
/// the two that are *not* `0A000` are the ones to read: `sum` overflowing, which is the visible
/// edge of ADR 0031, and the group order, which PostgreSQL does not promise and this node does.
const DIVERGENCES: &[(&str, &str, &str)] = &[
    // ADR 0031, and the whole of what an int8 sum costs.
    // avg over an integer column: numeric with sixteen fractional digits, which no float8 renders.
    // The group order, which is a promise PostgreSQL does not make and this node does.
    (
        "SELECT g, count(*) FROM agg GROUP BY g",
        "with no ORDER BY, PostgreSQL returns groups in hash order and this node returns them in \
         pg_cmp order of the key — deterministic, and a superset of what PostgreSQL guarantees",
        "UNMEASURED",
    ),
    (
        "SELECT count(*) FROM agg GROUP BY g LIMIT 1",
        "the same: with no ORDER BY, which group is first is PostgreSQL's hash order and ours is \
         the smallest key",
        "UNMEASURED",
    ),
    // Contract C2: parsed, named, not executed. Each is a unit of its own or explicitly out of
    // scope in `docs/plans/phase-9-rails.md` §5.
    (
        "SELECT g, count(*) FROM agg GROUP BY GROUPING SETS ((g), ())",
        "GROUP BY GROUPING SETS",
        "UNMEASURED",
    ),
    (
        "SELECT count(*) FILTER (WHERE n > 0) FROM agg",
        "an aggregate FILTER clause",
        "UNMEASURED",
    ),
    (
        "SELECT count(*) OVER () FROM agg",
        "a window function",
        "UNMEASURED",
    ),
    (
        "SELECT count(*) FROM wide GROUP BY 'x'",
        "a non-integer constant in GROUP BY is 42601 there and an ordinary one-group key here",
        "UNMEASURED",
    ),
    (
        "SELECT bool_and(b), bool_or(b) FROM agg",
        "bool_and and bool_or are not among the five aggregates",
        "UNMEASURED",
    ),
    (
        "SELECT string_agg(g, ',') FROM agg",
        "string_agg is not among the five aggregates",
        "UNMEASURED",
    ),
    ("SELECT count(*) + 1 FROM agg", "arithmetic", "UNMEASURED"),
    ("SELECT sum(n) + 0 FROM agg", "arithmetic", "UNMEASURED"),
    (
        "SELECT DISTINCT ON (g) g, n FROM agg ORDER BY g, n",
        "SELECT DISTINCT ON",
        "UNMEASURED",
    ),
    (
        "SELECT pg_typeof(count(*)), pg_typeof(sum(n)), pg_typeof(avg(n)), pg_typeof(sum(f)), \
         pg_typeof(avg(f)) FROM agg",
        "pg_typeof, which needs the catalog unit",
        "UNMEASURED",
    ),
];

#[test]
fn every_aggregate_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_aggregate.txt"),
        FIXTURE,
        &parity::Divergences {
            types: TYPE_DIVERGENCES,
            answers: DIVERGENCES,
        },
    );
    assert!(
        checked > 140,
        "only {checked} probes ran; the corpus did not load"
    );
}
