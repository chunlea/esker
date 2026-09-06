//! **`ON CONFLICT (…) WHERE <predicate>` — the partial-index form, which selects the index.**
//!
//! `insert_all(unique_by: :index_name)` over a partial unique index repeats that index's `where:`
//! in the statement, and `sqlparser` 0.62.0 stops at the keyword:
//!
//! ```text
//! test_insert_all_and_upsert_all_with_index_finding_options
//!   INSERT INTO "books" (…) ON CONFLICT ("isbn") WHERE (published_on IS NOT NULL)
//!     DO NOTHING RETURNING "id"
//!   PG::SyntaxError: Expected: DO, found: WHERE at Line: 1, Column: 180
//! test_upsert_all_does_not_perform_an_upsert_if_a_partial_index_doesnt_apply
//!   … DO UPDATE SET updated_at=(CASE …)     -- same clause, same stop
//! ```
//!
//! `0.62.0` is the latest release, so this is not a version away: the parser expects `DO` right
//! after the target list and its `OnConflict` has no field for a predicate. The clause comes off
//! the source in the one pre-parse rewrite site (`parse::strip_on_conflict_predicate`, beside the
//! seven shims already there) and is re-attached where the statement is lowered.
//!
//! # Measured on 19beta1
//!
//! ```text
//! CREATE UNIQUE INDEX g1oc_partial ON g1oc_books (isbn) WHERE published_on IS NOT NULL;
//!
//! ON CONFLICT ("isbn") WHERE (published_on IS NOT NULL) DO NOTHING   second row: INSERT 0 0
//! ON CONFLICT ("isbn") WHERE (published_on IS NOT NULL) DO UPDATE    updates the row already there
//! ON CONFLICT ("isbn")                       (no predicate)          42P10
//! ON CONFLICT ("isbn") WHERE (title IS NOT NULL)  (no such index)    42P10
//! ON CONFLICT (isbn) WHERE published_on IS NOT NULL                  matches — the parentheses
//!                                                                    ActiveRecord adds are not
//!                                                                    part of the predicate
//! two rows with isbn 'b' and published_on NULL                       both inserted; the index
//!                                                                    admits neither, so neither
//!                                                                    is a conflict
//! ```
//!
//! **The bare-target `42P10` is the point of one of the three tests**
//! (`…_does_not_perform_an_upsert_if_a_partial_index_doesnt_apply`), so widening the inference to
//! take a partial index for a target that did *not* name its predicate would turn a passing
//! refusal into a wrong answer.
//!
//! # What the matching is, and is not
//!
//! PostgreSQL infers an index whose predicate is **implied by** the statement's, so `WHERE a > 5`
//! may select an index `WHERE a > 0`. There is no implication machinery in this crate, so
//! `exec::dml::predicate_matches` compares the text after the two differences one predicate's two
//! spellings actually have — an enclosing pair of parentheses, and runs of whitespace. Every index
//! it infers is one a real server would infer; the ones it misses are `42P10` rather than a wrong
//! answer, which is the direction ADR 0031 asks for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `books`, with the partial unique index `insert_all(unique_by:)` names.
const FIXTURE: &[&str] = &[
    "CREATE TABLE g1oc_books (id bigserial primary key, isbn text, published_on date, title text)",
    "CREATE UNIQUE INDEX g1oc_partial ON g1oc_books (isbn) WHERE published_on IS NOT NULL",
];

/// The statement `insert_all` sends, verbatim in shape: target, predicate, `DO NOTHING`,
/// `RETURNING`.
#[test]
fn the_partial_index_form_parses_and_infers_that_index() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "INSERT INTO g1oc_books (isbn, published_on, title) VALUES ('a', DATE '2020-01-01', 'first') \
             ON CONFLICT (\"isbn\") WHERE (published_on IS NOT NULL) DO NOTHING RETURNING id"
        ),
        [["1"]]
    );
    // The second row conflicts, so `RETURNING` answers for no row at all.
    assert!(
        node.rows(
            "INSERT INTO g1oc_books (isbn, published_on, title) VALUES ('a', DATE '2020-01-01', 'second') \
             ON CONFLICT (\"isbn\") WHERE (published_on IS NOT NULL) DO NOTHING RETURNING id"
        )
        .is_empty()
    );
    assert_eq!(node.rows("SELECT count(*) FROM g1oc_books"), [["1"]]);
}

/// The same clause with `DO UPDATE`, which is the other half of `upsert_all`.
#[test]
fn the_do_update_form_updates_the_row_already_there() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "INSERT INTO g1oc_books (isbn, published_on, title) VALUES ('a', DATE '2020-01-01', 'first')",
    )
    .unwrap();
    assert_eq!(
        node.rows(
            "INSERT INTO g1oc_books (isbn, published_on, title) VALUES ('a', DATE '2020-01-01', 'third') \
             ON CONFLICT (\"isbn\") WHERE (published_on IS NOT NULL) DO UPDATE SET title = excluded.title \
             RETURNING id, title"
        ),
        [["1".to_owned(), "third".to_owned()]]
    );
    assert_eq!(node.rows("SELECT count(*) FROM g1oc_books"), [["1"]]);
}

