//! `date`, against PostgreSQL 19beta1 — tier 2's first type, and statement 255 of `schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // Nineteen, and **every one is a refusal or a feature this node does not have** — not one is a
    // value where PostgreSQL answers something else. Every `date` this node stores, orders,
    // compares, prints and reads back is byte-identical to a real server's, which is the column
    // ADR 0031 measures a type by. Ten of the nineteen are one gap: this crate has no arithmetic.
    answers: &[
        (
            "SELECT oid, typname, typlen, typinput, typelem, typcategory FROM pg_type WHERE typname = 'date'",
            "`pg_type.typlen` is a column this node's `pg_type` does not have, for every type. Not this unit's: every other column of the row agrees, and the `date` row is there with oid 1082 and `date_in`.",
        ),
        (
            "SELECT pg_typeof('2020-01-01'::date)",
            "`pg_typeof` is `0A000` naming itself, for every type. Not this unit's.",
        ),
        (
            "SELECT format_type(1082, -1), format_type(1082, 3)",
            "`format_type(1082, 3)` prints `date(3)` on a real server: it prints the typmod it is handed whether or not the type takes one. A `date` **has** no typmod — `CREATE TABLE t (d date(3))` is a syntax error there — so no column can reach this and only a hand-written oid can.",
        ),
        (
            "SELECT 'tomorrow'::date - 'today'::date, 'today'::date - 'yesterday'::date",
            "Arithmetic. This crate has **no arithmetic operators at all** — `a + 1` is `0A000` naming itself for every type — which is older than this unit and is the one thing `date` inherits rather than introduces. The nine lines below are the same gap.",
        ),
        (
            "SELECT '2020-01-02'::date::varchar, '2020-01-02'::date::char(4)",
            "`::char(n)` **truncates** on an explicit cast and raises `22001` only on an assignment; this node raises on both. Older than this unit — it is `value::fit_to_typmod`, and it does the same for a `text` source.",
        ),
        (
            "SELECT '2020-01-31'::date - '2020-01-01'::date",
            "Arithmetic; see above.",
        ),
        (
            "SELECT '2020-01-01'::date + 30, '2020-01-01'::date - 30",
            "Arithmetic; see above.",
        ),
        (
            "SELECT '2020-01-01'::date + '1 day'::interval, '2020-01-01'::date - '1 day'::interval",
            "Arithmetic, and `interval` is a type this node does not have — tier 2's own entry.",
        ),
        (
            "SELECT '2020-06-15'::date - '2020-06-15'::timestamp",
            "Arithmetic; see above.",
        ),
        (
            "SELECT age('2020-01-01'::date, '2019-01-01'::date)",
            "`age` answers an `interval`, which this node does not have.",
        ),
        (
            "SELECT 'infinity'::date - 1",
            "Arithmetic; see above. The **value** would be right — `infinity` absorbs it, and `date::as_micros` keeps the sentinel — there is simply no operator to apply.",
        ),
        (
            "SELECT '2020-01-01'::date + 1.5",
            "Arithmetic. PostgreSQL raises `42883` and this node `0A000`: both refuse, and the reason differs — there the operator is missing for that *pair*, here for every pair.",
        ),
        (
            "SELECT '2020-01-01'::date + '2020-01-01'::date",
            "Arithmetic; see above.",
        ),
        ("SELECT '2020-01-01'::date * 2", "Arithmetic; see above."),
        (
            "SELECT extract(year FROM '2020-06-15'::date), extract(doy FROM '2020-06-15'::date), extract(epoch FROM '2020-06-15'::date)",
            "`extract` is `0A000` naming itself, for every type. Its neighbour is worth keeping in the corpus for what it says: `extract` answers `numeric` and `date_part` answers `double precision` for the same question.",
        ),
        (
            "SELECT date_part('month', '2020-06-15'::date)",
            "`date_part` is `0A000` naming itself.",
        ),
        (
            "SELECT to_char('2020-06-15'::date, 'YYYY-MM-DD'), to_date('2020-06-15', 'YYYY-MM-DD')",
            "`to_char`/`to_date` are `0A000` naming themselves — a whole format-picture language, and nothing `ActiveRecord` sends.",
        ),
        (
            "SELECT greatest('2020-01-01'::date, '2019-01-01'::date), least('2020-01-01'::date, '2019-01-01'::date)",
            "`greatest`/`least` are `0A000` naming themselves, for every type.",
        ),
        (
            "SELECT '2020-01-01'::date = 1",
            "**Both raise `42883`, and the type named in the sentence differs**: `date = bigint` here where a real server says `date = integer`, because a bare integer constant is `int8` in this crate. The declared bare-integer divergence (`tests/unknown_literal.rs`), reached through a new type — and it is here rather than absent because this unit is what made the line raise at all: before it, a `date` compared to an integer answered **`f`**.",
        ),
    ],
};

