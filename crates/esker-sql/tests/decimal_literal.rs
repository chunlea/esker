//! **An unadorned decimal literal is a `numeric`**, against PostgreSQL 19beta1.
//!
//! r1's 349-probe wire sweep filed four rows under "COALESCE/CASE pick a common type wider than
//! PostgreSQL's". Two closed with the literal ladder's `int4` rung (ADR 0087). The other two are
//! one fact and it is not about `CASE`: this crate read `1.5` as a `float8` where a real server
//! reads it as a `numeric`, so the common type of `1` and `1.5` was `double precision` here and
//! `numeric` there.
//!
//! **It is a wrong value and not only a wrong type**, which is what separates it from the `int4`
//! rung — a narrowed integer prints the same characters and a float8 does not:
//!
//! ```text
//!                       node                   pg19
//!   SELECT 1.10         1.1                    1.10        a numeric keeps its scale
//!   SELECT 0.1 + 0.2    0.30000000000000004    0.3
//!   SELECT 10.0/3.0     3.3333333333333335     3.3333333333333333
//! ```
//!
//! **A `float8` still wins over a `numeric`**: `CASE WHEN true THEN 1 ELSE 1.5::float8 END` is a
//! `double precision` on both. What changed is only which type a bare `1.5` *is*.
//!
//! Measured in `tests/captures/pg19_decimal_literal.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_typeof` answers a `regtype` on a real server and `text` here (ADR 0077); every row
    // agrees. These are the statements that ask it and nothing else.
    types: &[
        "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 1.5 END), pg_typeof(COALESCE(1, 1.5))",
        "SELECT pg_typeof(1.5), pg_typeof(1.5::float8), pg_typeof(1e3)",
        "SELECT pg_typeof(CASE WHEN true THEN 1::int8 ELSE 1::int4 END)",
        "SELECT pg_typeof(GREATEST(1, 1.5)), pg_typeof(LEAST(1, 1.5))",
        "SELECT 1.5 + 1, pg_typeof(1.5 + 1)",
        "SELECT 1.5 * 3, pg_typeof(1.5 * 3)",
        "SELECT 0.1 + 0.2, pg_typeof(0.1 + 0.2)",
        "SELECT 1.10, pg_typeof(1.10)",
        "SELECT sum(v), pg_typeof(sum(v)) FROM (VALUES (1.10),(2.20)) t(v)",
        "SELECT avg(v), pg_typeof(avg(v)) FROM (VALUES (1.10),(2.20)) t(v)",
        "SELECT 10.0/3.0, pg_typeof(10.0/3.0)",
        "SELECT pg_typeof(COALESCE(NULL, 1.5))",
        "SELECT -1.5, pg_typeof(-1.5)",
        "SELECT 1.5e2, pg_typeof(1.5e2)",
        "SELECT abs(-1.5), pg_typeof(abs(-1.5))",
        "SELECT CASE WHEN true THEN 1 ELSE 1.5::float8 END, pg_typeof(CASE WHEN true THEN 1 ELSE \
         1.5::float8 END)",
        "SELECT ARRAY[1, 1.5], pg_typeof(ARRAY[1, 1.5])",
        "SELECT x, pg_typeof(x) FROM (VALUES (1.5),(2)) t(x) ORDER BY x",
        "SELECT 7.5 % 2, pg_typeof(7.5 % 2)",
        "SELECT 100000000000000000000.5, pg_typeof(100000000000000000000.5)",
    ],
    answers: &[
        // ----- `pg_typeof` over a branch whose type is the *common* one ---------------------------
        //
        // **One fact, five times, and it is not about the branches.** `pg_typeof` reads the datum,
        // and a `CASE` or `COALESCE` hands back the chosen branch's value — an `Int4` where the
        // column is declared `bigint`, a `Numeric` where it is declared `double precision`. The
        // `RowDescription` for every one of these is PostgreSQL's, which is what a client reads
        // and what `the_common_type_of_a_branch_is_postgresqls` and
        // `a_float_beside_a_numeric_is_still_a_float` assert below. Same seam as
        // `tests/name_array.rs`: closing it means resolving `pg_typeof` against the declared type
        // at plan time, which is its own unit and would close ADR 0077's half with it.
        (
            "SELECT pg_typeof(CASE WHEN true THEN 1::int4 ELSE 1.5::float8 END)",
            "`pg_typeof` reads the datum, which is the chosen branch's `Int4`; the column is \
             declared `double precision`, which is what a client is told.",
            "pg19_decimal_literal.txt:73",
        ),
        (
            "SELECT pg_typeof(CASE WHEN true THEN 1.5::numeric ELSE 1.5::float8 END)",
            "The same: the datum is the chosen branch's `Numeric` and the column is declared \
             `double precision`.",
            "pg19_decimal_literal.txt:74",
        ),
        (
            "SELECT pg_typeof(COALESCE(1::int4, 1::int8))",
            "The same, through `COALESCE`: the datum is the first branch's `Int4` and the column \
             is declared `bigint`.",
            "pg19_decimal_literal.txt:77",
        ),
        (
            "SELECT pg_typeof(COALESCE(1::numeric, 1::float8))",
            "The same, through `COALESCE`.",
            "pg19_decimal_literal.txt:78",
        ),
        (
            "SELECT pg_typeof(CASE WHEN true THEN NULL ELSE 1.5 END)",
            "The same over a NULL, which carries no type at all: the column is declared `numeric` \
             and `pg_typeof` has nothing to read it from.",
            "pg19_decimal_literal.txt:90",
        ),
        // **`^` is a `double precision` whatever it is given** in this crate — the one exception
        // `value::arith::result_type` writes down — where a real server's `numeric ^ integer` is a
        // `numeric` and carries `div_scale`'s sixteen digits. `8` against `8.0000000000000000`:
        // the same number, printed by two types. Its own unit; the literal is not what differs.
        // **`round(x, n)` is not built** — the one-argument form is, and the two-argument form
        // that takes a scale is `0A000` by name. Nothing about the literal: `round(1.55::numeric,
        // 1)` is the same refusal.
        (
            "SELECT round(1.55, 1), pg_typeof(round(1.55, 1))",
            "`round(x, n)` — the two-argument form that takes a scale — is `0A000` by name; the \
             one-argument form answers. Nothing about the literal.",
            "pg19_decimal_literal.txt:97",
        ),
        (
            "SELECT 2.0 ^ 3, pg_typeof(2.0 ^ 3)",
            "`^` yields a `double precision` whatever it is given here — the exception \
             `value::arith::result_type` records — where a real server keeps `numeric ^ integer` \
             a `numeric`, so the value prints `8` rather than `8.0000000000000000`.",
            "pg19_decimal_literal.txt:104",
        ),
    ],
};

