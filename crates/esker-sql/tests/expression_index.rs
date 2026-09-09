//! An expression index, against PostgreSQL 19beta1 — statement 78 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_class.relname` is a `name` on a real server and `text` here — the type this node does
    // not have, provided where the client's use of it is text-shaped (`catalog::pg_index`). The
    // rows agree; only the declared type does not.
    types: &[],
    // Three, all of them `pg_get_indexdef`'s **text** and none of them the index's behaviour:
    // this node stores the expression as its parser renders it and PostgreSQL re-prints a parsed
    // tree, so the two agree wherever sqlparser's rendering is PostgreSQL's and differ where it
    // is not. Both differences below are reachable and neither changes which rows are indexed.
    //
    // `lower(a)` over an `int8` column names `lower(bigint)` on both servers, which is why it is
    // *not* here — the bare-integer divergence `tests/unknown_literal.rs` declares is about a
    // literal, and a column carries its own type into the message.
    answers: &[
        (
            "SELECT pg_get_indexdef('xidx_not'::regclass, 1, true)",
            "the per-column form drops parentheses the whole definition keeps: PostgreSQL prints \
             (NOT b IS NULL) for one key part and ((NOT (b IS NULL))) inside the key list, and \
             this node prints the stored text in both",
            "UNMEASURED",
        ),
        (
            "SELECT pg_get_indexdef('xidx_cast'::regclass)",
            "a cast prints its target type in upper case (a::TEXT) and does not parenthesise its \
             operand, where PostgreSQL's deparser prints (a)::text",
            "UNMEASURED",
        ),
        (
            "SELECT pg_get_indexdef('xidx_cast'::regclass, 1, true)",
            "the same cast, one key part at a time",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_expression_index_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_expression_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The key is the **expression's** value, so two rows differing only in case collide.
///
/// This is the whole feature in four statements, and the one an index built over the column
/// instead of over the expression passes by accident: `Alpha` and `ALPHA` are different `text`
/// values and the same `lower(text)` one.
#[test]
fn the_unique_key_is_the_expressions_value_and_not_the_columns() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))",
        "INSERT INTO xi VALUES (1, 'Alpha')",
    ] {
        node.run(statement).unwrap();
    }

    let error = node.run("INSERT INTO xi VALUES (2, 'ALPHA')").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    // The DETAIL names the expression, not the column under it.
    assert!(
        error.to_string().contains("xi_expr"),
        "the constraint is named: {error}"
    );

    // A value with a different `lower` is admitted, and so is the row the first one blocked once
    // it no longer collides.
    node.run("INSERT INTO xi VALUES (2, 'beta')").unwrap();
    assert_eq!(
        node.rows("SELECT id, b FROM xi ORDER BY id"),
        vec![vec!["1", "Alpha"], vec!["2", "beta"]]
    );
}

/// An `UPDATE` moves the expression's value, and both directions have to be maintained.
///
/// The half a write-only implementation gets wrong: the entry for the row's *old* value has to go
/// before the entry for its new one arrives, or the second update below collides with a key its
/// own row still owns.
#[test]
fn an_update_moves_the_expressions_value() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))",
        "INSERT INTO xi VALUES (1, 'Alpha')",
        "INSERT INTO xi VALUES (2, 'beta')",
    ] {
        node.run(statement).unwrap();
    }

    // Row 1 onto row 2's key.
    let error = node
        .run("UPDATE xi SET b = 'BETA' WHERE id = 1")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");

    // Row 1 somewhere free, and then row 2 onto the key row 1 has just left.
    node.run("UPDATE xi SET b = 'delta' WHERE id = 1").unwrap();
    node.run("UPDATE xi SET b = 'DELTA' WHERE id = 2")
        .unwrap_err();
    node.run("DELETE FROM xi WHERE id = 1").unwrap();
    node.run("UPDATE xi SET b = 'DELTA' WHERE id = 2").unwrap();
    assert_eq!(
        node.rows("SELECT id, b FROM xi ORDER BY id"),
        vec![vec!["2", "DELTA"]]
    );
}

/// An index built over rows that already exist indexes the **expression's** value for each of
/// them, and a duplicate among them fails the statement.
#[test]
fn the_backfill_computes_the_expression_for_every_existing_row() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "INSERT INTO xi VALUES (1, 'Alpha')",
        "INSERT INTO xi VALUES (2, 'ALPHA')",
    ] {
        node.run(statement).unwrap();
    }
    let error = node
        .run("CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");

    // With the duplicate gone the index builds, and it holds the value the expression computes:
    // the row that would collide with an already-indexed one is refused afterwards.
    node.run("DELETE FROM xi WHERE id = 2").unwrap();
    node.run("CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))")
        .unwrap();
    let error = node.run("INSERT INTO xi VALUES (3, 'ALPHA')").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
}

