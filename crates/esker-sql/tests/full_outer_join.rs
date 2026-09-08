//! `FULL OUTER JOIN` — **the union of the two null-extensions.**
//!
//! `update_all_test.rb` sends `SELECT 1 AS one FROM "pets" FULL OUTER JOIN toys ON … LIMIT 1` and
//! got `0A000 a FULL JOIN is not supported`.
//!
//! Measured on PostgreSQL 19 over `pets (1 dog, 2 cat)` and `toys (10→1 ball, 11→99 orphan)`:
//!
//! ```text
//! p.id p.name t.id t.name        pets FULL JOIN toys ON t.pet_id = p.id
//!   1  dog     10  ball
//!   2  cat      -  -             the outer row that matched nothing
//!   -  -       11  orphan        the INNER row that matched nothing  <- the new half
//!
//! FULL JOIN ON false                 -> 4 rows, every one extended
//! … WHERE p.id IS NULL               -> 1, so a WHERE above it still filters
//! empty FULL JOIN toys               -> 2, both toys, outer columns NULL
//! pets  FULL JOIN empty              -> 2, both pets, inner columns NULL
//! ```
//!
//! **The third row is what makes this more than a flag.** A left join can decide an outer row's
//! fate as it goes; an inner row's fate is not known until *every* outer row has been tried
//! against it, so the inner side has to be held and remembered. That is why a full join gives up
//! its probe — a probe answers "which inner row matches this outer row" and never "which inner
//! rows matched nobody" — and why the empty-outer cases are in this file: they are the ones with
//! no outer row to learn the row width from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE pets (id bigint, name varchar)",
    "CREATE TABLE toys (id bigint, pet_id bigint, name varchar)",
    "CREATE TABLE nothing (id bigint)",
    "INSERT INTO pets VALUES (1,'dog'), (2,'cat')",
    "INSERT INTO toys VALUES (10,1,'ball'), (11,99,'orphan')",
];

fn rows(node: &mut parity::Node, sql: &str) -> Vec<Vec<String>> {
    node.rows(sql)
}

#[test]
fn the_statement_the_suite_sends() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        rows(
            &mut node,
            "SELECT 1 AS one FROM pets FULL OUTER JOIN toys ON toys.pet_id = pets.id LIMIT 1"
        ),
        vec![vec!["1".to_owned()]]
    );
}

#[test]
fn both_sides_keep_the_rows_that_matched_nothing() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        rows(
            &mut node,
            "SELECT p.id, p.name, t.id, t.name FROM pets p FULL OUTER JOIN toys t \
             ON t.pet_id = p.id ORDER BY p.id NULLS LAST, t.id"
        ),
        vec![
            vec![
                "1".to_owned(),
                "dog".to_owned(),
                "10".to_owned(),
                "ball".to_owned()
            ],
            vec![
                "2".to_owned(),
                "cat".to_owned(),
                "\\N".to_owned(),
                "\\N".to_owned()
            ],
            vec![
                "\\N".to_owned(),
                "\\N".to_owned(),
                "11".to_owned(),
                "orphan".to_owned()
            ],
        ]
    );
}

#[test]
fn a_condition_that_never_holds_keeps_every_row_of_both_sides() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        rows(
            &mut node,
            "SELECT count(*) FROM pets FULL OUTER JOIN toys ON false"
        ),
        vec![vec!["4".to_owned()]]
    );
}

/// **A `WHERE` above the join still filters**, which is the same separation a left join has: the
/// `ON` decides what is kept and extended, and the `WHERE` runs over what came out.
#[test]
fn a_where_above_the_join_filters_the_extended_rows() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        rows(
            &mut node,
            "SELECT count(*) FROM pets p FULL OUTER JOIN toys t ON t.pet_id = p.id \
             WHERE p.id IS NULL"
        ),
        vec![vec!["1".to_owned()]]
    );
}

/// The two cases with nothing to learn a row's width from — the reason the width is carried on the
/// plan rather than taken from the first outer row.
#[test]
fn an_empty_side_still_produces_the_others_rows() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        rows(
            &mut node,
            "SELECT n.id, t.id FROM nothing n FULL OUTER JOIN toys t ON t.pet_id = n.id \
             ORDER BY t.id"
        ),
        vec![
            vec!["\\N".to_owned(), "10".to_owned()],
            vec!["\\N".to_owned(), "11".to_owned()],
        ]
    );
    assert_eq!(
        rows(
            &mut node,
            "SELECT p.id, n.id FROM pets p FULL OUTER JOIN nothing n ON n.id = p.id ORDER BY p.id"
        ),
        vec![
            vec!["1".to_owned(), "\\N".to_owned()],
            vec!["2".to_owned(), "\\N".to_owned()],
        ]
    );
}

/// `USING` is refused **only** on a full join, and the reason is the row the merge has no answer
/// for: where the left side is missing, the merged value is the right's. PostgreSQL spells that
/// `COALESCE`; taking the left value would answer NULL for every row the new half keeps.
#[test]
fn a_full_join_with_using_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.run("SELECT * FROM pets FULL OUTER JOIN toys USING (id)")
            .unwrap_err()
            .to_string(),
        "a FULL JOIN with USING is not supported"
    );
    // And the same clause on the joins that can merge it is untouched.
    assert!(
        node.run("SELECT * FROM pets LEFT JOIN toys USING (id)")
            .is_ok()
    );
}
