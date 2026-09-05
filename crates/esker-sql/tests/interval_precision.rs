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

/// **A declared divergence, and the half this unit does not do**: the precision is now reported
/// and is not yet *applied* to the value.
///
/// Measured on 19beta1 — `'1.23456789 seconds'` into an `interval(3)` column is stored as
/// `00:00:01.235`, **rounded** to three fractional digits, not truncated. This node keeps all six:
/// `00:00:01.234568`.
///
/// Recorded here rather than fixed, because applying a typmod to a value is the type surface's
/// rule and not the catalog's — the same shape `numeric(p,s)` rounding and `varchar(n)` refusal
/// already have — and it is `b4-types`'. What this unit closed is the *reporting*, which is what
/// all seven `interval_test.rb` failures were.
///
/// **This test asserts the wrong answer on purpose.** When the value starts being rounded it goes
/// red and must be deleted, which is ADR 0031 rule 2 doing its job rather than a test to update.
#[test]
fn the_precision_is_reported_but_not_yet_applied_to_the_value() {
    let mut node = parity::Node::new(&["CREATE TABLE g1_v (a interval(3))"]);
    node.run("INSERT INTO g1_v VALUES ('1.23456789 seconds')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT a FROM g1_v"),
        [["00:00:01.234568".to_owned()]],
        "PostgreSQL stores 00:00:01.235; when this agrees, delete the test"
    );
    // The reporting half, in the same breath, so the divergence cannot be mistaken for the column
    // having lost its precision.
    assert_eq!(
        node.rows(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'g1_v'::regclass AND attnum > 0"
        ),
        [["interval(3)".to_owned()]]
    );
}
