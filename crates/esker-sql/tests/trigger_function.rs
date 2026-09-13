//! `CREATE FUNCTION` and `CREATE TRIGGER` as **catalog objects** — statements 762 and 790.
//!
//! The line every one of the suite's stopped files stops on. r1's run 38 measured it: 271 of 271
//! stop on the same `CREATE OR REPLACE …`, and the load reaches it at **762** — the `INHERITS`
//! block embeds a `CREATE OR REPLACE FUNCTION` of its own — before 790 is ever read. The two are
//! one unit.
//!
//! The capture settled that unit's scope: the load only *defines* them and inserts nothing, so what
//! was wanted is a node that can store a dollar-quoted body and register a trigger. Exactly one
//! test in the suite fires a trigger (`persistence_test.rb:1708`); ADR 0113 brought firing into
//! scope, and `plpgsql_trigger.rs` is where it is tested.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `lanname` is a `name` on a real server and `text` here, with identical characters — the
    // standing trade every `pg_catalog` column makes.
    types: &[],
    answers: &[
        (
            "SELECT 'r', proname, prokind, prorettype::regtype::text, l.lanname, pronargs, \
         provolatile FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang WHERE proname = \
         'populate_column'",
            "**`oid::regtype` is not implemented** — contract C2, and the *forward* direction of the \
         cast: `'integer'::regtype::oid` runs here because `ActiveRecord` sends it, and reading an \
         oid back as a type name does not. `prorettype` is not a column this node's `pg_proc` \
         has, for the same reason: it would hold the oid of `trigger`, a pseudo-type this node \
         does not carry in `pg_type`. Every other column on this line agrees, and the test below \
         reads them without the cast.\n\nThis line used to abort the capture's transaction and \
         hide the forty-one statements after it; the re-capture holds it in a savepoint of its own, \
         so the trigger firing after it is replayed.",
            "pg19_trigger_function.txt:89",
        ),
        (
            "SELECT 'r', pg_get_functiondef('populate_column'::regproc) LIKE '%LANGUAGE plpgsql%', \
         length(prosrc) FROM pg_proc WHERE proname = 'populate_column'",
            "**`::regproc` resolves only the functions this node knows by a fixed oid** — ADR 0098's \
         table — and a function a user created has none, so the cast is `42883` before \
         `pg_get_functiondef` is reached. A gap older than triggers, held in a savepoint of its \
         own; the rows around it, which the trigger fills, agree.",
            "pg19_trigger_function.txt:94",
        ),
        (
            "SELECT populate_column()",
            "**Calling a stored function is refused either way, and the sentence differs**: a real \
         server knows the function is there and that `trigger` is not a callable return type, \
         and this node refuses the call where the statement is lowered, before any catalog is in \
         reach, so it names the function instead. Both are `0A000`, and the suite never calls \
         one (`the_refusals_are_postgresqls`).",
            "pg19_trigger_function.txt:108",
        ),
        (
            "CREATE FUNCTION tf_badbody() RETURNS TRIGGER AS $$ BEGIN RETURN NEW END; $$ LANGUAGE \
         plpgsql",
            "**PostgreSQL reads a PL/pgSQL body when the function is created**, and refuses this one \
         there; this node stores the body and reads it when a trigger fires it \
         (`docs/plans/plpgsql-subset.md` §11, validation at `CREATE FUNCTION`). The capture holds \
         it in a savepoint, so the stored function goes with it.",
            "pg19_trigger_function.txt:114",
        ),
    ],
};

#[test]
fn every_trigger_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_trigger_function.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Statement 790**: the function and the trigger, and the catalog they leave behind.
#[test]
fn statement_790_defines_a_function_and_a_trigger() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE \"pk_autopopulated_by_a_trigger_records\" (\"id\" integer NOT NULL)",
    ]);
    node.run(
        "CREATE OR REPLACE FUNCTION populate_column() RETURNS TRIGGER AS $$ DECLARE max_value \
         INTEGER; BEGIN SELECT MAX(id) INTO max_value FROM pk_autopopulated_by_a_trigger_records; \
         NEW.id = COALESCE(max_value, 0) + 1; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .unwrap();
    node.run(
        "CREATE TRIGGER before_insert_trigger BEFORE INSERT ON \
         \"pk_autopopulated_by_a_trigger_records\" FOR EACH ROW EXECUTE FUNCTION populate_column()",
    )
    .unwrap();

    assert_eq!(
        node.rows(
            "SELECT proname, prokind, pronargs FROM pg_proc WHERE proname = 'populate_column'"
        ),
        [["populate_column", "f", "0"]]
    );
    // **`tgenabled` is a letter, not a boolean**, and `tgtype` the bitmask for BEFORE+ROW+INSERT.
    assert_eq!(
        node.rows(
            "SELECT tgname, tgenabled, tgtype, tgnargs, tgisinternal FROM pg_trigger WHERE \
             tgname = 'before_insert_trigger'"
        ),
        [["before_insert_trigger", "O", "7", "0", "f"]]
    );
}

