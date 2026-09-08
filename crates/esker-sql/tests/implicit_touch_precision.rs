//! The timestamp an **implicit touch** writes, and whether it keeps its sub-second part.
//!
//! `insert_all_test.rb` asserts it in the one way that is not flaky:
//!
//! ```ruby
//! has_subsecond_precision = (1..100).any? do |i|
//!   Book.upsert_all [{ id: 101, name: "... (Edition #{i})" }]
//!   Book.find(101).updated_at.usec > 0
//! end
//! assert has_subsecond_precision, "updated_at should have sub-second precision"
//! ```
//!
//! A hundred tries, because a single upsert *can* land exactly on a second. Two tests fail that
//! way here — `..._respects_created_at_precision_when_touched_implicitly` and the `updated_at`
//! one — and neither has a failing statement, so what is wrong is a value and not a refusal.
//!
//! This node's instant comes from the TSO and never from a clock it reads (invariant 6), and
//! `time_machine::micros_of_ts` is milliseconds multiplied by a thousand — so the microseconds
//! are a multiple of 1000 and non-zero unless the millisecond part is exactly zero. That is the
//! model; these tests are whether the value that reaches the column agrees with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `books` as `ActiveRecord`'s schema declares it, narrowed to the columns the test touches.
const FIXTURE: &[&str] = &[
    "CREATE TABLE books (id int8 PRIMARY KEY, name text, updated_at timestamp(6), \
     created_at timestamp(6), plain timestamp)",
];

/// The digits after the decimal point, or `""` when the value printed none.
fn fraction(printed: &str) -> String {
    printed
        .split_once('.')
        .map_or_else(String::new, |(_, tail)| tail.to_owned())
}

/// **A single-node test can see the precision now, and this is the assertion that says so.**
///
/// `MemoryBackend`'s oracle used to be a logical counter with a frozen physical half, so
/// `CURRENT_TIMESTAMP` in every single-node test was one instant with no sub-second part — which
/// is why this file asserts the *conversion* below rather than the end-to-end value, and why this
/// test once asserted the counter. The oracle follows the wall clock since `037f57a3`
/// (`Versions::mark`, `tests/wall_clock.rs`), so the shape `insert_all_test.rb` reads — a
/// sub-second part on the touched column — is reachable here: a millisecond is a thousandth of a
/// second, and twenty reads without one would be a clock that had stopped.
#[test]
fn a_single_node_test_reads_a_clock_with_sub_second_precision() {
    let mut node = parity::Node::new(FIXTURE);
    let reads: Vec<String> = (0..20)
        .map(|_| node.rows("SELECT CURRENT_TIMESTAMP")[0][0].clone())
        .collect();
    assert!(
        reads.iter().any(|printed| !fraction(printed).is_empty()),
        "twenty reads of CURRENT_TIMESTAMP and not one sub-second part: the oracle is a counter \
         again, and this file's reasoning needs revisiting: {reads:?}"
    );
    // And it is a clock: a decimal fraction without trailing zeros orders as its value does, so
    // the printed instants never go backwards between two reads.
    for pair in reads.windows(2) {
        assert!(pair[0] <= pair[1], "{} then {}", pair[0], pair[1]);
    }
}

/// **The conversion the real path depends on**, which is a product function and is deterministic.
///
/// A TSO timestamp is `physical_ms << 18 | logical` (`esker_pd::tso`), so its physical half is
/// milliseconds since the Unix epoch and `time_machine::micros_of_ts` multiplies by a thousand.
/// The microseconds of an instant are therefore a multiple of 1000 and **non-zero unless the
/// millisecond part is exactly zero** — one instant in a thousand. Over the hundred tries
/// `insert_all_test.rb` makes, a sub-second part is a certainty if this holds.
#[test]
fn a_tso_timestamp_keeps_its_milliseconds_through_the_conversion() {
    // 2024-05-06T07:08:09.123 UTC, a millisecond that is not a whole second.
    let unix_ms: u64 = 1_714_979_289_123;
    let printed = esker_sql::time_machine::render(esker_client::ts_at_ms(unix_ms));
    assert!(
        printed.ends_with(".123+00"),
        "a millisecond was lost in the conversion: {printed}"
    );
}

/// **The other half, and it does not need a clock: does the touch happen at all?**
///
/// `upsert_all` writes the implicit touch as a `CASE` — "keep the old value if nothing changed,
/// otherwise `CURRENT_TIMESTAMP`" — and a test named `..._respects_..._precision_when_touched_
/// implicitly` fails just as well if the column is never touched. That is observable here: the
/// row starts at an instant nothing else can produce, and the upsert has to move it.
#[test]
fn an_upsert_actually_applies_the_implicit_touch() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "INSERT INTO books (id, name, updated_at) VALUES (101, 'Out of the Silent Planet', \
         '2019-01-01 00:00:00')",
    )
    .unwrap();

    node.run(
        "INSERT INTO books (id, name) VALUES (101, 'Edition 2') ON CONFLICT (id) DO UPDATE SET \
         name = excluded.name, updated_at = (CASE WHEN books.name IS NOT DISTINCT FROM \
         excluded.name THEN books.updated_at ELSE CURRENT_TIMESTAMP END)",
    )
    .unwrap();

    let after = node.rows("SELECT updated_at FROM books WHERE id = 101");
    assert_ne!(
        after,
        vec![vec!["2019-01-01 00:00:00".to_owned()]],
        "the upsert left updated_at where it was: the implicit touch never fired"
    );

    // And the branch that keeps it: the same name, so the `CASE` takes `books.updated_at`.
    node.run("UPDATE books SET updated_at = '2019-01-01 00:00:00' WHERE id = 101")
        .unwrap();
    node.run(
        "INSERT INTO books (id, name) VALUES (101, 'Edition 2') ON CONFLICT (id) DO UPDATE SET \
         name = excluded.name, updated_at = (CASE WHEN books.name IS NOT DISTINCT FROM \
         excluded.name THEN books.updated_at ELSE CURRENT_TIMESTAMP END)",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT updated_at FROM books WHERE id = 101"),
        vec![vec!["2019-01-01 00:00:00"]],
        "the CASE's other branch moved a value it was told to keep"
    );
}

/// And a column with **no** declared precision keeps it too — `timestamp` is `timestamp(6)` on a
/// real server, not `timestamp(0)`.
#[test]
fn a_timestamp_with_no_typmod_is_not_a_timestamp_0() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO books (id, name, plain) VALUES (7, 'x', '2020-01-01 00:00:00.123456')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT plain FROM books WHERE id = 7"),
        vec![vec!["2020-01-01 00:00:00.123456"]]
    );
}
