//! **What one statement costs below the SQL**: KV reads, wire round trips, and how many regions it
//! touched.
//!
//! `docs/plans/debts-v1.1.md` #49 opened on three numbers nobody could produce. Run 113 timed three
//! catalog-introspection statements on the real topology at **0.5–0.8 s each** —
//! `pg_index ⋈ pg_attribute` 790 ms, `obj_description` 546 ms, `pg_inherits` 793 ms — and
//! [ADR 0102](../../../docs/adr/0102-the-catalogs-read-path.md)'s instrument, which was on for the
//! same run, accounts for **2.2 ms** of a 506 ms statement. So the half-second is not a slower
//! version of a known cost; it is an unmeasured one, and this is the instrument that names it.
//!
//! **The three numbers, and why each is separate.**
//!
//! * **Point reads and range scans, counted apart.** A statement that makes eleven point reads is
//!   a different shape from one that makes one scan of eleven rows, and the fix for each is a
//!   different fix. Counted at the store boundary, which is where `Txn::get` and `Txn::scan` are
//!   the only two doors.
//! * **Wire round trips**, from `esker_client::stmt_stats`, counted **per attempt** — a retry is a
//!   round trip. A read that the client answered from its buffer makes none, which is the whole
//!   reason this is not the same number as the one above.
//! * **Distinct regions**, from the same place. A scan that walks four regions is a different
//!   shape from a point read that retried four times on one, and the count of *distinct* regions
//!   is what tells them apart.
//!
//! **Off unless `ESKER_STMT_STATS` is set**, and an environment variable rather than a cargo
//! feature for the reason `crate::catalog::stats` gives: the run that should produce these numbers
//! is a suite against the released binary, and a feature would mean a special build nobody has.
//! Reading the variable happens once; when it is off, a statement pays one relaxed load at each
//! end and nothing else.
//!
//! **Per thread, because a statement is a thread.** The executor hands each statement to one
//! blocking thread and every read below it is synchronous, so what this thread did between the
//! guard's two ends is what this statement did.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Whether the instrument is on, read once from the environment.
#[must_use]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ESKER_STMT_STATS").is_some())
}

/// How slow a statement has to be before it gets a line of its own, in milliseconds.
///
/// **A summary every ten seconds is what a suite is read by; a line per statement is what a
/// *hunt* is read by**, and printing one per statement would bury the first in the second. The
/// default is 100 ms, which is two orders below the statements #49 is about and two above an
/// ordinary one.
fn per_statement_ms() -> u64 {
    static MS: OnceLock<u64> = OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("ESKER_STMT_STATS_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100)
    })
}

thread_local! {
    /// Point reads this thread has made since the statement began.
    static POINTS: Cell<u64> = const { Cell::new(0) };
    /// Range scans, counted apart from the point reads for the reason the module note gives.
    static RANGES: Cell<u64> = const { Cell::new(0) };
}

/// Statements finished, and what they did between them.
static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static POINT_READS: AtomicU64 = AtomicU64::new(0);
static RANGE_SCANS: AtomicU64 = AtomicU64::new(0);
static ROUND_TRIPS: AtomicU64 = AtomicU64::new(0);
/// The **sum** of each statement's distinct-region count, not the number of distinct regions
/// overall: the question is how many a statement touches, and averaging that over statements is
/// what the summary reports.
static REGIONS: AtomicU64 = AtomicU64::new(0);
/// Timestamps taken from the oracle, which on a real cluster is a round trip to the driver.
static TSO: AtomicU64 = AtomicU64::new(0);
/// `Prewrite` calls — a transaction's first phase.
static PREWRITES: AtomicU64 = AtomicU64::new(0);
/// `Commit` calls — its second.
static COMMITS: AtomicU64 = AtomicU64::new(0);
/// Mutations sent in those prewrites: the keys a statement actually wrote or checked.
static KEYS: AtomicU64 = AtomicU64::new(0);
/// Time spent asleep behind somebody else's lock, which is the one part of a statement's cost
/// that is not its own work.
static WAITED_MICROS: AtomicU64 = AtomicU64::new(0);
static MICROS: AtomicU64 = AtomicU64::new(0);
/// The worst statement seen, and what it did — kept as four numbers rather than a string so that
/// nothing here allocates on the path.
static WORST_MICROS: AtomicU64 = AtomicU64::new(0);

