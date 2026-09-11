//! **A literal array's cast still has to have a cast** — the guard the array path returned before.
//!
//! `'{x}'::"char"[]::bytea[]` answered `{"\x78"}` here and is
//! `42846 cannot cast type "char"[] to bytea[]` on 19beta1. The **scalar** pair was refused
//! (`'x'::"char"::bytea`) and the same pair through a **column** was refused; only a literal array
//! slipped, because `parse::lower`'s array arm reads the value's *text* and hands it to the
//! target's `array_in` — which reads whatever that element reader accepts — and returns before the
//! general `casts_to` guard below it ever runs.
//!
//! **Measured over every ordered pair of the wire v3 probe list's 100 spellings** — 9,900 casts on
//! both servers (`tests/captures/pg19_cast_matrix.txt`):
//!
//! ```text
//!                              before    after
//! node answers, PG refuses        391       95     <- the class ADR 0031 ranks worst
//! node refuses, PG answers        121      121
//! both refuse, different code   3,501      293
//!                               -----    -----
//!                               4,013      509
//! ```
//!
//! **The sqlstate half is the same fix.** A pair with no cast used to reach `array_in` and fail as
//! the element reader's `22P02` — "invalid input syntax" for a conversion that does not exist —
//! where a real server says `42846` before reading anything. One guard moved 3,504 rows and
//! regressed none.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **The four this was found through**, and the scalar beside each: the pair that says the defect
/// was the literal array path and not the cast table.
#[test]
fn a_literal_array_cast_is_refused_like_its_scalar() {
    let mut node = parity::Node::new(&[]);
    for (from, to, value) in [
        ("bit[]", "money[]", "{1}"),
        ("bit[]", "inet[]", "{1}"),
        ("money[]", "bit[]", "{$1.00}"),
        ("inet[]", "bit[]", "{127.0.0.1}"),
        ("\"char\"[]", "bytea[]", "{x}"),
    ] {
        assert_eq!(
            node.answer(&format!("SELECT '{value}'::{from}::{to}"))
                .to_string(),
            format!("!42846 cannot cast type {from} to {to}"),
            "'{value}'::{from}::{to}"
        );
    }
    // The scalar pairs, which were right all along.
    assert_eq!(
        node.answer("SELECT '1'::bit::money").to_string(),
        "!42846 cannot cast type bit to money"
    );
    assert_eq!(
        node.answer("SELECT 'x'::\"char\"::bytea").to_string(),
        "!42846 cannot cast type \"char\" to bytea"
    );
}

/// **And it is `42846` before the value is read, not the element reader's `22P02`.**
///
/// `'{x}'::"char"[]::bigint[]` used to reach `array_in` and come back
/// `22P02 invalid input syntax for type bigint: "x"` — a complaint about the *value* for a
/// conversion that does not exist. Three thousand of the four thousand diverging rows were that
/// substitution.
#[test]
fn the_refusal_is_about_the_cast_and_not_about_the_value() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer("SELECT '{x}'::\"char\"[]::bigint[]")
            .to_string(),
        "!42846 cannot cast type \"char\"[] to bigint[]"
    );
    // A value that *would* read cleanly is refused the same way, which is what says the check is
    // on the pair and not on the characters.
    assert_eq!(
        node.answer("SELECT '{1}'::\"char\"[]::bigint[]")
            .to_string(),
        "!42846 cannot cast type \"char\"[] to bigint[]"
    );
}

/// **The lower bound: the array casts that exist still work**, element-wise, which is the rule
/// `casts_to` has always had for a pair of arrays.
#[test]
fn the_array_casts_that_exist_still_work() {
    let mut node = parity::Node::new(&[]);
    for (sql, answer) in [
        ("SELECT ('{1,2}'::int4[]::int8[])::text", "{1,2}"),
        ("SELECT ('{1,2}'::int4[]::text[])::text", "{1,2}"),
        ("SELECT ('{1,2}'::int4[]::numeric[])::text", "{1,2}"),
        ("SELECT ('{a,b}'::text[]::varchar[])::text", "{a,b}"),
        ("SELECT ('{1}'::bit[]::varbit[])::text", "{1}"),
    ] {
        assert_eq!(node.rows(sql), vec![vec![answer]], "{sql}");
    }
    // And the empty array a cast supplies a type to, which this arm also answers.
    assert_eq!(
        node.rows("SELECT pg_typeof(ARRAY[]::int8[])"),
        vec![vec!["bigint[]"]]
    );
}

