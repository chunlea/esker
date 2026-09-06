//! `IntervalStyle`, which `ActiveRecord` cannot read an interval without.
//!
//! # Why a setting nobody reads was two errors on the board
//!
//! Run 100's `interval_test.rb` fails twice with `NoMethodError: undefined method 'iso8601' for
//! nil` — the value is **gone**, not wrong, and no error reached the client. The adapter sends
//! `SET intervalstyle = iso_8601` at connect (`postgresql_adapter.rb:1001`), and
//! `OID::Interval#cast_value` parses what comes back with `ActiveSupport::Duration.parse` and
//! **rescues the failure by returning `nil`**. So a server that answers `6 years 5 mons 4 days
//! 03:02:01` where the client asked for `P6Y5M4DT3H2M1S` hands back nothing at all, silently.
//!
//! This node stored the setting and ignored it — `crate::parameter`'s own note said so, and said
//! why: "it decides how an `interval` prints, and this node has no `interval`". That was true when
//! it was written.
//!
//! # What is measured
//!
//! All four styles over forty-one values, `tests/captures/pg19_interval_style.txt`. The sign rules
//! are the part that cannot be reasoned out: no two styles agree about `1 month -1 day`, and the
//! `postgres` style — the one this node already had — turned out to print a **`+`** on a field
//! that follows a negative one, which it was not doing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE iv (id int8 PRIMARY KEY, term interval, terms interval[])",
    "INSERT INTO iv VALUES (1, '6 years 5 mons 4 days 3 hours 2 mins 1 sec', \
     ARRAY['1 month'::interval, '1 year', '1 hour'])",
];

fn under(node: &mut parity::Node, style: &str, sql: &str) -> Vec<Vec<String>> {
    node.run(&format!("SET intervalstyle = '{style}'")).unwrap();
    node.rows(sql)
}

/// The two statements `test_interval_type` and `test_interval_type_cast_from_numeric` come down
/// to: a column read back under the style the connection asked for.
#[test]
fn a_column_prints_under_the_session_style() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        under(&mut node, "postgres", "SELECT term FROM iv"),
        [["6 years 5 mons 4 days 03:02:01".to_owned()]]
    );
    assert_eq!(
        under(&mut node, "iso_8601", "SELECT term FROM iv"),
        [["P6Y5M4DT3H2M1S".to_owned()]]
    );
    assert_eq!(
        under(&mut node, "postgres_verbose", "SELECT term FROM iv"),
        [["@ 6 years 5 mons 4 days 3 hours 2 mins 1 sec".to_owned()]]
    );
    assert_eq!(
        under(&mut node, "sql_standard", "SELECT term FROM iv"),
        [["+6-5 +4 +3:02:01".to_owned()]]
    );
}

/// **An array's elements print under it too**, which is `all_terms` in the same Rails test.
#[test]
fn an_array_element_prints_under_the_session_style() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        under(&mut node, "iso_8601", "SELECT terms FROM iv"),
        [["{P1M,P1Y,PT1H}".to_owned()]]
    );
    assert_eq!(
        under(&mut node, "postgres", "SELECT terms FROM iv"),
        [["{\"1 mon\",\"1 year\",01:00:00}".to_owned()]]
    );
}

