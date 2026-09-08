//! **`CREATE UNLOGGED TABLE … (…, EXCLUDE …)` — two shims that had to compose and did not.**
//!
//! `postgresql_specific_schema.rb` builds `test_exclusion_constraints` with three inline exclusion
//! constraints, and the whole of `hstore_test` died on it:
//!
//! ```text
//! CREATE UNLOGGED TABLE "test_exclusion_constraints" (… EXCLUDE …)
//!   syntax error: Expected: PRIMARY, UNIQUE, FOREIGN, or CHECK, found: EXCLUDE
//! ```
//!
//! Neither half was missing. `sqlparser` 0.62.0 reads no inline `EXCLUDE` and no `UNLOGGED`, and
//! this crate has carried a shim for each since long before: `strip_unlogged` takes the keyword off
//! and `strip_exclude_constraints` takes the clauses off, both in the one pre-parse rewrite site,
//! both with the clause text travelling on `Parsed`.
//!
//! **The bug was the `or_else` between them.** The site chains its rewrites with `or_else`, which
//! takes the *first* that matches — right for alternatives, since no statement is both a
//! `CREATE DATABASE` and a `CREATE DOMAIN`, and wrong for these two, because this statement is
//! both. `UNLOGGED` came off, the `EXCLUDE` clauses stayed, and the parser stopped on what was
//! left. The same statement **without** `UNLOGGED` had always worked, which is what said the clause
//! reader was fine and only the composition was not.
//!
//! # Measured on 19beta1
//!
//! ```text
//! g1x_overlap   EXCLUDE USING gist (daterange(start_date, end_date) WITH &&)
//!                 WHERE (((start_date IS NOT NULL) AND (end_date IS NOT NULL)))
//! g1x_deferred  EXCLUDE USING gist (daterange(start_date, end_date) WITH &&)
//!                 WHERE ((start_date IS NOT NULL)) DEFERRABLE INITIALLY DEFERRED
//! relpersistence                                                              u
//! ```
//!
//! Three parentheses around the compound predicate and two around the simple one — PostgreSQL
//! parenthesises each operand of a boolean chain, wraps that, and `pg_get_constraintdef` wraps it
//! again (`catalog::pg_constraint::exclude_definition`). And `relpersistence` is still `u`: the
//! keyword the first shim took off must reach the table, or the composition has traded one loss
//! for another.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The suite's own table, with the two constraint shapes it uses.
const FIXTURE: &[&str] = &[
    "CREATE UNLOGGED TABLE \"g1x_t\" (\"id\" bigserial primary key, \"start_date\" date, \
     \"end_date\" date, \
     CONSTRAINT \"g1x_overlap\" EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) \
     WHERE (start_date IS NOT NULL AND end_date IS NOT NULL), \
     CONSTRAINT \"g1x_deferred\" EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) \
     WHERE (start_date IS NOT NULL) DEFERRABLE INITIALLY DEFERRED)",
];

/// **The statement parses, and both constraints read back the way a real server prints them.**
#[test]
fn an_unlogged_table_takes_its_inline_exclude_constraints() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'g1x_t'::regclass AND contype = 'x' ORDER BY 1"
        ),
        [
            [
                "g1x_deferred".to_owned(),
                "EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) \
                 WHERE ((start_date IS NOT NULL)) DEFERRABLE INITIALLY DEFERRED"
                    .to_owned()
            ],
            [
                "g1x_overlap".to_owned(),
                "EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) \
                 WHERE (((start_date IS NOT NULL) AND (end_date IS NOT NULL)))"
                    .to_owned()
            ],
        ]
    );
}

/// **And it is still unlogged.** The first shim's keyword has to survive the second shim's rewrite,
/// which is the half a fix that simply re-ordered the chain would have lost.
#[test]
fn the_table_is_still_unlogged() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT relpersistence FROM pg_class WHERE relname = 'g1x_t'"),
        [["u"]]
    );
}

/// The constraint **enforces**, which is what says the clauses reached the catalog rather than
/// being parsed and dropped.
#[test]
fn the_constraint_still_excludes() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "INSERT INTO g1x_t (start_date, end_date) VALUES (DATE '2020-01-01', DATE '2020-02-01')",
    )
    .unwrap();
    assert!(
        node.answer(
            "INSERT INTO g1x_t (start_date, end_date) VALUES (DATE '2020-01-15', DATE '2020-03-01')"
        )
        .to_string()
        .starts_with("!23P01"),
        "an overlapping range must be refused"
    );
}

/// The two spellings agree, which is the assertion the bug lived between: a `LOGGED` table's inline
/// `EXCLUDE` worked all along, and only the pair failed.
#[test]
fn the_logged_and_unlogged_spellings_agree() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1x_logged (a date, b date, \
         CONSTRAINT g1x_l EXCLUDE USING gist (daterange(a, b) WITH &&))",
        "CREATE UNLOGGED TABLE g1x_unlogged (a date, b date, \
         CONSTRAINT g1x_u EXCLUDE USING gist (daterange(a, b) WITH &&))",
    ]);
    let definition = |node: &mut parity::Node, name: &str| {
        node.rows(&format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{name}'"
        ))
    };
    assert_eq!(
        definition(&mut node, "g1x_l"),
        definition(&mut node, "g1x_u")
    );
    assert_eq!(
        definition(&mut node, "g1x_l"),
        [["EXCLUDE USING gist (daterange(a, b) WITH &&)"]]
    );
}
