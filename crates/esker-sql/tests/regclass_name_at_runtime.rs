//! **A relation *name* reaches a `regclass` wherever the value does** — `debts-v1.1.md` #41,
//! second half.
//!
//! The first half was the comparison, and it needed no catalog: `=` over a `regclass` is `oideq`,
//! so a bare literal is an oid and a name is `22P02` (`tests/regclass_literal.rs`). This half is
//! everything that goes the *other* way — through `regclassin`, which resolves a name — and every
//! one of them needs the catalog at a point that did not have it.
//!
//! Three shapes, and the third is the one r1's wire gate found rather than this row:
//!
//! ```text
//! INSERT INTO t VALUES ('ra')                       an assignment
//! UPDATE t SET r = 'ra'                             an assignment
//! SELECT c::regclass FROM (VALUES ('pg_class')) s   a cast whose operand is a *value*
//! ```
//!
//! The third is the same fact seen from the far end: `'pg_class'::regclass` is resolved once per
//! statement by a pass that has a transaction, and a cast whose operand only exists per row cannot
//! be. It answered `an oid is an integer, not Text("pg_class")`, which is the shape of a value
//! arriving somewhere its type was decided without it.
//!
//! # Where the rule lives, and why not in `Env`
//!
//! Resolving a relation *name* needs `Executor::stored_name_written` — `pg_temp` rewriting, then
//! the search path walked against the catalog view — and every piece of that is Executor state.
//! `Env`, which is what the row evaluator has, carries a transaction, a catalog snapshot and the
//! search path, so a *nearly* faithful resolver could have been written there. That is exactly the
//! mistake this repository has already paid for: a second reader of one name grammar, agreeing
//! with the first until it does not.
//!
//! So the shape is the one #35 uses in the other direction. The Executor builds the rule —
//! `Executor::name_rule`, a `&dyn Fn(&str) -> Result<i64>` wrapping `relation_oid`, the mirror of
//! the `oid -> name` closure `decode_row` takes — and hands it down. It hangs on
//! `cursor::Settings::names` rather than on `Env`: the same seam, three construction sites instead
//! of `Env`'s, which is the deviation from the sketch above and was taken deliberately. Each of
//! the three shapes is then one arm: a `Datum::Text` cast to `regclass` in `exec::cursor`, the
//! assignment in `exec::dml`, and `resolve_in_list` wrapping each string item in that cast rather
//! than reconciling it as an oid.
//!
//! Measured in `tests/captures/pg19_regclass_name_at_runtime.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE ra (id int8)",
        "CREATE TABLE rb (id int8)",
        "CREATE TABLE rh (id int8, r regclass)",
    ])
}

/// **An assignment resolves the name**, which is what `regclassin` is for.
#[test]
fn an_assignment_takes_a_bare_name() {
    let mut node = node();
    node.run("INSERT INTO rh VALUES (1, 'ra')").unwrap();
    assert_eq!(node.rows("SELECT id, r FROM rh"), vec![vec!["1", "ra"]]);
    node.run("UPDATE rh SET r = 'rb' WHERE id = 1").unwrap();
    assert_eq!(node.rows("SELECT r FROM rh"), vec![vec!["rb"]]);
    // And it is the relation, not the characters: a rename follows it.
    node.run("ALTER TABLE rb RENAME TO rz").unwrap();
    assert_eq!(node.rows("SELECT r FROM rh"), vec![vec!["rz"]]);
}

/// **A cast whose operand is a value, not a literal** — the shape the wire gate found.
#[test]
fn a_runtime_text_becomes_a_regclass() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT c::regclass FROM (VALUES ('ra')) s(c)"),
        vec![vec!["ra"]]
    );
    // The declared type is the point: a client is told 2205, not 25.
    let outcome = node
        .run("SELECT c::regclass FROM (VALUES ('ra')) s(c)")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 2205);
    // A column of names, which is what a catalog query does with one.
    node.run("INSERT INTO rh VALUES (1, 'ra')").unwrap();
    assert_eq!(
        node.rows("SELECT (r::text)::regclass FROM rh"),
        vec![vec!["ra"]]
    );
}

/// **`IN` resolves its list**, which is the exception the comparison half had to declare.
///
/// **`= ANY` and `IN` are one rule everywhere but here.** This crate folds `x = ANY(array)` and
/// `x <> ALL(array)` into the same `InList` node as a written `IN`, so an index seek can use a
/// list it can see — and the two differ in exactly one place: a bare literal in the list. `IN`
/// coerces it through the *type's* input function and resolves the name; `= ANY` really is `=`, so
/// `r = ANY('{ra}')` is `22P02`. Measured, both. That is why the node carries an `any` flag rather
/// than the two being one thing or two variants.
///
/// **And it has to be decided before the common-type coercion.** That step gives every `unknown`
/// in the list the list's type, which for a `regclass` operand means reading the name as an oid —
/// the comparison's rule, arriving one step early. Putting this after it made `IN` a `22P02` no
/// matter what the flag said.
#[test]
fn an_in_list_resolves_its_names() {
    let mut node = node();
    node.run("INSERT INTO rh VALUES (1, 'ra'), (2, 'rb')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id FROM rh WHERE r IN ('ra','rb') ORDER BY id"),
        vec![vec!["1"], vec!["2"]]
    );
}
