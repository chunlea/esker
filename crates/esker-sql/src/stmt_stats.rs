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
    /// **What each of them read**, in order, when `ESKER_STMT_STATS_TRACE` is set.
    ///
    /// A count cannot answer `debts-v1.1.md` #49's question. A statement that reads the version
    /// key thirty-five times and one that reads thirty-five different relations are the same
    /// number and different problems — one is a cache, the other is a batch — and the fix for
    /// each is a different fix. This is the list that tells them apart.
    static TRACE: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether each read gets a line of its own, read once from the environment.
///
/// **A second switch and not the same one**, because the two are read by different people: the
/// counters are what a suite is read by and are cheap enough to leave on for one, and this
/// allocates a string per read and prints a paragraph per statement. It is a hunt's instrument.
#[must_use]
pub fn tracing_reads() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var_os("ESKER_STMT_STATS_TRACE").is_some())
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
static MICROS: AtomicU64 = AtomicU64::new(0);
/// The worst statement seen, and what it did — kept as four numbers rather than a string so that
/// nothing here allocates on the path.
static WORST_MICROS: AtomicU64 = AtomicU64::new(0);

/// Records one point read at the store boundary.
pub(crate) fn record_point(key: &[u8]) {
    if !enabled() {
        return;
    }
    POINTS.with(|points| points.set(points.get().saturating_add(1)));
    if tracing_reads() {
        trace(format!(
            "get  {}",
            crate::catalog::record::describe_key(key)
        ));
    }
}

/// Records one range scan at the store boundary.
pub(crate) fn record_range(start: &[u8], end: &[u8]) {
    if !enabled() {
        return;
    }
    RANGES.with(|ranges| ranges.set(ranges.get().saturating_add(1)));
    if tracing_reads() {
        trace(format!(
            "scan {}",
            crate::catalog::record::describe_range(start, end)
        ));
    }
}

/// Appends one line to this statement's trace.
fn trace(line: String) {
    TRACE.with(|reads| {
        if let Ok(mut reads) = reads.try_borrow_mut() {
            reads.push(line);
        }
    });
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
    if tracing_reads() {
        TRACE.with(|reads| {
            if let Ok(mut reads) = reads.try_borrow_mut() {
                reads.clear();
            }
        });
    }
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
        let (trips, regions) = esker_client::stmt_stats::taken();
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        POINT_READS.fetch_add(points, Ordering::Relaxed);
        RANGE_SCANS.fetch_add(ranges, Ordering::Relaxed);
        ROUND_TRIPS.fetch_add(trips, Ordering::Relaxed);
        REGIONS.fetch_add(u64::try_from(regions).unwrap_or(0), Ordering::Relaxed);
        MICROS.fetch_add(micros, Ordering::Relaxed);
        WORST_MICROS.fetch_max(micros, Ordering::Relaxed);
        if tracing_reads() {
            let reads = TRACE.with(|reads| {
                reads
                    .try_borrow()
                    .map(|reads| reads.join("\n    "))
                    .unwrap_or_default()
            });
            tracing::info!(
                target: "esker::stmt::stats",
                "{source}\n  {points} point reads, {ranges} range scans, {trips} round trips, \
                 {regions} regions, {micros} us\n    {reads}"
            );
        }
        if micros >= per_statement_ms().saturating_mul(1_000) {
            tracing::info!(
                target: "esker::stmt::stats",
                "{micros} us · point reads {points} · range scans {ranges} · round trips {trips} \
                 · regions {regions} · {source}"
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

/// Empties this thread's trace, so a caller can tell "read nothing" from "was never asked".
///
/// A statement that does not reach [`begin`] — transaction control goes a different way — would
/// otherwise leave the previous statement's list in place and a census would read it as its own.
pub fn clear_trace() {
    TRACE.with(|reads| {
        if let Ok(mut reads) = reads.try_borrow_mut() {
            reads.clear();
        }
    });
}

/// **What the statement this thread just ran read**, in order — empty unless
/// `ESKER_STMT_STATS_TRACE` is set.
///
/// Read after the statement rather than out of the log, because a measurement harness wants the
/// list and not a subscriber: the guard leaves it in place until the next statement begins, and
/// the executor runs an in-process statement on the caller's thread.
#[must_use]
pub fn last_trace() -> Vec<String> {
    TRACE.with(|reads| {
        reads
            .try_borrow()
            .map(|reads| reads.clone())
            .unwrap_or_default()
    })
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
         range scans {}, round trips {}, regions {}",
        micros.checked_div(statements).unwrap_or(0),
        WORST_MICROS.load(Ordering::Relaxed),
        per(points),
        per(ranges),
        per(trips),
        per(regions),
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
            record_point(b"m");
        }
        assert_eq!(counts(), before);
        assert!(
            summary().contains("statements 0"),
            "a disabled instrument counted a statement: {}",
            summary()
        );
    }
}
