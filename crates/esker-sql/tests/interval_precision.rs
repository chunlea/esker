//! **`interval(p)` keeps its precision**, which `interval_test.rb` fails seven times for want of.
//!
//! All seven failures in that file are one line of its `setup` — `assert_equal 3,
//! @column_min.precision` for `t.interval "minimum_term", precision: 3`. No test body has ever
//! run. The statement was accepted and the precision silently dropped.
//!
//! # The typmod, which is not a number
//!
//! PostgreSQL packs an interval's typmod as `(range_mask << 16) | precision`, where the low 16
//! bits are `0xFFFF` when no precision was written. Measured on 19beta1:
//!
//! ```text
//! format_type(1186, -1)          interval
//! format_type(1186, 2147418115)  interval(3)      -- 0x7FFF0003, full range and precision 3
//! format_type(1186, 2147418118)  interval(6)
//! format_type(1186, 589823)      interval day     -- 0x0008FFFF, a field mask and no precision
//! format_type(1186, 67698687)    interval day to hour
//! format_type(1186, 3)           ERROR:  invalid INTERVAL typmod: 0x3
//! ```
//!
//! That last line is why the **packed** value is what `ColumnDef::typmod` holds, as it does for
//! every other parameterised type: a bare `3` is not an interval typmod at all, and this node
//! hands `atttypmod` to clients raw on two wire surfaces.
//!
//! **The field mask stays dropped** and stays a declared divergence (`tests/interval.rs`): it says
//! which fields a value *keeps*, which is semantics and not a width. `ActiveRecord` writes only
//! `interval(p)`, which is the full range and a precision.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] =
    &["CREATE TABLE g1_p (p0 interval(0), p3 interval(3), p6 interval(6), plain interval)"];

/// The four goldens, together, because a client reads them together.
#[test]
fn a_declared_precision_is_stored_and_reported() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT attname, atttypmod, format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'g1_p'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        [
            [
                "p0".to_owned(),
                "2147418112".to_owned(),
                "interval(0)".to_owned()
            ],
            [
                "p3".to_owned(),
                "2147418115".to_owned(),
                "interval(3)".to_owned()
            ],
            [
                "p6".to_owned(),
                "2147418118".to_owned(),
                "interval(6)".to_owned()
            ],
            ["plain".to_owned(), "-1".to_owned(), "interval".to_owned()],
        ]
    );
}

/// `information_schema.columns.datetime_precision` — **and 6 for a plain `interval`**, which is
/// the half that was wrong even for a column with nothing written on it.
#[test]
fn the_information_schema_reports_the_precision_and_defaults_to_six() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT column_name, data_type, datetime_precision FROM information_schema.columns \
             WHERE table_name = 'g1_p' ORDER BY ordinal_position"
        ),
        [
            ["p0".to_owned(), "interval".to_owned(), "0".to_owned()],
            ["p3".to_owned(), "interval".to_owned(), "3".to_owned()],
            ["p6".to_owned(), "interval".to_owned(), "6".to_owned()],
            ["plain".to_owned(), "interval".to_owned(), "6".to_owned()],
        ]
    );
}

/// `format_type` over the raw numbers, including the two a real server refuses.
#[test]
fn format_type_reads_the_packed_typmod() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT format_type(1186, -1), format_type(1186, 2147418115), \
             format_type(1186, 2147418118)"
        ),
        [[
            "interval".to_owned(),
            "interval(3)".to_owned(),
            "interval(6)".to_owned(),
        ]]
    );
}

/// **`interval(7)` is a warning, not an error** — measured: `INTERVAL(7) precision reduced to
/// maximum allowed, 6`, and the column is created as `interval(6)`.
#[test]
fn a_precision_past_six_is_reduced_with_a_warning() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE g1_seven (a interval(7))")
        .expect("accepted, not refused");
    assert_eq!(
        node.rows(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'g1_seven'::regclass AND attnum > 0"
        ),
        [["interval(6)".to_owned()]]
    );
}

/// **The precision is applied to the value now**, which is what g1's placeholder was waiting for.
///
/// That test asserted this node's *unrounded* answer on purpose and named PostgreSQL's in its
/// message, so rounding turned it red and it was deleted rather than edited — ADR 0031 rule 2.
/// What replaces it is the family, measured on 19beta1 in one rolled-back session.
///
/// Three things a plausible implementation gets wrong:
///
/// * **half away from zero, and symmetric** — `0.0005` at `interval(3)` is `0.001`, and the
///   negative is `-0.001`, not `-0.000`;
/// * **the carry stops at days** — `1 mon 2 days 00:00:59.9999` at `interval(0)` is
///   `1 mon 2 days 00:01:00`: it reaches minutes and never touches the days or months, because an
///   interval's three fields do not carry into one another;
/// * a fraction that rounds away leaves **no fractional digits at all** — `0.9995` at
///   `interval(3)` prints `00:00:01`, not `00:00:01.000`.
#[test]
fn the_precision_is_applied_to_the_value() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE b4_iv (p0 interval(0), p1 interval(1), p3 interval(3), p6 interval(6), plain interval)",
    ]);
    let write = |node: &mut parity::Node, text: &str| {
        node.run(&format!(
            "DELETE FROM b4_iv; INSERT INTO b4_iv VALUES ('{text}','{text}','{text}','{text}','{text}')"
        ))
        .unwrap();
    };

    write(&mut node, "1.23456789 seconds");
    assert_eq!(
        node.rows("SELECT p0, p1, p3, p6, plain FROM b4_iv"),
        [[
            "00:00:01".to_owned(),
            "00:00:01.2".to_owned(),
            "00:00:01.235".to_owned(),
            "00:00:01.234568".to_owned(),
            "00:00:01.234568".to_owned(),
        ]],
        "a plain interval keeps all six digits; a precision rounds to it"
    );

    write(&mut node, "-1.23456789 seconds");
    assert_eq!(
        node.rows("SELECT p3 FROM b4_iv"),
        [["-00:00:01.235".to_owned()]],
        "and the negative rounds by the same rule, away from zero"
    );

    write(&mut node, "0.0005 seconds");
    assert_eq!(
        node.rows("SELECT p3 FROM b4_iv"),
        [["00:00:00.001".to_owned()]],
        "exactly half goes away from zero"
    );

    write(&mut node, "0.9995 seconds");
    assert_eq!(
        node.rows("SELECT p3 FROM b4_iv"),
        [["00:00:01".to_owned()]],
        "a fraction that rounds away leaves no fractional digits"
    );

    write(&mut node, "1 mon 2 days 00:00:59.9999");
    assert_eq!(
        node.rows("SELECT p0, p3 FROM b4_iv"),
        [[
            "1 mon 2 days 00:01:00".to_owned(),
            "1 mon 2 days 00:01:00".to_owned()
        ]],
        "the carry reaches minutes and stops at days"
    );
}

/// The same rule through a **cast**, which is the other way a precision meets a value.
#[test]
fn a_cast_to_an_interval_precision_rounds_too() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '1.23456789 seconds'::interval(3)"),
        [["00:00:01.235".to_owned()]]
    );
    // `pg_typeof` of one is the bare name — a typmod is not part of what it reports. Measured.
    assert_eq!(
        node.rows("SELECT pg_typeof('1.5 seconds'::interval(3))"),
        [["interval".to_owned()]]
    );
}
