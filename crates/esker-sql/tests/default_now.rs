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
    types: &[],
    answers: &[
        (
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef d JOIN pg_class c ON c.oid = \
             d.adrelid WHERE c.relname = 'dt' ORDER BY adnum",
            "`pg_attrdef` and `pg_get_expr` are e2-catalog's: reading a default back out of the \
             catalog needs the records that hold one. The line is here because it is the proof \
             that PostgreSQL records `CURRENT_TIMESTAMP` and `now()` as the *same* default, which \
             is why this node keeps one flag and not two.",
        ),
        (
            "SELECT pg_typeof(CURRENT_TIMESTAMP), pg_typeof(now()), pg_typeof(LOCALTIMESTAMP), \
             pg_typeof(CURRENT_DATE)",
            "`pg_typeof` is not implemented. Three of its four arguments now are \
             (`tests/scalar_functions.rs`); `LOCALTIMESTAMP` is the one that is not.",
        ),
        (
            "SELECT CURRENT_TIMESTAMP IS NOT NULL, LOCALTIMESTAMP IS NOT NULL",
            "The same: a default is not an expression. `LOCALTIMESTAMP` is the `timestamp` \
             without time zone form and is not recorded either — `ActiveRecord` does not emit it.",
        ),
        (
            "SELECT CURRENT_TIMESTAMP(0) IS NOT NULL",
            "A precision on the function, which is the same instant rounded. Carrying it needs \
             the precision stored beside the flag, and nothing reads it yet — refused by name \
             rather than silently given full precision.",
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

/// A volatile default that is **not** `CURRENT_TIMESTAMP` is still refused by name.
///
/// The refusal was never about defaults being hard; it was about a catalog that stores one value
/// being unable to hold an expression. One expression is admitted now, and the argument for
/// refusing the rest is unchanged.
#[test]
fn another_volatile_default_is_still_refused() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "CREATE TABLE r (id int8 PRIMARY KEY, a int8 DEFAULT random())",
        "CREATE TABLE r (id int8 PRIMARY KEY, a int8 DEFAULT (1+1))",
        "CREATE TABLE r (id int8 PRIMARY KEY, a timestamp DEFAULT CURRENT_TIMESTAMP(0))",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "{sql}");
    }
}
