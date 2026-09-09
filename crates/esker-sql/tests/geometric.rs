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
        "SELECT 'r', pg_typeof('(2,3),(5.5,7)'::lseg), pg_typeof('2,3,5.5,7'::box), \
         pg_typeof('{2,3,5.5}'::line)",
        // The standing `name`/`"char"` trade again, and **every value agrees**: `box` is the one
        // type in the catalog whose array delimiter is a semicolon, which r1's run-75 provenance
        // probe found answering `,` here.
        "SELECT 'r', typname, typdelim FROM pg_type WHERE typname IN \
         ('box','lseg','path','polygon','circle','line','point','xml') ORDER BY typname",
    ],
    answers: &[
        // **`typarray` is `0` for all six**, which is the one declared gap and it is a named one:
        // a real server pairs each shape with an array (`_lseg` 1018 and so on) and
        // `geometric_test.rb` declares none, so six more `ColumnType`s would buy no test. The
        // same call `floatrange[]` got. Every other column of this row agrees — the oids, the
        // widths (32, 32, 24, 24 and `-1` for the two point lists), category `G`, and
        // `poly_in` rather than `polygon_in`.
        (
            "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput FROM pg_type \
             WHERE typname IN ('lseg','box','path','polygon','circle','line') ORDER BY typname",
            "no array type for a shape here; every other column agrees",
            "UNMEASURED",
        ),
    ],
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
