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
    // The standing catalog type trade: `column_name` is a `name` on a real server and the two
    // `information_schema` columns beside it are its own domains, all three `text` here. **Every
    // value agrees**, and the values are what this line is for — `data_type` and `udt_name` both
    // say `point`, and the two defaults come back as `'(12.2,13.3)'::point`, which is what the
    // schema dumper reads.
    types: &[
        "SELECT 'r', column_name, column_default, data_type, udt_name FROM \
         information_schema.columns WHERE table_name = 'postgresql_points' ORDER BY \
         ordinal_position",
        // `pg_typeof` answers a `regtype` there and `text` here — the trade `'x'::regtype`
        // already makes — and the values are `point` and `point[]`, which is what these two ask.
        "SELECT 'r', pg_typeof('(1,2)'::point), '(1,2)'::point::text",
        "SELECT 'r', ARRAY['(1,2)'::point, '(3,4)'::point], pg_typeof(ARRAY['(1,2)'::point])",
        // And the catalog row: `typname` is a `name`, the two oids are `oid`, `typcategory` is a
        // `\"char\"` and `typinput` a `regproc`. Every character and every number agrees —
        // `point` is 600 with `typarray` 1017, `typlen` **16**, category **G**, input `point_in`.
        "SELECT 'r', t.typname, t.oid, t.typarray, t.typlen, t.typcategory, t.typinput FROM \
         pg_type t WHERE t.typname IN ('point','_point') ORDER BY t.typname",
    ],
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
