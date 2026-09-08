//! `date_trunc`, against PostgreSQL 19beta1.
//!
//! `PostgresqlTimestampFixtureTest#test_group_by_date` sends
//! `Topic.group("date_trunc('month', created_at)").count`, which this node refused by name. What
//! makes it a *type* unit rather than a missing function is the assertion:
//!
//! ```text
//! keys.each { |k| assert_kind_of Time, k }
//! ```
//!
//! The test reads the **keys**, so the declared type decides it. A `text` answer with the right
//! characters in it is a `String` in Ruby and fails on a row whose value is correct.
//!
//! Three things here are not what reasoning gives. A `date` argument resolves to the
//! **`timestamptz`** overload, not the unzoned one. A `timestamptz` is truncated **in the session
//! time zone**, so under `America/New_York` a `month` truncation of `2026-09-01 02:00:00+00` lands
//! in August — which is what makes the zone reader (ADR 0080) load-bearing for a function that
//! looks like arithmetic. And the unit is a synonym table that is not regular: `quarter` alone has
//! no plural and no abbreviation, while `m` is the *minute*.
//!
//! Everything asserted is measured in `tests/captures/pg19_date_trunc.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// The corpus's one table, which is `topics` under another name.
const CORPUS_FIXTURE: &[&str] = &[
    "CREATE TABLE dt_topics (id int8, created_at timestamp)",
    "INSERT INTO dt_topics VALUES (1,'2026-09-05 14:37:59.123456'),(2,'2026-09-20 01:02:03'),\
     (3,'2026-08-01 00:00:00')",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **One fact, six times**: `pg_typeof` answers a `regtype` on a real server and `text` here,
    // the trade `'x'::regtype` makes everywhere in this crate (ADR 0077). The rows are identical.
    types: &[
        "SELECT pg_typeof(date_trunc('day', TIMESTAMP '2026-09-05 14:37:59'))",
        "SELECT pg_typeof(date_trunc('day', TIMESTAMPTZ '2026-09-05 14:37:59+00'))",
        "SELECT pg_typeof(date_trunc('hour', INTERVAL '3 days 04:05:06.789'))",
        "SELECT pg_typeof(date_trunc('day', DATE '2026-09-05'))",
        "SELECT pg_typeof(date_trunc('day', TIMESTAMPTZ '2026-09-05 14:37:59+00', \
         'Australia/Sydney'))",
    ],
    answers: &[],
};

#[test]
fn every_date_trunc_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_date_trunc.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 100,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The statement the suite actually sends**, with the assertion the suite actually makes.
///
/// A `GROUP BY` over a function call is the shape; the type of the grouped column is the test.
#[test]
fn the_grouped_column_is_a_timestamp_and_not_its_text() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    let outcome = node
        .run(
            "SELECT COUNT(*) AS count_all, date_trunc('month', created_at) \
             AS date_trunc_month_created_at FROM dt_topics \
             GROUP BY date_trunc('month', created_at) ORDER BY 2",
        )
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, rows, .. } = outcome else {
        panic!("the grouped statement answered no rows");
    };
    assert_eq!(fields.len(), 2);
    // 20 is `bigint` and 1114 is `timestamp without time zone`. **1114 is the assertion**: 25
    // (`text`) would give `assert_kind_of Time` a String and fail the suite's test on rows whose
    // characters are right.
    assert_eq!(fields[0].type_oid, 20);
    assert_eq!(fields[1].type_oid, 1114);
    assert_eq!(rows.len(), 2);
}

/// **The zone is not decoration.** The same statement answers a different *month* in New York.
#[test]
fn a_timestamptz_is_cut_in_the_session_zone() {
    let mut node = parity::Node::new(&[]);
    let statement = "SELECT date_trunc('month', TIMESTAMPTZ '2026-09-01 02:00:00+00')";
    node.run("SET TimeZone = 'UTC'").unwrap();
    assert_eq!(node.rows(statement), vec![vec!["2026-09-01 00:00:00+00"]]);
    node.run("SET TimeZone = 'America/New_York'").unwrap();
    assert_eq!(node.rows(statement), vec![vec!["2026-08-01 00:00:00-04"]]);
    // An unzoned timestamp is cut where it is written and the setting does not reach it.
    assert_eq!(
        node.rows("SELECT date_trunc('day', TIMESTAMP '2026-09-05 14:37:59')"),
        vec![vec!["2026-09-05 00:00:00"]]
    );
}

/// **The synonym table, and the one unit that has no synonyms.**
#[test]
fn the_unit_is_a_synonym_table_that_is_not_regular() {
    let mut node = parity::Node::new(&[]);
    let at = "TIMESTAMP '2026-09-05 14:37:59.123456'";
    for (unit, expected) in [
        ("microseconds", "2026-09-05 14:37:59.123456"),
        ("usec", "2026-09-05 14:37:59.123456"),
        ("us", "2026-09-05 14:37:59.123456"),
        ("msec", "2026-09-05 14:37:59.123"),
        ("ms", "2026-09-05 14:37:59.123"),
        ("secs", "2026-09-05 14:37:59"),
        ("s", "2026-09-05 14:37:59"),
        // `m` is the minute. Nothing about the letter says so.
        ("m", "2026-09-05 14:37:00"),
        ("mins", "2026-09-05 14:37:00"),
        ("hrs", "2026-09-05 14:00:00"),
        ("h", "2026-09-05 14:00:00"),
        ("d", "2026-09-05 00:00:00"),
        ("w", "2026-08-31 00:00:00"),
        ("mon", "2026-09-01 00:00:00"),
        ("mons", "2026-09-01 00:00:00"),
        ("months", "2026-09-01 00:00:00"),
        ("yrs", "2026-01-01 00:00:00"),
        ("y", "2026-01-01 00:00:00"),
        ("dec", "2020-01-01 00:00:00"),
        ("centuries", "2001-01-01 00:00:00"),
        ("cent", "2001-01-01 00:00:00"),
        ("c", "2001-01-01 00:00:00"),
        ("millennia", "2001-01-01 00:00:00"),
        ("mil", "2001-01-01 00:00:00"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT date_trunc('{unit}', {at})")),
            vec![vec![expected.to_owned()]],
            "the unit {unit} did not truncate"
        );
    }
    // **`quarter` is the exception**: no plural, no abbreviation, while every unit above has one.
    assert_eq!(
        node.rows(&format!("SELECT date_trunc('quarter', {at})")),
        vec![vec!["2026-07-01 00:00:00".to_owned()]]
    );
    for refused in ["quarters", "q"] {
        let error = node
            .run(&format!("SELECT date_trunc('{refused}', {at})"))
            .unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::INVALID_PARAMETER_VALUE,
            "{refused} was not refused"
        );
    }
}

