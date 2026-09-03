//! `sum` and `avg` over the exact numeric types — the ladder's rung-3 blocker.
//!
//! Only `int8` and `float8` had either before this, and `avg(int8)`'s refusal said this node had
//! "no numeric type to reproduce" the answer with. It has had one since ADR 0045; the refusal
//! outlived its reason.
//!
//! Every expectation here was captured from PostgreSQL 19beta1 in one rolled-back session before
//! a line of it was written.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn loaded() -> parity::Node {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ag (id int8 PRIMARY KEY, a int2, b int4, c int8, d numeric, e numeric(10,2))",
        "INSERT INTO ag VALUES (1, 1, 10, 100, 1.5, 1.25)",
        "INSERT INTO ag VALUES (2, 2, 20, 200, 2.25, 2.50)",
        "INSERT INTO ag VALUES (3, NULL, NULL, NULL, NULL, NULL)",
    ] {
        node.run(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    node
}

/// **`sum` widens, and not uniformly.** `int2` and `int4` go to `bigint`; `int8` goes to
/// `numeric`, which is why a real server's `sum(bigint)` cannot overflow.
#[test]
fn sum_widens_the_way_postgresql_widens() {
    let mut node = loaded();
    assert_eq!(
        node.rows("SELECT sum(a), sum(b), sum(c), sum(d), sum(e) FROM ag"),
        vec![vec!["3", "30", "300", "3.75", "3.75"]]
    );

    // The overflow that is not one: `int8`'s maximum plus one is a value, because the sum is a
    // numeric. This node used to answer `22003` here and declared it as a divergence.
    for statement in [
        "CREATE TABLE big (id int8 PRIMARY KEY, c int8)",
        "INSERT INTO big VALUES (1, 9223372036854775807)",
        "INSERT INTO big VALUES (2, 1)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT sum(c) FROM big"),
        vec![vec!["9223372036854775808"]]
    );
}

/// **Every exact type averages to `numeric`**, at a scale that is not a fixed number of places.
///
/// PostgreSQL's division aims at sixteen *significant* digits, so an average that comes out
/// exactly one prints twenty decimals and one that does not prints sixteen. Both are here
/// because a fixed sixteen would have looked right in the second case and wrong in the first.
#[test]
fn avg_answers_numeric_at_postgresqls_own_scale() {
    let mut node = loaded();
    assert_eq!(
        node.rows("SELECT avg(a), avg(b), avg(c), avg(d), avg(e) FROM ag"),
        vec![vec![
            "1.5000000000000000",
            "15.0000000000000000",
            "150.0000000000000000",
            "1.8750000000000000",
            "1.8750000000000000",
        ]]
    );
    for statement in [
        "CREATE TABLE ex (id int8 PRIMARY KEY, b int4, w numeric)",
        "INSERT INTO ex VALUES (1, 1, 1.123456789012345678)",
        "INSERT INTO ex VALUES (2, 1, 2.0)",
        "INSERT INTO ex VALUES (3, 1, NULL)",
    ] {
        node.run(statement).unwrap();
    }
    // Twenty places, because the quotient is exactly one and the sixteen significant digits it
    // is entitled to start after the point.
    assert_eq!(
        node.rows("SELECT avg(b) FROM ex"),
        vec![vec!["1.00000000000000000000"]]
    );
    // The input's own scale wins when it is wider than sixteen.
    assert_eq!(
        node.rows("SELECT avg(w) FROM ex"),
        vec![vec!["1.561728394506172839"]]
    );
    // And one that does not divide exactly: sixteen places.
    node.run("UPDATE ex SET b = 2 WHERE id > 1").unwrap();
    assert_eq!(
        node.rows("SELECT avg(b) FROM ex"),
        vec![vec!["1.6666666666666667"]]
    );
}

/// NULLs are skipped and an empty group is NULL — for both, and for every type.
#[test]
fn nulls_are_skipped_and_nothing_averages_to_null() {
    let mut node = loaded();
    // The third row is all NULLs and did not count: three rows, two values.
    assert_eq!(
        node.rows("SELECT count(*), count(a), sum(a), avg(a) FROM ag"),
        vec![vec!["3", "2", "3", "1.5000000000000000"]]
    );
    // **Not zero.** A sum over no rows that answered `0` would be a wrong answer that looks like
    // data, which is why the accumulator starts at `None` rather than at nought.
    assert_eq!(
        node.rows("SELECT sum(a), avg(a), sum(d), avg(d) FROM ag WHERE id = 99"),
        vec![vec!["\\N", "\\N", "\\N", "\\N"]]
    );
}
