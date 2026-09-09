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
//! # Why these are `#[ignore]` and not deleted
//!
//! They are the handover: a red test says what a bug report cannot. **The mechanism is settled and
//! the plumbing is not.** Resolving a relation *name* needs `Executor::stored_name_written` —
//! `pg_temp` rewriting, then the search path walked against the catalog view — and every piece of
//! that is Executor state. `Env`, which is what the row evaluator has, carries a transaction, a
//! catalog snapshot and the search path, so a *nearly* faithful resolver could be written there —
//! and that is exactly the mistake this repository has paid for before: a second reader of one
//! name grammar, agreeing with the first until it does not.
//!
//! So the shape is the one #35 already uses in the other direction: the Executor builds the rule —
//! a `&dyn Fn(&str) -> Result<i64>`, the mirror of the `oid -> name` closure `decode_row` takes —
//! and hands it down. `Env` gains a field, its three construction sites pass it, and the three
//! shapes above become one arm each: a `Datum::Text` cast to `regclass` in `exec::cursor`, the
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
#[ignore = "debts-v1.1.md #41, assignment half: the name rule is not yet threaded to the evaluator"]
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
#[ignore = "debts-v1.1.md #41, assignment half: the name rule is not yet threaded to the evaluator"]
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
#[ignore = "debts-v1.1.md #41, assignment half: the name rule is not yet threaded to the evaluator"]
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
