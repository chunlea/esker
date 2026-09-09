//! `point` as a column type and a value, against PostgreSQL 19beta1.
//!
//! Run 58's tier-3 row: 15 tests in `adapters/postgresql/geometric_test.rb`, which declares seven
//! `point` columns and one `point[]`, defaults two of them, and reads all of it back out of the
//! schema dumper.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The `information_schema` line that used to be here is gone**: `column_name` took `name`
    // and `data_type`, `column_default` and `udt_name` took `character varying`, which is what
    // `character_data` is on the wire — so the row agrees whole and the entry is deleted (ADR 0031
    // rule 2). What stays below is `pg_typeof`, which is a `regtype` there and `text` here.
    // **Empty.** `pg_typeof` answers a `regtype` on both sides now (ADR 0093), and the catalog
    // row ran out of differences with it: `typname` is a `name` (ADR 0084), the two oids are `oid`
    // (ADR 0097), `typcategory` a `"char"` (ADR 0095) and `typinput` a `regproc` (ADR 0098). Every
    // character and every number always agreed — `point` is 600 with `typarray` 1017, `typlen`
    // **16**, category **G**, input `point_in`.
    types: &[],
    answers: &[
        // **A geometric subscript is not an array subscript**, and this node has only the latter:
        // `p[0]` and `p[1]` are the two coordinates and are **zero-based**, where every array here
        // starts at one. `geometric_test.rb` never writes one — `ActiveRecord::Point` parses the
        // text — so it is the operator family below rather than this unit, and the refusal names
        // the shape rather than inventing an answer.
        (
            "SELECT 'r', ('(1.5,2.5)'::point)[0], ('(1.5,2.5)'::point)[1], \
             pg_typeof(('(1,2)'::point)[0])",
            "a subscript of a point is not an array subscript; the geometric operators are their \
             own unit",
            "UNMEASURED",
        ),
        // **The two operators a point *does* have.** `=` and `<` do not exist for one — that is
        // the whole shape of this type and the corpus pins both refusals — and what exists instead
        // is `~=` (same-as) and `<->` (distance). Neither is written by the suite, and building
        // them is the geometric-operator unit that `lseg` and `line` will want anyway.
        (
            "SELECT 'r', '(1,2)'::point ~= '(1,2)'::point, '(1,2)'::point <-> '(4,6)'::point",
            "~= and <-> are the geometric operators, which are their own unit",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_point_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_point.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}
