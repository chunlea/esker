//! `i.indkey` as an `= ANY` operand and as `array_position`'s array — boot statement 17, and the
//! ladder's rung-3 blocker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade, and one addition to it that is this unit's own: `attname`
    // and `relname` are `name` on a real server, and **`indkey` is an `int2vector`** where it is
    // `text` here. That is the representation this unit chose deliberately and the reason is in
    // `crate::value::vector` — an array is a value the operators read out of its own text form,
    // because a `Datum` variant would be a row-codec type for something that can never be stored
    // and a `ColumnType` would put an array row in `pg_type` advertising a column this node
    // refuses to create.
    //
    // **Every row agrees**, on all seventeen statements, `2 3` included: `int2vectorout` is what
    // this column already held, and `ActiveRecord` reads it with `String#split(" ")`.
    types: &[
        "SELECT i.relname, x.indkey FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = 'ak'::regclass ORDER BY i.relname",
        "SELECT x.indkey, x.indkey::text FROM pg_index x WHERE x.indexrelid = 'ak_c'::regclass",
        "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) WHERE i.indrelid = 'ak'::regclass AND i.indisprimary ORDER BY array_position(i.indkey, a.attnum)",
        "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) WHERE i.indrelid = 'ak2'::regclass AND i.indisprimary ORDER BY array_position(i.indkey, a.attnum)",
        "SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) WHERE i.indexrelid = 'ak_ab'::regclass ORDER BY array_position(i.indkey, a.attnum)",
    ],
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
