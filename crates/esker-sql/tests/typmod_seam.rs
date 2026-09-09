//! **A cast truncates and an assignment refuses** — one rule, four types, two callers.
//!
//! `docs/plans/debts-v1.1.md` #36. The row was written about `bit`, from the varbit unit: five bits
//! into a `bit varying(3)` column were truncated here where a real server raises. The cause it
//! named was right — `value::fit_to_typmod` serves both the cast and the row write, and what it
//! holds is the cast's answer — and the row called the string types "the worked example".
//!
//! **They are not.** The two halves are exactly swapped, and each type is wired to whichever
//! caller it was written for:
//!
//! ```text
//!                     cast              assignment
//! varchar(3)/char(3)  22001  (wrong)    22001  (right)
//! bit(3)/varbit(3)    truncates (right) truncates (wrong)
//! ```
//!
//! `value::truncate_to_typmod` is the cast's rule and had **one** caller — the runtime cast in
//! `exec::cursor`. The *folded literal* cast in `parse::lower` and the *domain* cast in `exec`
//! both called `fit_to_typmod`, which is the write's rule; and `fit_to_typmod`'s own `bit` arm
//! held the cast's. So `'abcdef'::varchar(3)` was `22001` where a real server answers `abc`, and
//! `INSERT` of five bits into a `bit(3)` was accepted where it raises `22026`.
//!
//! Every line below is measured on PostgreSQL 19beta1 in `tests/captures/pg19_typmod_seam.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// **A cast truncates**, whichever of the three doors it comes through.
#[test]
fn a_cast_fits_the_value_to_the_modifier() {
    let mut node = parity::Node::new(&["CREATE DOMAIN d36 AS varchar(3)"]);
    for (statement, answer) in [
        ("SELECT 'abcdef'::varchar(3)", "abc"),
        ("SELECT 'abcdef'::character(3)", "abc"),
        // A `bit(n)` is an exact width, so a short value is padded on the **right**.
        ("SELECT '10101'::bit(3)", "101"),
        ("SELECT '1'::bit(3)", "100"),
        ("SELECT '10101'::varbit(3)", "101"),
        // The same statement through the runtime path rather than the fold.
        ("SELECT 'abcdef'::text::varchar(3)", "abc"),
        ("SELECT CAST('abcdef' AS varchar(3))", "abc"),
        // **A cast to a domain is a cast to its base type**, and takes the base's rule with it.
        ("SELECT 'abcdef'::d36", "abc"),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
    }
}

/// **An assignment refuses**, and the two codes are not interchangeable.
///
/// `22001` is "too long" and belongs to the types whose modifier is a **maximum**; `22026` is
/// "does not match" and belongs to `bit(n)`, whose modifier is an **exact width** — which is why
/// it is raised for a value that is too *short* as well.
#[test]
fn an_assignment_refuses_rather_than_fitting() {
    let mut node = parity::Node::new(&[
        "CREATE DOMAIN d36 AS varchar(3)",
        "CREATE TABLE s36 (v varchar(3), c character(3), b bit(3), vb varbit(3), d d36)",
    ]);
    for (statement, state, message) in [
        (
            "INSERT INTO s36(v) VALUES ('abcdef')",
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            "value too long for type character varying(3)",
        ),
        (
            "INSERT INTO s36(c) VALUES ('abcdef')",
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            "value too long for type character(3)",
        ),
        (
            "INSERT INTO s36(b) VALUES ('10101')",
            "22026",
            "bit string length 5 does not match type bit(3)",
        ),
        // **Too short is refused too**, which is what says a `bit(n)` is exact and not a maximum.
        (
            "INSERT INTO s36(b) VALUES ('1')",
            "22026",
            "bit string length 1 does not match type bit(3)",
        ),
        (
            "INSERT INTO s36(vb) VALUES ('10101')",
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            "bit string too long for type bit varying(3)",
        ),
        (
            "INSERT INTO s36(d) VALUES ('abcdef')",
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            "value too long for type character varying(3)",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), state, "{statement}");
        assert_eq!(error.to_string(), message, "{statement}");
    }
    // Nothing was written by any of them.
    assert_eq!(node.rows("SELECT count(*) FROM s36"), vec![vec!["0"]]);
}

/// **A value that fits is stored unchanged**, which is what says the refusals above are about the
/// modifier and not about the write path being broken.
#[test]
fn a_value_that_fits_is_stored() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE s36 (v varchar(3), c character(3), b bit(3), vb varbit(3))",
    ]);
    node.run("INSERT INTO s36 VALUES ('ab', 'ab', '101', '10')")
        .unwrap();
    // A `character(3)` pads on the right; the others keep what they were given.
    assert_eq!(
        node.rows("SELECT v, c, b, vb FROM s36"),
        vec![vec!["ab", "ab ", "101", "10"]]
    );
}
