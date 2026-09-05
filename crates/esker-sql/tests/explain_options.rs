//! `EXPLAIN (...)`: the option list, and the four documents `FORMAT` chooses between.
//!
//! # The four tests behind this
//!
//! `explain_test.rb` sends three option lists — `(ANALYZE, BUFFERS)`, `(ANALYZE)` and
//! `(VERBOSE, ANALYZE, FORMAT JSON)` — and asserts only that the statement was accepted and that a
//! `QUERY PLAN` column came back. `connection_test.rb`'s `test_statement_key_is_logged` is the one
//! that reads the answer: it looks the column's type up by OID and *deserializes* the document
//! through it, so a `json` document declared `text` would hand `ActiveRecord` a String where it
//! expected an Array. That is the shape of defect this file exists to catch, and a value-only
//! assertion cannot see it — which is exactly how the `->` operator's wrong type slipped through
//! in run 98.
//!
//! # What is measured and what is ours
//!
//! Every option name, every error message and every declared column type here was captured from
//! PostgreSQL 19beta1 (`tests/captures/pg19_explain_options.txt`). The plan *content* is this
//! server's own — there is no cost model here, so no `Total Cost` key — and the corpus carries
//! both sides.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Answer;

const FIXTURE: &[&str] = &[
    "CREATE TABLE authors (id int8 PRIMARY KEY, name text)",
    "CREATE TABLE posts (id int8 PRIMARY KEY, author_id int8, title text)",
    "INSERT INTO authors VALUES (1, 'a'), (2, 'b')",
];

fn answer(node: &mut parity::Node, sql: &str) -> Answer {
    node.answer(sql)
}

/// The declared type of the one column, and the document in it.
fn plan(node: &mut parity::Node, sql: &str) -> (String, Vec<String>) {
    match node.answer(sql) {
        Answer::Rows { types, rows } => (
            types.first().cloned().unwrap_or_default(),
            rows.into_iter().map(|row| row[0].clone()).collect(),
        ),
        other => panic!("{sql}: {other}"),
    }
}

fn refusal(node: &mut parity::Node, sql: &str) -> String {
    match node.answer(sql) {
        Answer::Refused(message) => message,
        other => panic!("{sql} was not refused: {other}"),
    }
}

/// The three statements `explain_test.rb` sends, each accepted and each answering `QUERY PLAN`.
#[test]
fn the_option_lists_rails_sends_are_accepted() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM authors WHERE id = 1",
        "EXPLAIN (ANALYZE) SELECT * FROM authors WHERE id = 1",
        "EXPLAIN (VERBOSE, ANALYZE, FORMAT JSON) SELECT * FROM authors WHERE id = 1",
    ] {
        let (ty, rows) = plan(&mut node, sql);
        assert!(!rows.is_empty(), "{sql} answered no rows");
        assert!(
            ty == "text" || ty == "json",
            "{sql} declared its column {ty}"
        );
    }
}

/// **The wire type is the format's**, asserted beside the value rather than instead of it.
///
/// `\gdesc` against 19beta1: `EXPLAIN` and `EXPLAIN (FORMAT TEXT)` declare `text`,
/// `(FORMAT JSON)` declares `json`, `(FORMAT XML)` declares `xml`, and `(FORMAT YAML)` declares
/// `text` — YAML is a layout, not a type.
#[test]
fn each_format_declares_its_own_column_type() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, expected) in [
        ("EXPLAIN SELECT * FROM authors", "text"),
        ("EXPLAIN (FORMAT TEXT) SELECT * FROM authors", "text"),
        ("EXPLAIN (FORMAT JSON) SELECT * FROM authors", "json"),
        ("EXPLAIN (FORMAT XML) SELECT * FROM authors", "xml"),
        ("EXPLAIN (FORMAT YAML) SELECT * FROM authors", "text"),
    ] {
        assert_eq!(plan(&mut node, sql).0, expected, "{sql}");
    }
}

/// A structured document is **one row**; the text layout is one row per line. A JSON document
/// split across rows would not parse, which is why this is a fact about the format and not about
/// the renderer.
#[test]
fn a_structured_format_answers_one_row() {
    let mut node = parity::Node::new(FIXTURE);
    for format in ["JSON", "XML", "YAML"] {
        let (_, rows) = plan(
            &mut node,
            &format!("EXPLAIN (FORMAT {format}) SELECT * FROM authors WHERE id = 1"),
        );
        assert_eq!(
            rows.len(),
            1,
            "FORMAT {format} answered {} rows",
            rows.len()
        );
    }
    let (_, text) = plan(&mut node, "EXPLAIN SELECT * FROM authors WHERE id = 1");
    assert!(text.len() > 1, "the text layout collapsed to one row");
}