#[test]
fn every_date_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_date.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// A `date` column round-trips through a **row**, an **index key** and the catalog record.
///
/// The corpus proves the values; this proves the storage under them, which is the half a type that
/// only ever lived in a `SELECT` would pass by accident.
#[test]
fn a_date_column_stores_orders_and_keys() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE dt (id int8 PRIMARY KEY, d date)",
        "CREATE UNIQUE INDEX dt_d ON dt (d)",
        "INSERT INTO dt VALUES (1, '2020-01-02')",
        "INSERT INTO dt VALUES (2, '1999-12-31')",
        "INSERT INTO dt VALUES (3, '4713-01-01 BC')",
        "INSERT INTO dt VALUES (4, 'infinity')",
    ] {
        node.run(statement).unwrap();
    }
    // The unique index is over the encoded day, so a duplicate collides.
    let error = node
        .run("INSERT INTO dt VALUES (5, '2020-01-02')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");

    // Key order is value order, the ends included.
    assert_eq!(
        node.rows("SELECT id FROM dt ORDER BY d"),
        [["3"], ["2"], ["1"], ["4"]]
    );
    // And the index answers a pinned lookup with the row the scan would have found.
    assert_eq!(
        node.rows("SELECT id FROM dt WHERE d = '1999-12-31'"),
        [["2"]]
    );
}

/// The two `22008` messages, and the hint that belongs to only one of them.
///
/// `2020-13-01` carries `Perhaps you need a different "DateStyle" setting.` **because a 13 could
/// have been a day**; `2020-02-30` does not, because a 30 could not have been a month. Measured
/// both ways, and for `timestamp` as well — the rule is the datetime parser's, not the type's.
#[test]
fn the_two_out_of_range_messages_and_the_one_hint() {
    let mut node = parity::Node::new(&[]);
    for (sql, message, hint) in [
        (
            "SELECT '5874898-01-01'::date",
            "date out of range: \"5874898-01-01\"",
            None,
        ),
        (
            "SELECT '2020-02-30'::date",
            "date/time field value out of range: \"2020-02-30\"",
            None,
        ),
        (
            "SELECT '2020-13-01'::date",
            "date/time field value out of range: \"2020-13-01\"",
            Some("Perhaps you need a different \"DateStyle\" setting."),
        ),
        (
            "SELECT '2020-13-01 00:00:00'::timestamp",
            "date/time field value out of range: \"2020-13-01 00:00:00\"",
            Some("Perhaps you need a different \"DateStyle\" setting."),
        ),
        (
            "SELECT '2020-02-30 00:00:00'::timestamp",
            "date/time field value out of range: \"2020-02-30 00:00:00\"",
            None,
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "22008", "for {sql}");
        assert_eq!(error.to_string(), message, "for {sql}");
        assert_eq!(error.hint().as_deref(), hint, "for {sql}");
    }
}

/// There is no year zero, and the rule is about the year **as written**.
#[test]
fn there_is_no_year_zero_and_one_bc_is_a_value() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '0001-01-01 BC'::date, '0001-01-01'::date"),
        [["0001-01-01 BC", "0001-01-01"]]
    );
    let error = node.run("SELECT '0000-01-01'::date").unwrap_err();
    assert_eq!(error.sqlstate(), "22008");
}

/// A `date` is not a number, in either direction — `42846` before any value is read.
#[test]
fn a_date_has_no_cast_to_a_number() {
    let mut node = parity::Node::new(&[]);
    for (sql, message) in [
        (
            "SELECT '2020-01-01'::date::int",
            "cannot cast type date to integer",
        ),
        ("SELECT 1::date", "cannot cast type integer to date"),
        (
            "SELECT '2020-06-15'::date::json",
            "cannot cast type date to json",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42846", "for {sql}");
        assert_eq!(error.to_string(), message, "for {sql}");
    }
}

/// A `date` compares with a `timestamp` — it is the midnight it names — and **not** with a number.
///
/// The second half is what this unit fixed rather than added: before the family check reached two
/// literals, `'2020-01-01'::date = 1` answered `f`, which is a value where a real server raises.
#[test]
fn a_date_compares_with_a_timestamp_and_not_with_a_number() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '2020-01-01'::date = '2020-01-01'::timestamp"),
        [["t"]]
    );
    assert_eq!(
        node.rows("SELECT '2020-01-01'::date < '2020-01-01 00:00:01'::timestamp"),
        [["t"]]
    );
    let error = node.run("SELECT '2020-01-01'::date = 1").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
}

/// `DATE '2020-01-01'` is the same thing as `'2020-01-01'::date`, which is what a real server
/// records: SQL's typed-literal spelling and the cast are one node.
#[test]
fn a_typed_literal_is_the_cast_written_the_other_way() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT DATE '2020-01-01', '2020-01-01'::date"),
        [["2020-01-01", "2020-01-01"]]
    );
    assert_eq!(
        node.rows("SELECT TIMESTAMP '2020-01-01 12:00:00'"),
        [["2020-01-01 12:00:00"]]
    );
}
