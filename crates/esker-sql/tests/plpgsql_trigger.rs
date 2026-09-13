//! Row triggers and their PL/pgSQL functions — the subset [ADR 0113] builds — held to PostgreSQL
//! 19's answers.
//!
//! `corpus/pg19_plpgsql_trigger.txt` is the capture, in three sessions: `BEFORE` and `AFTER` on
//! every event, enabling and disabling, a body's errors; the census's two trigger shapes statement
//! for statement, and the shapes just outside the subset; and what `NEW`, `OLD`, generated columns
//! and a foreign key's actions do around a trigger. The replay is the test of record. The tests
//! after it pin what each shape is for, so that a failure names it rather than a line.
//!
//! [ADR 0113]: ../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The reason the refusals below share.
const OUTSIDE: &str = "a trigger shape outside the subset of ADR 0113, which the Rails suite never \
                       sends: PostgreSQL runs it, and this node refuses it by name rather than \
                       store a trigger that would fire in an order of its own \
                       (docs/plans/plpgsql-subset.md §7)";

/// The reason a `RETURNING` a trigger left without a row is listed.
const RETURNED_NO_ROW: &str = "the capture records a `RETURNING` that returned no row as a command: \
                               `\\gdesc` describes each statement on a connection of its own, \
                               where the capture's uncommitted table does not exist, so it has no \
                               types to tell a result set of no rows from a command. This node \
                               answers the result set — `integer`, and no row — which is what \
                               an `INSERT … RETURNING` sends";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "insert into postgresql_partitioned_table_parent (number) VALUES (1) RETURNING id",
            RETURNED_NO_ROW,
            "pg19_plpgsql_trigger.txt:152",
        ),
        (
            "select 'r', count(*) from only postgresql_partitioned_table_parent",
            "`FROM ONLY` is read as a relation called `only` — a gap older than triggers and not \
             about them, which is why the capture holds it in a savepoint of its own. The rows it \
             counts are asserted through inheritance on the lines around it",
            "pg19_plpgsql_trigger.txt:157",
        ),
        (
            "INSERT INTO t VALUES (1, 'one') RETURNING id",
            RETURNED_NO_ROW,
            "pg19_plpgsql_trigger.txt:233",
        ),
        (
            "INSERT INTO s2t_rec VALUES (1)",
            "the code and the sentence are PostgreSQL's and the HINT is not: it names \
             `max_stack_depth`, a setting this node does not have, where the bound here is the \
             number of PL/pgSQL bodies running inside one another (`exec::plpgsql::MAX_DEPTH`)",
            "pg19_plpgsql_trigger.txt:132",
        ),
        (
            "CREATE TRIGGER s2t_stmt_t AFTER INSERT ON s2t FOR EACH STATEMENT EXECUTE FUNCTION \
             s2t_stmt()",
            OUTSIDE,
            "pg19_plpgsql_trigger.txt:166",
        ),
        (
            "CREATE TRIGGER s2t_args_t BEFORE INSERT ON s2t FOR EACH ROW EXECUTE FUNCTION \
             s2t_upper('x')",
            OUTSIDE,
            "pg19_plpgsql_trigger.txt:169",
        ),
        (
            "CREATE TRIGGER s2t_parted_t BEFORE INSERT ON s2t_parted FOR EACH ROW EXECUTE \
             FUNCTION s2t_stmt()",
            OUTSIDE,
            "pg19_plpgsql_trigger.txt:174",
        ),
        (
            "CREATE TRIGGER s2t_parted_a_t BEFORE INSERT ON s2t_parted_a FOR EACH ROW EXECUTE \
             FUNCTION s2t_stmt()",
            OUTSIDE,
            "pg19_plpgsql_trigger.txt:177",
        ),
        (
            "INSERT INTO s2t (id, name) VALUES (4, 'conflict') ON CONFLICT (id) DO UPDATE SET \
             name = 'updated'",
            OUTSIDE,
            "pg19_plpgsql_trigger.txt:182",
        ),
        (
            "CREATE TRIGGER s2t_int_t BEFORE INSERT ON s2t FOR EACH ROW EXECUTE FUNCTION \
             s2t_int()",
            "a function's return type is not stored — `FunctionDef` has no field for it, and one \
             is a format change no suite statement needs (docs/plans/plpgsql-subset.md §7) — so \
             this node cannot tell a function returning `integer` from one returning `trigger`, \
             and stores the trigger",
            "pg19_plpgsql_trigger.txt:201",
        ),
        (
            "INSERT INTO t SELECT n, 'sel' FROM generate_series(20, 21) n RETURNING id, name",
            "`INSERT … SELECT` is refused where the statement is lowered, before any trigger could \
             fire — a gap older than triggers and not about them. The per-row firing this row \
             shows is the `VALUES` path's, which every other insert here measures",
            "pg19_plpgsql_trigger.txt:292",
        ),
    ],
};

