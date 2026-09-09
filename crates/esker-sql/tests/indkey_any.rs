//! `i.indkey` as an `= ANY` operand and as `array_position`'s array — boot statement 17, and the
//! ladder's rung-3 blocker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade: `attname` and `relname` are `name` on a real server and
    // `text` here.
    //
    // **`indkey` is no longer part of it.** This list used to carry it too, on the argument that a
    // `ColumnType` "would put an array row in `pg_type` advertising a column this node refuses to
    // create" — and the answer to that turned out to be that `int2vector` is not an array: it is a
    // scalar type on a real server, in `pg_type` with oid 22, and `CREATE TABLE t (v int2vector)`
    // works there. So it is a `ColumnType` now, sharing text's representation the way `json` and
    // `jsonb` do, and the entry whose only difference was `indkey` has come off under rule 2.
    //
    // **Every row still agrees**: `int2vectorout` is the space-separated numbers this column
    // already held, and `ActiveRecord` reads it with `String#split(" ")`.
    types: &[],
    answers: &[],
};

#[test]
fn every_indkey_any_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_indkey_any.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The statement `ActiveRecord` actually sends, against a **composite** key — where the answer is
/// wrong in a way that still looks sorted.
///
/// `PRIMARY KEY (y, x)` has `indkey` `2 1`: the key's order is not `attnum` order. So the
/// `ORDER BY array_position(...)` is what makes the answer `y, x` rather than `x, y`, and an
/// implementation that dropped it, or that sorted by `attnum`, hands `ActiveRecord` a reversed
/// composite primary key.
#[test]
fn a_composite_key_comes_back_in_key_order_and_not_attnum_order() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ak2 (x int8, y text, PRIMARY KEY (y, x))")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND \
             a.attnum = ANY(i.indkey) WHERE i.indrelid = 'ak2'::regclass AND i.indisprimary ORDER \
             BY array_position(i.indkey, a.attnum)"
        ),
        [["y"], ["x"]]
    );
}

/// `int2vector` is **0-based** and an ordinary array is 1-based, and the difference is invisible to
/// a sort.
///
/// This is the fact boot 17 cannot catch: `ORDER BY` only compares, so an off-by-one orders
/// identically and every caller that reads the number is wrong. Both spellings are asserted
/// together, because the rule is the contrast and not either half.
#[test]
fn array_position_is_zero_based_over_an_int2vector_and_one_based_over_an_array() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ak (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX ak_ab ON ak (a, b)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT array_position(x.indkey, 2::int2), array_position(x.indkey, 3::int2) FROM \
             pg_index x WHERE x.indexrelid = 'ak_ab'::regclass"
        ),
        [["0", "1"]],
        "the first column of an int2vector is at 0"
    );
    assert_eq!(
        node.rows("SELECT array_position('{a,b,c}'::text[], 'a')"),
        [["1"]],
        "and the first element of an array is at 1"
    );
    // Stated directly, which is the same fact without the search.
    assert_eq!(
        node.rows(
            "SELECT array_lower(x.indkey, 1), array_upper(x.indkey, 1) FROM pg_index x WHERE \
             x.indexrelid = 'ak_ab'::regclass"
        ),
        [["0", "1"]]
    );
}

/// `= ANY` over a column is the same three-valued rule `IN` already has.
///
/// The half that is not "does it contain it": a NULL on the left is NULL rather than false, which
/// is what makes `= ANY` a comparison and not a membership test.
#[test]
fn any_over_an_indkey_is_three_valued() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ak (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX ak_ab ON ak (a, b)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT 2 = ANY(x.indkey), 9 = ANY(x.indkey), NULL::int2 = ANY(x.indkey) FROM \
             pg_index x WHERE x.indexrelid = 'ak_ab'::regclass"
        ),
        [["t", "f", "\\N"]]
    );
}
