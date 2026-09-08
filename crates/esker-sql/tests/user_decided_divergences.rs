//! **The refusals the user decided to keep**, each with the text it answers today.
//!
//! Two of the Rails suite's remaining failures are not gaps waiting for a unit: they are places
//! where this node deliberately answers differently from PostgreSQL, by a ruling recorded in
//! `docs/plans/phase-9-rails.md`. A declared divergence with no test is a sentence in a document
//! that nothing checks — so each one is pinned here, and if the refusal ever changes these go red
//! and the record gets read again (ADR 0031, rule 2).
//!
//! **PostgreSQL accepts every statement below.** Measured on 19beta1 as the superuser the suite
//! connects as:
//!
//! ```text
//! DO $$ DECLARE r record; BEGIN FOR r IN (SELECT 1 AS x) LOOP EXECUTE 'SELECT 1'; END LOOP; END $$
//!                                                                        -> DO
//! DELETE FROM pg_depend WHERE objid = 0                                  -> DELETE 0
//! UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE …       -> UPDATE 0
//! ```
//!
//! So these are divergences in the strict sense — a real server answers and this one refuses —
//! and each refusal is the *answer the user chose*, not an accident of the parser.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **`DO $$ … $$` with a body: the "DO minimal" ruling.**
///
/// `fixtures_test.rb` sends one to unvalidate every foreign key in the schema —
/// `FOR r IN (SELECT FORMAT('UPDATE pg_catalog.pg_constraint SET convalidated=false …')) LOOP
/// EXECUTE …` — and it costs two tests, `test_does_not_raise_if_no_fk_violations` and
/// `test_raises_fk_violations`. Running it needs a procedural language: a loop, a record variable,
/// dynamic `EXECUTE`, and a write to a system catalog that the *next* test would read back.
///
/// The user's ruling was **minimal**: the one `DO` shape the suite depends on structurally —
/// `create_enum`'s `IF NOT EXISTS` guard — is read and turned into the `CREATE TYPE` it guards
/// (`parse::strip_do_create_enum`), and every other body is refused **by name** rather than
/// half-run. `0A000` naming the construct is contract C2, and it is the answer here.
#[test]
fn a_do_block_with_a_loop_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer(
            "DO $$ DECLARE r record; BEGIN FOR r IN (SELECT 1 AS x) LOOP EXECUTE 'SELECT 1'; \
             END LOOP; END $$"
        )
        .to_string(),
        "!0A000 DO is not supported"
    );
}

/// And the one shape that is **not** refused, so that "minimal" has a lower bound as well as an
/// upper one: `create_enum`'s guard still becomes the `CREATE TYPE` inside it.
///
/// **Verbatim, because the shim matches the client's shape and not a generalisation of it.** A
/// hand-shortened guard — the same `IF NOT EXISTS` without the `pg_namespace` join — is refused
/// like any other body, which is the shim being narrow on purpose: it reads the one statement
/// `postgresql_adapter.rb:556` builds, and anything else is a `DO` nobody measured.
#[test]
fn the_create_enum_guard_is_still_read() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "DO $$ BEGIN IF NOT EXISTS ( SELECT 1 FROM pg_type t JOIN pg_namespace n ON \
         t.typnamespace = n.oid WHERE t.typname = 'g1d_mood' AND n.nspname = ANY \
         (current_schemas(false)) ) THEN CREATE TYPE \"g1d_mood\" AS ENUM ('sad', 'ok'); \
         END IF; END $$;",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT typname FROM pg_type WHERE typname = 'g1d_mood'"),
        [["g1d_mood"]]
    );
}

/// **A write to a system catalog: `42501`, whatever the writer's rights.**
///
/// `postgresql_adapter_test.rb`'s `test_pk_and_sequence_for_with_collision_pg_class_oid` deletes
/// from `pg_depend` to detach a sequence from its column — a deliberate corruption of the catalog,
/// to see whether the adapter still answers. A real server lets a superuser do it; this node's
/// catalog is computed from records rather than stored as tables, so there is no row to delete and
/// no honest way to pretend there is. Refusing is the ADR 0031 call: a wrong answer where a
/// refusal is available is the worse of the two.
///
/// The same guard is what the `DO` block above would have hit had it run, which is why the two
/// records sit together: `UPDATE pg_catalog.pg_constraint SET convalidated = false` is refused for
/// this reason and not for the `DO` one.
#[test]
fn a_write_to_a_system_catalog_is_refused() {
    let mut node = parity::Node::new(&[]);
    for (sql, relation) in [
        ("DELETE FROM pg_depend WHERE objid = 1", "pg_depend"),
        (
            "UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'x'",
            "pg_constraint",
        ),
        ("INSERT INTO pg_class (relname) VALUES ('x')", "pg_class"),
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            format!("!42501 permission denied: \"{relation}\" is a system catalog"),
            "{sql}"
        );
    }
}
