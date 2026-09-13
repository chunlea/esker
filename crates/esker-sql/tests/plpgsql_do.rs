//! `DO $$ … $$` in PL/pgSQL — the subset [ADR 0113] builds — held to PostgreSQL 19's answers.
//!
//! `corpus/pg19_plpgsql_do.txt` is the capture: every construct of the subset, the sentences a
//! malformed body answers, and the forms just outside the subset, which a real server runs and this
//! node refuses by name. The replay is the test of record. The tests after it pin the one thing each
//! construct is for, so that a failure names the construct rather than a line of the capture.
//!
//! [ADR 0113]: ../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The reason every declared divergence below shares.
const OUTSIDE: &str = "a construct outside the PL/pgSQL subset of ADR 0113, which the Rails suite \
                       never sends: PostgreSQL runs it, and this node refuses it by name before \
                       any of the body runs (docs/plans/plpgsql-subset.md §11)";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "DO $$ DECLARE n integer := 1; BEGIN IF n = 1 THEN NULL; ELSE NULL; END IF; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:98",
        ),
        (
            "DO $$ BEGIN PERFORM 1; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:101",
        ),
        (
            "DO $$ BEGIN WHILE false LOOP NULL; END LOOP; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:110",
        ),
        (
            "DO $$ BEGIN BEGIN NULL; END; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:113",
        ),
        (
            "DO $$ BEGIN NULL; EXCEPTION WHEN others THEN NULL; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:116",
        ),
        (
            "DO $$ BEGIN RAISE WARNING 'a %', 'b'; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:171",
        ),
        (
            "DO $$ DECLARE n integer; BEGIN FOR n IN SELECT 1 LOOP NULL; END LOOP; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:183",
        ),
        (
            "DO $$ BEGIN FOR i IN 1..3 LOOP NULL; END LOOP; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:186",
        ),
        (
            "DO $$ DECLARE r record; BEGIN r = NULL; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:189",
        ),
        (
            "DO $$ BEGIN RAISE WARNING 'x' USING HINT = 'y'; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:198",
        ),
        (
            "DO $$ BEGIN RAISE INFO 'i'; END $$",
            "`INFO` has no severity token on this wire (`error::Severity` has four), and \
             downgrading it would print a client the wrong word — ADR 0058's reason, unchanged \
             by ADR 0113",
            "pg19_plpgsql_do.txt:201",
        ),
        (
            "DO $$ BEGIN IF found THEN NULL; END IF; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:207",
        ),
        (
            "DO $$ DECLARE n integer; m integer; BEGIN SELECT 1, 2 INTO n, m; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:216",
        ),
        (
            "DO $$ DECLARE r record; BEGIN SELECT 1 AS a INTO r; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:219",
        ),
        (
            "DO $$ DECLARE n integer; BEGIN INSERT INTO nosuch_t VALUES (1) RETURNING 1 INTO n; \
             END $$",
            "`INSERT … RETURNING … INTO` is outside the subset, and a body is read whole before \
             any of it runs — so this node names the construct where PostgreSQL, which runs it, \
             reaches the missing table first",
            "pg19_plpgsql_do.txt:222",
        ),
        (
            "DO $$ BEGIN RAISE EXCEPTION 'x' USING ERRCODE = 'P0002'; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:225",
        ),
        (
            "DO $$ DECLARE r record; BEGIN FOR r IN INSERT INTO s2p_t VALUES (42, 'returned') \
             RETURNING id LOOP NULL; END LOOP; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:249",
        ),
        (
            "DO $$ BEGIN SET LOCAL search_path = public; END $$",
            OUTSIDE,
            "pg19_plpgsql_do.txt:255",
        ),
    ],
};

#[test]
fn every_plpgsql_do_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_plpgsql_do.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 150,
        "only {checked} statements ran; the corpus did not load"
    );
}

fn node() -> parity::Node {
    parity::Node::new(&["CREATE TABLE t (id integer PRIMARY KEY, name text)"])
}

/// **`DECLARE`, and `SELECT … INTO`**: a variable starts `NULL`, takes the first column of the first
/// row, and a query that finds nothing leaves it `NULL`.
#[test]
fn a_variable_starts_null_and_takes_the_first_value_of_a_select_into() {
    let mut node = node();
    node.run("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    node.run(
        "DO $$ DECLARE n integer; m integer; BEGIN SELECT id, name INTO n FROM t ORDER BY id; \
         SELECT id INTO m FROM t WHERE id = 99; INSERT INTO t VALUES (n + 10, m::text); END $$",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, name IS NULL FROM t WHERE id = 11"),
        [["11", "t"]]
    );
}

