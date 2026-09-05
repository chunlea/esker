//! **`WITH` is optional in `CREATE EXTENSION`, and leaving it out was a syntax error here.**
//!
//! `postgresql_adapter_test.rb` and `extension_migration_test.rb` send
//!
//! ```sql
//! CREATE EXTENSION hstore SCHEMA custom_schema
//! CREATE EXTENSION IF NOT EXISTS "hstore" SCHEMA other_schema
//! ```
//!
//! and got `42601 … Expected: end of statement, found: SCHEMA` at columns 25 and 41 — which is
//! exactly where the bare `SCHEMA` starts in each. `sqlparser` 0.62.0 reads the three options only
//! after a `WITH`; PostgreSQL makes the keyword optional, measured for all four combinations
//! (`SCHEMA`, `VERSION`, `CASCADE`, and the three together).
//!
//! **The fix is not about `hstore`.** Whether an extension is available is the allowlist's
//! question and the answer does not change here — what changes is that a valid statement stops
//! being a *parse* error, which is contract C1. The options are still refused by name, the same
//! `0A000` the `WITH` spelling has always had.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The two spellings answer the same thing, which is the whole claim.
#[test]
fn the_bare_and_with_spellings_agree() {
    for (bare, with) in [
        (
            "CREATE EXTENSION hstore SCHEMA custom_schema",
            "CREATE EXTENSION hstore WITH SCHEMA custom_schema",
        ),
        (
            "CREATE EXTENSION IF NOT EXISTS \"hstore\" SCHEMA custom_schema",
            "CREATE EXTENSION IF NOT EXISTS \"hstore\" WITH SCHEMA custom_schema",
        ),
    ] {
        // **A node each, because the statement now succeeds.** This compared two *refusals* when
        // it was written, so one node served both spellings; the day `SCHEMA` was implemented the
        // first spelling installed the extension and the second answered
        // `42710 extension "hstore" already exists`, and the test failed while its claim was still
        // true. The claim is about the two spellings being one statement, so each gets a node in
        // the same state.
        let mut node = parity::Node::new(&["CREATE SCHEMA custom_schema"]);
        let bare_answer = node.answer(bare).to_string();
        let mut node = parity::Node::new(&["CREATE SCHEMA custom_schema"]);
        assert_eq!(bare_answer, node.answer(with).to_string(), "{bare}");
        assert!(
            !bare_answer.contains("42601"),
            "a valid statement must not be a syntax error: {bare_answer}"
        );
        // **This asserted the `0A000` and now asserts the answer.** The clause was implemented in
        // `tests/create_extension_in_schema.rs`, and a declared refusal that starts working is
        // deleted rather than kept green against the old text (ADR 0031 rule 2). What this file
        // is still for is C1 and the two spellings: `sqlparser` 0.62.0 reads the options only
        // after a `WITH`, PostgreSQL makes the keyword optional, and both must reach the same
        // statement.
        assert_eq!(bare_answer, "(a command, no result set)");
    }
}

/// `VERSION` and `CASCADE` are the other two the `WITH` gates, and they are refused by their own
/// names rather than by the first one's.
#[test]
fn version_and_cascade_are_named_for_themselves() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer("CREATE EXTENSION pgcrypto VERSION '1.3'")
            .to_string(),
        "!0A000 CREATE EXTENSION ... VERSION is not supported"
    );
    assert_eq!(
        node.answer("CREATE EXTENSION pgcrypto CASCADE").to_string(),
        "!0A000 CREATE EXTENSION ... CASCADE is not supported"
    );
}

/// **An extension on the allowlist still installs**, which is what says the rewrite did not change
/// the statement's meaning — only whether the parser could read it.
#[test]
fn an_allowed_extension_is_unaffected() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE EXTENSION IF NOT EXISTS pgcrypto").unwrap();
    node.run("CREATE EXTENSION \"uuid-ossp\"").unwrap();
}
