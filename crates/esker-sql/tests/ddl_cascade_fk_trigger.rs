//! `DROP … CASCADE`, an inline `FOREIGN KEY` and `ALTER TABLE … ENABLE/DISABLE TRIGGER`, against
//! PostgreSQL 19beta1 — statements 403, 405 and 430 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** The one entry named `conname`, `relname` and `contype` — the 64-byte `name`
    // (ADR 0084) and the one-byte `"char"` (ADR 0095) — and all three are those types here now.
    // **Every row agrees**, on all thirty-five statements, and so does every declared type.
    types: &[],
    answers: &[],
};

#[test]
fn every_ddl_cascade_fk_trigger_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_ddl_cascade_fk_trigger.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// What `CASCADE` destroys, which the corpus deliberately does not ask.
///
/// A cascading `DROP` reports what it took on a channel this corpus format cannot hold — a
/// `NOTICE` whose `DETAIL` runs over several unprefixed lines — so the capture drops only tables
/// with no dependents and describes this case in prose instead. It is the half that matters, so
/// it is measured here against what a real server leaves behind: **the constraint goes and the
/// child does not**. Measured on PostgreSQL 19 — after `DROP TABLE tp CASCADE`, `tc` still holds
/// its row and `pg_constraint` has one entry fewer.
#[test]
fn cascade_drops_the_constraint_and_keeps_the_child() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY, n text)",
        "CREATE TABLE tc (id int8 PRIMARY KEY, p int8 REFERENCES tp (id))",
        "INSERT INTO tp VALUES (1, 'x')",
        "INSERT INTO tc VALUES (1, 1)",
    ] {
        node.run(statement).unwrap();
    }
    // `RESTRICT` is the default written out, and refuses for the same reason writing nothing does.
    let error = node.run("DROP TABLE tp RESTRICT").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    node.run("DROP TABLE tp CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT id, p FROM tc ORDER BY id"),
        [["1", "1"]],
        "the child's rows are untouched"
    );
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint WHERE conrelid = 'tc'::regclass AND contype = 'f'"
        ),
        Vec::<Vec<String>>::new(),
        "and its foreign key is gone with the table it pointed at"
    );
    // The child is a table like any other afterwards: nothing still refuses a row on its behalf.
    node.run("INSERT INTO tc VALUES (2, 99)").unwrap();
}

/// **Two** constraints from one child to one parent, both cascaded.
///
/// The bug this prevents: removing the *first* constraint that names the parent and stopping.
/// `fkc2` in the corpus is exactly this shape — `CONSTRAINT c1 … REFERENCES fkp, CONSTRAINT c2 …
/// REFERENCES fkp` — and a child left holding `c2` would point at a table that is not there, which
/// every later write on it would fail on.
#[test]
fn cascade_takes_every_constraint_that_names_the_parent() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY)",
        "CREATE TABLE tc (a int8, b int8, CONSTRAINT c1 FOREIGN KEY (a) REFERENCES tp (id), \
         CONSTRAINT c2 FOREIGN KEY (b) REFERENCES tp (id))",
    ] {
        node.run(statement).unwrap();
    }
    node.run("DROP TABLE tp CASCADE").unwrap();
    assert_eq!(
        node.rows("SELECT conname FROM pg_constraint WHERE conrelid = 'tc'::regclass"),
        Vec::<Vec<String>>::new()
    );
    node.run("INSERT INTO tc VALUES (7, 8)").unwrap();
}

/// `IF EXISTS` and `CASCADE` are **independent**, and the missing-table error says `table`.
///
/// Both measured: `DROP TABLE IF EXISTS x CASCADE` on a table that never existed is a plain
/// success, and without `IF EXISTS` it is `42P01 table "x" does not exist` — note **`table`**,
/// where almost every other `42P01` in these corpora says `relation`.
#[test]
fn if_exists_and_cascade_are_independent() {
    let mut node = parity::Node::new(&[]);
    node.run("DROP TABLE IF EXISTS nothing_at_all CASCADE")
        .unwrap();
    node.run("DROP TABLE IF EXISTS nothing_at_all RESTRICT")
        .unwrap();
    let error = node.run("DROP TABLE nothing_at_all CASCADE").unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
    assert_eq!(error.to_string(), "table \"nothing_at_all\" does not exist");
}