/// Records one point read at the store boundary.
pub(crate) fn record_point() {
    if !enabled() {
        return;
    }
    POINTS.with(|points| points.set(points.get().saturating_add(1)));
}

/// Records one range scan at the store boundary.
pub(crate) fn record_range() {
    if !enabled() {
        return;
    }
    RANGES.with(|ranges| ranges.set(ranges.get().saturating_add(1)));
}

/// Starts one statement's accounting, and reports it when the guard drops.
///
/// Held beside the cancellation guards, which is the one place every statement goes through
/// whether it arrived over a socket or not.
#[must_use]
pub(crate) fn begin(source: &str) -> Guard {
    if !enabled() {
        return Guard { began: None };
    }
    POINTS.with(|points| points.set(0));
    RANGES.with(|ranges| ranges.set(0));
    esker_client::stmt_stats::reset();
    start_reporting();
    Guard {
        began: Some((Instant::now(), source.chars().take(120).collect())),
    }
}

/// What one statement did, added to the totals when it ends.
pub(crate) struct Guard {
    /// `None` when the instrument is off, which is the whole of what off costs.
    began: Option<(Instant, String)>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some((began, source)) = self.began.take() else {
            return;
        };
        let micros = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        let points = POINTS.with(Cell::get);
        let ranges = RANGES.with(Cell::get);
        let cost = esker_client::stmt_stats::taken();
        let (trips, regions) = (cost.round_trips, cost.regions);
        let waited = u64::try_from(cost.waited.as_micros()).unwrap_or(u64::MAX);
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        POINT_READS.fetch_add(points, Ordering::Relaxed);
        RANGE_SCANS.fetch_add(ranges, Ordering::Relaxed);
        ROUND_TRIPS.fetch_add(trips, Ordering::Relaxed);
        REGIONS.fetch_add(u64::try_from(regions).unwrap_or(0), Ordering::Relaxed);
        TSO.fetch_add(cost.tso, Ordering::Relaxed);
        PREWRITES.fetch_add(cost.prewrites, Ordering::Relaxed);
        COMMITS.fetch_add(cost.commits, Ordering::Relaxed);
        KEYS.fetch_add(cost.keys, Ordering::Relaxed);
        WAITED_MICROS.fetch_add(waited, Ordering::Relaxed);
        MICROS.fetch_add(micros, Ordering::Relaxed);
        WORST_MICROS.fetch_max(micros, Ordering::Relaxed);
        if micros >= per_statement_ms().saturating_mul(1_000) {
            tracing::info!(
                target: "esker::stmt::stats",
                "{micros} us · point reads {points} · range scans {ranges} · round trips {trips} \
                 · regions {regions} · tso {tso} · prewrites {prewrites} · commits {commits} \
                 · keys {keys} · waited {waited} us · {source}",
                tso = cost.tso,
                prewrites = cost.prewrites,
                commits = cost.commits,
                keys = cost.keys,
                waited = waited
            );
        }
    }
}

/// Starts the ten-second reporter, once. A thread rather than a task, for the reason
/// `crate::catalog::stats` gives: this runs on blocking executor threads and knows nothing about
/// the runtime around it.
fn start_reporting() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("stmt-stats".to_owned())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(10));
                tracing::info!(target: "esker::stmt::stats", "{}", summary());
            }
        });
}

/// The counters, for a measurement that wants a difference rather than a line.
///
/// `(statements, point reads, range scans, round trips, summed distinct regions)`.
#[must_use]
pub fn counts() -> (u64, u64, u64, u64, u64) {
    (
        STATEMENTS.load(Ordering::Relaxed),
        POINT_READS.load(Ordering::Relaxed),
        RANGE_SCANS.load(Ordering::Relaxed),
        ROUND_TRIPS.load(Ordering::Relaxed),
        REGIONS.load(Ordering::Relaxed),
    )
}

