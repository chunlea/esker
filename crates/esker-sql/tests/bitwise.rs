//! The integer bitwise operators, against PostgreSQL 19beta1.
//!
//! `connection_test.rb#test_get_and_release_advisory_lock` builds its lock key the way
//! `ActiveRecord` builds every advisory-lock key:
//!
//! ```sql
//! (a::bigint << 32) | b::bigint
//! ```
//!
//! and this node answered `the operator | is not supported`. The five operators are one family and
//! land together, because a client that has `|` and not `&` is in a worse position than one that
//! has neither: the missing one looks like a typo rather than a gap.
//!
//! Three of the rules are not what reasoning gives. **A shift keeps its left operand's type**
//! where the other four take the wider of the two. **A shift count wraps modulo the width** —
//! `1::int4 << 32` is `1` and `1::int4 << -1` is `1 << 31` — so nothing here overflows and nothing
//! is refused. And **two unknown literals are `42725 operator is not unique`**, not the `42883` a
//! wrong type gets, because every integer width offers a candidate and none of them wins.
//!
//! Measured in `tests/corpus/pg19_bitwise.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One fact, seven times, and it is not about these operators**: `pg_typeof` answers a
    // `regtype` on a real server and `text` here, the trade `'x'::regtype` makes everywhere in
    // this crate (ADR 0077). The rows are identical — these seven pin the operators' own typing
    // rules, and nine more stood beside them until the literal ladder's `int4` rung made a bare
    // operand `integer` on both sides.
    types: &[
        "SELECT pg_typeof((3::bigint << 32) | 5::bigint)",
        "SELECT pg_typeof(12::int2 | 10::int2)",
        "SELECT pg_typeof(12::int4 | 10::int8)",
        "SELECT pg_typeof(12::int2 | 10::int4)",
        "SELECT pg_typeof(1::int8 << 4)",
        "SELECT pg_typeof(1::int2 << 4)",
        // **This one moved here from `answers`** when the `int4` rung landed: it used to read
        // `bigint` against a real server's `integer` and now the row agrees, leaving only the
        // `regtype`/`text` trade the six above make.
        "SELECT pg_typeof(12 | 10)",
    ],
    answers: &[
        // **`~` is refused by name.** Unary bitwise NOT needs an expression variant of its own —
        // `Expr::Negate` has seventeen match sites and this would have as many — and nothing in
        // the suite writes one: `ActiveRecord` builds its lock key from `<<` and `|`. Named
        // rather than approximated, which is what this crate does with every operator it has not
        // built.
        (
            "SELECT ~12",
            "The unary bitwise NOT is not built: it needs its own expression variant, and the \
             suite writes only `<<` and `|`. Refused by name rather than approximated.",
            "pg19_bitwise.txt:29",
        ),
        (
            "SELECT pg_typeof(~12::int2)",
            "The same gap, read through pg_typeof.",
            "pg19_bitwise.txt:36",
        ),
        // A decimal literal is a `double precision` in this crate and a `numeric` to PostgreSQL's
        // resolver, so both operands of a refused operator are named differently. The refusal and
        // its code are the same; only the two type names in the sentence differ.
        (
            "SELECT 1.5 | 2",
            "Half of this closed when a bare decimal became a `numeric`: the message reads \
             `numeric | numeric` where a real server reads `numeric | integer`. This crate \
             resolves both operands to one type before deciding no operator exists, so a refusal \
             has one type to name and PostgreSQL names the two it was given. Same code, same \
             refusal, one word apart.",
            "pg19_bitwise.txt:53",
        ),
        (
            "SELECT 'a' | 'b'",
            "An unadorned string literal is `text` in this crate and `unknown` to PostgreSQL's \
             resolver. A real server then has a candidate at every integer width and cannot \
             choose, which is `42725 operator is not unique`; here the operands already have a \
             type and there is no `text | text`, which is `42883`. Same refusal, one class apart, \
             and the cause is the literal rather than the operator.",
            "pg19_bitwise.txt:52",
        ),
    ],
};

