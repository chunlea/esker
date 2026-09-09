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
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
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