/// What `test_statement_key_is_logged` does to the answer: parse it, and expect a non-empty array.
#[test]
fn the_json_document_is_an_array_of_one_plan() {
    let mut node = parity::Node::new(FIXTURE);
    let (ty, rows) = plan(&mut node, "EXPLAIN (FORMAT JSON) SELECT * FROM authors");
    assert_eq!(ty, "json");
    let document = &rows[0];
    assert!(
        document.starts_with("[\n  {\n    \"Plan\": {"),
        "{document}"
    );
    assert!(document.ends_with("\n  }\n]"), "{document}");
    assert!(
        document.contains("\"Node Type\": \"Seq Scan\""),
        "{document}"
    );
    assert!(
        document.contains("\"Relation Name\": \"authors\""),
        "{document}"
    );
    // **No cost keys, rather than zeroed ones**: this server has no cost model and says so by
    // omission (`crate::plan::explain`).
    assert!(!document.contains("Total Cost"), "{document}");
}

/// The subtree becomes `Plans`, nested, in all three structured formats.
#[test]
fn a_child_node_nests_under_plans() {
    let mut node = parity::Node::new(FIXTURE);
    let query = "SELECT name FROM authors WHERE name = 'a'";
    let (_, json) = plan(&mut node, &format!("EXPLAIN (FORMAT JSON) {query}"));
    assert!(json[0].contains("\"Plans\": ["), "{}", json[0]);
    let (_, xml) = plan(&mut node, &format!("EXPLAIN (FORMAT XML) {query}"));
    assert!(xml[0].contains("<Plans>"), "{}", xml[0]);
    assert!(xml[0].contains("<Node-Type>"), "{}", xml[0]);
    let (_, yaml) = plan(&mut node, &format!("EXPLAIN (FORMAT YAML) {query}"));
    assert!(yaml[0].starts_with("- Plan:\n"), "{}", yaml[0]);
    assert!(yaml[0].contains("Plans:\n"), "{}", yaml[0]);
}

/// A statement with no access path still has to answer in every format.
#[test]
fn a_statement_without_a_plan_still_renders() {
    let mut node = parity::Node::new(FIXTURE);
    let (ty, rows) = plan(
        &mut node,
        "EXPLAIN (FORMAT JSON) CREATE TABLE later (a int8)",
    );
    assert_eq!(ty, "json");
    assert!(
        rows[0].contains("\"Node Type\": \"Create Table\""),
        "{}",
        rows[0]
    );
    assert!(
        rows[0].contains("\"Relation Name\": \"later\""),
        "{}",
        rows[0]
    );
}

/// **A name outside `public`**, which is where a relation name that is resolved twice shows it.
/// The plan must name the relation the user wrote, not the stored composite key (ADR 0071).
#[test]
fn a_plan_names_a_relation_in_another_schema() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA other",
        "CREATE TABLE other.t (a int8 PRIMARY KEY)",
    ]);
    let (_, rows) = plan(&mut node, "EXPLAIN (FORMAT JSON) SELECT * FROM other.t");
    assert!(rows[0].contains("\"Relation Name\": \"t\""), "{}", rows[0]);
    assert!(!rows[0].contains('\u{0}'), "a stored name reached a client");
    let (_, text) = plan(&mut node, "EXPLAIN SELECT * FROM other.t");
    assert!(
        text.concat().contains("Seq Scan on t"),
        "{}",
        text.join(" / ")
    );
}

/// `42601 unrecognized EXPLAIN option "nosuchoption"`, downcased as PostgreSQL's grammar leaves it.
#[test]
fn an_unrecognized_option_is_a_syntax_error() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        refusal(&mut node, "EXPLAIN (NOSUCHOPTION) SELECT * FROM authors"),
        "42601 unrecognized EXPLAIN option \"nosuchoption\""
    );
}

/// `22023` for a value, where an unrecognized *name* is `42601` — PostgreSQL's own split between a
/// word that means nothing and a parameter that will not read.
#[test]
fn an_unrecognized_format_is_an_invalid_parameter_value() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        refusal(
            &mut node,
            "EXPLAIN (FORMAT NOSUCHFORMAT) SELECT * FROM authors"
        ),
        "22023 unrecognized value for EXPLAIN option \"format\": \"nosuchformat\""
    );
}

/// `defGetBoolean` and `defGetString`, whose sentences carry the option name unquoted.
#[test]
fn a_bad_option_argument_says_what_it_needed() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        refusal(&mut node, "EXPLAIN (ANALYZE MAYBE) SELECT * FROM authors"),
        "42601 analyze requires a Boolean value"
    );
    assert_eq!(
        refusal(&mut node, "EXPLAIN (COSTS 2) SELECT * FROM authors"),
        "42601 costs requires a Boolean value"
    );
    assert_eq!(
        refusal(&mut node, "EXPLAIN (FORMAT) SELECT * FROM authors"),
        "42601 format requires a parameter"
    );
}

