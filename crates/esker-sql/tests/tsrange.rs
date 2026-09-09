//! `tsrange` as a column type and a value, against PostgreSQL 19beta1.
//!
//! Run 50's ranking: `the type tsrange is not supported`, **46 tests in one file**
//! (`adapters/postgresql/range_test.rb`), which declares eight range columns — `tsrange` is the one
//! the board names because it is the first the node refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade — `name` and `"char"` answered as `text`, whose values are
    // identical, which is why the row agrees. What the row *says* is the point: `tsrange`,
    // `tstzrange` and `int4range` are `r`/`R` and `_tsrange` is `b`/`A`, all four right.
    //
    // **Three more join it with the six-range unit**, and every value in them agrees: `typelem`
    // and `typinput` are a `regtype` and a `regproc` there and `text` here, `typname` is a `name`,
    // and `pg_typeof` is the same trade `'x'::regtype` already makes. The `points_back` column —
    // the two-way link `pg_19_array_type_map.txt` checks — is a `boolean` on both and is `t` for
    // all six range types.
    types: &[
        "SELECT 'r', t.typname, t.typelem::regtype, t.typcategory, t.typinput FROM pg_type t \
         WHERE t.typname IN ('_tsrange','_tstzrange') ORDER BY t.typname",
        "SELECT 'r', typname, typtype, typcategory FROM pg_type WHERE typname IN \
         ('tsrange','tstzrange','int4range','_tsrange') ORDER BY typname",
    ],
    answers: &[
        // **The standing constant-width trade, seen through a range bound.** Every integer here
        // is an `i64`, so `int4range`'s subtype reports `bigint` where a real server names
        // `integer`. The `22P02` and the quoted text agree; only the type in the sentence differs,
        // and it differs for the same reason `2147483648` is accepted as an `int8` literal.
        (
            "SELECT 'r', '[''1'', ''10'']'::int4range",
            "an int4range's bound is read as an int8 here, so the message names bigint",
            "pg19_tsrange.txt:106",
        ),
        // **The standing text-collation divergence, seen through a range bound**, and it is the
        // bound *parsing* that makes it visible rather than any new rule: `["a,b", "c,d"]` has
        // bounds `a,b` and ` c,d`, and whether that range is legal is a comparison. Under the
        // oracle's `en_US.utf8` the leading space is ignored and `a,b < " c,d"`; under this node's
        // byte order it is not, so the bounds are the wrong way round and the answer is `22000`.
        // Measured on the oracle itself: `'a,b' < ' c,d'` is `t` and the same pair
        // `COLLATE "C"` is `f`. Same divergence `tests/corpus/pg19_order.txt` records for `text`,
        // and a project that compiles no C cannot close it (`crate::row`).
        (
            "SELECT 'r', '[\"a,b\", \"c,d\"]'::stringrange",
            "the bounds compare byte-wise here and under en_US.utf8 there",
            "pg19_tsrange.txt:118",
        ),
        (
            "SELECT 'r', lower('[\"a,b\", \"c,d\"]'::stringrange), upper('[\"a,b\", \"c,d\"]'::stringrange)",
            "the same collation divergence, one line on",
            "pg19_tsrange.txt:119",
        ),
        // The same again with no quotes in sight: `[a, f]`'s bounds are `a` and ` f`, and the
        // space is the whole of the difference.
        (
            "SELECT 'r', '[a, f]'::stringrange, '[''a'', ''f'']'::stringrange",
            "a leading space in a bound is ignored by en_US.utf8 and is not by byte order",
            "pg19_tsrange.txt:120",
        ),
        // **`DateStyle` is not a run-time parameter here.** The text form of a timestamp depends on
        // it on a real server, and this node has one spelling — so the parameter would be a knob
        // that changes nothing, which is worse than not having it. `TimeZone` beside it answers,
        // and the two are one statement.
        (
            "SELECT 'r', current_setting('DateStyle'), current_setting('TimeZone')",
            "DateStyle is not a run-time parameter here; this node has one text form",
            "pg19_tsrange.txt:44",
        ),
        // **`pg_range` is not built**, and this statement is the *evidence* for the unit rather
        // than part of it: `rngcanonical` is `int4range_canonical` for `int4range` and `-` for
        // `tsrange`, which is what says one subtype is canonicalised and the other is not. That
        // rule is now in `crate::value::range::canonicalise`; the table a client could read it out
        // of is a catalog view of its own.
        (
            "SELECT 'r', rngsubtype::regtype, rngcanonical, rngsubdiff FROM pg_range r JOIN \
             pg_type t ON t.oid = r.rngtypid WHERE t.typname IN ('tsrange','int4range') ORDER BY \
             t.typname",
            "pg_range is not built",
            "pg19_tsrange.txt:46",
        ),
        // **Array containment, which is not a range fact.** `@>` between two arrays is the array
        // operator `tests/array.rs` already declares for every element type — `'{1,2}'::int[] @>
        // '{1}'::int[]` is the same `0A000` — and it arrives here because the corpus asks it of a
        // range array. The `=` beside it on the same line answers, and answers **`t`**, which is
        // the half this unit is about: an array's equality is its elements', and an `int4range`
        // element compares canonically, so `ARRAY['[1,10]'] = ARRAY['[1,11)']` is true.
        (
            "SELECT 'r', ARRAY['[1,10]'::int4range] = ARRAY['[1,11)'::int4range], \
             ARRAY['[1,10]'::int4range] @> ARRAY['[1,11)'::int4range]",
            "@> between two arrays is the array operator, unbuilt for every element type",
            "pg19_tsrange.txt:146",
        ),
        (
            "SELECT 'r', ARRAY['[1,10]'::int4range] @> '[2,3]'::int4range",
            "both refuse — array containment needs two arrays — and only the code differs",
            "pg19_tsrange.txt:148",
        ),
    ],
};

#[test]
fn every_tsrange_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_tsrange.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}
