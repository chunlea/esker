//! **The oracle follows the wall clock, so a default that calls a function is wall-true.**
//!
//! `fixtures_test#test_insert_with_default_function` inserts a row that omits a column declared
//! `DEFAULT CURRENT_TIMESTAMP` and asserts the stored value is within 1.1 seconds of the client's
//! own clock. It failed by **six days**:
//!
//! ```text
//! Expected |2026-09-05 10:20:48 -0700 - 2026-08-30 14:00:00 UTC| (530448.150242) to be <= 1.1
//! ```
//!
//! and the second instant was a constant in this crate — `MemoryBackend`'s fake oracle started at
//! `2026-08-30 14:00:00 UTC` and advanced only the *logical* half of its timestamp per commit, so
//! every `now()` a process ever answered named that one millisecond. The difference between the
//! failure's two instants is 530448.0 s, against the 530448.150242 reported: the "actual" value
//! **was** the constant, to the second.
//!
//! Nothing folded the default. `exec::dml::column_default_value` parses and evaluates
//! `default_expr` per row exactly as designed, and `CURRENT_TIMESTAMP` there is
//! `micros_of_ts(txn.start_ts())` — the transaction's TSO instant, because `CLAUDE.md` invariant 6
//! makes the oracle the only clock a node may read. The oracle was the thing that was wrong.
//!
//! # Why a *server* cannot have a frozen one
//!
//! The scoreboard node runs `MemoryBackend` (`bin/esker-sql.rs`), so this is not a detail of a
//! test fixture: every `now()`, `CURRENT_TIMESTAMP`, `CURRENT_DATE` and evaluated default that
//! node has ever answered has been six days stale, and every client comparison against them lies.
//! A deterministic instant is worth keeping in a *unit* test — and no test in this crate pinned
//! the constant, which is why nothing caught it.
//!
//! `Versions::mark` is `esker_pd::tso`'s own rule, `max(last, clock)`, so the sequence stays
//! monotone across a clock that stands still, a clock that jumps backwards, and
//! [`MemoryBackend::advance_ms`], which is now a persistent offset — added to the *reading* rather
//! than to the mark, so the wall clock catching up cannot swallow it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::backend::{Backend, MemoryBackend};
// `to_text` is `PgDatum`'s, and a trait has to be in scope to be called through.
use esker_sql::value::{Datum, PgDatum};

#[path = "parity_harness/mod.rs"]
mod parity;

/// 2000-01-01 UTC in Unix microseconds: the epoch every stored timestamp counts from.
const POSTGRES_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

/// The **host's** clock, `offset_secs` away, rendered the way the node renders a `timestamp`.
///
/// The comparison has to cross the boundary: asking the node whether its own `CURRENT_TIMESTAMP`
/// is close to its own stored value passes just as happily on a frozen clock, which is exactly how
/// this went unnoticed.
fn wall_timestamp(offset_secs: i64) -> String {
    let unix_micros = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the host clock is after 1970")
            .as_micros(),
    )
    .expect("a Unix instant fits in an i64 of microseconds");
    let micros = unix_micros - POSTGRES_EPOCH_UNIX_MICROS + offset_secs * 1_000_000;
    Datum::Timestamp(micros)
        .to_text()
        .expect("a timestamp renders")
}

/// **The `fixtures_test` shape**: a column defaulted to a function, a row that omits it, and the
/// host's clock as the judge.
///
/// The window is ±5 s where Rails allows 1.1, because a gate box under load is not a fair
/// stopwatch — and it is six *days* that this is proving did not happen.
#[test]
fn a_function_default_is_written_at_the_wall_clock() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1wc_aircraft (name text, wheels_count integer DEFAULT 0, \
         manufactured_at timestamp DEFAULT CURRENT_TIMESTAMP)",
    ]);
    let lower = wall_timestamp(-5);
    node.run("INSERT INTO g1wc_aircraft (name) VALUES ('boeing-with-no-manufactured-at')")
        .unwrap();
    let upper = wall_timestamp(5);
    assert_eq!(
        node.rows(&format!(
            "SELECT manufactured_at > TIMESTAMP '{lower}' AND manufactured_at < TIMESTAMP '{upper}' \
             FROM g1wc_aircraft"
        )),
        [["t"]],
        "the stored default is {:?}, the window {lower} .. {upper}",
        node.rows("SELECT manufactured_at FROM g1wc_aircraft")
    );
}

/// `CURRENT_TIMESTAMP` asked directly is the same instant, so the default is not a second path
/// that happens to be right.
#[test]
fn current_timestamp_itself_is_the_wall_clock() {
    let mut node = parity::Node::new(&[]);
    let lower = wall_timestamp(-5);
    let upper = wall_timestamp(5);
    assert_eq!(
        node.rows(&format!(
            "SELECT LOCALTIMESTAMP > TIMESTAMP '{lower}' AND LOCALTIMESTAMP < TIMESTAMP '{upper}'"
        )),
        [["t"]]
    );
}

/// **`advance_ms` still moves the clock, and now it stays moved.**
///
/// The offset is what makes this deterministic. A bump added to the mark would be swallowed the
/// moment the wall clock caught up, so a test asking for two versions in different milliseconds
/// would pass or fail on how fast the machine was.
#[test]
fn advance_ms_moves_the_clock_forward_and_it_stays_moved() {
    let backend = MemoryBackend::new();
    let physical = |ts: u64| ts >> esker_client::TSO_LOGICAL_BITS;

    let before = physical(backend.now().unwrap());
    backend.advance_ms(3_600_000);
    let after = physical(backend.now().unwrap());
    assert!(
        after >= before + 3_600_000,
        "an hour on: {after} against {before}"
    );

    // It stays: a later reading is not back where it started, which a one-shot bump on a
    // wall-clock mark would be.
    let again = physical(backend.now().unwrap());
    assert!(again >= after, "{again} went backwards from {after}");

    // And a second call adds a second hour rather than re-asserting the first.
    backend.advance_ms(3_600_000);
    assert!(
        physical(backend.now().unwrap()) >= before + 7_200_000,
        "two hours on"
    );
}

/// A commit is **above** the mark and the sequence never repeats — the property the real oracle
/// has, checked here because this fake now reads a clock that can stand still.
#[test]
fn every_commit_takes_a_timestamp_above_the_last() {
    let backend = MemoryBackend::new();
    let mut seen = Vec::new();
    for round in 0..8_u8 {
        let mut txn = backend.begin().unwrap();
        txn.put(b"k", &[round]);
        seen.push(
            txn.commit()
                .unwrap()
                .expect("a write commits at a timestamp"),
        );
    }
    for pair in seen.windows(2) {
        assert!(pair[1] > pair[0], "{seen:?} does not ascend");
    }
}