/// **An expression index is never read**, for the reason a partial one is not.
///
/// There is no constant in a `WHERE` to pin an expression to, and pinning it to the column
/// underneath would answer `WHERE b = 'X'` from an index on `lower(b)`. So `SELECT` scans, and
/// gets every row rather than the ones an index lookup would have found.
#[test]
fn an_expression_index_constrains_without_narrowing_a_read() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))",
        "INSERT INTO xi VALUES (1, 'Alpha')",
        "INSERT INTO xi VALUES (2, 'beta')",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(node.rows("SELECT id FROM xi WHERE b = 'Alpha'"), [["1"]]);
    assert_eq!(
        node.rows("SELECT id FROM xi WHERE lower(b) = 'alpha'"),
        [["1"]]
    );
}

/// A `DELETE` removes the entry the expression built, and not one built from the column.
///
/// The check that would fail if `remove_row` read `b` where `write_row` wrote `lower(b)`: the
/// entry would survive the delete and the re-insert would collide with a row that is gone.
#[test]
fn a_delete_removes_the_entry_the_expression_built() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))",
        "INSERT INTO xi VALUES (1, 'Alpha')",
    ] {
        node.run(statement).unwrap();
    }
    node.run("DELETE FROM xi WHERE id = 1").unwrap();
    node.run("INSERT INTO xi VALUES (2, 'ALPHA')").unwrap();
    assert_eq!(node.rows("SELECT id FROM xi"), [["2"]]);
}

/// Past the last key part, `pg_get_indexdef(oid, n, true)` is the **empty string**.
///
/// Not NULL and not an error, measured — and it cannot be written in a corpus line, because an
/// empty rows field there is a row of one empty column and an empty *types* field is a command
/// with no result set. The same vanishing empty string `pg_19_varchar.txt`'s header describes.
#[test]
fn a_key_part_past_the_last_one_is_the_empty_string() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "CREATE UNIQUE INDEX xi_expr ON xi ((lower(b)))",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('xi_expr'::regclass, 2, true)"),
        [[""]]
    );
}

/// A `CONCURRENTLY` index over an expression is built by the staged job, entry for entry the same
/// as the one `CREATE INDEX` builds inside its own transaction.
///
/// The fourth place index entries are made (ADR 0020's backfill job), and the one a test that only
/// exercises the plain form leaves uncovered. The job is driven by the statement itself now, which
/// is PostgreSQL's contract (`tests/invalid_index.rs`) — the same backfill code either way, and
/// nothing left over to step by hand.
#[test]
fn a_concurrent_expression_index_is_built_by_the_job() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE xi (id int8 PRIMARY KEY, b text)",
        "INSERT INTO xi VALUES (1, 'Alpha')",
        "INSERT INTO xi VALUES (2, 'beta')",
        "CREATE UNIQUE INDEX CONCURRENTLY xi_expr ON xi ((lower(b)))",
    ] {
        node.run(statement).unwrap();
    }
    assert!(
        node.rows("SELECT * FROM esker_schema_jobs()").is_empty(),
        "the build finished inside the statement that asked for it"
    );
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('xi_expr'::regclass)"),
        [["CREATE UNIQUE INDEX xi_expr ON public.xi USING btree (lower(b))"]]
    );
    let error = node.run("INSERT INTO xi VALUES (3, 'ALPHA')").unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
}

/// An index expression whose value is not a function of the row alone is `42P17`.
///
/// The entry is written from the value the expression had at insert and looked up from the value
/// it has at read, and nothing ever notices they differ — so this is a wrong index rather than a
/// slow one. `lower` and `upper` are the two functions this node has that are safe here; a
/// sequence call writes, and a `pg_catalog` function reads a catalog the index would then have to
/// be rebuilt for. PostgreSQL 19 answers `42P17` for both, measured.
#[test]
fn an_expression_that_is_not_a_function_of_the_row_is_refused() {
    let mut node = parity::Node::new(&[]);
    // `bigserial` is how a sequence comes into being here: `CREATE SEQUENCE` is still `0A000`.
    node.run("CREATE TABLE xi (id bigserial PRIMARY KEY, b text)")
        .unwrap();
    for statement in [
        "CREATE INDEX xi_vol ON xi ((nextval('xi_id_seq')))",
        "CREATE INDEX xi_cat ON xi ((format_type(id, -1)))",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), "42P17", "for {statement}");
        assert_eq!(
            error.to_string(),
            "functions in index expression must be marked IMMUTABLE"
        );
    }
}