/// **An array's cast is the element's cast, and it was the source array's *text* read back.**
///
/// The scalar pair and the array pair disagreed, and the scalar was right. Measured on 19beta1
/// (`127.0.0.1:55432`, 2026-09-10) — every answer below is that server's:
///
/// ```text
/// 1.5::float8::integer            2       '{1.5,2.5}'::float8[]::integer[]      was 22P02
/// '2020-01-01'::date::character   2       '{2020-01-01}'::date[]::character[]   was 22001
/// ```
///
/// **Two roundings, because `pg_cast` has two functions**: a float is half to **even** (`1.5` and
/// `2.5` are both `2`) and a `numeric` is half **away from zero** (`2.5` is `3`, `-1.5` is `-2`).
/// An array that went through `array_in` had neither — it had `int4in`, which refuses a decimal
/// point.
#[test]
fn an_array_cast_is_its_element_cast() {
    let mut node = parity::Node::new(&[]);
    for (written, answer) in [
        // The two roundings, one array each.
        ("'{1.5,2.5}'::float8[]::integer[]", "{2,2}"),
        ("'{2.5,-1.5}'::numeric[]::integer[]", "{3,-2}"),
        // **The element's modifier is the array cast's**, which is what makes this truncate:
        // a bare `character` is `character(1)` and an explicit cast truncates in silence.
        ("'{2020-01-01}'::date[]::character[]", "{2}"),
        // `pg_cast`'s two `bool`/`int4` rows, both ways.
        ("'{t,f}'::boolean[]::integer[]", "{1,0}"),
        ("'{0,1}'::integer[]::boolean[]", "{f,t}"),
        // **The geometric conversions are computed, and an element never reached the arm that
        // computes them**: a `box`'s text handed to `circle_in` is `22P02`.
        (
            "'{\"((0,0),(1,1))\"}'::box[]::circle[]",
            "{\"<(0.5,0.5),0.7071067811865476>\"}",
        ),
        (
            "'{\"((0,0),(1,1))\"}'::box[]::polygon[]",
            "{\"((0,0),(0,1),(1,1),(1,0))\"}",
        ),
        ("'{\"((0,0),(1,1))\"}'::box[]::point[]", "{\"(0.5,0.5)\"}"),
        // **The control**: a pair whose element cast and whose `array_in` reading agree, so it
        // answered before this change and has to answer after it.
        ("'{1}'::text[]::bit[]", "{1}"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec![answer]],
            "{written} is {answer} on 19beta1"
        );
    }
}

/// **All three routes into the cast, because the literal was blamed and the column does it too.**
///
/// `d4be1a60` closed the pairs with **no** cast and its note says the same pair through a column
/// was refused correctly — true there, and not true for a pair that *has* a cast: the literal, the
/// constructor and the column all reached the same text round trip.
#[test]
fn every_route_into_an_array_cast_is_the_same_cast() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (f float8[])",
        "INSERT INTO t VALUES ('{1.5}')",
    ]);
    for written in [
        "'{1.5}'::float8[]::integer[]",
        "ARRAY[1.5::float8]::integer[]",
        "(SELECT f FROM t)::integer[]",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec!["{2}"]],
            "{written} is {{2}} on 19beta1"
        );
    }
    assert_eq!(
        node.rows("SELECT (f::integer[])::text FROM t"),
        vec![vec!["{2}"]],
        "and through the column, which is the route the note said was already right"
    );
}

/// **Ten pairs this node casts and 19beta1 refuses**, which is the class ADR 0031 ranks worst —
/// a *value* where a real server raises.
///
/// They are what is left of the matrix's "node answers, PG refuses" column, and they are one
/// mechanism: a cast whose **target** has its own lowering arm never asks `pg_cast` for
/// permission. `casts_to` is the gate every other cast goes through, and `CastTarget::OidVector`,
/// `::RegType`, `::RegClass` and `::Oid` each return before it — the same shape the literal-array
/// arm had before `d4be1a60`, third time in this family.
///
/// Measured on 19beta1 (`127.0.0.1:55432`, 2026-09-10), each one `42846 cannot cast type X to Y`:
///
/// ```text
/// '1'::bit::oid                     '{1}'::integer[]::oidvector
/// 'pg_class'::regclass::regtype     '1 2'::int2vector::oidvector
/// 'int4'::regtype::regclass         '{1}'::bigint[]::oidvector
/// ```
///
/// **`text -> lquery` is not in this list and was in the capture's**: 19beta1 answers it
/// (`'x'::text::lquery` is `x`, `'a.b'` likewise), so the node and the oracle agree and that row
/// needs re-taking, like the four the capture's header already names.
#[test]
fn a_cast_with_its_own_lowering_arm_still_asks_pg_cast() {
    let mut node = parity::Node::new(&["CREATE TABLE pg_class_probe (id bigint)"]);
    for (written, from, to) in [
        ("'1'::bit::oid", "bit", "oid"),
        ("'pg_class'::regclass::regtype", "regclass", "regtype"),
        ("'int4'::regtype::regclass", "regtype", "regclass"),
        ("'1 2'::int2vector::oidvector", "int2vector", "oidvector"),
        ("'{1}'::integer[]::oidvector", "integer[]", "oidvector"),
        ("'{1}'::bigint[]::oidvector", "bigint[]", "oidvector"),
        ("'{1}'::oid[]::oidvector", "oid[]", "oidvector"),
    ] {
        let answer = node.answer(&format!("SELECT {written}")).to_string();
        assert_eq!(
            answer,
            format!("!42846 cannot cast type {from} to {to}"),
            "{written} is 42846 on 19beta1"
        );
    }
    // **And the string source still casts**, which is the half `casts_to` already has right: a
    // cast *out of* a string type is the target's input function and needs no `pg_cast` row.
    assert_eq!(
        node.rows("SELECT pg_typeof('25 1043'::text::oidvector)"),
        vec![vec!["oidvector"]],
        "the I/O conversion out of a string is not what this refuses"
    );
    assert_eq!(
        node.rows("SELECT ('a.b'::text::lquery)::text"),
        vec![vec!["a.b"]],
        "and 19beta1 answers this one too, which the capture's row did not say"
    );
}
