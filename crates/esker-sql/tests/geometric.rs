//! The six geometric shapes, against PostgreSQL 19beta1.
//!
//! Run 71's row: 9 tests in `adapters/postgresql/geometric_test.rb`, which declares five of them
//! in one `create_table` and `line` in a table of its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade: `column_name` and `udt_name` are `name`, `data_type` is
    // `information_schema`'s own domain, and `pg_typeof` answers a `regtype` — all `text` here.
    // **Every value agrees**, which is what these two are for: the five columns report their own
    // type names, and a cast's `pg_typeof` names the shape rather than the text it is stored as.
    types: &[
        // The standing `pg_catalog` trade again — down to the oids and `regproc` now that `name`
        // (ADR 0084) and `"char"` (ADR 0095) are types here — and **every value agrees**: `box` is the one
        // type in the catalog whose array delimiter is a semicolon, which r1's run-75 provenance
        // probe found answering `,` here.
        // **`typarray` was `0` for all six and this was an `answers` entry**: a real server pairs
        // each shape with an array and `geometric_test.rb` declares none, so it was a named gap
        // rather than six more types. r1's wire sweep made that reason false — `array_agg` over a
        // `circle` came back a scalar `text` — and all five that were left arrived at once
        // (ADR 0091). Every value in this row agrees now; what is left is the catalog's own
        // `oid` and `regproc` against `bigint` and `text`, each its own unit.
        "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput FROM pg_type \
         WHERE typname IN ('lseg','box','path','polygon','circle','line') ORDER BY typname",
    ],
    answers: &[],
};

#[test]
fn every_geometric_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_geometric.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
