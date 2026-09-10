//! **A `reg*` compares as the `oid` it is** — wire v3 family **F8**, the second step.
//!
//! `pg_operator` has **no `=` for `regtype`**, and none for `regclass` or `regproc` either: the
//! comparison is `oid`'s. So an `unknown` beside one is coerced to `oid` and read by `oidin` —
//! **digits, not a name** — which is why a column of `regtype` reports `invalid input syntax for
//! type oid`.
//!
//! Measured on 19beta1 over all three types
//! (`tests/captures/pg19_reg_comparison.txt`), and it is not about parameters: the literal and the
//! bound parameter behave identically, and the **`INSERT` takes the name** while the comparison
//! does not.
//!
//! ```text
//! c = 'int4'          22P02      c = 'int4'::regtype   1 row
//! c IN ('int4')       22P02      c IN ('int4','text')  2 rows
//! c > 'int4'          22P02      c = 23                1 row
//! 'int4' = c          22P02      c::text = 'integer'   1 row
//! c = ANY(ARRAY['int4'])         42883 operator does not exist: regtype = text
//! ```
//!
//! **`IN` is two rules and the length decides.** A list of two or more becomes a
//! `ScalarArrayOpExpr` whose array is built through the *type's input function*, so the names
//! resolve; a list of one is rewritten to `=` and goes through the operator. This crate had the
//! first half only, and only for `regclass`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// A table of each `reg*`, holding a value written by name.
fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE ra (id bigint)",
        "CREATE TABLE rb (id bigint)",
        "CREATE TABLE gc (c regclass)",
        "INSERT INTO gc VALUES ('ra'), ('rb')",
        "CREATE TABLE gp (c regproc)",
        "INSERT INTO gp VALUES ('int4in')",
        "CREATE TABLE gt (c regtype)",
        "INSERT INTO gt VALUES ('int4'), ('text')",
    ])
}

/// **Every comparison shape with a bare name refuses, on all three types.**
#[test]
fn a_bare_name_beside_a_reg_type_is_read_as_an_oid() {
    let mut node = node();
    for (table, name) in [("gc", "ra"), ("gp", "int4in"), ("gt", "int4")] {
        for sql in [
            format!("SELECT c::text FROM {table} WHERE c = '{name}'"),
            format!("SELECT c::text FROM {table} WHERE c > '{name}'"),
            format!("SELECT c::text FROM {table} WHERE '{name}' = c"),
            format!("SELECT c::text FROM {table} WHERE c IN ('{name}')"),
        ] {
            assert_eq!(
                node.answer(&sql).to_string(),
                format!("!22P02 invalid input syntax for type oid: \"{name}\""),
                "{sql}"
            );
        }
    }
}

/// **And the spellings that do answer**, which is what says the refusal above is about the
/// operator and not about the type.
#[test]
fn a_cast_a_number_and_a_list_of_two_all_answer() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT c::text FROM gt WHERE c = 'int4'::regtype"),
        vec![vec!["integer"]]
    );
    assert_eq!(
        node.rows("SELECT c::text FROM gt WHERE c = 23"),
        vec![vec!["integer"]],
        "'int4'::regtype = 23 is t on 19beta1: a reg* compares as the oid it is"
    );
    assert_eq!(
        node.rows("SELECT c::text FROM gt WHERE c IN ('int4','text') ORDER BY c::text"),
        vec![vec!["integer"], vec!["text"]],
        "a list of two is built through the type's input function"
    );
    assert_eq!(
        node.rows("SELECT c::text FROM gp WHERE c IN ('int4in','int8in')"),
        vec![vec!["int4in"]],
        "and the same for a regproc, which this crate's IN rule never reached"
    );
    assert_eq!(
        node.rows("SELECT c::text FROM gc WHERE c IN ('ra','rb') ORDER BY c::text"),
        vec![vec!["ra"], vec!["rb"]],
        "the one it did reach, unchanged"
    );
}

/// **`= ANY(ARRAY[…])` is a third answer and not a synonym for `IN`** — the array's element is
/// `text`, so there is no operator at all.
#[test]
fn any_over_an_array_of_names_has_no_operator() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT c::text FROM gt WHERE c = ANY(ARRAY['int4'])")
            .to_string(),
        "!42883 operator does not exist: regtype = text \
         DETAIL: No operator of that name accepts the given argument types. \
         HINT: You might need to add explicit type casts."
    );
}

/// **An assignment takes the name**, which is the asymmetry the whole family turns on: `regtypein`
/// resolves a name and `oideq`'s operand is an oid, so the same text is read two ways depending on
/// where it stands.
#[test]
fn an_assignment_still_takes_the_name() {
    let mut node = node();
    assert_eq!(node.rows("SELECT count(*)::text FROM gt"), vec![vec!["2"]]);
    assert_eq!(
        node.rows("SELECT c::text FROM gt ORDER BY c::text"),
        vec![vec!["integer"], vec!["text"]],
        "both rows were inserted by name and print as the canonical one"
    );
}

/// **A `reg*` **array** is a different mechanism and this step does not close it**, pinned with
/// 19beta1's answers beside what this node gives.
///
/// Measured today:
///
/// ```text
/// '{t}'::regclass[]              regclass[]  {t}        here the same, value and type
/// '{t}'::text::regclass[]        regclass[]  {t}        here 0A000 … without a catalog
/// '{nosuch}'::regclass[]         !42P01 relation "nosuch" does not exist
/// '{int4,text}'::regtype[]       regtype[]   {integer,text}
/// unnest(ARRAY['{t}'::regclass[]])           regclass   here text
/// ```
///
/// The **direct** spelling is right. What is not is the cast **at the use site**: the scalar's
/// names are resolved in `Executor::bound`, before the plan, because a per-row catalog read is
/// what that pass exists to avoid, and `'{…}'::text::regclass[]` is not a shape it walks — so the
/// cast reaches the row evaluator, which has no catalog. It wants the same pass over an array's
/// elements, and a `42P01` for a name that is not there — its own step, sized from these rows.
///
/// The `unnest` row is wire v3 family **F10**'s last one, which is here because this is the
/// mechanism that owns it.
#[test]
fn a_reg_array_literal_is_still_text_here() {
    let mut node = node();
    // **The direct spelling is right, value and type**, which is what narrows the gap to the cast
    // at the use site. An earlier reading of this said the type was `text`; the probe had wrapped
    // the expression in `::text` and was reporting its own cast.
    assert_eq!(
        node.rows("SELECT ('{ra}'::regclass[])::text"),
        vec![vec!["{ra}"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof('{ra}'::regclass[])"),
        vec![vec!["regclass[]"]]
    );
    assert_eq!(
        node.answer("SELECT ('{ra}'::text::regclass[])::text")
            .to_string(),
        "!0A000 a relation name read as a regclass without a catalog is not supported",
        "19beta1 answers the array; the cast reaches the row evaluator because nothing resolved the \
         array's elements before the plan"
    );
}
