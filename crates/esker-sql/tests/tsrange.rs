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
    types: &[
        "SELECT 'r', typname, typtype, typcategory FROM pg_type WHERE typname IN \
         ('tsrange','tstzrange','int4range','_tsrange') ORDER BY typname",
    ],
    answers: &[
        // **`DateStyle` is not a run-time parameter here.** The text form of a timestamp depends on
        // it on a real server, and this node has one spelling — so the parameter would be a knob
        // that changes nothing, which is worse than not having it. `TimeZone` beside it answers,
        // and the two are one statement.
        (
            "SELECT 'r', current_setting('DateStyle'), current_setting('TimeZone')",
            "DateStyle is not a run-time parameter here; this node has one text form",
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