/// `DISABLE TRIGGER ALL` suspends this table's referential checks, and `USER` does not.
///
/// This is the statement's whole purpose and the reason `ActiveRecord` writes it:
/// `disable_referential_integrity` wraps every fixture load in it so rows can be inserted before
/// their parents. The corpus cannot show it — it never writes a row while triggers are off — so
/// all four halves were measured against PostgreSQL 19 for this unit and are asserted here:
///
/// * with the **child's** triggers off, a row with no parent goes in;
/// * with the **parent's** off, a referenced row can be deleted, leaving the child pointing at
///   nothing;
/// * disabling the **parent's** does *not* let a bad row into the child, because the two triggers
///   are on two different tables;
/// * `USER` suspends none of it, because it covers only triggers a user created.
#[test]
fn disable_trigger_all_suspends_the_checks_and_user_does_not() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY, n text)",
        "CREATE TABLE tc (id int8 PRIMARY KEY, p int8 REFERENCES tp (id))",
    ] {
        node.run(statement).unwrap();
    }
    let error = node.run("INSERT INTO tc VALUES (1, 99)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");

    // The child's own check, suspended.
    node.run("ALTER TABLE tc DISABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO tc VALUES (1, 99)").unwrap();
    assert_eq!(node.rows("SELECT id, p FROM tc"), [["1", "99"]]);
    node.run("ALTER TABLE tc ENABLE TRIGGER ALL").unwrap();
    let error = node.run("INSERT INTO tc VALUES (2, 98)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");

    // The parent's, which is a different trigger on a different table.
    node.run("INSERT INTO tp VALUES (5, 'x')").unwrap();
    node.run("INSERT INTO tc VALUES (3, 5)").unwrap();
    let error = node.run("DELETE FROM tp WHERE id = 5").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    node.run("ALTER TABLE tp DISABLE TRIGGER ALL").unwrap();
    // Disabling the parent's does **not** suspend the child's insert check.
    let error = node.run("INSERT INTO tc VALUES (4, 97)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    node.run("DELETE FROM tp WHERE id = 5").unwrap();
    assert_eq!(
        node.rows("SELECT id, p FROM tc ORDER BY id"),
        [["1", "99"], ["3", "5"]],
        "the child keeps the row that now points at nothing"
    );
    node.run("ALTER TABLE tp ENABLE TRIGGER ALL").unwrap();

    // `USER` covers only triggers a user created, of which there are none.
    node.run("ALTER TABLE tc DISABLE TRIGGER USER").unwrap();
    let error = node.run("INSERT INTO tc VALUES (6, 96)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
    node.run("ALTER TABLE tc ENABLE TRIGGER USER").unwrap();
}

/// The disabled state is **stored**, not held beside the connection.
///
/// PostgreSQL's is `pg_trigger.tgenabled`, which outlives the transaction that set it and which
/// every session sees. A flag kept in memory would leave a second client enforcing what the first
/// one turned off, and would forget across a restart — so it lives in the table record, and a
/// `ROLLBACK` takes it back the way it takes any other catalog write back.
#[test]
fn the_disabled_state_is_a_catalog_write() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY)",
        "CREATE TABLE tc (id int8 PRIMARY KEY, p int8 REFERENCES tp (id))",
        "BEGIN",
        "ALTER TABLE tc DISABLE TRIGGER ALL",
        "INSERT INTO tc VALUES (1, 99)",
        "ROLLBACK",
    ] {
        node.run(statement).unwrap();
    }
    // The row went back, and so did the flag.
    assert_eq!(node.rows("SELECT id FROM tc"), Vec::<Vec<String>>::new());
    let error = node.run("INSERT INTO tc VALUES (1, 99)").unwrap_err();
    assert_eq!(error.sqlstate(), "23503");
}

/// A named trigger is `42704`, and the name is not a keyword.
///
/// `ALL` and `USER` are keywords only in this position, so a **quoted** `"ALL"` is a trigger
/// called `ALL` and joins every other name in the refusal. This node has no triggers to name.
#[test]
fn a_named_trigger_does_not_exist() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    for written in ["nosuchtrigger", "\"ALL\"", "\"USER\""] {
        let error = node
            .run(&format!("ALTER TABLE t DISABLE TRIGGER {written}"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), "42704", "for {written}");
    }
    assert_eq!(
        node.run("ALTER TABLE t DISABLE TRIGGER nosuchtrigger")
            .unwrap_err()
            .to_string(),
        "trigger \"nosuchtrigger\" for table \"t\" does not exist"
    );
    // And the table has to be there first.
    let error = node
        .run("ALTER TABLE nosuchtable DISABLE TRIGGER ALL")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P01");
}

/// `pg_class.relhastriggers` is `t` for **either side** of a foreign key and `f` for a table with
/// none — because a foreign key *is* two internal triggers.
///
/// Measured, all three: the child is `t`, the parent is `t`, and a table with no constraint at all
/// is `f`. Disabling them does not change it; the column says whether there are any, not whether
/// they are running.
#[test]
fn relhastriggers_is_true_for_both_sides_of_a_foreign_key() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY)",
        "CREATE TABLE tc (id int8 PRIMARY KEY, p int8 REFERENCES tp (id))",
        "CREATE TABLE tn (id int8 PRIMARY KEY)",
        "ALTER TABLE tc DISABLE TRIGGER ALL",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows(
            "SELECT relname, relhastriggers FROM pg_class WHERE relname IN ('tp','tc','tn') \
             ORDER BY relname"
        ),
        vec![vec!["tc", "t"], vec!["tn", "f"], vec!["tp", "t"],]
    );
    // An **index** on a table that has one is `f`: the trigger is on the table, not on the index.
    node.run("CREATE INDEX tci ON tc (p)").unwrap();
    assert_eq!(
        node.rows("SELECT relhastriggers FROM pg_class WHERE relname = 'tci'"),
        [["f"]]
    );
}