#[test]
fn every_plpgsql_trigger_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_plpgsql_trigger.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 250,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`persistence_test.rb:1708`**: `PkAutopopulatedByATriggerRecord.create`. The table's key has
/// no default, and the id the record reads back through `RETURNING` is the one the trigger put
/// there — even over a value the statement wrote.
#[test]
fn the_persistence_tests_record_takes_its_id_from_the_trigger() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE \"pk_autopopulated_by_a_trigger_records\" (\"id\" integer NOT NULL)",
        "CREATE OR REPLACE FUNCTION populate_column() RETURNS TRIGGER AS $$ DECLARE max_value \
         INTEGER; BEGIN SELECT MAX(id) INTO max_value FROM pk_autopopulated_by_a_trigger_records; \
         NEW.id = COALESCE(max_value, 0) + 1; RETURN NEW; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER before_insert_trigger BEFORE INSERT ON \
         \"pk_autopopulated_by_a_trigger_records\" FOR EACH ROW EXECUTE FUNCTION populate_column()",
    ]);
    for expected in ["1", "2"] {
        assert_eq!(
            node.rows(
                "INSERT INTO \"pk_autopopulated_by_a_trigger_records\" DEFAULT VALUES RETURNING \
                 \"id\""
            ),
            [[expected]]
        );
    }
    assert_eq!(
        node.rows(
            "INSERT INTO \"pk_autopopulated_by_a_trigger_records\" (\"id\") VALUES (99) RETURNING \
             \"id\""
        ),
        [["3"]]
    );
}

/// **Statement 762's trigger**: a row inserted into the parent goes into the child instead, through
/// `NEW.*` — so the parent's insert writes, returns and counts nothing, and the two rows the parent
/// reaches through inheritance are the two the child holds.
#[test]
fn statement_762s_trigger_moves_each_row_into_the_child() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE postgresql_partitioned_table_parent ( id SERIAL PRIMARY KEY, number integer )",
        "CREATE TABLE postgresql_partitioned_table ( ) INHERITS \
         (postgresql_partitioned_table_parent)",
        "CREATE OR REPLACE FUNCTION partitioned_insert_trigger() RETURNS TRIGGER AS $$ BEGIN \
         INSERT INTO postgresql_partitioned_table VALUES (NEW.*); RETURN NULL; END; $$ LANGUAGE \
         plpgsql",
        "CREATE TRIGGER insert_partitioning_trigger BEFORE INSERT ON \
         postgresql_partitioned_table_parent FOR EACH ROW EXECUTE PROCEDURE \
         partitioned_insert_trigger()",
    ]);
    let outcome = node
        .run("insert into postgresql_partitioned_table_parent (number) VALUES (1)")
        .unwrap();
    assert_eq!(format!("{outcome:?}"), "Done { tag: \"INSERT 0 0\" }");
    assert_eq!(
        node.rows(
            "insert into postgresql_partitioned_table_parent (number) VALUES (2) RETURNING id"
        ),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(
        node.rows("SELECT max(id), count(*) FROM postgresql_partitioned_table_parent"),
        [["2", "2"]]
    );
    assert_eq!(
        node.rows("SELECT id, number FROM postgresql_partitioned_table ORDER BY id"),
        [["1", "1"], ["2", "2"]]
    );
}

/// **`DISABLE TRIGGER ALL` is what Rails wraps a fixture load in**, and a disabled trigger does not
/// fire: the fixture's row goes in as written, and the trigger fires again once it is enabled.
#[test]
fn a_trigger_disabled_for_a_fixture_load_does_not_fire() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer, name text)",
        "CREATE FUNCTION shout() RETURNS trigger AS $$ BEGIN NEW.name = upper(NEW.name); RETURN \
         NEW; END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER t_shout BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION shout()",
    ]);
    node.run("ALTER TABLE t DISABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO t VALUES (1, 'fixture')").unwrap();
    node.run("ALTER TABLE t ENABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO t VALUES (2, 'live')").unwrap();
    assert_eq!(
        node.rows("SELECT id, name FROM t ORDER BY id"),
        [["1", "fixture"], ["2", "LIVE"]]
    );
}

