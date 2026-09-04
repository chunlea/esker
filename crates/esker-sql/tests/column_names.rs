//! What a target-list entry is called when nobody wrote `AS` — PostgreSQL 19's `FigureColname`,
//! measured one statement at a time with `\gdesc`.
//!
//! Run 75 stopped `PostgresqlGeometricTest#test_geometric_function` on it: the test reads its value
//! back **by the column's name**, and a bare `area(box '…')` was `?column?` here where a real server
//! says `area`. It is a naming rule rather than a geometry one, so this file covers the whole
//! family — functions, casts, operators, literals and the keyword-shaped calls — and the geometric
//! case is one row of it.
//!
//! **The parity corpora cannot catch this.** They compare the declared *types* and the rows, and a
//! column's name is neither. That is why the expectations here are a table of names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::session::Outcome;

/// The name of each column a statement answers with, or `None` when this node does not have the
/// construct at all.
///
/// **A refusal is not this unit's business and must not be its blind spot either.** Several of the
/// rows below are functions this node has not implemented, and deleting them would mean the day one
/// lands nobody checks what its column is called. So a refusal is accepted here and an *answer* is
/// always checked — the table stays complete and starts testing each name by itself.
fn names(node: &mut parity::Node, sql: &str) -> Option<Vec<String>> {
    match node.run(sql) {
        Ok(Outcome::Rows { fields, .. }) => {
            Some(fields.into_iter().map(|field| field.name).collect())
        }
        Ok(other) => panic!("{sql} answered {other:?}"),
        // `0A000`/`42883`: the construct is refused by name, which is contract C2 and someone
        // else's unit. Anything else is a real failure.
        Err(error) if matches!(error.sqlstate(), "0A000" | "42883" | "42601") => None,
        Err(error) => panic!("{sql}: {} {error}", error.sqlstate()),
    }
}

/// Every rule, measured against PostgreSQL 19 on 2026-09-04.
#[test]
fn a_target_list_entry_is_named_the_way_postgresql_names_it() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cn (a bigint, b text)",
        "INSERT INTO cn VALUES (1, 'x')",
    ]);
    let mut refused = Vec::new();
    let mut wrong = Vec::new();
    for (sql, expected) in [
        // A function is its own name, unqualified, and the **outermost** one wins.
        ("SELECT length('abc')", "length"),
        ("SELECT upper(lower('A'))", "upper"),
        ("SELECT pg_catalog.length('abc')", "length"),
        ("SELECT count(*) FROM cn", "count"),
        ("SELECT max(a) FROM cn", "max"),
        ("SELECT abs(-1)", "abs"),
        // The alias wins over everything.
        ("SELECT length('abc') AS n", "n"),
        // A column keeps its name, qualified or not.
        ("SELECT a FROM cn", "a"),
        ("SELECT cn.a FROM cn", "a"),
        // **A cast takes the argument's name when it has one, and the type's when it does not.**
        ("SELECT a::text FROM cn", "a"),
        ("SELECT (a + 1)::text FROM cn", "text"),
        // The keyword-shaped calls are named after the keyword, lower-cased.
        ("SELECT coalesce(a, 2) FROM cn", "coalesce"),
        ("SELECT nullif(a, 2) FROM cn", "nullif"),
        ("SELECT greatest(a, 2) FROM cn", "greatest"),
        ("SELECT CASE WHEN true THEN 1 ELSE 2 END", "case"),
        ("SELECT extract(year FROM date '2026-01-01')", "extract"),
        ("SELECT exists(SELECT 1)", "exists"),
        ("SELECT now()", "now"),
        ("SELECT current_date", "current_date"),
        ("SELECT substring('abc' FROM 2)", "substring"),
        // **`trim` is named after the function it really is**, not after the syntax that called it.
        ("SELECT trim(both from '  x  ')", "btrim"),
        // And everything else is `?column?`: literals, operators, tests and subqueries.
        ("SELECT 1", "?column?"),
        ("SELECT 'text'", "?column?"),
        ("SELECT NULL", "?column?"),
        ("SELECT 1 + 1", "?column?"),
        ("SELECT a + 1 FROM cn", "?column?"),
        ("SELECT 'a' || 'b'", "?column?"),
        ("SELECT a IS NULL FROM cn", "?column?"),
        ("SELECT -a FROM cn", "?column?"),
        ("SELECT (SELECT 1)", "?column?"),
    ] {
        match names(&mut node, sql) {
            Some(answered) if answered == vec![expected.to_owned()] => {}
            // **Collected, not asserted one at a time.** A table of rules is worth one run that
            // says which rules are wrong, not one that stops at the first.
            Some(answered) => {
                wrong.push(format!("{sql}: {answered:?}, PostgreSQL says {expected}"));
            }
            None => refused.push(sql),
        }
    }
    // Printed rather than asserted: what this node cannot yet run is another unit's number, and a
    // list that shrinks silently is worse than one that is read.
    println!("not implemented here, so unnamed: {refused:?}");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    // Two of the same function is two columns of the same name, which PostgreSQL allows.
    assert_eq!(
        names(&mut node, "SELECT abs(-1), abs(-2)"),
        Some(vec!["abs".to_owned(), "abs".to_owned()])
    );
}

/// **What is folded at plan time cannot be named after itself** — the one divergence this rule
/// leaves, declared with its cause rather than left to be rediscovered.
///
/// PostgreSQL names `1::text` after the target type, and `1::numeric(5,2)` after the type **without
/// its modifier**. This crate folds a cast over a literal **at plan time** — `1::text` becomes the
/// text literal `1` and there is no cast node left to read — so the name cannot be figured where
/// every other name in this file is figured, from the expression.
///
/// Closing it is a plan change rather than a rule change: either the lowering carries the figured
/// name beside the expression, or a folded cast keeps a marker saying what it was. Neither is worth
/// doing on a guess about which client needs it; nothing in the Rails suite reads a literal cast by
/// name, and what run 75 stopped on was a **function**, which this rule now names.
#[test]
fn a_cast_over_a_literal_is_the_one_name_this_node_does_not_figure() {
    let mut node = parity::Node::new(&[]);
    for (sql, postgresql) in [
        ("SELECT 1::text", "text"),
        ("SELECT CAST(1 AS text)", "text"),
        ("SELECT 1::numeric(5,2)", "numeric"),
        // The same cause with a different keyword: an array constructor over constants is folded
        // to its value too, so there is no `ARRAY[…]` left to name.
        ("SELECT ARRAY[1,2]", "array"),
    ] {
        assert_eq!(
            names(&mut node, sql),
            Some(vec!["?column?".to_owned()]),
            "{sql} is `{postgresql}` on PostgreSQL 19; if this line fails the divergence has \
             closed and the entry should go"
        );
    }
}
