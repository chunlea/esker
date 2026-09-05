//! `SET TIME ZONE` with a name, and the three things that then change.
//!
//! ADR 0080 gave this node the IANA table. Before it, every zone that was not a spelling of UTC
//! was `0A000` — the honest answer while a `timestamptz` printed in UTC and nowhere else, because
//! a setting honoured in `SHOW` and ignored in every row is a setting that lies.
//!
//! Everything asserted here is measured in `tests/captures/pg19_time_zone.txt`.
//!
//! # What a corpus cannot hold
//!
//! A parity corpus replays one session, and the zone's whole point is that the *same* value reads
//! differently in two of them. These tests set a zone and ask, which is a thing about a session
//! rather than about a statement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// A table with an instant in it, so the **column** path is exercised and not only a literal.
const FIXTURE: &[&str] = &[
    "CREATE TABLE moments (id int8 PRIMARY KEY, at timestamptz)",
    "INSERT INTO moments VALUES (1, '2011-01-01 23:30:00+00'), (2, '2020-07-01 00:00:00+00'), \
     (3, '1880-01-01 00:00:00+00')",
];

/// The name is checked against the table and stored **canonically**, whatever case it arrived in.
#[test]
fn a_zone_name_is_resolved_and_read_back_canonically() {
    let mut node = parity::Node::new(FIXTURE);

    node.run("SET TIME ZONE 'america/new_york'").unwrap();
    assert_eq!(node.rows("SHOW TimeZone"), vec![vec!["America/New_York"]]);

    node.run("SET TIME ZONE 'AMERICA/NEW_YORK'").unwrap();
    assert_eq!(node.rows("SHOW TimeZone"), vec![vec!["America/New_York"]]);

    // `Etc/UTC` is its own canonical name and reads back as itself, which is what a real server
    // does with it — the one row of the old hand-written UTC list that was right.
    node.run("SET TIME ZONE 'Etc/UTC'").unwrap();
    assert_eq!(node.rows("SHOW TimeZone"), vec![vec!["Etc/UTC"]]);
}

/// A name the table does not have is PostgreSQL's own `22023`, not a `0A000` of ours.
#[test]
fn a_zone_the_table_does_not_have_is_22023() {
    let mut node = parity::Node::new(FIXTURE);

    let error = node.run("SET TIME ZONE 'Nowhere/Notreal'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        error.to_string(),
        "invalid value for parameter \"TimeZone\": \"Nowhere/Notreal\""
    );

    // And the refused `SET` leaves the old value alone.
    assert_eq!(node.rows("SHOW TimeZone"), vec![vec!["UTC"]]);
}

/// The same stored instant, read in three zones.
#[test]
fn one_instant_reads_differently_in_three_zones() {
    let mut node = parity::Node::new(FIXTURE);

    assert_eq!(
        node.rows("SELECT at FROM moments WHERE id = 2"),
        vec![vec!["2020-07-01 00:00:00+00"]]
    );

    node.run("SET TIME ZONE 'America/New_York'").unwrap();
    assert_eq!(
        node.rows("SELECT at FROM moments WHERE id = 2"),
        vec![vec!["2020-06-30 20:00:00-04"]]
    );

    node.run("SET TIME ZONE 'Asia/Kathmandu'").unwrap();
    assert_eq!(
        node.rows("SELECT at FROM moments WHERE id = 2"),
        vec![vec!["2020-07-01 05:45:00+05:45"]]
    );

    // **The offset is printed in the shortest form that says it**, and a zone that is not a whole
    // number of minutes says so to the second: New York kept local mean time until 1883.
    node.run("SET TIME ZONE 'America/New_York'").unwrap();
    assert_eq!(
        node.rows("SELECT at FROM moments WHERE id = 3"),
        vec![vec!["1879-12-31 19:03:58-04:56:02"]]
    );
}

/// **The claim `tests/assignment_cast_date.rs` makes about the column form.**
///
/// A cast over a *literal* is folded at lowering, where there is no session, and that gap is
/// declared there. A cast over a **column** goes through `cursor::evaluate`, which renders under
/// the session — so the calendar day an instant falls on is the day it falls on *here*.
#[test]
fn a_cast_of_a_column_to_date_asks_which_day_it_is_in_this_zone() {
    let mut node = parity::Node::new(FIXTURE);

    // 23:30 UTC on the 1st is still the 1st in UTC.
    assert_eq!(
        node.rows("SELECT at::date FROM moments WHERE id = 1"),
        vec![vec!["2011-01-01"]]
    );

    // In Auckland it is 12:30 on the 2nd, and PostgreSQL answers the 2nd.
    node.run("SET TIME ZONE 'Pacific/Auckland'").unwrap();
    assert_eq!(
        node.rows("SELECT at::date FROM moments WHERE id = 1"),
        vec![vec!["2011-01-02"]]
    );
    assert_eq!(
        node.rows("SELECT at FROM moments WHERE id = 1"),
        vec![vec!["2011-01-02 12:30:00+13"]]
    );

    // And in New York it is 18:30 on the 1st.
    node.run("SET TIME ZONE 'America/New_York'").unwrap();
    assert_eq!(
        node.rows("SELECT at::date FROM moments WHERE id = 1"),
        vec![vec!["2011-01-01"]]
    );
}

/// **An instant that arrives as an expression takes the session's calendar day.**
///
/// This is the half of `timestamptz` -> `date` that works: `has_assignment_cast` takes the pair
/// and `value::assignment_cast` moves the instant into the zone before it takes the day off it.
/// The half that does not is a *literal* operand, which is folded at lowering where there is no
/// session — declared in `tests/assignment_cast_date.rs`, and the reason that file's row about a
/// stored date is still listed.
#[test]
fn an_instant_stored_in_a_date_column_is_the_day_it_is_here() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE days (id int8 PRIMARY KEY, at timestamptz, d date)")
        .unwrap();
    node.run("INSERT INTO days (id, at) VALUES (1, '2011-01-01 23:30:00+00')")
        .unwrap();

    // `d = at` is a **column**, so the value reaches `assign::into_column` with the session in
    // hand rather than being folded at lowering.
    node.run("SET TIME ZONE 'Pacific/Auckland'").unwrap();
    node.run("UPDATE days SET d = at").unwrap();
    assert_eq!(node.rows("SELECT d FROM days"), vec![vec!["2011-01-02"]]);

    // The same instant, stored from a session in New York, is the day before.
    node.run("SET TIME ZONE 'America/New_York'").unwrap();
    node.run("UPDATE days SET d = at").unwrap();
    assert_eq!(node.rows("SELECT d FROM days"), vec![vec!["2011-01-01"]]);
}

/// A cast to `timestamp` takes the **local** clock reading and drops the offset, which is a
/// second thing the zone changed: the input function used to *apply* a trailing offset, and did
/// so invisibly while every offset was `+00`.
#[test]
fn a_cast_to_timestamp_keeps_the_local_clock_and_drops_the_offset() {
    let mut node = parity::Node::new(FIXTURE);

    node.run("SET TIME ZONE 'Pacific/Auckland'").unwrap();
    assert_eq!(
        node.rows("SELECT at::timestamp FROM moments WHERE id = 1"),
        vec![vec!["2011-01-02 12:30:00"]]
    );
}