/// **An assignment converts to the variable's type** — through an assignment cast where there is
/// one, through the value's text where there is not — and `=` and `:=` are one operator.
#[test]
fn an_assignment_converts_to_the_variables_type() {
    let mut node = node();
    node.run(
        "DO $$ DECLARE a integer; b integer; c bigint; BEGIN a = 7.6; b := '7'; SELECT count(*) \
         INTO c FROM pg_class; INSERT INTO t VALUES (a, 'a'), (b, 'b'); END $$",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, name FROM t ORDER BY id"),
        [["7", "b"], ["8", "a"]]
    );
    assert_eq!(
        node.answer("DO $$ DECLARE n integer; BEGIN n = 'abc'; END $$")
            .to_string(),
        "!22P02 invalid input syntax for type integer: \"abc\""
    );
    assert_eq!(
        node.answer("DO $$ DECLARE n integer; BEGIN n = 1, 2; END $$")
            .to_string(),
        "!42601 assignment source returned 2 columns"
    );
}

/// **`IF … THEN … END IF`** runs its statements only when the condition is true; `NULL` is not, and
/// a non-boolean is read through its text.
#[test]
fn if_runs_its_statements_only_when_the_condition_is_true() {
    let mut node = node();
    node.run(
        "DO $$ BEGIN IF true THEN INSERT INTO t VALUES (1, 'true'); END IF; IF false THEN INSERT \
         INTO t VALUES (2, 'false'); END IF; IF NULL THEN INSERT INTO t VALUES (3, 'null'); END \
         IF; IF 1 THEN INSERT INTO t VALUES (4, 'one'); END IF; END $$",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT id FROM t ORDER BY id"), [["1"], ["4"]]);
}

/// **`RAISE`**: a notice at its level, an exception as `P0001` with the literal as the whole message,
/// and a bare `RAISE` as PostgreSQL's `0Z002`.
#[test]
fn raise_answers_at_its_level() {
    let mut node = node();
    node.run("DO $$ BEGIN RAISE NOTICE 'a %% sign'; RAISE WARNING 'careful'; END $$")
        .unwrap();
    let notices: Vec<(String, String)> = node
        .executor_notices()
        .iter()
        .map(|notice| (notice.severity().as_str().to_owned(), notice.to_string()))
        .collect();
    assert_eq!(
        notices,
        [
            ("NOTICE".to_owned(), "a % sign".to_owned()),
            ("WARNING".to_owned(), "careful".to_owned())
        ]
    );
    assert_eq!(
        node.answer("DO $$ BEGIN RAISE 'no level'; END $$")
            .to_string(),
        "!P0001 no level"
    );
    assert_eq!(
        node.answer("DO $$ BEGIN RAISE; END $$").to_string(),
        "!0Z002 RAISE without parameters cannot be used outside an exception handler"
    );
}

/// **`FOR <record> IN <query> LOOP`** runs its body once per row with the row in the record, and a
/// field is read by its column's name.
#[test]
fn a_for_loop_runs_its_body_once_per_row() {
    let mut node = node();
    node.run("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    node.run(
        "DO $$ DECLARE r record; BEGIN FOR r IN (SELECT id, name FROM t ORDER BY id) LOOP INSERT \
         INTO t VALUES (r.id + 100, r.name || '!'); END LOOP; END $$",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, name FROM t WHERE id > 100 ORDER BY id"),
        [["101", "one!"], ["102", "two!"]]
    );
    assert_eq!(
        node.answer("DO $$ DECLARE r record; BEGIN IF r.id = 1 THEN NULL; END IF; END $$")
            .to_string(),
        "!55000 record \"r\" is not assigned yet DETAIL: The tuple structure of a \
         not-yet-assigned record is indeterminate."
    );
    assert_eq!(
        node.answer(
            "DO $$ DECLARE r record; BEGIN FOR r IN SELECT 1 AS a LOOP IF r.b = 1 THEN NULL; END \
             IF; END LOOP; END $$"
        )
        .to_string(),
        "!42703 record \"r\" has no field \"b\""
    );
}

/// **`EXECUTE`** runs the text its expression evaluates to — every statement in it — and refuses
/// `NULL` and transaction commands in PostgreSQL's words.
#[test]
fn execute_runs_the_text_its_expression_evaluates_to() {
    let mut node = node();
    node.run(
        "DO $$ DECLARE tbl text; BEGIN tbl = 't'; EXECUTE 'INSERT INTO ' || tbl || ' VALUES (1, \
         ''x''); INSERT INTO t VALUES (2, ''y'')'; EXECUTE 'SELECT 1'; END $$",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, name FROM t ORDER BY id"),
        [["1", "x"], ["2", "y"]]
    );
    assert_eq!(
        node.answer("DO $$ BEGIN EXECUTE NULL; END $$").to_string(),
        "!22004 query string argument of EXECUTE is null"
    );
    assert_eq!(
        node.answer("DO $$ BEGIN EXECUTE 'BEGIN'; END $$")
            .to_string(),
        "!0A000 EXECUTE of transaction commands is not implemented"
    );
}

/// **The loop `check_all_foreign_keys_valid!` is built from** — a record, a query, `EXECUTE` of a
/// field — which `tests/user_decided_divergences.rs` pinned as refused until the user's ruling of
/// 2026-09-13 reopened it.
#[test]
fn a_loop_that_executes_a_field_runs() {
    let mut node = node();
    assert_eq!(
        node.answer(
            "DO $$ DECLARE r record; BEGIN FOR r IN (SELECT 1 AS x) LOOP EXECUTE 'SELECT 1'; END \
             LOOP; END $$"
        )
        .to_string(),
        "(a command, no result set)"
    );
}

/// **An SQL statement in a body runs in the statement's transaction**: its effects stay, an error
/// part-way undoes the whole block, and a `ROLLBACK` around the `DO` takes everything it did.
#[test]
fn a_body_runs_in_the_statements_transaction() {
    let mut node = node();
    node.run(
        "DO $$ BEGIN CREATE TABLE made_here (a int); INSERT INTO made_here VALUES (1); END $$",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT a FROM made_here"), [["1"]]);

    node.run("INSERT INTO t VALUES (1, 'one')").unwrap();
    let error = node
        .run("DO $$ BEGIN INSERT INTO t VALUES (30, 'first'); INSERT INTO t VALUES (1, 'dup'); END $$")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(node.rows("SELECT count(*) FROM t WHERE id = 30"), [["0"]]);

    node.run("BEGIN").unwrap();
    node.run("DO $$ BEGIN INSERT INTO t VALUES (40, 'rolled back'); END $$")
        .unwrap();
    node.run("ROLLBACK").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM t WHERE id = 40"), [["0"]]);
}

/// **Rows with nowhere to go are refused**: a `SELECT` with the `HINT` naming `PERFORM`, an
/// `INSERT … RETURNING` without it.
#[test]
fn rows_with_nowhere_to_go_are_refused() {
    let mut node = node();
    assert_eq!(
        node.answer("DO $$ BEGIN SELECT 1; END $$").to_string(),
        "!42601 query has no destination for result data HINT: If you want to discard the \
         results of a SELECT, use PERFORM instead."
    );
    assert_eq!(
        node.answer("DO $$ BEGIN INSERT INTO t VALUES (1, 'x') RETURNING id; END $$")
            .to_string(),
        "!42601 query has no destination for result data"
    );
    assert_eq!(node.rows("SELECT count(*) FROM t"), [["0"]]);
}

/// **A name that is both a variable and a column is `42702`**, PostgreSQL's
/// `plpgsql.variable_conflict = error` — never silently the variable.
#[test]
fn a_variable_that_is_also_a_column_is_ambiguous() {
    let mut node = node();
    assert_eq!(
        node.answer(
            "DO $$ DECLARE id integer; BEGIN SELECT name INTO id FROM t WHERE id = 1; END $$"
        )
        .to_string(),
        "!42702 column reference \"id\" is ambiguous DETAIL: It could refer to either a PL/pgSQL \
         variable or a table column."
    );
}

/// **`RETURN` ends a `DO` block**, and the statements after it do not run.
#[test]
fn return_ends_the_block() {
    let mut node = node();
    node.run("DO $$ BEGIN INSERT INTO t VALUES (1, 'before'); RETURN; INSERT INTO t VALUES (2, 'after'); END $$")
        .unwrap();
    assert_eq!(node.rows("SELECT id FROM t"), [["1"]]);
}

/// **A body is read whole before any of it runs**, so a construct outside the subset refuses the
/// statement with nothing done — not half a block.
#[test]
fn a_body_is_read_whole_before_any_of_it_runs() {
    let mut node = node();
    let error = node
        .run("DO $$ BEGIN INSERT INTO t VALUES (1, 'x'); PERFORM 1; END $$")
        .unwrap_err();
    assert_eq!(error.to_string(), "PL/pgSQL PERFORM is not supported");
    assert_eq!(node.rows("SELECT count(*) FROM t"), [["0"]]);
}

/// **A `DO` that `EXECUTE`s a `DO` is bounded**: past the bound the statement is PostgreSQL's
/// `54001`, and inside it the nesting simply runs.
#[test]
fn a_do_inside_a_do_is_bounded() {
    fn nested(depth: usize) -> String {
        let mut sql = "INSERT INTO t VALUES (1, 'deepest')".to_owned();
        for level in 0..depth {
            sql = format!("DO $d{level}$ BEGIN EXECUTE $e{level}${sql}$e{level}$; END $d{level}$");
        }
        sql
    }
    let mut node = node();
    let error = node.run(&nested(40)).unwrap_err();
    assert_eq!(error.sqlstate(), "54001");
    node.run(&nested(4)).unwrap();
    assert_eq!(node.rows("SELECT name FROM t"), [["deepest"]]);
}
