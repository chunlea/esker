//! A column of `ltree`, against PostgreSQL 19beta1.
//!
//! Run 70's row: 4 tests in `adapters/postgresql/ltree_test.rb`. The file is small — one
//! extension, one column, a round trip and a schema dump — and the type under it is not: an
//! `ltree`'s **order is not its text's**, which is the one thing this corpus is mostly about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus installs its own extension and creates its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // The standing catalog trade: `typname`, `udt_name` and `extname` are `name` on a real
        // server and `data_type` is `information_schema`'s own domain, all `text` here, all
        // comparing identically.
        "SELECT 'r', extname, extversion FROM pg_extension WHERE extname = 'ltree'",
        "SELECT 'r', typname, typlen, typcategory, typinput, typtype, typdelim FROM pg_type \
         WHERE typname IN ('ltree','_ltree') ORDER BY typname",
        "SELECT 'r', column_name, data_type, udt_name, character_maximum_length FROM \
         information_schema.columns WHERE table_name = 'ltrees' ORDER BY ordinal_position",
        "SELECT 'r', indexname FROM pg_indexes WHERE tablename = 'ltrees' ORDER BY indexname",
        // `pg_typeof` is a `regtype` there and `text` here; the value beside it agrees, and it is
        // the one this asks — an `ltree[]` literal is an `ltree[]`.
        "SELECT 'r', '{a.b,c.d}'::ltree[], pg_typeof('{a.b}'::ltree[])",
    ],
    answers: &[
        // **`COLLATE` is not lowered here at all**, and it is a parser gap for every type rather
        // than one of `ltree`'s: `sqlparser` gives it as an infix it has no parser for in one
        // spelling and an expression this crate refuses in the other. The two statements are here
        // because a byte-ordered comparison is the *control* for the ltree ordering above them —
        // the pair is what proves the two orders differ — and this node's own answer to that
        // control is in the statement above: `ORDER BY path` gives the labels' order, which is
        // not what a byte sort would.
        (
            "SELECT 'r', path FROM ltrees ORDER BY path::text COLLATE \"C\"",
            "COLLATE is not lowered here, for any type",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', 'a.b'::ltree < 'a-b'::ltree, 'a.b' < 'a-b' COLLATE \"C\"",
            "COLLATE is not lowered here, for any type",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_ltree_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_ltree.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
