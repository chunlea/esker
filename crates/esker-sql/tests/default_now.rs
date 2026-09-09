//! `DEFAULT CURRENT_TIMESTAMP`, against PostgreSQL 19beta1.
//!
//! Statement 30 of `schema.rb`, and the first default this node stores that is not a value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        "SELECT pg_typeof(CURRENT_TIMESTAMP), pg_typeof(now()), pg_typeof(LOCALTIMESTAMP), pg_typeof(CURRENT_DATE)",
    ],
    answers: &[
        // **Two entries stood here and both are deleted** (ADR 0031, rule 2). One said this
        // node could not tell `DEFAULT CURRENT_TIMESTAMP` from `DEFAULT now()`, because a default
        // was a *tag* over a closed set of expressions and the two shared it; the other said
        // `CURRENT_TIMESTAMP` was a default and not an expression, so `SELECT CURRENT_TIMESTAMP =
        // now()` was `0A000`. Generalising a default to any expression answered both at once: the
        // catalog holds the text the user wrote, so the spellings survive, and the evaluator has
        // the function, so the comparison runs.
        // **A third entry stood here and is deleted** (ADR 0031, rule 2): `LOCALTIMESTAMP`
        // answered the transaction's instant as `CURRENT_TIMESTAMP` did, where a real server
        // gives the unzoned form of it. `insert_all` needed the difference — a `timestamp`
        // column takes one with no cast and the other through one — so the two spellings are
        // now two members with two types (`tests/values_catalog_function.rs`).
        (
            "SELECT CURRENT_TIMESTAMP(0) IS NOT NULL",
            "A precision on the function, which is the same instant rounded. Carrying it needs \
             the precision stored beside the flag, and nothing reads it yet — refused by name \
             rather than silently given full precision.",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_default_now_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_default_now.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 6,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The default fills, and two columns in one `INSERT` hold the **same** instant.
///
/// That is what makes it the transaction's timestamp rather than the statement's or a clock
/// reading, and it is PostgreSQL's rule: `CURRENT_TIMESTAMP` is constant within a transaction.
#[test]
fn every_column_defaulting_to_now_gets_one_instant() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE dt (id int8 PRIMARY KEY, a timestamp DEFAULT CURRENT_TIMESTAMP, \
         b timestamp(6) DEFAULT CURRENT_TIMESTAMP, c timestamptz DEFAULT CURRENT_TIMESTAMP, \
         d timestamp DEFAULT now())",
        "INSERT INTO dt (id) VALUES (1)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id, a IS NOT NULL, b IS NOT NULL, c IS NOT NULL, d IS NOT NULL FROM dt"),
        vec![vec!["1", "t", "t", "t", "t"]]
    );
    // The same instant in every one of them, which a wall-clock read per column would not give.
    assert_eq!(node.rows("SELECT a = b FROM dt"), vec![vec!["t"]]);
}

/// An explicit value still wins, and `DEFAULT` in a `VALUES` list still means the default.
#[test]
fn an_explicit_value_beats_the_default_and_the_keyword_asks_for_it() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE w (id int8 PRIMARY KEY, at timestamp DEFAULT CURRENT_TIMESTAMP)",
        "INSERT INTO w VALUES (1, '2020-01-01 00:00:00')",
        "INSERT INTO w VALUES (2, DEFAULT)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id, at FROM w WHERE id = 1"),
        vec![vec!["1", "2020-01-01 00:00:00"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM w WHERE at > '2020-01-02 00:00:00' ORDER BY id"),
        vec![vec!["2"]]
    );
}

/// **A volatile default is no longer refused, and the refusal it replaced was this node's own.**
///
/// This test asserted the opposite until the `DEFAULT` expression unit: a function call in a
/// default was `0A000 … which may be volatile`, on the argument that a catalog holding one value
/// cannot hold an expression. The catalog holds the expression now, so `random()` is taken and
/// evaluated per row — which is what PostgreSQL does, and it applies no volatility test at all.
///
/// What is still refused is `CURRENT_TIMESTAMP(0)`, and for a reason that has nothing to do with
/// volatility: this node has no sub-second precision argument on the clock, so honouring the
/// number would mean ignoring it.
#[test]
fn a_volatile_default_is_taken_and_a_precision_argument_is_not() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE r (id int8 PRIMARY KEY, a float8 DEFAULT random())")
        .unwrap();
    for id in 0..4 {
        node.run(&format!("INSERT INTO r (id) VALUES ({id})"))
            .unwrap();
    }
    assert_eq!(
        node.rows("SELECT count(DISTINCT a), count(*) FROM r"),
        [["4", "4"]],
        "per row, not once at CREATE TABLE"
    );

    // `CURRENT_TIMESTAMP(0)` is a **precision argument**, which a real server takes and this node
    // does not have a clock for — refused as a wrong arity (`42883`), where PostgreSQL answers the
    // instant rounded to the second. A refusal either way, and the code differs because the
    // function exists here with one signature rather than two.
    let error = node
        .run("CREATE TABLE p (id int8 PRIMARY KEY, a timestamp DEFAULT CURRENT_TIMESTAMP(0))")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
}