/// **A foreign key's actions fire the child's triggers**, because PostgreSQL runs each as a
/// statement on the child: `ON UPDATE CASCADE` and `SET NULL` its `UPDATE` triggers, and
/// `ON DELETE CASCADE` its `DELETE` ones.
#[test]
fn a_foreign_keys_actions_fire_the_childs_triggers() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE audit (seq serial PRIMARY KEY, note text)",
        "CREATE TABLE p (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer PRIMARY KEY, p integer REFERENCES p ON DELETE CASCADE ON \
         UPDATE CASCADE)",
        "CREATE TABLE n (id integer PRIMARY KEY, p integer REFERENCES p ON DELETE SET NULL)",
        "CREATE FUNCTION deleting() RETURNS trigger AS $$ BEGIN INSERT INTO audit (note) VALUES \
         ('delete ' || OLD.id); RETURN OLD; END $$ LANGUAGE plpgsql",
        "CREATE FUNCTION updating() RETURNS trigger AS $$ BEGIN INSERT INTO audit (note) VALUES \
         ('update ' || OLD.id || ' to ' || coalesce(NEW.p::text, 'null')); RETURN NEW; END $$ \
         LANGUAGE plpgsql",
        "CREATE TRIGGER c_delete BEFORE DELETE ON c FOR EACH ROW EXECUTE FUNCTION deleting()",
        "CREATE TRIGGER c_update AFTER UPDATE ON c FOR EACH ROW EXECUTE FUNCTION updating()",
        "CREATE TRIGGER n_update BEFORE UPDATE ON n FOR EACH ROW EXECUTE FUNCTION updating()",
        "INSERT INTO p VALUES (1), (2)",
        "INSERT INTO c VALUES (10, 1), (20, 2)",
        "INSERT INTO n VALUES (30, 1)",
    ]);
    node.run("UPDATE p SET id = 3 WHERE id = 2").unwrap();
    node.run("DELETE FROM p WHERE id = 1").unwrap();
    assert_eq!(
        node.rows("SELECT note FROM audit ORDER BY note"),
        [["delete 10"], ["update 20 to 3"], ["update 30 to null"]]
    );
    assert_eq!(node.rows("SELECT id, p FROM c ORDER BY id"), [["20", "3"]]);
    assert_eq!(node.rows("SELECT id, p IS NULL FROM n"), [["30", "t"]]);
}

/// **What a trigger writes is its statement's**: a statement that fails after its trigger ran
/// leaves nothing of what the trigger wrote.
#[test]
fn a_triggers_writes_are_undone_with_its_statement() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE audit (note text)",
        "CREATE TABLE u (id integer PRIMARY KEY)",
        "CREATE FUNCTION noted() RETURNS trigger AS $$ BEGIN INSERT INTO audit VALUES ('u ' || \
         NEW.id); RETURN NEW; END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER u_noted BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION noted()",
        "INSERT INTO u VALUES (1)",
    ]);
    assert_eq!(
        node.answer("INSERT INTO u VALUES (2), (1)").to_string(),
        "!23505 duplicate key value violates unique constraint \"u_pkey\" DETAIL: Key (id)=(1) \
         already exists."
    );
    assert_eq!(node.rows("SELECT note FROM audit"), [["u 1"]]);
}

/// **A trigger that fires itself stops at the bound**, as PostgreSQL's stops at its stack depth:
/// the statement is `54001`, and nothing of it is left.
#[test]
fn a_trigger_that_fires_itself_is_stopped_at_the_bound() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE r (id integer)",
        "CREATE FUNCTION again() RETURNS trigger AS $$ BEGIN INSERT INTO r VALUES (NEW.id + 1); \
         RETURN NEW; END $$ LANGUAGE plpgsql",
        "CREATE TRIGGER r_again BEFORE INSERT ON r FOR EACH ROW EXECUTE FUNCTION again()",
    ]);
    assert_eq!(
        node.answer("INSERT INTO r VALUES (1)").to_string(),
        "!54001 stack depth limit exceeded"
    );
    assert_eq!(node.rows("SELECT count(*) FROM r"), [["0"]]);
}
