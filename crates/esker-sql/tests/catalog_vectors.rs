//! The catalog's two vector shapes, against PostgreSQL 19beta1.
//!
//! `pg_index.indkey` is an `int2vector` and `pg_constraint.conkey` is a `smallint[]`: they print
//! differently, they are subscripted from different ends, and both were `text` here. The follow-up
//! to `3b1cd1a`, which made the second one visible — a comparison against `attnum` went from
//! answering to `42883` because an `int2` was meeting a `text`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The `int2vector` half.** Every one of these has the right rows and a `text` where a real
    // server says `int2vector` or `smallint`: `indkey` is text here, so its elements are too. The
    // `conkey` half of the same file is not in this list any more, which is the unit.
    types: &[
        "SELECT 'r', indkey[0], indkey[1], array_length(indkey, 1) FROM pg_index WHERE indexrelid = 'vt_ab'::regclass",
        "SELECT 'r', a.attname FROM pg_attribute a, pg_constraint c WHERE c.conname = 'vt_pkey' AND a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) ORDER BY a.attname",
        "SELECT 'r', a.attname FROM pg_attribute a, pg_constraint c WHERE c.conname = 'vt_pkey' AND a.attrelid = c.conrelid AND a.attnum = c.conkey[1]",
        "SELECT 'r', pg_typeof(c.conkey[1]), pg_typeof(i.indkey[0]) FROM pg_constraint c, pg_index i WHERE c.conname = 'vt_pkey' AND i.indexrelid = 'vt_ab'::regclass",
    ],
    answers: &[
        // **`int2vector` is a type this node does not have.** `indkey` and `indoption` are `text`
        // holding the same characters — `1 2`, space-separated and zero-based — which is what
        // every reader of them here expects. Giving them the array type would change both the
        // printed form and the subscript base, so they stay text and this line says so. `conkey`
        // is a `smallint[]` on a real server and is one here now, which is the rest of this file.
        (
            "SELECT 'r', pg_typeof(indkey), pg_typeof(indoption) FROM pg_index WHERE indexrelid = \
             'vt_ab'::regclass",
            "int2vector is a type this node does not have; indkey and indoption are text",
            "UNMEASURED",
        ),
        // `confkey` is NULL on a primary key, and `pg_typeof` here reads the **value** rather than
        // the static type — the standing divergence of that function. `conkey` beside it in the
        // same row answers `smallint[]`, which is what this unit changed.
        (
            "SELECT 'r', pg_typeof(conkey), pg_typeof(confkey) FROM pg_constraint WHERE conname = \
             'vt_pkey'",
            "pg_typeof reads the value, and confkey is NULL on a constraint that is not a foreign \
             key",
            "UNMEASURED",
        ),
        // A set-returning function in the **target list** is a different feature from one in
        // `FROM`, which is where `generate_subscripts` is implemented. Both lines below are that
        // gap and neither is about the vectors.
        // **An implicit `LATERAL`.** A set-returning function in a comma `FROM` list may name a
        // table to its left on a real server — `generate_subscripts(c.conkey, 1)` after
        // `pg_constraint c` — and here the entries are independent, so `c` is not in scope. The
        // same query with the function first is what `tests/generate_subscripts.rs` runs.
        (
            "SELECT 'r', a.attname FROM (SELECT idx, c.conkey[idx] AS elem FROM pg_constraint c, \
             generate_subscripts(c.conkey, 1) AS idx WHERE c.conname = 'vt_pkey') k JOIN \
             pg_attribute a ON a.attnum = k.elem AND a.attrelid = 'vt'::regclass ORDER BY idx",
            "a set-returning function in a comma FROM list cannot see the entry to its left \
             (implicit LATERAL)",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_catalog_vector_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_vectors.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 13,
        "only {checked} statements ran; the corpus did not load"
    );
}
