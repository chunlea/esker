//! Storage parameters — the surface `ALTER TABLE … SET (columnar_replicas = N)` lands on.
//!
//! `corpus/pg19_storage_parameters.txt` records what a real PostgreSQL 19beta1 does *around* this
//! node's own parameter, and it was replayed by nothing. Its rows are asserted here rather than
//! through `parity::replay` because the file predates the harness's format: it writes
//! `statement <tab> ok` where the harness wants `statement <tab> types <tab> rows`.
//!
//! **The point of the file is one deliberate divergence.** `columnar_replicas` is Esker's own
//! ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5), so PostgreSQL
//! refuses it and this node accepts it. Everything else is what a real server does with the
//! *shape*, and this node should agree.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The listing surface, which is the only way to see what a statement did to the setting.
///
/// `ALTER TABLE … SET (columnar_replicas = N)` is acted on by the placement driver and nothing in
/// a `TableDef` changes, so a test that only checks the command tag cannot tell "applied" from
/// "silently cleared".
fn listed(node: &mut parity::Node) -> Vec<Vec<String>> {
    node.rows("SELECT * FROM esker_columnar_replicas()")
}

/// **A namespaced parameter is valid syntax, and answering `42601` broke contract C1.**
///
/// `sqlparser` 0.62.0 stops at the dot inside `SetOptionsParens` — `Expected: =, found: .` — so
/// both of these were a *parse error* here where PostgreSQL answers one `ok` and the other with a
/// **semantic** error naming the namespace. C1 is that no valid PostgreSQL 19 statement is a parse
/// error, and PostgreSQL's own pair of answers is the proof that the syntax is fine.
#[test]
fn a_namespaced_parameter_is_read_and_answered_by_name() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);

    // `toast.` is accepted, whatever follows it — measured on both a parameter this node has and
    // one it does not, because the namespace is decided before the name is looked at.
    for sql in [
        "ALTER TABLE t SET (toast.columnar_replicas = 1)",
        "ALTER TABLE t SET (toast.autovacuum_enabled = false)",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }

    // And a namespace a real server does not have carries its own sentence.
    assert_eq!(
        node.answer("ALTER TABLE t SET (esker.columnar_replicas = 1)")
            .to_string(),
        "!22023 unrecognized parameter namespace \"esker\""
    );
}

/// **A parameter with nowhere to land must not clear one that landed.**
///
/// `toast.*` is accepted and applied to nothing — this node has no TOAST. The first version of
/// that spelled "nothing" as `SetColumnarReplicas { replicas: None }`, reusing the variant that
/// was already there. That is not nothing: it is `RESET`, and it *deletes* the table's columnar
/// setting. So `SET (toast.autovacuum_enabled = …)` silently took a table's columnar copies away
/// and told the placement driver to act on it.
///
/// The command tag is identical either way, which is why this is asserted on the listing.
#[test]
fn a_toast_parameter_leaves_the_columnar_setting_alone() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    node.run("ALTER TABLE t SET (columnar_replicas = 2)")
        .unwrap();
    assert_eq!(listed(&mut node), [["t".to_owned(), "2".to_owned()]]);

    node.run("ALTER TABLE t SET (toast.autovacuum_enabled = false)")
        .unwrap();
    assert_eq!(
        listed(&mut node),
        [["t".to_owned(), "2".to_owned()]],
        "a parameter this node has nowhere to put must not delete one it has"
    );
}

/// **The rewrite fires only inside a `SET (…)`**, which is what keeps it from touching the dots
/// that mean something else. A qualified column is the case that would break first.
#[test]
fn a_dot_outside_a_storage_parameter_is_untouched() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    assert_eq!(node.rows("SELECT t.id FROM t WHERE t.id = 1").len(), 0);
    node.run("ALTER TABLE t SET (columnar_replicas = 1)")
        .unwrap();
}

/// What the capture records that this node still answers differently, so the gaps are visible
/// rather than implied. Each is in `docs/plans/storage-parameters.md` §3 with its cost.
#[test]
fn the_parameter_surface_itself_is_still_refused() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    // PostgreSQL accepts this one; this node has no storage-parameter catalogue, so it names the
    // parameter rather than pretending to apply it.
    assert_eq!(
        node.answer("ALTER TABLE t SET (autovacuum_enabled = false)")
            .to_string(),
        "!0A000 the storage parameter autovacuum_enabled is not supported"
    );
    // `RESET` accepts any name on a real server and validates nothing — the capture's own
    // headline. `sqlparser` has no table-level `ResetOptionsParens`, so reading it is a `parse`
    // unit of its own.
    assert_eq!(
        node.answer("ALTER TABLE t RESET (no_such_thing_at_all)")
            .to_string(),
        "!0A000 ALTER TABLE ... RESET is not supported"
    );
}