/// **A bare target over a partial index is still `42P10`**, which is what one of the three Rails
/// tests asserts. A fix that inferred the index anyway would turn that pass into a wrong answer.
#[test]
fn a_bare_target_over_a_partial_index_is_still_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "INSERT INTO g1oc_books (isbn, published_on) VALUES ('a', DATE '2020-01-01') \
             ON CONFLICT (\"isbn\") DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}

/// A predicate no index has is the same refusal, not a silently ignored clause.
#[test]
fn a_predicate_no_index_has_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "INSERT INTO g1oc_books (isbn, published_on) VALUES ('a', DATE '2020-01-01') \
             ON CONFLICT (\"isbn\") WHERE (title IS NOT NULL) DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}

/// **The parentheses `ActiveRecord` writes are not part of the predicate.** Measured: the index is
/// declared without them and the statement sends them, and a real server matches the two.
#[test]
fn the_predicate_matches_with_or_without_the_parentheses() {
    for clause in [
        "WHERE (published_on IS NOT NULL)",
        "WHERE published_on IS NOT NULL",
        "WHERE  ( published_on   IS NOT NULL )",
    ] {
        let mut node = parity::Node::new(FIXTURE);
        node.run("INSERT INTO g1oc_books (isbn, published_on) VALUES ('a', DATE '2020-01-01')")
            .unwrap();
        node.run(&format!(
            "INSERT INTO g1oc_books (isbn, published_on) VALUES ('a', DATE '2020-01-01') \
             ON CONFLICT (isbn) {clause} DO NOTHING"
        ))
        .unwrap_or_else(|error| panic!("{clause}: {error}"));
        assert_eq!(
            node.rows("SELECT count(*) FROM g1oc_books"),
            [["1"]],
            "{clause}"
        );
    }
}

/// **A row the index does not admit is not a conflict.** Two books with no publication date share
/// an ISBN happily, because the partial index holds an entry for neither — measured, and the half
/// that makes a partial index worth inferring at all.
#[test]
fn a_row_the_predicate_excludes_is_not_a_conflict() {
    let mut node = parity::Node::new(FIXTURE);
    for title in ["one", "two"] {
        node.run(&format!(
            "INSERT INTO g1oc_books (isbn, title) VALUES ('b', '{title}') \
             ON CONFLICT (\"isbn\") WHERE (published_on IS NOT NULL) DO NOTHING"
        ))
        .unwrap();
    }
    assert_eq!(node.rows("SELECT count(*) FROM g1oc_books"), [["2"]]);
}

/// **The shim's boundary, stated as a test.** A column called `"DO"` sits inside the predicate;
/// the clause's own `DO` is the one outside quotes, and cutting at the wrong one would leave the
/// statement unparseable or the predicate truncated.
#[test]
fn a_quoted_do_inside_the_predicate_is_not_the_clauses() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1oc_odd (id bigserial primary key, isbn text, \"DO\" text)",
        "CREATE UNIQUE INDEX g1oc_odd_ix ON g1oc_odd (isbn) WHERE \"DO\" IS NOT NULL",
    ]);
    node.run("INSERT INTO g1oc_odd (isbn, \"DO\") VALUES ('a', 'x')")
        .unwrap();
    node.run(
        "INSERT INTO g1oc_odd (isbn, \"DO\") VALUES ('a', 'y') \
         ON CONFLICT (isbn) WHERE (\"DO\" IS NOT NULL) DO NOTHING",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1oc_odd"), [["1"]]);
}

/// A statement with **no** predicate still infers only an index with none — the rule that was here
/// before this clause could parse, unchanged.
#[test]
fn a_statement_with_no_predicate_still_takes_the_index_with_none() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1oc_two (id bigserial primary key, isbn text, published_on date)",
        "CREATE UNIQUE INDEX g1oc_two_full ON g1oc_two (isbn)",
        "CREATE UNIQUE INDEX g1oc_two_part ON g1oc_two (published_on) WHERE isbn IS NOT NULL",
    ]);
    node.run("INSERT INTO g1oc_two (isbn, published_on) VALUES ('a', DATE '2020-01-01')")
        .unwrap();
    node.run(
        "INSERT INTO g1oc_two (isbn, published_on) VALUES ('a', DATE '2021-01-01') \
         ON CONFLICT (isbn) DO NOTHING",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM g1oc_two"), [["1"]]);
    // …and the partial one is only reachable by repeating its predicate.
    assert_eq!(
        node.answer(
            "INSERT INTO g1oc_two (isbn, published_on) VALUES ('b', DATE '2020-01-01') \
             ON CONFLICT (published_on) DO NOTHING"
        )
        .to_string(),
        "!42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
}
