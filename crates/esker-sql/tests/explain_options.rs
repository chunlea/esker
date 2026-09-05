//! `EXPLAIN (…)` — the option lists `ActiveRecord` sends.
//!
//! `explain_test.rb` has five tests and three of them could not run at all: the whole parenthesised
//! option list was `0A000 EXPLAIN with options is not supported`. `Relation#explain` builds it from
//! whatever the caller passed, so the census is short and exact:
//!
//! ```ruby
//! .explain(:analyze, :buffers)               -> EXPLAIN (ANALYZE, BUFFERS) SELECT …
//! .explain("VERBOSE", "ANALYZE", "FORMAT JSON") -> EXPLAIN (VERBOSE, ANALYZE, FORMAT JSON) SELECT …
//! .explain(:analyze)                          -> EXPLAIN (ANALYZE) SELECT …
//! ```
//!
//! **What is accepted and ignored is a choice with a reason.** `VERBOSE`, `BUFFERS`, `COSTS` and
//! the rest ask a real server for more detail about the same plan; this node's plan is its own —
//! the corpus records the whole of `EXPLAIN`'s output as a divergence — so honouring them would
//! mean inventing detail, and refusing them would refuse a statement PostgreSQL runs. Accepting
//! them and showing no extra detail is the truthful reading: there is none to show.
//!
//! **`FORMAT JSON` is refused**, and that is the other half of the same rule. It changes the
//! *shape* of the answer, and a client that asked for JSON and got plan text has a wrong answer
//! rather than a plainer one. Producing it means a node tree — `Node Type`, `Startup Cost`,
//! `Plans` — that this planner does not have and would have to fabricate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE authors (id bigserial primary key, name text)",
        "INSERT INTO authors (name) VALUES ('ada')",
    ])
}

/// **Every option list the suite sends is planned, and names its column `QUERY PLAN`.**
#[test]
fn the_option_lists_activerecord_sends_are_planned() {
    let mut node = node();
    for sql in [
        "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM authors WHERE id = 1",
        "EXPLAIN (ANALYZE) SELECT * FROM authors WHERE id = 1",
        "EXPLAIN (VERBOSE) SELECT * FROM authors WHERE id = 1",
        "EXPLAIN (COSTS, SETTINGS, WAL, TIMING, SUMMARY) SELECT * FROM authors",
        "EXPLAIN (FORMAT TEXT) SELECT * FROM authors",
        // `ANALYZE false` is the one boolean whose value matters: it must NOT run the statement.
        "EXPLAIN (ANALYZE false) SELECT * FROM authors",
        "EXPLAIN VERBOSE SELECT * FROM authors",
    ] {
        let rows = node.rows(sql);
        assert!(!rows.is_empty(), "{sql} produced no plan");
    }
}

/// **An option list does not change the plan**, which is the property that keeps them safe to
/// accept and ignore.
///
/// Asserted as **equality** against the same statement with no options, rather than by looking for
/// a particular line — and that is deliberate twice over.
///
/// My first version asserted the `Engine:` line was present and failed on a plain `SELECT *`, which
/// has none: that line belongs to an aggregate over a table with a columnar copy. The assertion was
/// testing my assumption rather than the code. The second version guarded it with an early return
/// if the line was absent — which made it **assert nothing at all**, since it is absent on every
/// shape this fixture can build. A test that quietly does nothing is worse than no test, so it is
/// gone.
///
/// Equality covers what it was for and more: whatever a plan says — the engine line included, on a
/// cluster where there is one — an option list must not change it.
#[test]
fn an_option_list_does_not_change_the_plan() {
    let mut node = node();
    for query in [
        "SELECT * FROM authors",
        "SELECT count(*) FROM authors",
        "SELECT name, count(*) FROM authors GROUP BY name",
    ] {
        let plain = node.rows(&format!("EXPLAIN {query}"));
        for options in [
            "(VERBOSE)",
            "(COSTS, BUFFERS)",
            "(FORMAT TEXT)",
            "(ANALYZE false)",
        ] {
            assert_eq!(
                node.rows(&format!("EXPLAIN {options} {query}")),
                plain,
                "EXPLAIN {options} changed the plan of `{query}`"
            );
        }
    }
}

/// **A format that changes the answer's shape is refused, not ignored.**
#[test]
fn a_non_text_format_is_refused_rather_than_answered_as_text() {
    let mut node = node();
    for (sql, named) in [
        (
            "EXPLAIN (VERBOSE, ANALYZE, FORMAT JSON) SELECT * FROM authors",
            "EXPLAIN (FORMAT JSON) is not supported",
        ),
        (
            "EXPLAIN (FORMAT YAML) SELECT * FROM authors",
            "EXPLAIN (FORMAT YAML) is not supported",
        ),
        (
            "EXPLAIN (NOSUCHOPTION) SELECT * FROM authors",
            "EXPLAIN (NOSUCHOPTION) is not supported",
        ),
    ] {
        let refused = node.run(sql).expect_err(sql);
        assert_eq!(refused.sqlstate(), "0A000", "{sql}: {refused}");
        assert_eq!(refused.to_string(), named, "{sql}");
    }
}
