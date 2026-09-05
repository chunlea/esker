//! `hstore` as a column type and a value, against PostgreSQL 19beta1.
//!
//! Run 47's ranking row 7 — `extension "…" is not available`, 114 tests over 9 files — and hstore
//! is the bulk of it. The extension's *install* mechanics are `tests/extension.rs`'s; this is the
//! type once it is installed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus installs its own extension and builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The standing catalog type trade**, and nothing about hstore: `name`, `oid`, `"char"` and
    // `regproc` are types this node does not have and answers as `text` and `bigint`, whose
    // *values* are identical — which is why all three rows agree. These are the adapter's own boot
    // queries, and what it reads out of them is the typname and the typinput, both of which are
    // right.
    types: &[
        "SELECT 'r', extname, extversion FROM pg_extension WHERE extname IN ('hstore') ORDER BY \
         extname",
        "SELECT 'r', t.typname, t.typelem, t.typdelim, t.typinput, t.typtype, t.typbasetype, \
         t.typcategory, t.typlen FROM pg_type as t WHERE t.typname IN ('hstore') ORDER BY \
         t.typname",
        "SELECT 'r', t.typname, t.oid = 0 AS oid_is_zero, t.typarray = 0 AS no_array_type FROM \
         pg_type t WHERE t.typname IN ('hstore') ORDER BY t.typname",
    ],
    answers: &[
        // **Two operators this unit did not build**, and they are the two nothing in the suite
        // sends: `hstore - text` deletes a key and `?&` asks for all of a list. They are in the
        // corpus because they were probed while the ordering rule was — a measurement is cheap and
        // a guess later is not — and the statement sits in a `SAVEPOINT` so one `0A000` cannot
        // take the rest of the file with it. `-` is the harder of the two and not for the reason
        // it looks: it arrives as `ArithOp::Sub` and would have to be told apart from arithmetic,
        // where `->`, `?`, `@>` and `||` are carried as calls the way `&&` already is.
        (
            "SELECT 'r', 'a=>1, b=>2'::hstore - 'a'::text, 'a=>1'::hstore ?& ARRAY['a']",
            "hstore's - and ?& operators are not built",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_hstore_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_hstore.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}
