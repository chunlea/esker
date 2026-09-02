//! `FOREIGN KEY`, against PostgreSQL 19beta1 — statement 52 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_constraint.conname` is a `name` and `contype`/`confupdtype`/`confdeltype` are `"char"`
    // on a real server; all four are `text` here — types this node does not have, provided where
    // the client's use of them is text-shaped (`catalog::pg_constraint`). **Every row agrees**,
    // which is the column that matters: the behaviour, the messages and the definition text are
    // byte-identical to PostgreSQL 19 across all of them.
    types: &[
        "SELECT conname, contype, condeferrable, condeferred, convalidated, confupdtype, confdeltype, pg_get_constraintdef(c.oid), c.conkey::text, c.confkey::text, t2.relname FROM pg_constraint c JOIN pg_class t2 ON t2.oid = c.confrelid WHERE c.conrelid = 'fxc'::regclass ORDER BY conname",
        "SELECT conname, pg_get_constraintdef(oid), confupdtype, confdeltype FROM pg_constraint WHERE conrelid = 'fxd'::regclass AND contype = 'f' ORDER BY conname",
        "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'fxe'::regclass AND contype = 'f'",
        "SELECT conname, pg_get_constraintdef(oid), confupdtype, confdeltype FROM pg_constraint WHERE conrelid = 'fxc'::regclass AND contype = 'f' ORDER BY conname",
    ],
    answers: &[],
};

#[test]
fn every_foreign_key_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_foreign_key.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A **self-reference** declared in the statement that creates the table.
///
/// The case that cannot be resolved out of the catalog: the table is not there yet in this shape,
/// whether it is being created or altered, so the version under construction is what answers.
#[test]
fn a_table_can_reference_itself() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE fs (id int8 PRIMARY KEY, parent int8 REFERENCES fs)")
        .unwrap();
    node.run("INSERT INTO fs VALUES (1, NULL)").unwrap();
    node.run("INSERT INTO fs VALUES (2, 1)").unwrap();
    let error = node.run("INSERT INTO fs VALUES (3, 99)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    // The parent of a row something points at cannot go.
    let error = node.run("DELETE FROM fs WHERE id = 1").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    node.run("DELETE FROM fs WHERE id = 2").unwrap();
    node.run("DELETE FROM fs WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'f'"),
        [["FOREIGN KEY (parent) REFERENCES fs(id)"]]
    );
}

/// A constraint may reference a **unique index** and not only the primary key.
///
/// `42830` is about there being no single row to point at, which a whole unique key gives and a
/// non-key column does not.
#[test]
fn a_reference_may_follow_a_unique_index() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE fp (id int8 PRIMARY KEY, code text)",
        "CREATE UNIQUE INDEX fp_code ON fp (code)",
        "CREATE TABLE fc (id int8 PRIMARY KEY, code text)",
        "ALTER TABLE fc ADD CONSTRAINT fc_code FOREIGN KEY (code) REFERENCES fp (code)",
        "INSERT INTO fp VALUES (1, 'a')",
        "INSERT INTO fc VALUES (10, 'a')",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("INSERT INTO fc VALUES (11, 'zz')").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    let error = node.run("DELETE FROM fp WHERE id = 1").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
}

/// A **partial** unique index is not a key: it constrains only the rows its predicate admits, so
/// the referenced column can still repeat among the rest.
#[test]
fn a_partial_unique_index_is_not_a_key_to_reference() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE fp (id int8 PRIMARY KEY, code text, live int8)",
        "CREATE UNIQUE INDEX fp_code ON fp (code) WHERE live IS NOT NULL",
        "CREATE TABLE fc (id int8 PRIMARY KEY, code text)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node
        .run("ALTER TABLE fc ADD CONSTRAINT fc_code FOREIGN KEY (code) REFERENCES fp (code)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42830");
}

/// `INITIALLY DEFERRED` is refused by name, and `DEFERRABLE INITIALLY IMMEDIATE` is not.
///
/// The one form that would change an answer rather than nothing: a transaction that violates the
/// constraint in the middle and repairs it before `COMMIT` succeeds on a real server, and every
/// check here is immediate. `ActiveRecord` writes only the immediate form.
#[test]
fn initially_deferred_is_refused_and_initially_immediate_is_not() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE fp (id int8 PRIMARY KEY)",
        "CREATE TABLE fc (id int8 PRIMARY KEY, p int8)",
        "ALTER TABLE fc ADD CONSTRAINT fc_ok FOREIGN KEY (p) REFERENCES fp (id) DEFERRABLE INITIALLY IMMEDIATE",
    ] {
        node.run(statement).unwrap();
    }
    let error = node
        .run(
            "ALTER TABLE fc ADD CONSTRAINT fc_no FOREIGN KEY (p) REFERENCES fp (id) DEFERRABLE \
             INITIALLY DEFERRED",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(
        error.to_string().contains("INITIALLY DEFERRED"),
        "the refusal names itself: {error}"
    );
    // The accepted one is `condeferrable = t`, `condeferred = f`, and prints `DEFERRABLE`.
    assert_eq!(
        node.rows(
            "SELECT condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE contype = 'f'"
        ),
        [["t", "f", "FOREIGN KEY (p) REFERENCES fp(id) DEFERRABLE"]]
    );
}

/// A dropped **child** stops holding the parent, which the back-reference has to learn.
///
/// The bug this prevents: a back-reference the child's `DROP TABLE` left behind would make the
/// parent permanently undroppable and every `DELETE` on it read a table that is not there.
#[test]
fn dropping_the_child_releases_the_parent() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE fp (id int8 PRIMARY KEY)",
        "CREATE TABLE fc (id int8 PRIMARY KEY, p int8 REFERENCES fp)",
        "INSERT INTO fp VALUES (1)",
        "INSERT INTO fc VALUES (10, 1)",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("DROP TABLE fp").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    node.run("DROP TABLE fc").unwrap();
    node.run("DELETE FROM fp WHERE id = 1").unwrap();
    node.run("DROP TABLE fp").unwrap();
}

/// `ON DELETE CASCADE` inside an explicit transaction, rolled back.
///
/// Every check and every cascade is the statement's own transaction, so a `ROLLBACK` takes the
/// cascaded deletes with it — which is what "inside the same transaction" has to mean.
#[test]
fn a_cascade_rolls_back_with_its_transaction() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE fp (id int8 PRIMARY KEY)",
        "CREATE TABLE fc (id int8 PRIMARY KEY, p int8 REFERENCES fp ON DELETE CASCADE)",
        "INSERT INTO fp VALUES (1)",
        "INSERT INTO fc VALUES (10, 1)",
        "INSERT INTO fc VALUES (11, 1)",
        "BEGIN",
        "DELETE FROM fp WHERE id = 1",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id FROM fc ORDER BY id"),
        Vec::<Vec<String>>::new()
    );
    node.run("ROLLBACK").unwrap();
    assert_eq!(
        node.rows("SELECT id FROM fc ORDER BY id"),
        [["10"], ["11"]],
        "the cascade went back with the transaction"
    );
    assert_eq!(node.rows("SELECT id FROM fp"), [["1"]]);
}
