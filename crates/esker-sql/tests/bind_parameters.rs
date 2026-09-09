//! `$1` over the **extended** protocol — what `ActiveRecord` sends with `prepared_statements: true`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    types: &[],
    answers: &[
        // **An entry stood here and is deleted** (ADR 0031, rule 2): `IS NOT DISTINCT FROM` was
        // an operator this node did not have. It arrived with the `upsert_all` template that
        // needed it (`tests/values_catalog_function.rs`), and the parameter half had always
        // worked — the identical statement with `=` binds NULL two lines above. The entry was
        // *found* by this harness learning rule 2, which it did not have: a listed answer
        // divergence was skipped without being compared, so closing one was absorbed silently.
        (
            "SELECT pg_typeof($1)",
            "**A parameter in a function argument needs overload resolution**, which this node \
             does not do: PostgreSQL cannot choose an overload of `pg_typeof` for an unknown, so \
             it is `42P18` even though a value was supplied — while `SELECT $1` in the target \
             list resolves to `text` and works, two lines above. Answering `text` here is the \
             fallback being applied where a real server refuses; closing it needs a function \
             signature table, not a parameter rule",
            "pg19_bind_parameters.txt:78",
        ),
    ],
};

#[test]
fn every_bind_answer_is_postgresql_19_s() {
    let checked = bind::replay(
        include_str!("corpus/pg19_bind_parameters.txt"),
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The bug run 46 counted 98 times.** A parameter in any clause the substitution walker did not
/// visit survived it and reached the row evaluator, which is `42P02 there is no parameter $n`.
///
/// One test per clause, because each was its own oversight and a clause added later without its
/// walker entry is the same bug again.
#[test]
fn a_parameter_binds_in_every_clause_that_can_hold_one() {
    let mut node = bind::Node::new();
    node.bound(
        "CREATE TABLE bp (id bigserial primary key, author_id bigint, n integer, title text)",
        &[],
    )
    .unwrap();
    node.bound(
        "INSERT INTO bp (author_id, n, title) VALUES (1, 1, 'a'), (1, 2, 'b'), (2, 3, 'c')",
        &[],
    )
    .unwrap();
    node.bound(
        "CREATE TABLE bp_author (id bigserial primary key, name text)",
        &[],
    )
    .unwrap();
    node.bound("INSERT INTO bp_author (name) VALUES ('x'), ('y')", &[])
        .unwrap();

    let one = |text: &str| Some(text.as_bytes().to_vec());

    // `HAVING` — `calculations_test.rb` alone, twenty of the ninety-eight.
    assert_eq!(
        node.answer(
            "SELECT author_id FROM bp GROUP BY author_id HAVING count(*) > $1 ORDER BY author_id",
            &[one("1")]
        ),
        bind::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["1".to_owned()]],
        }
    );
    // A join's `ON`.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM bp JOIN bp_author ON bp_author.id = bp.author_id AND bp.n > $1",
            &[one("1")]
        ),
        bind::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["2".to_owned()]],
        }
    );
    // A derived table, whose parameters are numbered in the same statement.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM (SELECT n FROM bp WHERE n > $1) AS inner_bp",
            &[one("1")]
        ),
        bind::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["2".to_owned()]],
        }
    );
    // `GROUP BY`.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM bp GROUP BY n > $1 ORDER BY 1",
            &[one("1")]
        ),
        bind::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["1".to_owned()], vec!["2".to_owned()]],
        }
    );
    // `ON CONFLICT … DO UPDATE SET`, which `upsert_all` writes.
    node.bound("CREATE UNIQUE INDEX bp_n ON bp (n)", &[])
        .unwrap();
    assert_eq!(
        node.answer(
            "INSERT INTO bp (n, title) VALUES ($1, $2) ON CONFLICT (n) DO UPDATE SET title = $3",
            &[one("1"), one("z"), one("updated")]
        ),
        bind::Answer::Done
    );
    assert_eq!(
        node.answer("SELECT title FROM bp WHERE n = $1", &[one("1")]),
        bind::Answer::Rows {
            types: vec!["text".to_owned()],
            rows: vec![vec!["updated".to_owned()]],
        }
    );
}

/// **A parameter inside a subquery is numbered in the statement holding it**, and the pair of
/// walkers stopped at the boundary.
///
/// The same bug the clause list above fixed, one level up: `walk_select_mut` visits every clause
/// of *a* `SELECT` and neither walker descended into a nested one, so a `$1` under an `IN
/// (SELECT …)` was never counted, never typed and never substituted — `42P18 could not determine
/// data type of parameter $1` at `Parse`, for a value the client was about to send.
///
/// Nothing here writes. It is in this file rather than beside the `delete_all` corpus that found
/// it because it is not a fact about writing: `SELECT` had it too, and no corpus had reached it.
#[test]
fn a_parameter_binds_inside_a_subquery() {
    let mut node = bind::Node::new();
    node.bound(
        "CREATE TABLE sq_bp (id bigserial primary key, n integer, title text)",
        &[],
    )
    .unwrap();
    node.bound(
        "INSERT INTO sq_bp (n, title) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        &[],
    )
    .unwrap();
    let one = |text: &str| Some(text.as_bytes().to_vec());
    let count = |rows: &str| bind::Answer::Rows {
        types: vec!["bigint".to_owned()],
        rows: vec![vec![rows.to_owned()]],
    };

    // The subquery's own `WHERE`: `$1` is typed by `n`, which is a column of a table the outer
    // statement does not name in its `FROM`.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM sq_bp WHERE id IN (SELECT id FROM sq_bp WHERE n > $1)",
            &[one("1")]
        ),
        count("2")
    );
    // The subquery's `LIMIT`, which is what `delete_all` on a limited relation sends.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM sq_bp WHERE id IN (SELECT id FROM sq_bp LIMIT $1)",
            &[one("2")]
        ),
        count("2")
    );
    // **The operand side of the `IN`**, which is a second field the arm has to walk: it is held
    // on the subquery expression rather than beside it, so an arm that walked only the
    // sub-`SELECT` would leave this one behind.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM sq_bp WHERE $1 IN (SELECT n FROM sq_bp)",
            &[one("2")]
        ),
        count("3")
    );
    // An `EXISTS` correlated with the outer row, with the parameter inside it.
    assert_eq!(
        node.answer(
            "SELECT count(*) FROM sq_bp o WHERE EXISTS (SELECT 1 FROM sq_bp i WHERE i.id = o.id \
             AND i.title = $1)",
            &[one("b")]
        ),
        count("1")
    );
    // A scalar subquery in the target list.
    assert_eq!(
        node.answer(
            "SELECT (SELECT count(*) FROM sq_bp WHERE n > $1)",
            &[one("1")]
        ),
        count("2")
    );
}
