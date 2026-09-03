//! `a[i]` — subscripting an array. A piece of boot statements 35 and 36.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // An element's declared type: `smallint` on a real server, `text` here — an array is text on
    // this node and so are its elements (`crate::value::vector`). **Every row agrees**, and the
    // join two lines down is the proof that the *value* carries the right type where it matters:
    // it is an `int2` column matched against a subscript, and it finds the column.
    types: &[
        "SELECT d.indkey[0], d.indkey[1], d.indkey[2] FROM pg_index d WHERE d.indexrelid = 'sb_ab'::regclass",
        "SELECT a.attname FROM pg_index d JOIN pg_attribute a ON a.attrelid = d.indrelid AND a.attnum = d.indkey[0] WHERE d.indexrelid = 'sb_ab'::regclass",
        "SELECT a.attname FROM pg_index d JOIN pg_attribute a ON a.attrelid = d.indrelid AND a.attnum = d.indkey[1] WHERE d.indexrelid = 'sb_ab'::regclass",
        "SELECT a.attname FROM pg_constraint c JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] WHERE c.conrelid = 'sbc'::regclass AND c.contype = 'f'",
    ],
    // One, and it is **not the subscript**: `pg_constraint.conkey` is filled here only for a
    // foreign key, where a real server fills it for every constraint that has columns — a primary
    // key's is `{1}` there and NULL here. So the subscript is NULL for the right reason and the
    // wrong array. The foreign-key rows two lines below it in the corpus are exact, which is what
    // separates the two claims.
    answers: &[],
};

#[test]
fn every_subscript_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_subscript.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The subscript is **absolute**, so the same written number means different positions in the two
/// array-ish types the catalog holds.
///
/// `indkey[0]` is an index's first column and `conkey[1]` is a constraint's, because an
/// `int2vector` starts at 0 and an `int2[]` starts at 1. An implementation that treated the
/// subscript as an offset from the first element gets one of the two wrong, and both are shapes
/// `ActiveRecord` writes.
#[test]
fn a_subscript_follows_the_arrays_own_lower_bound() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE sb (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX sb_ab ON sb (a, b)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT d.indkey[0], d.indkey[1], d.indkey[2] FROM pg_index d WHERE d.indexrelid = \
             'sb_ab'::regclass"
        ),
        [["2", "3", "\\N"]]
    );
    // A **foreign key's** `conkey`, because that is the only constraint kind this node fills it
    // for — a pre-existing `pg_constraint` gap the corpus declares, not the subscript's.
    node.run("CREATE TABLE sbp (id int8 PRIMARY KEY)").unwrap();
    node.run("CREATE TABLE sbc (id int8 PRIMARY KEY, p int8 REFERENCES sbp (id))")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.conkey[1], c.conkey[2] FROM pg_constraint c WHERE c.conrelid = \
             'sbc'::regclass AND c.contype = 'f'"
        ),
        [["2", "\\N"]]
    );
}

/// The element is compared **as its own type**, which is what makes the join work.
///
/// `a.attnum = d.indkey[0]` is an `int2` column against a subscript. A node that answered the
/// element as text and compared it to a number would find nothing — and report an empty join
/// rather than an error, which is the failure that looks like data.
#[test]
fn a_subscript_joins_against_a_typed_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE sb (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX sb_ab ON sb (a, b)",
    ]);
    for (subscript, name) in [(0, "a"), (1, "b")] {
        assert_eq!(
            node.rows(&format!(
                "SELECT a.attname FROM pg_index d JOIN pg_attribute a ON a.attrelid = d.indrelid \
                 AND a.attnum = d.indkey[{subscript}] WHERE d.indexrelid = 'sb_ab'::regclass"
            )),
            [[name]],
            "for indkey[{subscript}]"
        );
    }
}

/// Five ways to miss, one answer: NULL, and never an error.
#[test]
fn every_way_of_missing_is_null() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT ('{a,b,c}'::text[])[0], ('{a,b,c}'::text[])[9], ('{}'::text[])[1], \
             (NULL::text[])[1], ('{a,b,c}'::text[])[NULL]"
        ),
        [["\\N", "\\N", "\\N", "\\N", "\\N"]]
    );
}
