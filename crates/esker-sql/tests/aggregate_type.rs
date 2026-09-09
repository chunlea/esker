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
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        // `array_agg(1)` joined them with the literal ladder's `int4` rung: its rows used to
        // differ too — `bigint[]` against a real server's `integer[]` — and now only `pg_typeof`'s
        // own answer does.
        "SELECT 'r', pg_typeof(array_agg(1)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i2)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(d)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(b)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(ts)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(u)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(t || 'x')) FROM ag",
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
        // Moved up from `answers` by `Literal::TypedNull`: the cast survives lowering, so the
        // argument is an `int4` and the aggregate declares `integer[]` — the row PostgreSQL
        // gives. Only the `regtype` trade above it is left, which is what this list is.
        "SELECT 'r', pg_typeof(array_agg(NULL::int4)) FROM ag",
        "SELECT 'r', pg_typeof(array_agg(i4)) FROM ag GROUP BY t",
    ],
    answers: &[
        // **This node has four array types** (ADR 0047: `bigint[]`, `integer[]`, `numeric[]`,
        // `text[]`), which is what the Rails schema needs and no more. `array_agg` over any other
        // element gathers the same array and has no type to declare for it, so the column stays
        // `text` and `pg_typeof` — which reads the value — says so. Five element types, one fact.
        // `pg_typeof` reads the **value** here and the static type there, and an aggregate over no
        // rows is NULL — which has no type. The declared type of the column is right either way;
        // it is the function that cannot see it.
        (
            "SELECT 'r', pg_typeof(array_agg(i4)) FROM ag WHERE false",
            "pg_typeof reads the value, and an aggregate over no rows is NULL",
            "UNMEASURED",
        ),
        // Two functions this node does not have. Named rather than approximated, and neither is
        // about the result type this file is for.
        (
            "SELECT 'r', pg_typeof(string_agg(t, ',')) FROM ag",
            "string_agg is not implemented",
            "UNMEASURED",
        ),
        // **A bare NULL is still resolved to `text` where PostgreSQL calls it `unknown`.** The
        // entry beside this one — `array_agg(NULL::int4)` — has gone: `Literal::TypedNull` keeps
        // the cast, so the two spellings are no longer one expression. What is left is the bare
        // one: PostgreSQL raises `42725 function array_agg(unknown) is not unique` and this node
        // answers `text[]`, because `exec::aggregate::is_unknown` takes only a quoted string.
        // Its doc comment named the dropped cast as the reason it could not take a NULL as well,
        // and that reason is now gone — the one-line widening is unblocked, and what it needs
        // first is a capture of the bare-NULL argument across all six aggregates (`sum` and `avg`
        // should raise with `array_agg`; `min`, `max` and `count` should not), because only
        // `array_agg`'s half of that family has been put to the oracle.
        (
            "SELECT 'r', pg_typeof(array_agg(NULL)) FROM ag",
            "a bare NULL is text here and unknown there, so this answers where PostgreSQL raises \
             42725; exec::aggregate::is_unknown takes only a quoted string",
            "UNMEASURED",
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
