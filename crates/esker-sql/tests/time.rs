//! `time` without time zone, against PostgreSQL 19beta1 — statement 574 of `schema.rb`.
//!
//! Rails writes one `t.time :bonus_time` column and nothing else. The corpus is r1's, captured in
//! one session and replayed against a real server for idempotence before it was imported: 62
//! statements, of which the ones below are the answers this node does not reproduce.
//!
//! **Almost every divergence here is one of two absent types.** `interval` is what a `time`
//! answers with whenever it is subtracted, multiplied or aggregated, and `timetz` is a separate
//! type with its own OID. Neither exists yet, so each of those statements is `0A000` naming the
//! type rather than a wrong value — ADR 0031's rule, applied to the two halves of this type's
//! arithmetic that cannot be reached from here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// One of `DIVERGENCES`' reasons: the result type does not exist here.
const INTERVAL: &str = "**`interval` is not a type this node has.** Every arithmetic a `time` \
     takes part in answers one — `time - time`, `time * 2`, `time + interval`, `sum` and `avg` — \
     so each is `0A000` naming the construct rather than a number of the wrong type. The type is \
     ADR 0033's own unit and is not reachable from this one: an `interval` is months, days and \
     microseconds in three fields, not a count of anything, and nothing below `esker-sql` has a \
     place to put it.";

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
        // **A cast's declared type carries no typmod here**, for any parameterised type: the
        // three below answer the right *values* — the rounding is exact, including `time(7)`
        // clamping to six digits — and report `time without time zone` where a real server
        // reports `time(3)`, `time(0)` and `time(6)`. Not this type's: `tests/numeric.rs`
        // declares the identical thing for `1.5::numeric(10,2)`, and a **column**'s typmod is
        // reported correctly by both (`tests/corpus/pg19_typmod.txt`). It closes when a cast
        // node carries the typmod it was written with into the row description.
        "SELECT '12:34:56'::time(3), '12:34:56.9999'::time(3), '12:34:56.0005'::time(3)",
        "SELECT '12:34:56.5'::time(0)",
        "SELECT '12:34:56'::time(7)",
        // `pg_type.typlen` is a column this node's catalog view does not have at all, so the
        // statement fails before a type is reached. Not `time`'s: the same query names `timetz`
        // too, and the column is missing for all sixteen types.
        "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname IN \
         ('time','timetz') ORDER BY oid",
        "SELECT pg_typeof('12:34:56'::time)",
        "SELECT sum(t) FROM tq",
        "SELECT avg(t) FROM tq",
        "SELECT '12:34:56'::time::varchar, '12:34:56'::time::char(5)",
        "SELECT '12:34:56'::time::interval, '24:00:00'::time::interval",
        "SELECT '1 day 02:00:00'::interval::time",
        "SELECT '12:34:56'::time::timetz",
        "SELECT '12:34:56'::timetz::time",
        "SELECT '12:34:56'::time - '01:00:00'::time",
        "SELECT '12:34:56'::time + '1 hour'::interval",
        "SELECT '24:00:00'::time + '1 second'::interval",
        "SELECT '12:34:56'::time * 2",
        "SELECT extract(hour FROM '12:34:56'::time), extract(epoch FROM '12:34:56'::time)",
        "SELECT date_part('minute', '12:34:56'::time)",
        "SELECT '12:34:56'::time = '12:34:56'::timetz",
        "SELECT greatest('12:00:00'::time, '13:00:00'::time), least('12:00:00'::time, \
         '13:00:00'::time)",
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
        ),
        ("SELECT pg_typeof('12:34:56'::time)", FUNCTIONS),
        ("SELECT sum(t) FROM tq", INTERVAL),
        ("SELECT avg(t) FROM tq", INTERVAL),
        (
            "SELECT '12:34:56'::time(-1)",
            "Both refuse with `42601`; the text differs. PostgreSQL's parser stops at the `-` and \
             says so; `sqlparser` says `Expected: literal int, found: -`. A negative precision is \
             a syntax error on both sides and neither reaches this type.",
        ),
        (
            "SELECT '12:34:56'::time::varchar, '12:34:56'::time::char(5)",
            "**An explicit cast to `character(n)` truncates on a real server and raises `22001` \
             here.** `'12:34:56'::char(5)` is `12:34` there. Nothing to do with `time` — the \
             `varchar` half agrees, and the same `::char(5)` of any over-long string diverges the \
             same way — but this statement is where the corpus meets it, so it is recorded here \
             and belongs to `bpchar`'s cast path.",
        ),
        (
            "SELECT '12:34:56'::time::interval, '24:00:00'::time::interval",
            INTERVAL,
        ),
        ("SELECT '1 day 02:00:00'::interval::time", INTERVAL),
        ("SELECT '12:34:56'::time::timetz", TIMETZ),
        ("SELECT '12:34:56'::timetz::time", TIMETZ),
        ("SELECT '12:34:56'::time - '01:00:00'::time", INTERVAL),
        ("SELECT '12:34:56'::time + '1 hour'::interval", INTERVAL),
        ("SELECT '24:00:00'::time + '1 second'::interval", INTERVAL),
        ("SELECT '12:34:56'::time * 2", INTERVAL),
        (
            "SELECT '12:34:56'::time + '12:34:56'::time",
            "Both refuse, with different codes and for different reasons. PostgreSQL has **too \
             many** candidates — `42725 operator is not unique`, with `Could not choose a best \
             candidate operator`, because a `time` can promote to an `interval` in more than one \
             way — where this node has none and says `0A000`. The one addition in this type that \
             is an error rather than a missing feature, and it closes when `interval` lands.",
        ),
        (
            "SELECT extract(hour FROM '12:34:56'::time), extract(epoch FROM '12:34:56'::time)",
            FUNCTIONS,
        ),
        ("SELECT date_part('minute', '12:34:56'::time)", FUNCTIONS),
        ("SELECT '12:34:56'::time = '12:34:56'::timetz", TIMETZ),
        (
            "SELECT '12:34:56'::time = 1",
            "Both refuse with `42883` and name a different integer: PostgreSQL says `integer` \
             because a bare `1` is an `int4` there, and this node says `bigint` because it types \
             an unsuffixed integer literal as `int8`. A pre-existing divergence of the literal, \
             not of this type — every type's `= 1` says it — and it closes when integer literals \
             are typed by width.",
        ),
        (
            "SELECT greatest('12:00:00'::time, '13:00:00'::time), least('12:00:00'::time, \
             '13:00:00'::time)",
            FUNCTIONS,
        ),
        (
            "SELECT '12:34:56'::timetz, '12:34:56+02'::timetz, '12:34:56+02'::timetz::text",
            TIMETZ,
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

/// `date + time` is folded over **constants**, and a column pair is still named.
///
/// The boundary is deliberate and is the same one `lower_cast` draws. A per-row `+` needs a
/// `plan::BinaryOp::Plus` that produces a *value* where every one of that enum's seventy uses
/// assumes a comparison producing a boolean — an arithmetic unit, not a `time` one, and
/// `interval` (what every other `time` arithmetic answers) would have to land first for the
/// family to make sense. This test exists so that the gap is pinned rather than discovered.
#[test]
fn a_column_plus_a_column_is_still_named() {
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

    // The per-row form is still `0A000` naming the operator — a refusal, never a wrong value.
    // Arithmetic over the numeric types answers now (`tests/arithmetic.rs`); the temporal
    // operators are the commit after it, and until then the refusal says **which** operator over
    // **which** types rather than claiming `+` does not exist. PostgreSQL has this one.
    let error = node.run("SELECT d + t FROM tp").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(
        error.to_string(),
        "the operator + over date and time without time zone is not supported"
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
