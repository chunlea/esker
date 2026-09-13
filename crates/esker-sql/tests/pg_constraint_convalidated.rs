//! **The one write to a system catalog this node takes**: `UPDATE pg_catalog.pg_constraint SET
//! convalidated = …` — [ADR 0113] §6, ruled (b) by the user on 2026-09-13.
//!
//! `check_all_foreign_keys_valid!` marks every foreign key unvalidated and then validates it, because
//! `VALIDATE CONSTRAINT` does nothing to a constraint already validated — measured on 19beta1, with a
//! violating row still in the table (`esker-coord/s2-fk3.out`). The flag it writes is the `NOT VALID`
//! state a foreign key or a `CHECK` already stores, so the write is taken for exactly that and
//! refused for everything else.
//!
//! [ADR 0113]: ../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE parent (id int PRIMARY KEY)",
        "CREATE TABLE child (id int PRIMARY KEY, parent_id int, CONSTRAINT child_fk FOREIGN KEY \
         (parent_id) REFERENCES parent (id), CONSTRAINT child_check CHECK (id > 0))",
        "INSERT INTO parent VALUES (1)",
        "INSERT INTO child VALUES (1, 1)",
    ])
}

fn validated(node: &mut parity::Node, name: &str) -> String {
    node.rows(&format!(
        "SELECT convalidated FROM pg_constraint WHERE conname = '{name}'"
    ))
    .remove(0)
    .remove(0)
}

/// **The flag moves, and `VALIDATE CONSTRAINT` then checks the rows again** — which is the whole of
/// what the write is for: a row that slipped in under `DISABLE TRIGGER ALL` is found.
#[test]
fn an_unvalidated_foreign_key_is_checked_again() {
    let mut node = node();
    node.run("ALTER TABLE child DISABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO child VALUES (2, 99)").unwrap();
    node.run("ALTER TABLE child ENABLE TRIGGER ALL").unwrap();

    // Validated already, so validating finds nothing to do — PostgreSQL's answer too.
    node.run("ALTER TABLE child VALIDATE CONSTRAINT child_fk")
        .unwrap();

    assert_eq!(
        node.answer(
            "UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'child_fk'"
        )
        .to_string(),
        "(a command, no result set)"
    );
    assert_eq!(validated(&mut node, "child_fk"), "f");
    assert_eq!(
        node.answer("ALTER TABLE child VALIDATE CONSTRAINT child_fk")
            .to_string(),
        "!23503 insert or update on table \"child\" violates foreign key constraint \"child_fk\" \
         DETAIL: Key (parent_id)=(99) is not present in table \"parent\"."
    );
}

/// A `CHECK` stores the same flag, and takes the same write.
#[test]
fn a_check_takes_the_write_too() {
    let mut node = node();
    node.run(
        "UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'child_check'",
    )
    .unwrap();
    assert_eq!(validated(&mut node, "child_check"), "f");
    node.run("ALTER TABLE child VALIDATE CONSTRAINT child_check")
        .unwrap();
    assert_eq!(validated(&mut node, "child_check"), "t");
}

/// **The write is the transaction's**: a `ROLLBACK` puts the flag back, and a predicate that selects
/// nothing is `UPDATE 0`, which is PostgreSQL's answer for the ruling's own pinned statement.
#[test]
fn the_write_belongs_to_its_transaction() {
    let mut node = node();
    node.run("BEGIN").unwrap();
    node.run("UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'child_fk'")
        .unwrap();
    assert_eq!(validated(&mut node, "child_fk"), "f");
    node.run("ROLLBACK").unwrap();
    assert_eq!(validated(&mut node, "child_fk"), "t");

    let outcome = node
        .run("UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'x'")
        .unwrap();
    assert_eq!(format!("{outcome:?}"), "Done { tag: \"UPDATE 0\" }");
}

/// **Everything but that shape is still the catalog's refusal**: a row whose constraint stores no
/// flag, another column, a value that is not a constant.
#[test]
fn every_other_write_is_still_refused() {
    let mut node = node();
    for sql in [
        "UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'child_pkey'",
        "UPDATE pg_catalog.pg_constraint SET convalidated = false",
        "UPDATE pg_catalog.pg_constraint SET conname = 'renamed' WHERE conname = 'child_fk'",
        "UPDATE pg_catalog.pg_constraint SET convalidated = (1 = 2) WHERE conname = 'child_fk'",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "!42501 permission denied: \"pg_constraint\" is a system catalog",
            "{sql}"
        );
    }
    assert_eq!(validated(&mut node, "child_fk"), "t");
}

/// **`check_all_foreign_keys_valid!`'s block, with the schema predicate left out** — which is the
/// half of its `UPDATE` that needs `regnamespace`. Clean, the block answers `DO`; with a violating
/// row, PostgreSQL's `23503` from inside the `EXECUTE`.
#[test]
fn the_census_block_finds_a_violation() {
    let block = "do $$ declare r record; BEGIN FOR r IN ( SELECT FORMAT( 'UPDATE \
                 pg_catalog.pg_constraint SET convalidated=false WHERE conname = ''%1$I''; ALTER \
                 TABLE %2$I.%3$I VALIDATE CONSTRAINT %1$I;', constraint_name, table_schema, \
                 table_name ) AS constraint_check FROM information_schema.table_constraints WHERE \
                 constraint_type = 'FOREIGN KEY' ) LOOP EXECUTE (r.constraint_check); END LOOP; \
                 END; $$;";
    let mut node = node();
    assert_eq!(node.answer(block).to_string(), "(a command, no result set)");
    assert_eq!(validated(&mut node, "child_fk"), "t");

    node.run("ALTER TABLE child DISABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO child VALUES (2, 99)").unwrap();
    node.run("ALTER TABLE child ENABLE TRIGGER ALL").unwrap();
    assert_eq!(
        node.answer(block).to_string(),
        "!23503 insert or update on table \"child\" violates foreign key constraint \"child_fk\" \
         DETAIL: Key (parent_id)=(99) is not present in table \"parent\"."
    );
}