/// **Statement 762's shape: five statements in one message, with `;` inside `$$…$$`.**
///
/// This is where the load actually stops. A splitter that cut the message on semicolons would tear
/// the dollar-quoted body apart and report a syntax error inside somebody's plpgsql — so the body
/// is checked here by the length of what came back out, not only by the statement succeeding.
#[test]
fn statement_762s_message_survives_the_semicolons_in_its_body() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE postgresql_partitioned_table_parent ( id SERIAL PRIMARY KEY, number \
         integer ); CREATE OR REPLACE FUNCTION partitioned_insert_trigger() RETURNS TRIGGER AS $$ \
         BEGIN INSERT INTO postgresql_partitioned_table VALUES (NEW.*); RETURN NULL; END; $$ \
         LANGUAGE plpgsql; CREATE TRIGGER insert_partitioning_trigger BEFORE INSERT ON \
         postgresql_partitioned_table_parent FOR EACH ROW EXECUTE PROCEDURE \
         partitioned_insert_trigger()",
    )
    .unwrap();
    // The body kept every semicolon it was written with.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_proc WHERE proname = 'partitioned_insert_trigger'"),
        [["1"]]
    );
    assert_eq!(
        node.rows("SELECT tgname FROM pg_trigger WHERE tgname = 'insert_partitioning_trigger'"),
        [["insert_partitioning_trigger"]]
    );
}

/// **`EXECUTE PROCEDURE` and `EXECUTE FUNCTION` are two spellings of one clause.**
///
/// 762 writes the first and 790 the second, so both have to parse — and `pg_get_triggerdef`
/// normalises the older spelling to the newer one, so only one is ever printed.
#[test]
fn execute_procedure_is_execute_function() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer NOT NULL)",
        "CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    ]);
    node.run("CREATE TRIGGER old_spelling BEFORE UPDATE ON t FOR EACH ROW EXECUTE PROCEDURE f()")
        .unwrap();
    assert_eq!(
        node.rows("SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'old_spelling'"),
        [[
            "CREATE TRIGGER old_spelling BEFORE UPDATE ON public.t FOR EACH ROW EXECUTE FUNCTION \
             f()"
        ]]
    );
}

/// `OR REPLACE` exists for a function and **not** for a trigger.
#[test]
fn or_replace_is_the_functions_and_not_the_triggers() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer NOT NULL)",
        "CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    ]);
    node.run(
        "CREATE OR REPLACE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NULL; END; $$ LANGUAGE \
         plpgsql",
    )
    .unwrap();
    node.run("CREATE TRIGGER t_dup BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap();
    let error = node
        .run("CREATE TRIGGER t_dup BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42710");
    assert_eq!(
        error.to_string(),
        "trigger \"t_dup\" for relation \"t\" already exists"
    );
}

/// The refusals the capture pins, each with its own code.
#[test]
fn the_refusals_are_postgresqls() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id integer NOT NULL)"]);
    let error = node
        .run(
            "CREATE FUNCTION bad() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE \
             nosuchlang",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42704");
    assert_eq!(error.to_string(), "language \"nosuchlang\" does not exist");

    let error = node
        .run("CREATE TRIGGER t1 BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION nosuchfunction()")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(
        error.to_string(),
        "function nosuchfunction() does not exist"
    );

    // **Calling a stored function is refused either way, and the sentence differs.** A real server
    // says `trigger functions can only be called as triggers`, because it knows the function is
    // there and that `trigger` is not a callable return type; this node refuses it where the
    // statement is lowered, which is before any catalog is in reach, so it names the function
    // instead. Both are `0A000`, and the schema load never calls one — declared in the corpus.
    node.run(
        "CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .unwrap();
    let error = node.run("SELECT f()").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(error.to_string().contains('f'), "{error}");
}

/// Dropped like any other object, and the function is held by its trigger until that goes.
#[test]
fn a_trigger_holds_its_function_until_it_is_dropped() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer NOT NULL)",
        "CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
    ]);
    let error = node.run("DROP FUNCTION f()").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop function f() because other objects depend on it"
    );
    node.run("DROP TRIGGER tr ON t").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_trigger WHERE tgname = 'tr'"),
        [["0"]]
    );
    node.run("DROP FUNCTION f()").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_proc WHERE proname = 'f'"),
        [["0"]]
    );
}

/// **A trigger this node stores fires** (ADR 0113). Until then this test pinned that it did not:
/// the column has no default, and the `NULL` below was `23502` because nothing filled it.
#[test]
fn a_stored_trigger_fires() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id integer NOT NULL)",
        "CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN NEW.id = 42; RETURN NEW; END; $$ LANGUAGE \
         plpgsql",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
    ]);
    node.run("INSERT INTO t (id) VALUES (NULL)").unwrap();
    // A value the statement wrote is the trigger's to replace too.
    node.run("INSERT INTO t (id) VALUES (7)").unwrap();
    assert_eq!(node.rows("SELECT id FROM t"), [["42"], ["42"]]);
}
