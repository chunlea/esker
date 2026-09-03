//! `$1` over the **extended** protocol — what `ActiveRecord` sends with `prepared_statements: true`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT $1::int4 + $2::int4",
            "**A cast over a parameter is its own unit.** A cast to anything but `text` is done \
             where the statement is lowered, over a *literal* — and a `$1` is not a literal until \
             it is substituted, which happens later. Making `$n::T` work means carrying the type \
             on the parameter so the inference reads it, and nothing `ActiveRecord` sends writes \
             one: the adapter emits `$1` bare and lets the column decide",
        ),
        (
            "SELECT pg_typeof($1::int8)",
            "The same cast, and the same unit",
        ),
        (
            "SELECT COUNT(*) FROM bp_posts WHERE n IS NOT DISTINCT FROM $1",
            "`IS NOT DISTINCT FROM` is an **operator this node does not have**, and nothing about \
             it is bind parameters — `pg19_on_conflict.txt` declares the same gap for the same \
             operator. The parameter half works: the identical statement with `=` binds NULL and \
             answers `0` two lines above",
        ),
        (
            "SELECT pg_typeof($1)",
            "**A parameter in a function argument needs overload resolution**, which this node \
             does not do: PostgreSQL cannot choose an overload of `pg_typeof` for an unknown, so \
             it is `42P18` even though a value was supplied — while `SELECT $1` in the target \
             list resolves to `text` and works, two lines above. Answering `text` here is the \
             fallback being applied where a real server refuses; closing it needs a function \
             signature table, not a parameter rule",
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
