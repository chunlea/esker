//! `time` without time zone, against PostgreSQL 19beta1 — statement 574 of `schema.rb`.
//!
//! Rails writes one `t.time :bonus_time` column and nothing else. The corpus is r1's, captured in
//! one session and replayed against a real server for idempotence before it was imported: 62
//! statements, of which the ones below are the answers this node does not reproduce.
//!
//! **The divergences here began as two absent types and are down to one.** `interval` is what a
//! `time` answers with whenever it is subtracted, multiplied or aggregated; it arrived, its
//! arithmetic came off this list one row at a time, and the last of it — the two casts between
//! the two types — closed with the cast matrix's residue. What is left is `timetz`, a separate
//! type with its own OID and a zone this node has nowhere to put, refused by name: ADR 0031's
//! rule, applied to the half of this type's surface that cannot be reached from here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// The other absent type.
const TIMETZ: &str = "**`timetz` is a different type** — OID 1266, twelve bytes, a `time` plus a \
     zone offset — and this node has neither it nor a zone to put in one. Refused by name. It is \
     in the corpus because a real server compares the two (`time = timetz` is `t`), which is the \
     only place they meet.";

/// Functions this node does not have, of which `time` is only the latest caller.
const FUNCTIONS: &str = "A function this node does not implement for any type, named rather than \
     answered. `extract` and `date_part` are one pair with two result types (`numeric` and \
     `double precision`), `greatest`/`least` are n-ary and typed by their arguments, and \
     `pg_typeof` reads the catalog; none is `time`'s to add, and each closes for every type at \
     once when it lands.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // **Moved from `answers` by parity rule 4.** Its reason was `pg_typeof` not being
        // implemented; it answers now, and what is left is the standing one — `pg_typeof` is a
        // `regtype` there and `text` here (ADR 0077).
        // **A cast's declared type carries no typmod here**, for any parameterised type: the
        // three below answer the right *values* — the rounding is exact, including `time(7)`
        // clamping to six digits — and report `time without time zone` where a real server
        // reports `time(3)`, `time(0)` and `time(6)`. Not this type's: `tests/numeric.rs`
        // declares the identical thing for `1.5::numeric(10,2)`, and a **column**'s typmod is
        // reported correctly by both (`tests/corpus/pg19_typmod.txt`). It closes when a cast
        // node carries the typmod it was written with into the row description.
        // `pg_type.typlen` is a column this node's catalog view does not have at all, so the
        // statement fails before a type is reached. Not `time`'s: the same query names `timetz`
        // too, and the column is missing for all sixteen types.
        "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname IN \
         ('time','timetz') ORDER BY oid",
        "SELECT '12:34:56'::time::timetz",
        "SELECT '12:34:56'::timetz::time",
        "SELECT extract(hour FROM '12:34:56'::time), extract(epoch FROM '12:34:56'::time)",
        "SELECT date_part('minute', '12:34:56'::time)",
        "SELECT '12:34:56'::time = '12:34:56'::timetz",
        "SELECT '12:34:56'::timetz, '12:34:56+02'::timetz, '12:34:56+02'::timetz::text",
    ],
    answers: &[
        (
            "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname IN \
             ('time','timetz') ORDER BY oid",
            "**`typlen` is a `pg_type` column this node's catalog view does not have**, so the \
             statement is `42703` before any type is looked at. `time`'s own row is right — \
             `tests/pg_catalog.rs` asserts `1083|time|…|time_in` in ActiveRecord's own query — \
             and the missing column belongs to the catalog surface, not to this type. The row \
             for `timetz` would be absent in any case.",
            "pg19_time.txt:49",
        ),
        (
            "SELECT '12:34:56'::time(-1)",
            "Both refuse with `42601`; the text differs. PostgreSQL's parser stops at the `-` and \
             says so; `sqlparser` says `Expected: literal int, found: -`. A negative precision is \
             a syntax error on both sides and neither reaches this type.",
            "pg19_time.txt:84",
        ),
        // `::char(n)` truncating on a cast and raising only on an assignment was declared
        // here and is closed: `debts-v1.1.md` #36 gave the cast and the row write their own
        // sides of one seam (`tests/typmod_seam.rs`). It had nothing to do with this type,
        // which is what every copy of it said — five entries across four files, one cause.
        // **Both casts between `time` and `interval` are closed and neither is listed here any
        // more.** `'1 day 02:00:00'::interval::time` went first, when `value::convert_without_text`
        // was given the pair (`debts-v1.1.md` #43); `'12:34:56'::time::interval` went with the cast
        // matrix's residue, and it was never a missing conversion — `refused_cast`'s table said the
        // pair had no cast at all, while `pg_cast` on a real server has one row for it. The arm
        // beside it already carried the finding in words ("an interval casts to a time too, which
        // the time unit could not know when it wrote this list") and the other direction was left
        // standing for a day.
        (
            "SELECT '12:34:56'::time::timetz",
            TIMETZ,
            "pg19_time.txt:89",
        ),
        (
            "SELECT '12:34:56'::timetz::time",
            TIMETZ,
            "pg19_time.txt:90",
        ),
        (
            "SELECT '12:34:56'::time + '12:34:56'::time",
            "Both refuse, with different codes and for different reasons. PostgreSQL has **too \
             many** candidates — `42725 operator is not unique`, with `Could not choose a best \
             candidate operator`, because a `time` can promote to an `interval` in more than one \
             way — where this node has none and says `0A000`. The one addition in this type that \
             is an error rather than a missing feature, and it closes when `interval` lands.",
            "pg19_time.txt:101",
        ),
        (
            "SELECT extract(hour FROM '12:34:56'::time), extract(epoch FROM '12:34:56'::time)",
            FUNCTIONS,
            "pg19_time.txt:102",
        ),
        (
            "SELECT date_part('minute', '12:34:56'::time)",
            FUNCTIONS,
            "pg19_time.txt:103",
        ),
        (
            "SELECT '12:34:56'::time = '12:34:56'::timetz",
            TIMETZ,
            "pg19_time.txt:105",
        ),
        (
            "SELECT '12:34:56'::timetz, '12:34:56+02'::timetz, '12:34:56+02'::timetz::text",
            TIMETZ,
            "pg19_time.txt:109",
        ),
    ],
};