#[test]
fn every_decimal_literal_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_decimal_literal.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`** — the path r1's sweep reads.
fn described(node: &mut parity::Node, statement: &str) -> u32 {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")[0]
        .type_oid
}

/// **The four rows r1 filed as group D**, over the wire. 1700 is `numeric`, 23 `integer`.
#[test]
fn the_common_type_of_a_branch_is_postgresqls() {
    let mut node = parity::Node::new(&[]);
    for (statement, oid) in [
        ("SELECT COALESCE(a, 1) AS v FROM (VALUES (1)) s(a)", 23),
        ("SELECT COALESCE(1, 1.5) AS v", 1700),
        ("SELECT CASE WHEN true THEN 1 ELSE 2 END AS v", 23),
        ("SELECT CASE WHEN true THEN 1 ELSE 1.5 END AS v", 1700),
    ] {
        assert_eq!(described(&mut node, statement), oid, "{statement}");
    }
}

/// **A bare `1.5` is a `numeric`**, and so are the three spellings that are not obviously one.
#[test]
fn every_spelling_of_a_decimal_literal_is_a_numeric() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "SELECT 1.5 AS v",
        // An exponent, whose value is a whole number and whose type is still `numeric`.
        "SELECT 1.5e2 AS v",
        "SELECT -1.5 AS v",
        // **Past what a `float8` can hold exactly**, which is the case that makes this a value
        // question rather than a naming one.
        "SELECT 100000000000000000000.5 AS v",
    ] {
        assert_eq!(described(&mut node, statement), 1700, "{statement}");
    }
    // And the one that is not: an explicit cast still wins.
    assert_eq!(described(&mut node, "SELECT 1.5::float8 AS v"), 701);
}

/// **A `float8` still wins over a `numeric`**, which is the half that must not move.
#[test]
fn a_float_beside_a_numeric_is_still_a_float() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "SELECT CASE WHEN true THEN 1 ELSE 1.5::float8 END AS v",
        "SELECT CASE WHEN true THEN 1.5::numeric ELSE 1.5::float8 END AS v",
        "SELECT COALESCE(1::numeric, 1::float8) AS v",
    ] {
        assert_eq!(described(&mut node, statement), 701, "{statement}");
    }
}

/// **The three values a `float8` gets wrong**, which is the whole reason this is not a rename.
#[test]
fn the_scale_survives_and_the_arithmetic_is_exact() {
    let mut node = parity::Node::new(&[]);
    // A `numeric` keeps the scale it was written with; a `float8` cannot.
    assert_eq!(node.rows("SELECT 1.10"), vec![vec!["1.10"]]);
    // The oldest example of binary floating point there is.
    assert_eq!(node.rows("SELECT 0.1 + 0.2"), vec![vec!["0.3"]]);
    // Sixteen *significant* digits, which is `numeric::div_scale`'s rule (ADR 0045).
    assert_eq!(
        node.rows("SELECT 10.0/3.0"),
        vec![vec!["3.3333333333333333"]]
    );
    // Multiplication **adds** the scales, so two one-place operands make a one-place answer.
    assert_eq!(
        node.rows("SELECT 1.5 + 1.5, 1.5 - 0.5, 1.5 * 2, 3.0 / 2"),
        vec![vec!["3.0", "1.0", "3.0", "1.5000000000000000"]]
    );
    // An aggregate over decimal literals is exact too, which is where a client would see it.
    assert_eq!(
        node.rows("SELECT sum(v) FROM (VALUES (1.10),(2.20)) t(v)"),
        vec![vec!["3.30"]]
    );
}

/// A decimal literal reaches a **column**, an **array** and a `VALUES` relation as a `numeric`.
#[test]
fn it_carries_its_type_into_the_shapes_around_it() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(described(&mut node, "SELECT ARRAY[1, 1.5] AS v"), 1231);
    assert_eq!(node.rows("SELECT ARRAY[1, 1.5]"), vec![vec!["{1,1.5}"]]);
    assert_eq!(
        described(
            &mut node,
            "SELECT x AS v FROM (VALUES (1.5),(2)) t(x) ORDER BY x"
        ),
        1700
    );
    // **Both rows**, so the second is the common type's and not its own.
    assert_eq!(
        node.rows("SELECT x FROM (VALUES (1.5),(2)) t(x) ORDER BY x"),
        vec![vec!["1.5"], vec!["2"]]
    );
}

/// The casts out of it round half away from zero, as they do out of any `numeric`.
#[test]
fn a_cast_out_of_one_rounds_half_away_from_zero() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 1.5::int4, 1.5::int8, 2.5::int4"),
        vec![vec!["2", "2", "3"]]
    );
    // `round(x, n)` is `0A000` by name and is declared; `abs` and `||` answer.
    assert_eq!(
        node.rows("SELECT abs(-1.5), 1.5 || 'x'"),
        vec![vec!["1.5", "1.5x"]]
    );
}