/// The three options that are about a run and not about a plan, checked **after** the whole list —
/// which is why `(TIMING, ANALYZE)` is legal and `(TIMING)` is not.
#[test]
fn timing_wal_and_serialize_require_analyze() {
    let mut node = parity::Node::new(FIXTURE);
    for (option, name) in [
        ("TIMING", "TIMING"),
        ("WAL", "WAL"),
        ("SERIALIZE", "SERIALIZE"),
    ] {
        assert_eq!(
            refusal(
                &mut node,
                &format!("EXPLAIN ({option}) SELECT * FROM authors")
            ),
            format!("22023 EXPLAIN option {name} requires ANALYZE")
        );
        let sql = format!("EXPLAIN ({option}, ANALYZE) SELECT * FROM authors");
        assert!(
            matches!(answer(&mut node, &sql), Answer::Rows { .. }),
            "{sql} was refused"
        );
    }
}

/// The boolean spellings `defGetBoolean` reads, and the options this server accepts and ignores.
#[test]
fn every_option_postgresql_accepts_is_accepted() {
    let mut node = parity::Node::new(FIXTURE);
    for list in [
        "COSTS",
        "COSTS FALSE",
        "COSTS 0",
        "COSTS on",
        "BUFFERS",
        "SETTINGS",
        "SUMMARY",
        "MEMORY",
        "GENERIC_PLAN",
        "VERBOSE",
        "ANALYZE TRUE, COSTS FALSE",
        "ANALYZE on, TIMING off",
        "analyze 1",
        "ANALYZE, ANALYZE",
    ] {
        let sql = format!("EXPLAIN ({list}) SELECT * FROM authors");
        assert!(
            matches!(answer(&mut node, &sql), Answer::Rows { .. }),
            "{sql} was refused"
        );
    }
}

/// `EXPLAIN VERBOSE` and `EXPLAIN ANALYZE VERBOSE` without parentheses — the legacy spelling, which
/// was `0A000` here until the option list arrived.
#[test]
fn the_legacy_keywords_are_the_same_vocabulary() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "EXPLAIN VERBOSE SELECT * FROM authors",
        "EXPLAIN ANALYZE VERBOSE SELECT * FROM authors",
    ] {
        assert!(
            matches!(answer(&mut node, sql), Answer::Rows { .. }),
            "{sql} was refused"
        );
    }
}

/// `FORMAT` outside the parentheses is a **syntax error** on a real server, and answering a plan
/// here would be answering where PostgreSQL raises.
#[test]
fn format_outside_the_parentheses_is_a_syntax_error() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "EXPLAIN FORMAT JSON SELECT * FROM authors",
        "EXPLAIN ANALYZE FORMAT JSON SELECT * FROM authors",
    ] {
        assert_eq!(
            refusal(&mut node, sql),
            "42601 syntax error at or near \"FORMAT\""
        );
    }
}

/// `ANALYZE` still refuses to run anything that writes, whichever spelling asked for it.
#[test]
fn analyze_of_a_write_is_still_refused() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "EXPLAIN (ANALYZE) INSERT INTO authors VALUES (9, 'z')",
        "EXPLAIN ANALYZE DELETE FROM authors WHERE id = 9",
    ] {
        assert!(
            refusal(&mut node, sql).starts_with("0A000"),
            "{sql} was not refused"
        );
    }
    assert_eq!(
        node.rows("SELECT count(*) FROM authors"),
        [["2".to_owned()]]
    );
}

/// **An option this node does not honour must not change the plan** — which is the other half of
/// "accepted and ignored", and the half a reader has to be able to check.
///
/// Carried over from h1's version of this unit, which was written in parallel with this one and
/// landed in main first (`580da5fa`). Its own doc comment records why it is an equality rather
/// than a search for an `Engine:` line: the first version asserted a line that a plain `SELECT *`
/// never has, and the second guarded it into asserting nothing at all. Equality covers what it was
/// for and more — whatever a plan says, an option list must not move it.
#[test]
fn an_option_list_does_not_change_the_plan() {
    let mut node = parity::Node::new(FIXTURE);
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
            "(SETTINGS, SUMMARY, MEMORY, GENERIC_PLAN)",
        ] {
            assert_eq!(
                node.rows(&format!("EXPLAIN {options} {query}")),
                plain,
                "EXPLAIN {options} changed the plan of `{query}`"
            );
        }
    }
}
