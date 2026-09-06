//! A function call as another function's argument, against PostgreSQL 19beta1.
//!
//! `unsafe_raw_sql_test.rb` sends `length(trim(title))` in two places — an `ORDER BY` and a
//! `pluck` — and run 103's triage recorded it as a refusal whose cause was **the nesting**:
//! "`length(trim(title))` refused as *the expression TRIM(title) is not supported* while
//! `trim(title)` alone works". That reading is wrong, and this file is what says so.
//!
//! `trim` did not work alone either. It reached `lower_expr`'s catch-all, which prints the
//! expression it could not lower — and the expression it names is the innermost one, so a report
//! written from the message alone blames the argument and acquits the call around it. The gap was
//! `TRIM`, it closed with `GREATEST`/`TRIM` (`tests/greatest_trim.rs`), and nesting was never a
//! separate mechanism: `lower_expr` recurses through the operand, so a call inside a call has only
//! ever cost whatever the inner call costs.
//!
//! Nothing here needed a fix. It is here because nothing tested it, and a gap that closed as a
//! side effect of another unit is the kind that reopens quietly. Measured in
//! `tests/captures/pg19_nested_function.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE nposts (id int8, author_id int4, title text)",
    "INSERT INTO nposts VALUES (1,2,'  bb  '),(2,1,' a '),(3,1,'ccc')",
];

/// **The two statements the suite sends**, both of them a call inside a call.
#[test]
fn the_suites_two_nested_calls_answer() {
    let mut node = parity::Node::new(FIXTURE);
    // `Post.order("author_id, length(trim(title))").pluck(:id)` — two sort keys, the second a
    // nested call, and the ids come back in that order.
    assert_eq!(
        node.rows("SELECT id FROM nposts ORDER BY author_id, length(trim(title))"),
        vec![vec!["2"], vec!["3"], vec!["1"]]
    );
    // `Post.pluck("length(trim(title))")` — the same expression as the whole target list.
    assert_eq!(
        node.rows("SELECT length(trim(title)) FROM nposts ORDER BY 1"),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
}

/// The nesting is not a mechanism of its own: it is `lower_expr` recursing, so any pair composes.
#[test]
fn a_call_inside_a_call_composes_in_both_directions() {
    let mut node = parity::Node::new(FIXTURE);
    for (statement, expected) in [
        ("SELECT upper(trim(title)) FROM nposts ORDER BY 1", "A"),
        ("SELECT trim(upper(title)) FROM nposts ORDER BY 1", "A"),
        (
            "SELECT length(trim(both ' ' from title)) FROM nposts ORDER BY 1",
            "1",
        ),
        ("SELECT length(btrim(title)) FROM nposts ORDER BY 1", "1"),
        // Three deep, which is no different from two.
        (
            "SELECT length(upper(trim(title))) FROM nposts ORDER BY 1",
            "1",
        ),
        (
            "SELECT abs(length(trim(title))) FROM nposts ORDER BY 1",
            "1",
        ),
    ] {
        assert_eq!(
            node.rows(statement).first().map(Vec::as_slice),
            Some([expected.to_owned()].as_slice()),
            "{statement}"
        );
    }
}

/// **A `trim` call is named `btrim`**, which is the function it resolves to and not the word the
/// user wrote — measured, and the reason the outer call in the suite's statement is what names the
/// column.
#[test]
fn the_column_is_named_for_the_function_that_ran() {
    let mut node = parity::Node::new(FIXTURE);
    let outcome = node
        .run("SELECT trim(title), length(trim(title)) FROM nposts")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].name, "btrim");
    assert_eq!(fields[1].name, "length");
    // 25 is `text` and 23 is `integer`: the outer call's type, not the inner one's.
    assert_eq!(fields[0].type_oid, 25);
    assert_eq!(fields[1].type_oid, 23);
}
