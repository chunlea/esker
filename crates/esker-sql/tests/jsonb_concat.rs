//! **`||` over `jsonb` is a document merge**, and `json` has no `||` at all.
//!
//! This replaces a test that asserted the opposite. When `||` over text was built it shipped one
//! wrong answer with it — two `jsonb` documents concatenated as strings — and that was guarded by
//! refusing the call, which was the right trade under
//! [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md) (a gap beats a wrong
//! answer) and never the right answer.
//!
//! **The representation turned out not to be the obstacle.** `jsonb` is stored as a `Datum::Text`,
//! and `docs/plans/jsonb-representation.md` argued from that it needed a `Datum` of its own before
//! it could have a comparison — which is
//! [ADR 0042](../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)'s
//! rule. But the text it shares is **canonical**: `value::json::canonicalise` sorts keys by length
//! then bytes, drops duplicate keys with the last winning, and normalises every separator, so two
//! documents that are equal as `jsonb` are already the same string. Sharing a representation is
//! exactly what ADR 0042 permits when the comparison comes with it, and here it does. The plan was
//! wrong about the obstacle and the unit was a tenth of its size.
//!
//! What was missing was only the operator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// The one column of the one row of a single-value query, as text.
fn only(session: &mut cluster::Session, sql: &str) -> String {
    session
        .rows(sql)
        .first()
        .and_then(|row| row.first())
        .cloned()
        .flatten()
        .unwrap_or_else(|| panic!("{sql} answered no value"))
}

#[test]
fn jsonb_concatenation_is_a_document_merge() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE jm (id int8, body jsonb, other jsonb, plain text)")
        .unwrap();
    session
        .run("INSERT INTO jm VALUES (1, '{\"a\":1}', '{\"b\":2}', 'x')")
        .unwrap();

    // **Columns**, which is the shape that reaches the evaluator with its type intact.
    assert_eq!(
        only(&mut session, "SELECT body || other FROM jm"),
        r#"{"a": 1, "b": 2}"#,
        "two jsonb columns merge"
    );

    // **`jsonb || text` is not an operator on a real server**, so both sides fall back to text and
    // the answer is the two documents' characters run together — measured, `{\"a\": 1}x`. Getting
    // this wrong is how a merge that fired on one jsonb operand would corrupt an ordinary
    // concatenation.
    assert_eq!(
        only(&mut session, "SELECT body || plain FROM jm"),
        r#"{"a": 1}x"#,
        "a jsonb column beside a text column is text concatenation, not a merge"
    );
    assert_eq!(
        only(&mut session, "SELECT plain || body FROM jm"),
        r#"x{"a": 1}"#,
        "and the same the other way round"
    );

    // **`json` has no `||`.** Not a gap in this node — PostgreSQL has no such operator either, and
    // says so with the same sqlstate.
    let error = session
        .run(r#"SELECT '{"a":1}'::json || '{"b":2}'::json"#)
        .expect_err("json || json must be undefined");
    assert_eq!(
        error.sqlstate(),
        esker_sql::sqlstate::UNDEFINED_FUNCTION,
        "json || json is 42883 on a real server: {error}"
    );
}