#[test]
fn every_bitwise_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_bitwise.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 28,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The statement the suite sends**, and the number it has to produce.
#[test]
fn the_advisory_lock_key_is_built_the_way_activerecord_builds_it() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT (3::bigint << 32) | 5::bigint"),
        vec![vec!["12884901893"]]
    );
    let outcome = node.run("SELECT (3::bigint << 32) | 5::bigint").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    // 20 is `bigint`: a key that came back as `numeric` would be a different value to the driver.
    assert_eq!(fields[0].type_oid, 20);
}

/// **A shift keeps its left operand's type**; the other four take the wider of the two.
#[test]
fn a_shift_keeps_its_left_type_and_the_rest_take_the_wider() {
    let mut node = parity::Node::new(&[]);
    for (statement, ty) in [
        ("SELECT pg_typeof(1::int2 << 4)", "smallint"),
        ("SELECT pg_typeof(1::int8 << 4)", "bigint"),
        ("SELECT pg_typeof(12::int2 | 10::int2)", "smallint"),
        ("SELECT pg_typeof(12::int2 | 10::int4)", "integer"),
        ("SELECT pg_typeof(12::int4 | 10::int8)", "bigint"),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![ty.to_owned()]],
            "{statement}"
        );
    }
}

/// **A shift count wraps modulo the width**, so nothing overflows and nothing is refused.
#[test]
fn a_shift_count_wraps_and_a_right_shift_keeps_the_sign() {
    let mut node = parity::Node::new(&[]);
    for (statement, answer) in [
        // 32 places on a 32-bit type is no places at all.
        ("SELECT 1::int4 << 32", "1"),
        ("SELECT 1::int8 << 64", "1"),
        ("SELECT 1::int4 << 31", "-2147483648"),
        ("SELECT 1::int8 << 63", "-9223372036854775808"),
        // -1 modulo 32 is 31, so a negative count shifts left rather than right.
        ("SELECT 1::int4 << -1", "-2147483648"),
        // Arithmetic, not logical: the sign bit is copied.
        ("SELECT (-1)::int4 >> 1", "-1"),
        ("SELECT (-1)::int8 >> 1", "-1"),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
    }
}

/// `&` binds tighter than `|`, and `<<` tighter than either.
#[test]
fn the_precedence_is_postgresqls() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 2 | 3 & 1"), vec![vec!["1"]]);
    assert_eq!(node.rows("SELECT 1 << 2 | 1"), vec![vec!["5"]]);
    assert_eq!(node.rows("SELECT 12 | 10 | 3"), vec![vec!["15"]]);
}

/// **Two classes of refusal**, and the one for two bare literals is the surprising one.
#[test]
fn the_refusals_tell_a_wrong_type_from_an_ambiguous_one() {
    let mut node = parity::Node::new(&[]);

    let error = node.run("SELECT true | false").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "operator does not exist: boolean | boolean"
    );

    // **`numeric | numeric`, where a real server says `numeric | integer`.** The rule under test
    // is that a non-integer operand is refused at all, which is what this asserts. The left name
    // used to read `double precision` and is right since a bare decimal became a `numeric`; what
    // is left is the *right* one, because this crate resolves both operands to one type before it
    // decides there is no operator, so a refusal has only that one type to name. Declared in
    // `DIVERGENCES`.
    let error = node.run("SELECT 1.5 | 2").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "operator does not exist: numeric | numeric"
    );

    // **A real server answers `42725 operator is not unique` here**, because two `unknown`
    // literals give it a candidate at every integer width and no way to choose. This node types a
    // bare literal as `text` before the operator is resolved, so it has no candidates rather than
    // too many, and the refusal is the class next door. Declared in `DIVERGENCES`; asserted here
    // as what this node actually says, so the day literals become `unknown` this line reddens.
    let error = node.run("SELECT 'a' | 'b'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(error.to_string(), "operator does not exist: text | text");
}

/// NULL propagates, as it does through arithmetic.
#[test]
fn a_null_operand_is_a_null_answer() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 5 | NULL"), vec![vec!["\\N"]]);
    assert_eq!(node.rows("SELECT NULL::int8 & 3"), vec![vec!["\\N"]]);
}

/// **The one member of the family that is not here**, named rather than approximated.
#[test]
fn the_unary_not_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("SELECT ~12").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert!(
        error.to_string().contains('~'),
        "the refusal did not name the operator: {error}"
    );
}
