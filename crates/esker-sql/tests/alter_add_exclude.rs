//! **`ALTER TABLE … ADD CONSTRAINT … EXCLUDE`, which is the only `EXCLUDE` form still refused.**
//!
//! `exclusion_constraint_test.rb` sends one shape, eight times:
//!
//! ```ruby
//! add_exclusion_constraint :invoices, "daterange(start_date, end_date) WITH &&", using: :gist
//! ```
//!
//! with `deferrable:` varying. The same constraint written into a `CREATE TABLE` already works —
//! the expression key, `USING gist` and the `&&` operator are all built — so what was missing is
//! the `ALTER` spelling, which `sqlparser` 0.62.0 cannot read and which therefore reached the
//! refusal table.
//!
//! # Measured on 19beta1
//!
//! ```text
//! ALTER TABLE t ADD CONSTRAINT c EXCLUDE USING gist (daterange(start_date, end_date) WITH &&)
//!   on a table whose rows do not conflict   ok
//!   on a table whose rows do conflict       23P01 could not create exclusion constraint "c"
//!     DETAIL: Key (daterange(start_date, end_date))=([2020-01-01,2020-06-01)) conflicts with
//!             key (daterange(start_date, end_date))=([2020-03-01,2020-09-01)).
//! ```
//!
//! **That is a different sentence from the write-time one** — `conflicting key value violates
//! exclusion constraint` — and the `DETAIL` says "conflicts with key" where the write-time one
//! says "existing key". A constraint added `DEFERRABLE INITIALLY DEFERRED` is validated at once
//! all the same, measured: deferral is about later writes, not about the rows already there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &["CREATE TABLE invoices (id int8, start_date date, end_date date)"];

const ADD: &str = "ALTER TABLE invoices ADD CONSTRAINT c1 EXCLUDE USING gist \
                   (daterange(start_date, end_date) WITH &&)";

/// The suite's statement, and then the constraint actually holding.
#[test]
fn the_alter_form_adds_a_working_constraint() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(ADD).unwrap();
    node.run("INSERT INTO invoices VALUES (1, '2020-01-01', '2020-06-01')")
        .unwrap();
    let answer = node
        .answer("INSERT INTO invoices VALUES (2, '2020-03-01', '2020-09-01')")
        .to_string();
    assert!(
        answer.starts_with("!23P01 conflicting key value violates exclusion constraint \"c1\""),
        "the added constraint must be enforced on write: {answer}"
    );
    // And a row that does not overlap is fine.
    node.run("INSERT INTO invoices VALUES (3, '2021-01-01', '2021-06-01')")
        .unwrap();
}

/// The three `deferrable:` spellings the file sends, each accepted.
#[test]
fn the_deferrable_spellings_are_accepted() {
    for tail in [
        "",
        " DEFERRABLE INITIALLY DEFERRED",
        " DEFERRABLE INITIALLY IMMEDIATE",
        " NOT DEFERRABLE",
    ] {
        let mut node = parity::Node::new(FIXTURE);
        let sql = format!("{ADD}{tail}");
        assert_eq!(
            node.answer(&sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
}

/// **Existing rows are validated, and the sentence is the one a real server gives for *creating*
/// the constraint** — not the one it gives for writing a row.
#[test]
fn rows_that_already_conflict_refuse_the_constraint() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO invoices VALUES (1, '2020-01-01', '2020-06-01')")
        .unwrap();
    node.run("INSERT INTO invoices VALUES (2, '2020-03-01', '2020-09-01')")
        .unwrap();
    let answer = node.answer(ADD).to_string();
    assert!(
        answer.starts_with("!23P01 could not create exclusion constraint \"c1\""),
        "expected the creation-time sentence, got {answer}"
    );
    // The constraint must not be half-added: the table still takes an overlapping row.
    node.run("INSERT INTO invoices VALUES (3, '2020-04-01', '2020-05-01')")
        .unwrap();
}

/// `DROP CONSTRAINT` removes it again, which is what `test_remove_exclusion_constraint` needs.
#[test]
fn the_constraint_can_be_dropped_again() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(ADD).unwrap();
    node.run("ALTER TABLE invoices DROP CONSTRAINT c1").unwrap();
    // With it gone, two overlapping rows are legal.
    node.run("INSERT INTO invoices VALUES (1, '2020-01-01', '2020-06-01')")
        .unwrap();
    node.run("INSERT INTO invoices VALUES (2, '2020-03-01', '2020-09-01')")
        .unwrap();
}