/// A literal, an expression and a `RETURNING` reach the client by three different paths.
#[test]
fn every_path_to_the_client_uses_the_style() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("SET intervalstyle = 'iso_8601'").unwrap();
    assert_eq!(
        node.rows("SELECT '1 year 2 mons'::interval"),
        [["P1Y2M".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT term + '1 day'::interval FROM iv"),
        [["P6Y5M5DT3H2M1S".to_owned()]]
    );
    assert_eq!(
        node.rows("INSERT INTO iv VALUES (2, '10 hours', NULL) RETURNING term"),
        [["PT10H".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT max(term) FROM iv"),
        [["P6Y5M4DT3H2M1S".to_owned()]]
    );
}

/// **A cast to `text` does not move with the style yet, and this test asserts that on purpose.**
///
/// `interval_out` is one function on a real server, so *everything* that turns an interval into a
/// string follows the setting — measured under `iso_8601`, all seven answering `P1Y`:
///
/// ```text
/// ('1 year'::interval)::text        P1Y
/// cast('1 year'::interval AS text)  P1Y
/// '1 year'::interval || ''          P1Y
/// format('%s', '1 year'::interval)  P1Y
/// ('1 year'::interval)::varchar     P1Y
/// array_to_string(ARRAY[...], ',')  P1Y
/// jsonb_build_object('a', ...)      {"a": "P1Y"}
/// ```
///
/// The setting **does** reach expression evaluation now — `SELECT term::text FROM iv` answers
/// `P6Y5M4DT3H2M1S` — because `cursor::Settings` carries it beside `search_path`. What is left is
/// narrower and is what this test pins: a cast whose operand is a **literal** is folded at
/// lowering, where there is no session at all, so `('1 year'::interval)::text` is decided before
/// any of this runs. `||` is the other one, evaluated in a function that has no `Env`.
///
/// The value below is **this node's, not PostgreSQL's**, with PostgreSQL's in the message: a
/// placeholder that can only be got rid of by being deleted, which is the shape g1 handed this
/// lane for `interval(p)` and the reason that one could not be quietly edited away.
#[test]
fn a_cast_to_text_does_not_move_with_the_style_yet() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("SET intervalstyle = 'iso_8601'").unwrap();
    // The column case, which is what the session now reaches.
    assert_eq!(
        node.rows("SELECT term::text FROM iv"),
        [["P6Y5M4DT3H2M1S".to_owned()]]
    );
    assert_eq!(
        node.rows("SELECT ('1 year'::interval)::text"),
        [["1 year".to_owned()]],
        "PostgreSQL 19beta1 answers P1Y here: `interval_out` is one function and every cast, \
         concatenation and format call goes through it. A cast over a COLUMN follows the \
         setting here; this one is folded at lowering, where there is no session. When lowering \
         stops folding an interval's output function, delete this test rather than editing it."
    );
}

/// The style is session state: `RESET` and a new value both take effect, and the boot value is
/// `postgres`.
#[test]
fn the_setting_is_read_back_and_reset() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SHOW intervalstyle"), [["postgres".to_owned()]]);
    node.run("SET intervalstyle = 'iso_8601'").unwrap();
    assert_eq!(node.rows("SHOW intervalstyle"), [["iso_8601".to_owned()]]);
    node.run("RESET intervalstyle").unwrap();
    assert_eq!(
        node.rows("SELECT term FROM iv"),
        [["6 years 5 mons 4 days 03:02:01".to_owned()]]
    );
}

/// **The `postgres` style puts a `+` on a field that follows a negative one**, which this node was
/// not doing. Measured on 19beta1, and the reason it matters is that the same rule decides whether
/// the text reads back as itself.
#[test]
fn the_postgres_style_signs_a_field_after_a_negative_one() {
    let mut node = parity::Node::new(&[]);
    for (written, expected) in [
        ("-1 month 1 day", "-1 mons +1 day"),
        ("-1 day 1 second", "-1 days +00:00:01"),
        ("-1 month 1 hour", "-1 mons +01:00:00"),
        ("1 month -1 day", "1 mon -1 days"),
        ("1 day -1 second", "1 day -00:00:01"),
        ("-2 months -3 hours", "-2 mons -03:00:00"),
        ("1 year -1 month", "11 mons"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT '{written}'::interval")),
            [[expected.to_owned()]],
            "{written}"
        );
    }
}

/// The sign rules of the other three styles, over the values that separate them.
#[test]
fn each_style_signs_a_mixed_value_its_own_way() {
    let mut node = parity::Node::new(&[]);
    for (style, written, expected) in [
        ("iso_8601", "-1 month 1 day", "P-1M1D"),
        ("iso_8601", "-4 hours -5 mins -6 secs", "PT-4H-5M-6S"),
        ("iso_8601", "-0.5 seconds", "PT-0.5S"),
        ("iso_8601", "0 seconds", "PT0S"),
        ("iso_8601", "59.9999999 seconds", "PT1M"),
        ("iso_8601", "1 month 1 hour", "P1MT1H"),
        ("postgres_verbose", "-1 month 1 day", "@ 1 mon -1 days ago"),
        (
            "postgres_verbose",
            "-4 hours -5 mins -6 secs",
            "@ 4 hours 5 mins 6 secs ago",
        ),
        ("postgres_verbose", "-1 day 1 second", "@ 1 day -1 sec ago"),
        ("postgres_verbose", "1 day -1 second", "@ 1 day -1 sec"),
        ("postgres_verbose", "0 seconds", "@ 0"),
        ("postgres_verbose", "-0.5 seconds", "@ 0.5 secs ago"),
        ("postgres_verbose", "1 hour", "@ 1 hour"),
        ("sql_standard", "-1 month 1 day", "-0-1 +1 +0:00:00"),
        ("sql_standard", "1 day -1 hour", "+0-0 +1 -1:00:00"),
        ("sql_standard", "-1 month", "-0-1"),
        ("sql_standard", "-1 day", "-1 0:00:00"),
        ("sql_standard", "-4 hours -5 mins -6.5 secs", "-4:05:06.5"),
        ("sql_standard", "1 hour", "1:00:00"),
        ("sql_standard", "0 seconds", "0"),
        ("sql_standard", "1 year 2 mons 3 days", "+1-2 +3 +0:00:00"),
        ("sql_standard", "1 year 2 mons 4 hours", "+1-2 +0 +4:00:00"),
    ] {
        node.run(&format!("SET intervalstyle = '{style}'")).unwrap();
        assert_eq!(
            node.rows(&format!("SELECT '{written}'::interval")),
            [[expected.to_owned()]],
            "{style}: {written}"
        );
    }
}

/// **All fifty values under all four styles**, replayed from the capture's own table.
///
/// The focused tests above name the traps; this one is the measurement, so a rule that happens to
/// be right for the seven values a reader thought of cannot pass. Two hundred answers, and a
/// corpus with no divergence column because every one of them agrees.
#[test]
fn every_measured_value_agrees_under_every_style() {
    let mut node = parity::Node::new(&[]);
    let mut checked = 0;
    for (at, line) in include_str!("corpus/pg19_interval_style.txt")
        .lines()
        .enumerate()
    {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 5, "line {}: {line}", at + 1);
        let written = fields[0];
        for (style, expected) in ["postgres", "iso_8601", "postgres_verbose", "sql_standard"]
            .into_iter()
            .zip(&fields[1..])
        {
            node.run(&format!("SET intervalstyle = '{style}'")).unwrap();
            assert_eq!(
                node.rows(&format!("SELECT '{written}'::interval")),
                [[(*expected).to_owned()]],
                "line {}: '{written}' under {style}",
                at + 1
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 200,
        "only {checked} answers ran; the corpus did not load"
    );
}

/// **`IntervalStyle` changes how a leading sign is *read*, and that half is not done here.**
///
/// Under `sql_standard` a real server applies a leading negative to every field that follows, so
/// `'-1 month 1 day'` is minus one month and minus one **day**; under the other three styles the
/// day is positive. Measured on 19beta1 — `extract(day from '-1 month 1 day'::interval)` is `-1`
/// under `sql_standard` and `1` under `postgres`, in the same session, one `SET` apart.
///
/// This node reads a literal the same way whatever the setting is, because the input function is
/// reached from lowering (where there is no session at all) as well as from the executor. The
/// value below is **this node's**, with PostgreSQL's in the message.
///
/// It is also how the corpus beside this was found to be wrong: its first version parsed each
/// string under the style it then rendered in, so it was measuring two rules at once and
/// disagreed with a hand-taken probe of the same value.
#[test]
fn a_leading_sign_is_read_the_same_way_under_every_style() {
    let mut node = parity::Node::new(&[]);
    node.run("SET intervalstyle = 'sql_standard'").unwrap();
    assert_eq!(
        node.rows("SELECT '-1 month 1 day'::interval"),
        [["-0-1 +1 +0:00:00".to_owned()]],
        "PostgreSQL 19beta1 answers -0-1 -1 +0:00:00 here: under sql_standard the leading \
         negative applies to the day as well. When the input function can see the session, \
         delete this test rather than editing it."
    );
}

/// **A column's stored `DEFAULT` is rendered through the same output function**, which is the whole
/// of `test_schema_dump_with_default_value`.
///
/// `pg_get_expr` prints a stored constant by *printing* it, so one catalog row has two texts:
///
/// ```text
/// IntervalStyle = postgres   '3 years'::interval        '00:00:01.235'::interval(3)
/// IntervalStyle = iso_8601   'P3Y'::interval            'PT1.235S'::interval(3)
/// ```
///
/// Traced through Rails by g1: `extract_value_from_default("'3 years'::interval")` gives
/// `"3 years"`, `Duration.parse` raises, `cast_value` answers `nil`, and the column spec gets **no
/// `default:` key at all** — which is exactly the dump line run 100 reports.
#[test]
fn a_column_default_prints_under_the_session_style() {
    const DEFAULTS: &str = "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
                            JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
                            WHERE d.adrelid = 'ivd'::regclass ORDER BY a.attnum";
    let mut node = parity::Node::new(&[
        "CREATE TABLE ivd (a interval DEFAULT 'P3Y', b interval(3) DEFAULT '1.23456 seconds')",
    ]);
    assert_eq!(
        node.rows(DEFAULTS),
        [
            ["'3 years'::interval".to_owned()],
            ["'00:00:01.235'::interval(3)".to_owned()]
        ]
    );
    node.run("SET intervalstyle = 'iso_8601'").unwrap();
    assert_eq!(
        node.rows(DEFAULTS),
        [
            ["'P3Y'::interval".to_owned()],
            ["'PT1.235S'::interval(3)".to_owned()]
        ]
    );
    // `information_schema.columns` reads the same expression and must not disagree with it.
    assert_eq!(
        node.rows(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'ivd' ORDER BY ordinal_position"
        ),
        [
            ["'P3Y'::interval".to_owned()],
            ["'PT1.235S'::interval(3)".to_owned()]
        ]
    );
}