/// One line, for a log a harness can grep.
#[must_use]
pub fn summary() -> String {
    let (statements, points, ranges, trips, regions) = counts();
    let micros = MICROS.load(Ordering::Relaxed);
    // Integer arithmetic throughout: a per-statement mean in tenths is enough to read and a
    // `u64 as f64` is a lint this workspace refuses on purpose.
    let per = |total: u64| {
        let tenths = total
            .saturating_mul(10)
            .checked_div(statements)
            .unwrap_or(0);
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    format!(
        "statements {statements}, mean {} us, worst {} us, per statement: point reads {}, \
         range scans {}, round trips {}, regions {}, tso {}, prewrites {}, commits {}, keys {}, \
         waited {} us",
        micros.checked_div(statements).unwrap_or(0),
        WORST_MICROS.load(Ordering::Relaxed),
        per(points),
        per(ranges),
        per(trips),
        per(regions),
        per(TSO.load(Ordering::Relaxed)),
        per(PREWRITES.load(Ordering::Relaxed)),
        per(COMMITS.load(Ordering::Relaxed)),
        per(KEYS.load(Ordering::Relaxed)),
        WAITED_MICROS
            .load(Ordering::Relaxed)
            .checked_div(statements)
            .unwrap_or(0),
    )
}

/// **Names the key heads `esker-client` recorded**, which it deliberately cannot do itself.
///
/// The client keeps a fixed window of bytes per read and knows nothing about what they mean
/// (`CLAUDE.md` invariant 7). Here is where they become a catalog kind: the reserved layout puts
/// `'m'` in front of every metadata key, `esker-sql`'s own records follow it with `"sql"` and one
/// byte naming the kind (`catalog/record.rs`'s header), and everything else is named by its
/// namespace alone.
///
/// A `Vec` of `(name, count)` rather than a map of bytes, because the only caller is a measurement
/// that prints it and a byte array in a report is a puzzle rather than an answer.
#[must_use]
pub fn name_heads(
    heads: &std::collections::BTreeMap<[u8; esker_client::stmt_stats::HEAD], u64>,
) -> Vec<(String, u64)> {
    // **Merged by name, because the head is wider than a kind.** The client keeps eight opaque
    // bytes; a kind is five, and the rest is the start of a tenant — so one kind read for two
    // tenants arrives as two entries. Merging here rather than narrowing the window keeps the
    // client's record free of any assumption about where a kind ends.
    let mut merged: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (head, count) in heads {
        *merged.entry(name_of_head(head)).or_default() += count;
    }
    merged.into_iter().collect()
}

/// One head, named. Split out so the mapping is testable without building a map.
fn name_of_head(head: &[u8]) -> String {
    match head.first().copied() {
        Some(esker_keys::prefix::META) => {
            // `'m' ++ "sql" ++ kind` for this crate's records; another component's metadata keys
            // are named by their namespace and left alone.
            if head.len() > 4 && &head[1..4] == b"sql" {
                let kind = head[4];
                format!("catalog '{}'", char::from(kind))
            } else {
                "metadata (not the catalog's)".to_owned()
            }
        }
        Some(esker_keys::prefix::SQL) => "row or index data".to_owned(),
        Some(esker_keys::prefix::TXN) => "txn record".to_owned(),
        Some(esker_keys::prefix::RAW) => "raw".to_owned(),
        Some(other) => format!("namespace {other:#04x}"),
        None => "empty key".to_owned(),
    }
}

/// The write side of [`counts`], for a test that prices one statement at a time:
/// `(tso, prewrites, commits, keys, waited micros)`.
#[must_use]
pub fn write_counts() -> (u64, u64, u64, u64, u64) {
    (
        TSO.load(Ordering::Relaxed),
        PREWRITES.load(Ordering::Relaxed),
        COMMITS.load(Ordering::Relaxed),
        KEYS.load(Ordering::Relaxed),
        WAITED_MICROS.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::{begin, counts, enabled, record_point, summary};

    /// **Off by default, and off counts nothing.** The instrument is on the path of every
    /// statement and every read below it.
    #[test]
    fn it_is_off_unless_the_environment_says_otherwise() {
        if std::env::var_os("ESKER_STMT_STATS").is_some() {
            assert!(enabled());
            return;
        }
        assert!(!enabled());
        let before = counts();
        {
            let _guard = begin("SELECT 1");
            record_point();
        }
        assert_eq!(counts(), before);
        assert!(
            summary().contains("statements 0"),
            "a disabled instrument counted a statement: {}",
            summary()
        );
    }
}
