//! What an aggregate's result **type** is, against PostgreSQL 19beta1.
//!
//! `array_agg` is why this file exists: its result is the array type of its argument, and this node
//! declared `text` for every one of them. The rows were right everywhere, so nothing failed except
//! the declared type — in three corpora at once. The rest of the aggregate surface is captured
//! beside it so that fixing one cannot quietly move another.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One fact, fourteen times**: `pg_typeof` answers a `regtype` on a real server and `text`
    // here, which is the trade `'x'::regtype` already makes everywhere in this crate. Every row
    // below is identical; only the column's declared type differs, and the day `regtype` becomes
    // a type of its own these entries fail rather than pass quietly.
    types: &[
        "SELECT 'r', pg_typeof(array_agg(i4)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(id)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(t)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(n)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(DISTINCT i4)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i4 ORDER BY i4 DESC)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i4 + 1)) FROM ag",
        "SELECT 'r', pg_typeof(min(i4)), pg_typeof(max(i2)), pg_typeof(count(*)) FROM ag",
        "SELECT 'r', pg_typeof(sum(i4)), pg_typeof(sum(i2)), pg_typeof(sum(id)), pg_typeof(sum(n)), pg_typeof(sum(d)) FROM ag",
        "SELECT 'r', pg_typeof(avg(i4)), pg_typeof(avg(id)), pg_typeof(avg(n)), pg_typeof(avg(d)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i4)::text) FROM ag",
        "SELECT 'r', pg_typeof(array_agg('lit'::text)) FROM ag",
        "SELECT 'r', array_agg(NULL::int4) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i4)) FROM ag GROUP BY t",
    ],
    answers: &[
        // **This node has four array types** (ADR 0047: `bigint[]`, `integer[]`, `numeric[]`,
        // `text[]`), which is what the Rails schema needs and no more. `array_agg` over any other
        // element gathers the same array and has no type to declare for it, so the column stays
        // `text` and `pg_typeof` — which reads the value — says so. Five element types, one fact.
        (
            "SELECT 'r', pg_typeof(array_agg(i2)) FROM ag",
            "no smallint[] type here: this node has four array types (ADR 0047)",
        ),
        (
            "SELECT 'r', pg_typeof(array_agg(d)) FROM ag",
            "no double precision[] type here: this node has four array types (ADR 0047)",
        ),
        (
            "SELECT 'r', pg_typeof(array_agg(b)) FROM ag",
            "no boolean[] type here: this node has four array types (ADR 0047)",
        ),
        (
            "SELECT 'r', pg_typeof(array_agg(ts)) FROM ag",
            "no timestamptz[] type here: this node has four array types (ADR 0047)",
        ),
        (
            "SELECT 'r', pg_typeof(array_agg(u)) FROM ag",
            "no uuid[] type here: this node has four array types (ADR 0047)",
        ),
        // `pg_typeof` reads the **value** here and the static type there, and an aggregate over no
        // rows is NULL — which has no type. The declared type of the column is right either way;
        // it is the function that cannot see it.
        (
            "SELECT 'r', pg_typeof(array_agg(i4)) FROM ag WHERE false",
            "pg_typeof reads the value, and an aggregate over no rows is NULL",
        ),
        // Two functions this node does not have. Named rather than approximated, and neither is
        // about the result type this file is for.
        (
            "SELECT 'r', pg_typeof(array_agg(t || 'x')) FROM ag",
            "the || operator is not implemented",
        ),
        (
            "SELECT 'r', pg_typeof(string_agg(t, ',')) FROM ag",
            "string_agg is not implemented",
        ),
        // The standing constant-width divergence, one array deeper: a bare integer constant is
        // `int8` here and `int4` there, so an array of them is `bigint[]`.
        (
            "SELECT 'r', pg_typeof(array_agg(1)) FROM ag",
            "a bare integer constant is int8 here and int4 there",
        ),
        // **A cast on a NULL is dropped at lowering** (`Literal::Null` carries no type), so
        // `NULL` and `NULL::int4` are the same expression here. That costs both directions at
        // once: the bare one is answered where PostgreSQL raises `42725`, and the cast one is
        // `text[]` where PostgreSQL says `integer[]`. Refusing both would refuse a statement a
        // real server answers, which is the worse of the two, so the rule that raises `42725`
        // takes only a quoted string. Giving `Literal::Null` a type is the unit that closes both.
        (
            "SELECT 'r', pg_typeof(array_agg(NULL)) FROM ag",
            "a cast on a NULL is dropped at lowering, so an untyped NULL cannot be told from a \
             typed one",
        ),
        (
            "SELECT 'r', pg_typeof(array_agg(NULL::int4)) FROM ag",
            "a cast on a NULL is dropped at lowering, so an untyped NULL cannot be told from a \
             typed one",
        ),
    ],
};

#[test]
fn every_aggregate_type_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_aggregate_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 36,
        "only {checked} statements ran; the corpus did not load"
    );
}
