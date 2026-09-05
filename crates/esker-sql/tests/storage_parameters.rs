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

/// **`RESET` validates nothing**, which is the capture's own headline and the thing reading the
/// documentation would have got wrong: `SET` of a name no server knows is `22023` and `RESET` of
/// the same name is accepted.
///
/// Measured on 19beta1, all `ok`: a list of several, the same name twice, a quoted name, a name in
/// the `toast` namespace — and, the part that surprises, **a namespace no server has**, where the
/// matching `SET` is `22023 unrecognized parameter namespace "esker"`. `RESET` does not reach the
/// name at all.
#[test]
fn reset_accepts_every_name_there_is() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    for sql in [
        "ALTER TABLE t RESET (columnar_replicas)",
        "ALTER TABLE t RESET (no_such_thing_at_all)",
        "ALTER TABLE t RESET (autovacuum_enabled)",
        "ALTER TABLE t RESET (a, b)",
        "ALTER TABLE t RESET (a, a)",
        "ALTER TABLE t RESET (\"MixedCase\")",
        "ALTER TABLE t RESET (toast.autovacuum_enabled)",
        "ALTER TABLE t RESET (esker.whatever)",
        "ALTER TABLE t RESET ( columnar_replicas )",
        "alter table t reset (columnar_replicas);",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
}

/// `RESET (columnar_replicas)` **forgets** the setting, which is the one name here that has
/// somewhere to be forgotten from.
///
/// Zero and absent are different histories to a placement driver — "somebody turned it off" is not
/// "nobody ever turned it on" — so this asserts the row is gone rather than that it reads zero.
#[test]
fn reset_forgets_the_one_parameter_this_node_keeps() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    node.run("ALTER TABLE t SET (columnar_replicas = 2)")
        .unwrap();
    assert_eq!(listed(&mut node), [["t".to_owned(), "2".to_owned()]]);

    node.run("ALTER TABLE t RESET (columnar_replicas)").unwrap();
    assert!(listed(&mut node).is_empty(), "{:?}", listed(&mut node));

    // A name that is not this node's leaves it alone, the way a real server leaves everything
    // alone: `RESET` of one parameter is not `RESET` of the table. A *namespaced*
    // `columnar_replicas` is not this node's either — a real server accepts any namespace here and
    // does nothing with it.
    node.run("ALTER TABLE t SET (columnar_replicas = 3)")
        .unwrap();
    for sql in [
        "ALTER TABLE t RESET (no_such_thing_at_all)",
        "ALTER TABLE t RESET (toast.columnar_replicas)",
    ] {
        node.run(sql).unwrap();
        assert_eq!(
            listed(&mut node),
            [["t".to_owned(), "3".to_owned()]],
            "{sql}"
        );
    }
}

/// A missing table is `IF EXISTS`'s notice and not an error, and `ONLY` keeps the answer it has
/// everywhere else on this statement.
#[test]
fn reset_carries_the_rest_of_the_alter_table_grammar() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id int8)"]);
    assert_eq!(
        node.answer("ALTER TABLE IF EXISTS no_such_table RESET (a)")
            .to_string(),
        "(a command, no result set)"
    );
    assert_eq!(
        node.answer("ALTER TABLE no_such_table RESET (a)")
            .to_string(),
        "!42P01 relation \"no_such_table\" does not exist"
    );
    // A real server takes `ONLY` here; this node refuses it on every `ALTER TABLE` and says so by
    // name, which is contract C2 rather than a divergence of this statement's own.
    assert_eq!(
        node.answer("ALTER TABLE ONLY t RESET (a)").to_string(),
        "!0A000 ALTER TABLE ONLY is not supported"
    );
}

/// **The capture, replayed from the file.** Every row is put to the node and compared with what a
/// real server answered, and the ones that differ are named here with their reason.
///
/// The file is read rather than restated, so a row that changes on either side changes this test.
/// And a divergence that starts *agreeing* fails it — [ADR 0031](../../../docs/adr/0031-declared-divergences.md)
/// rule 2 is that a declared divergence which has stopped diverging is deleted, not left standing.
///
/// The rows are asserted here instead of through `parity::replay` because this file predates the
/// harness's format: it writes `statement <tab> ok` where the harness wants
/// `statement <tab> types <tab> rows`.
#[test]
fn the_capture_replays_and_only_the_declared_rows_differ() {
    /// Why each row that differs does. Every one is `columnar_replicas` being Esker's own
    /// parameter, or the storage-parameter catalogue this node does not have.
    const DIVERGENCES: &[(&str, &str)] = &[
        (
            "ALTER TABLE t SET (columnar_replicas = 1)",
            "ADR 0022 Decision 5: this node's own parameter, and PostgreSQL accepts no spelling of \
             one — an arbitrary namespace is refused too, which is what the rest of this file shows",
        ),
        (
            "CREATE TABLE t (id int8) WITH (columnar_replicas = 1)",
            "the same divergence at creation time, where this node reads no storage parameters at \
             all yet",
        ),
        (
            "ALTER TABLE t SET (autovacuum_enabled = false)",
            "no storage-parameter catalogue here; the parameter is named rather than pretended at",
        ),
        (
            "ALTER TABLE t SET (autovacuum_enabled = 'banana')",
            "the same: validating the value needs the catalogue that would hold its type",
        ),
    ];

    let corpus = include_str!("corpus/pg19_storage_parameters.txt");
    let mut rows = 0;
    for line in corpus.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (statement, captured) = line.split_once('\t').expect(line);
        rows += 1;

        // A fresh node per row: the capture was taken statement by statement and two of these
        // create the same table.
        let mut node = parity::Node::new(&[]);
        if !statement.starts_with("CREATE TABLE") {
            node.run("CREATE TABLE t (id int8)").unwrap();
        }
        let answered = node.answer(statement).to_string();
        let expected = if captured == "ok" {
            "(a command, no result set)".to_owned()
        } else {
            captured.to_owned()
        };

        match DIVERGENCES.iter().find(|(sql, _)| *sql == statement) {
            Some((_, reason)) => assert_ne!(
                answered, expected,
                "{statement} agrees now; delete the declared divergence ({reason})"
            ),
            None => assert_eq!(answered, expected, "{statement}"),
        }
    }
    assert_eq!(rows, 8, "the capture holds eight statements");
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
    // The shapes a real server refuses with `42601` are refused here too, but by name: the reader
    // does not take them, so the refusal table answers. `0A000` about a malformed statement is
    // this module's standing choice of C2 over C1's shortfall — and the shapes it costs are listed
    // on `crate::parse::AlterTableReset`, not left to be discovered.
    for sql in [
        "ALTER TABLE t RESET ()",
        "ALTER TABLE t RESET (a.b.c)",
        "ALTER TABLE public.t RESET (a)",
        "ALTER TABLE t RESET (a), SET (columnar_replicas = 1)",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "!0A000 ALTER TABLE ... RESET is not supported",
            "{sql}"
        );
    }
}
