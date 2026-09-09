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
    // **`pg_typeof` answers a `regtype` on both now** (ADR 0093): it is resolved at plan
    // time from the argument's declared type, so what this list recorded has no difference
    // left in it.
    types: &[],
    answers: &[
        // **This node has four array types** (ADR 0047: `bigint[]`, `integer[]`, `numeric[]`,
        // `text[]`), which is what the Rails schema needs and no more. `array_agg` over any other
        // element gathers the same array and has no type to declare for it, so the column stays
        // `text` and `pg_typeof` — which reads the value — says so. Five element types, one fact.
        // `pg_typeof` reads the **value** here and the static type there, and an aggregate over no
        // rows is NULL — which has no type. The declared type of the column is right either way;
        // it is the function that cannot see it.
        // One function this node does not have, named rather than approximated. `string_agg` was
        // the other and is built now, so its entry came off under rule 2
        // (`tests/aggregate_groups.rs`); its result type is `text` on both sides.
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
