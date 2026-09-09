//! `current_schemas(bool)` as a value — boot statement 22.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a function of the session alone.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade in both its shapes: a schema name is `name` on a real
    // server and an array of them is `name[]`; both are `text` here, which compares identically
    // and prints identically. **Every row agrees**, the `{pg_catalog,public}` text included.
    types: &[],
    answers: &[],
};

#[test]
fn every_current_schemas_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_current_schemas_value.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 11,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The two lower bounds, in one test, because the rule is the contrast.
///
/// `current_schemas` is an ordinary array and is **1-based**; `pg_index.indkey` is an
/// `int2vector` and is **0-based**. Both are read by the same operators out of the same kind of
/// text, and the only thing that distinguishes them is the bound each carries.
#[test]
fn a_schema_array_is_one_based_where_an_indkey_is_zero_based() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE cs (id int8 PRIMARY KEY, a int4, b text)",
        "CREATE INDEX cs_ab ON cs (a, b)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT array_lower(current_schemas(true), 1), array_position(current_schemas(true), 'public')"),
        [["1", "2"]]
    );
    assert_eq!(
        node.rows(
            "SELECT array_lower(x.indkey, 1), array_position(x.indkey, 2::int2) FROM pg_index x \
             WHERE x.indexrelid = 'cs_ab'::regclass"
        ),
        [["0", "0"]]
    );
}

/// The `ANY` form still expands where it is lowered, which is what lets an index seek use it.
///
/// The value form is an addition, not a replacement: `= ANY (current_schemas(false))` is a list
/// the planner can see, and turning it into a per-row array read would cost every catalog query
/// `ActiveRecord` sends the plan it has now.
#[test]
fn the_any_form_is_still_a_list_the_planner_can_see() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT n.nspname FROM pg_namespace n WHERE n.nspname = ANY (current_schemas(false))"
        ),
        [["public"]]
    );
    assert_eq!(
        node.rows(
            "SELECT 'public' = ANY(current_schemas(false)), 'nope' = ANY(current_schemas(false))"
        ),
        [["t", "f"]]
    );
}
