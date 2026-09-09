//! **Every array type has a `pg_type` row**, including the ten no `ColumnType` stands for.
//!
//! `type_lookup_test#test_array_delimiters_are_looked_up_correctly` asks the adapter's type map for
//! 1020 and 1007 and compares their delimiters. This node had 1007 `_int4` and not 1020 `_box`, so
//! the lookup returned a bare `ActiveModel::Type::Value` and the test died on `undefined method
//! 'delimiter'` — a symptom of a missing catalog row, not of the delimiter logic.
//!
//! `_box` is the only array in the suite whose delimiter is a semicolon, and the reason is in the
//! type: a box prints as `(1,1),(0,0)`, so `{(1,1),(0,0);(3,3),(2,2)}` is two boxes where the same
//! string with commas would be four points.
//!
//! **A `pg_type` row is a fact about the catalog, not a column type this node can store.** Ten base
//! types have a `typarray` on a real server and no array type here (ADR 0047) — the six geometric
//! shapes, `regclass`, the two vectors and `lquery` — and leaving their rows out made `typarray` a
//! pointer at nothing. The adapter follows it: `box` is registered by name, so 603 enters the type
//! map; then every type whose `typelem` is a known oid is asked for, which is where 1020 comes
//! from; and an array is registered only when its element is already there, so both rows are
//! needed and neither alone is enough.
//!
//! `tests/corpus/pg19_array_types.txt` carries the measurement, and the audit behind it: the whole
//! of this node's `pg_type` diffed against the oracle's, column by column, for every type both
//! have.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every probe reads the catalog.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_array_type_row_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_types.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 20, "the corpus shrank: {checked} statements");
}

/// The two lookups the Rails test makes, and the row it could not find.
#[test]
fn the_adapters_two_rows_carry_their_own_delimiters() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT t.typname, t.typelem, t.typdelim, t.typinput, t.typtype FROM pg_type t \
             WHERE t.oid IN (1007, 1020) ORDER BY t.oid"
        ),
        [
            [
                "_int4".to_owned(),
                "23".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "b".to_owned()
            ],
            [
                "_box".to_owned(),
                "603".to_owned(),
                ";".to_owned(),
                "array_in".to_owned(),
                "b".to_owned()
            ],
        ]
    );
    // The element's row is the other half: without it the adapter never asks for 1020.
    assert_eq!(
        node.rows("SELECT t.oid, t.typarray FROM pg_type t WHERE t.typname = 'box'"),
        [["603".to_owned(), "1020".to_owned()]]
    );
    // And the oid prints as a name through both functions that print one — the third and
    // fourth places the link had to reach.
    assert_eq!(
        node.rows(
            "SELECT 'box[]'::regtype::oid, 1020::regtype::text, format_type(1020, NULL), \
             0::regtype::text"
        ),
        [[
            "1020".to_owned(),
            "box[]".to_owned(),
            "box[]".to_owned(),
            "-".to_owned()
        ]]
    );
}