/// **Two classes, and the boundary is not where reasoning puts it.**
///
/// A unit the table does not hold is `22023`; a unit it holds that this function cannot apply is
/// `0A000`. `epoch` is an `EXTRACT` field — a real unit — and it lands in the *first* class.
#[test]
fn an_unknown_unit_and_an_inapplicable_one_are_different_errors() {
    let mut node = parity::Node::new(&[]);
    let at = "TIMESTAMP '2026-09-05 14:37:59'";

    for (unit, message) in [
        ("fortnight", "fortnight"),
        ("epoch", "epoch"),
        // Not trimmed.
        (" month", " month"),
    ] {
        let error = node
            .run(&format!("SELECT date_trunc('{unit}', {at})"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            error.to_string(),
            format!("unit \"{message}\" not recognized for type timestamp without time zone")
        );
    }

    let error = node
        .run(&format!("SELECT date_trunc('timezone', {at})"))
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "unit \"timezone\" not supported for type timestamp without time zone"
    );

    // An interval refuses one field, and says why.
    let error = node
        .run("SELECT date_trunc('week', INTERVAL '3 days')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "unit \"week\" not supported for type interval"
    );
    // The sentence is a `DETAIL` line of its own, which is where a real server puts it.
    assert_eq!(
        error.detail().as_deref(),
        Some("Months usually have fractional weeks.")
    );
}

/// A century begins in year 1, so 2000 belongs to the one before 2001's.
#[test]
fn the_century_of_2000_is_not_the_century_of_2001() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT date_trunc('century', TIMESTAMP '2000-06-01 00:00:00')"),
        vec![vec!["1901-01-01 00:00:00"]]
    );
    assert_eq!(
        node.rows("SELECT date_trunc('century', TIMESTAMP '2001-06-01 00:00:00')"),
        vec![vec!["2001-01-01 00:00:00"]]
    );
    assert_eq!(
        node.rows("SELECT date_trunc('millennium', TIMESTAMP '2000-06-01 00:00:00')"),
        vec![vec!["1001-01-01 00:00:00"]]
    );
}

/// **An interval keeps its own fields and loses the ones below**, and truncating past its size
/// gives `00:00:00` rather than an empty interval.
#[test]
fn an_interval_truncated_past_its_size_is_zero() {
    let mut node = parity::Node::new(&[]);
    let iv = "INTERVAL '3 years 4 mons 5 days 06:07:08.9'";
    assert_eq!(
        node.rows(&format!("SELECT date_trunc('day', {iv})")),
        vec![vec!["3 years 4 mons 5 days"]]
    );
    assert_eq!(
        node.rows(&format!("SELECT date_trunc('quarter', {iv})")),
        vec![vec!["3 years 3 mons"]]
    );
    assert_eq!(
        node.rows(&format!("SELECT date_trunc('decade', {iv})")),
        vec![vec!["00:00:00"]]
    );
}

/// The arity failures are `42883` naming the signature, and they name two different things.
#[test]
fn the_arity_failures_name_the_signature() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("SELECT date_trunc('day', 42)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "function date_trunc(unknown, integer) does not exist"
    );
    // **`integer`, not `bigint`.** An unadorned `42` is an `integer` to PostgreSQL's resolver and
    // an `int8` to this node's value layer, which is why the refusal is raised where the written
    // literal is still visible rather than where its datum is.
    assert_eq!(
        error.detail().as_deref(),
        Some("No function of that name accepts the given argument types.")
    );

    // The wrong *number* of arguments is the same code and a different sentence.
    let error = node.run("SELECT date_trunc('day')").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "function date_trunc(unknown) does not exist"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("No function of that name accepts the given number of arguments.")
    );
}

/// Either argument NULL is NULL, and the zone argument is checked against the same reader
/// `SET TimeZone` uses.
#[test]
fn a_null_argument_and_an_unknown_zone() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT date_trunc(NULL, TIMESTAMP '2026-09-05 14:37:59') IS NULL"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT date_trunc('day', NULL::timestamp) IS NULL"),
        vec![vec!["t"]]
    );
    let error = node
        .run("SELECT date_trunc('day', TIMESTAMPTZ '2026-09-05 14:37:59+00', 'Mars/Olympus')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        error.to_string(),
        "time zone \"Mars/Olympus\" not recognized"
    );
}

/// **An infinite date is cut to itself.** `date_trunc('day', 'infinity'::date)` is `infinity` on
/// a real server; the `date` arm used to multiply the sentinel's day count into microseconds and
/// overflow.
#[test]
fn an_infinite_date_truncates_to_itself() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT date_trunc('day', 'infinity'::date), date_trunc('month', '-infinity'::date)"
        ),
        vec![vec!["infinity", "-infinity"]]
    );
}
