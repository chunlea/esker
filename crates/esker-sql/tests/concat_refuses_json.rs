//! **`||` refuses a `json` or `jsonb` operand**, rather than concatenating two documents.
//!
//! The corpus records that the refusal is a divergence from PostgreSQL, which merges. This pins
//! the refusal itself — that it happens, with the sqlstate it had before `||` over text existed —
//! because the failure mode being guarded against is not a missing answer but a **wrong** one, and
//! a corpus row that says "we differ here" would go on passing if the guard were deleted and the
//! concatenation came back.
//!
//! Two forms, because the type is visible in two different layers: a **cast** is folded to its
//! value before the executor runs, so it is caught in `parse::lower` where `::jsonb` is still
//! written down; a **column** reaches the evaluator as an `Expr::Ordinal` that still carries its
//! type. Both are asserted, because a guard in one layer only would leave the other answering.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use cluster::Cluster;

/// The sqlstate every "this node does not do that" answer carries.
const FEATURE_NOT_SUPPORTED: &str = "0A000";

#[test]
fn concat_refuses_a_json_operand_in_both_layers() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    session
        .run("CREATE TABLE docs (id bigserial PRIMARY KEY, body jsonb, plain text)")
        .unwrap();
    session
        .run("INSERT INTO docs (body, plain) VALUES ('{\"a\":1}', 'x')")
        .unwrap();

    for sql in [
        // The cast form, caught in the lowerer.
        "SELECT '{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb",
        "SELECT '{\"a\":1}'::json || '{\"b\":2}'::json",
        // A document beside a plain string is still a document on one side.
        "SELECT '{\"a\":1}'::jsonb || 'tail'",
        "SELECT 'head' || '{\"a\":1}'::jsonb",
        // The column form, caught in the evaluator — the layer the cast never reaches.
        "SELECT body || body FROM docs",
        "SELECT body || plain FROM docs",
        "SELECT plain || body FROM docs",
    ] {
        let error = session
            .run(sql)
            .expect_err(&format!("{sql} must be refused, not concatenated"));
        assert_eq!(
            error.sqlstate(),
            FEATURE_NOT_SUPPORTED,
            "{sql}: {error} is not the refusal this guards"
        );
    }

    // **And the guard is narrow**: a plain string beside a plain string still concatenates, which
    // is the whole of the unit this sits on top of.
    assert_eq!(
        session.rows("SELECT plain || '!' FROM docs"),
        [[Some("x!".to_owned())]],
        "the guard must not take text concatenation with it"
    );
}