#[test]
fn every_time_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_time.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 55,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `date + time` answers the same timestamp over **columns** as over constants.
///
/// It did not, and the gap this test was written to pin is closed. The constant form was a fold
/// at lowering; the per-row form needed an operator that produces a *value*, which every use of
/// `plan::BinaryOp` assumed was a boolean — so it waited for `plan::ArithOp` and the temporal
/// table beside it (`crate::value::temporal`, ADR 0046). What it asserts now is the property that
/// mattered either way: **the two forms agree**. A fold that answered something the evaluator
/// would not is the failure it was guarding against, and it is still the failure.
#[test]
fn a_column_plus_a_column_answers_what_the_constants_do() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE tp (id int8 PRIMARY KEY, t time, d date)",
        "INSERT INTO tp VALUES (1, '12:34:56', '2020-01-01')",
    ] {
        node.run(statement).unwrap();
    }

    // The constant form answers, both ways round.
    assert_eq!(
        node.rows("SELECT '12:34:56'::time + '2020-01-01'::date"),
        vec![vec!["2020-01-01 12:34:56"]]
    );
    assert_eq!(
        node.rows("SELECT '2020-01-01'::date + '12:34:56'::time"),
        vec![vec!["2020-01-01 12:34:56"]]
    );

    // And the per-row form, which is the half that was refused.
    assert_eq!(
        node.rows("SELECT d + t FROM tp"),
        vec![vec!["2020-01-01 12:34:56"]]
    );
    assert_eq!(
        node.rows("SELECT t + d FROM tp"),
        vec![vec!["2020-01-01 12:34:56"]]
    );
}

/// A `time` casts to a string and to nothing else, and the refusal is `42846` before any value.
#[test]
fn a_cast_with_no_path_is_refused_before_the_value_is_read() {
    let mut node = parity::Node::new(&[]);
    for (statement, target) in [
        (
            "SELECT '12:34:56'::time::timestamp",
            "timestamp without time zone",
        ),
        ("SELECT '12:34:56'::time::int", "integer"),
        ("SELECT '12:34:56'::time::json", "json"),
        ("SELECT '12:34:56'::time::numeric", "numeric"),
        ("SELECT '12:34:56'::time::date", "date"),
        ("SELECT '12:34:56'::time::bytea", "bytea"),
        ("SELECT '12:34:56'::time::boolean", "boolean"),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), "42846", "{statement}");
        assert_eq!(
            error.to_string(),
            format!("cannot cast type time without time zone to {target}"),
            "{statement}"
        );
    }

    // The three that do have a path, and the one that comes back the other way.
    assert_eq!(
        node.rows("SELECT '12:34:56'::time::text, '12:34:56'::time::varchar"),
        vec![vec!["12:34:56", "12:34:56"]]
    );
    assert_eq!(
        node.rows("SELECT '2020-01-01 12:34:56'::timestamp::time"),
        vec![vec!["12:34:56"]]
    );
    // ...and the shapes `time_in` will *not* read, which is narrower than a timestamp's parser:
    // the separator must be a space and there must be a time after it.
    for text in ["2020-01-01", "2020-01-01T12:34:56", "Jan 2 2020 12:34:56"] {
        let error = node.run(&format!("SELECT '{text}'::time")).unwrap_err();
        assert_eq!(error.sqlstate(), "22007", "{text}");
    }
}
