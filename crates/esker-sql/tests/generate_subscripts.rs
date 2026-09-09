//! `generate_subscripts` — a set-returning function in `FROM`, and rung 3's blocker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The rows agree and the types do not**, which is one trade made twice. A `pg_catalog`
    // column is `text` here where a real server has `name` or `\"char\"` or `smallint[]`, and an
    // aggregate that builds an array reports the element's type because an array is text here and
    // has no type of its own. Every value in these seven is identical to PostgreSQL's; what
    // differs is the OID a client is told to expect, and closing it means the array type of ADR
    // 0033's third tier rather than anything this unit could do.
    types: &[
        "SELECT conname, contype, conkey FROM pg_constraint WHERE conrelid = 'gsc'::regclass ORDER BY conname",
    ],
    answers: &[
        (
            "SELECT 'g', generate_subscripts('{a,b,c}'::text[])",
            "**A set-returning function in the SELECT list**, which is a second mechanism and not this one: there it multiplies the rows of the query it is written in, and two of them run in lockstep rather than as a cross product (line 68: the shorter is padded with NULL, not cycled). This node has the `FROM` form, which is what the schema dump and boot statements 35 and 36 use; the target-list form is refused by name and counted here. Its two `42883`s (lines 69 and 70) carry the same refusal for the same reason — the arity and argument types are checked by the `FROM` path, which these never reach.",
            "pg19_generate_subscripts.txt:69",
        ),
        (
            "SELECT 'g', generate_subscripts(1, 1)",
            "**A set-returning function in the SELECT list**, which is a second mechanism and not this one: there it multiplies the rows of the query it is written in, and two of them run in lockstep rather than as a cross product (line 68: the shorter is padded with NULL, not cycled). This node has the `FROM` form, which is what the schema dump and boot statements 35 and 36 use; the target-list form is refused by name and counted here. Its two `42883`s (lines 69 and 70) carry the same refusal for the same reason — the arity and argument types are checked by the `FROM` path, which these never reach.",
            "pg19_generate_subscripts.txt:70",
        ),
        (
            "SELECT pg_typeof(generate_subscripts('{a}'::text[], 1))",
            "`pg_typeof` is not built. It would answer `integer` here — the function's rows are `int4`, which the `FROM` form already reports — but the function itself is a separate unit and is refused by name rather than special-cased for one argument.",
            "pg19_generate_subscripts.txt:62",
        ),
    ],
};

#[test]
fn every_generate_subscripts_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_generate_subscripts.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
