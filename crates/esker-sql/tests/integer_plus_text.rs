//! An untyped **bind parameter** takes its type from where it is used, against PostgreSQL 19beta1.
//!
//! Run 51's top shape — 282 + 38 tests over 44 files — reported as
//! `operator does not exist: integer + text`. No test adds a string to a number; see the corpus
//! header, which is r1's finding and the reason this file is not about an operator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    // **The rows agree and the declared width does not**: a bare integer constant is an `int8`
    // here and an `integer` there, so `1 + $1` is `5` on both sides and is called `bigint` on one.
    // The standing divergence `tests/unknown_literal.rs` holds. What matters for this unit is on
    // the other side of it — the parameter resolved to a *number* at all.
    types: &[],
    answers: &[
        // **`||` over text is not built**, which is the debt `tests/aggregate_type.rs` has
        // declared since the array unit — three statements here, and none of them is about
        // parameters. `title || $1` types its parameter correctly now and then has no operator to
        // apply; it is `0A000` naming itself rather than an hstore parse of a string that was
        // never one, which is what it briefly was while the hstore operators took a `Datum::Text`
        // on either side.
        // **Two parameters with nothing to resolve against.** PostgreSQL leaves both `unknown` and
        // says `42725 operator is not unique: unknown + unknown`; this node has no `unknown` to
        // leave them as, so an unresolved parameter keeps the `text` fallback and the answer is
        // `42883 operator does not exist: text + text`. Both refuse and neither guesses — what
        // differs is the sentence, and closing it means an `unknown` that survives inference,
        // which is the same type surface `Literal::TypedNull` opened for NULLs.
        (
            "SELECT 'r', $1 + $2",
            "an unresolved parameter is text here and unknown there, so 42883 rather than 42725",
            "UNMEASURED",
        ),
        // **The standing constant-width divergence, in four sentences.** A bare integer constant
        // is `int8` here and `integer` there (`tests/unknown_literal.rs`), so every message that
        // names the type says `bigint`. `pg_typeof('2')` adds the other half: an unquoted literal
        // is `unknown` on a real server and `text` here, which is *why* this node had to be told
        // to resolve a parameter from context rather than defaulting it — the fallback is the bug
        // this unit fixed, and these rows are where it is still visible.
        //
        // `1 + '2'::text` also names its operands in the **wrong order** — `text + bigint` where
        // PostgreSQL says `integer + text`. The resolution table collapses
        // `(constant, typed)` and `(typed, constant)` into one arm and reports the typed side
        // first, which is right for the type it picks and wrong for the sentence.
        (
            "SELECT 'r', pg_typeof(1 + '2'), pg_typeof('2'), pg_typeof('2'::text)",
            "a bare integer constant is int8 here and int4 there, and an unquoted literal is text \
             here and unknown there",
            "pg19_integer_plus_text.txt:60",
        ),
        (
            "SELECT 'r', 1 + '2'::text",
            "the same constant width, and the operands are named typed-side-first",
            "pg19_integer_plus_text.txt:65",
        ),
        (
            "SELECT 'r', '2'::text + 1",
            "the same constant width",
            "pg19_integer_plus_text.txt:68",
        ),
        // **`pg_operator` is not a relation this node has.** The statement is in the capture
        // because it is where "does not exist" is read out of on a real server — there are no `+`
        // operators with `text` on either side — and it is a catalog view of its own rather than
        // anything this unit needed.
        (
            "SELECT 'r', count(*) FROM pg_operator WHERE oprname = '+' AND 'text'::regtype IN \
             (oprleft, oprright)",
            "pg_operator is not built",
            "pg19_integer_plus_text.txt:79",
        ),
    ],
};

#[test]
fn every_integer_plus_text_answer_is_postgresql_19_s() {
    let checked = bind::replay(
        include_str!("corpus/pg19_integer_plus_text.txt"),
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
