//! Contract C3 for a **table alias**, and the two errors that exist because an alias replaces a
//! name rather than adding one.
//!
//! Forty-two statements put to a real PostgreSQL 19beta1 in one session and replayed against one
//! node. The trap the corpus exists for is at the top of the file: after `FROM al AS t`, `al.id`
//! is `42P01` — and a *different* `42P01` from a qualifier the query never had.
//!
//! Why it matters here and not as a nicety: nineteen of the thirty-six statements `ActiveRecord`
//! sends open `FROM pg_type AS t` or `FROM pg_class c`, including the first one it ever sends
//! (`docs/plans/phase-9-rails.md` §2, unit 5). Every one of them is behind this before its catalog
//! is worth building.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// The same two tables the corpus builds, for the tests that are not a replay.
const FIXTURE: &[&str] = &[
    "CREATE TABLE al (id int8 PRIMARY KEY, n text, k int8)",
    "CREATE TABLE ar (id int8 PRIMARY KEY, m text)",
    "INSERT INTO al VALUES (1, 'one', 7), (2, 'two', NULL)",
    "INSERT INTO ar VALUES (1, 'uno'), (3, 'tres')",
];

/// What this node answers differently, and why. Both are features the alias brought within reach
/// and neither is one: they are refused by name, which is contract C2, and counted here so that
/// building one cannot be absorbed silently.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT c FROM al AS t (c) ORDER BY c",
            "a column alias list renames the columns as well, which is a second feature: after it \
             the table's own names are gone. Refused by name rather than ignored, because \
             ignoring it would answer a query about `c` with a column called `id`.",
        ),
        (
            "SELECT c, d FROM al AS t (c, d) ORDER BY c",
            "a column alias list, as above.",
        ),
        (
            "SELECT t.id FROM al AS t (c)",
            "a column alias list, as above. PostgreSQL's answer here is `42703` — the column is \
             gone, not the feature — which is the clearest statement of what the clause does.",
        ),
        (
            "DELETE FROM al AS t WHERE t.id = 2",
            "an alias on a DELETE, as above.",
        ),
    ],
};

#[test]
fn every_alias_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(
        include_str!("corpus/pg19_alias.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The trap, on its own: an alias **replaces** the name, and the two ways of getting a qualifier
/// wrong are different mistakes with different sentences.
///
/// One is a table nobody put in the query. The other is a table that is right there under a name
/// the user did not write — and answering the first for both would tell them to add a `FROM` entry
/// they already have.
#[test]
fn an_alias_replaces_the_name_and_says_so() {
    let mut node = parity::Node::new(FIXTURE);

    let error = node.run("SELECT al.id FROM al AS t").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        error.to_string(),
        "invalid reference to FROM-clause entry for table \"al\""
    );
    assert_eq!(
        error.hint().as_deref(),
        Some("Perhaps you meant to reference the table alias \"t\".")
    );

    let error = node.run("SELECT u.id FROM al AS t").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        error.to_string(),
        "missing FROM-clause entry for table \"u\""
    );
    assert_eq!(error.hint(), None);
}

/// Two FROM entries under one name is `42712`, and the check is over the name each is **referred
/// as** rather than over the table it names.
///
/// The regression this pins is the silent one: without the check, `SELECT al.id FROM al JOIN al`
/// resolves to the outer side and a self-join answers with one table's column twice, no error.
#[test]
fn two_from_entries_may_not_share_a_name() {
    let mut node = parity::Node::new(FIXTURE);

    for statement in [
        "SELECT al.id FROM al JOIN al ON true",
        "SELECT a.id FROM al JOIN al AS al ON true",
        "SELECT x.id FROM al AS x JOIN ar AS x ON x.id = x.id",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_ALIAS, "{statement}");
        assert!(
            error.to_string().ends_with("specified more than once"),
            "{statement}: {error}"
        );
    }

    // The alias freed the name, so this one is legal — which is what makes the check about the
    // referred-as name and not about the table.
    node.run("SELECT t.id FROM al AS t JOIN ar AS al ON t.id = al.id")
        .unwrap();

    // And a self-join under two aliases, which is the only self-join this crate's one join can do.
    node.run("SELECT a.id, b.id FROM al AS a JOIN al AS b ON a.id = b.id")
        .unwrap();
}

/// An alias renames the table for *every* clause, including the ones that build their own row
/// space above the scan. `GROUP BY` and `ORDER BY` were both wrong before the scope carried names:
/// they resolved against the table, and a `42803` printed a qualifier the user never typed.
#[test]
fn the_alias_is_the_name_every_clause_and_every_message_uses() {
    let mut node = parity::Node::new(FIXTURE);

    let error = node
        .run("SELECT t.n, count(*) FROM al AS t GROUP BY t.k")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::GROUPING_ERROR);
    assert_eq!(
        error.to_string(),
        "column \"t.n\" must appear in the GROUP BY clause or be used in an aggregate function"
    );

    node.run("SELECT t.id FROM al AS t ORDER BY t.k DESC")
        .unwrap();
    node.run("SELECT t.* FROM al AS t").unwrap();
    node.run("SELECT t.id FROM al AS t WHERE t.k = 7").unwrap();
}

/// A column alias list is refused **by name**, which is contract C2 — never a syntax error, and
/// never silently dropped, because dropping it would answer a query about `c` with `id`.
#[test]
fn a_column_alias_list_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node.run("SELECT c FROM al AS t (c)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(error.to_string(), "a column alias list is not supported");
}

/// `EXPLAIN` still names the **table**, because that is what is scanned. The alias is the query's
/// name for it and the plan is about the relation.
#[test]
fn explain_names_the_table_and_not_the_alias() {
    let mut node = parity::Node::new(FIXTURE);
    let plan = node.rows("EXPLAIN SELECT t.id FROM al AS t WHERE t.id = 1");
    assert!(
        plan.concat().concat().contains("al"),
        "the plan does not name the table: {plan:?}"
    );
}
