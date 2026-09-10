//! **What the catalog's region costs, counted at the one place every statement goes through.**
//!
//! [ADR 0102](../../../../docs/adr/0102-the-catalogs-read-path.md) asks whether the catalog living
//! in the left-most region — on the path of every statement on every node — is worth doing anything
//! about, and says the choice between its three shapes has to be made against numbers. These are
//! the first of those numbers: **how many catalog version reads a workload makes, and what they
//! cost.**
//!
//! **Off unless `ESKER_CATALOG_STATS` is set**, and an environment variable rather than a cargo
//! feature on purpose: the run that should produce these numbers is the ActiveRecord suite against
//! the *released* node binary, and a feature would mean a special build nobody has. Reading the
//! variable happens once.
//!
//! **What it does not measure, and the ADR says so too**: which region each read landed in. The
//! catalog's keys sort below all data so it is region 1 on every cluster this project has run, but
//! *proving* that per read needs the client's routing to report what it chose, which is a larger
//! change than an instrument. This counts the reads and times them; the share is the next
//! instrument, not this one.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Whether the instrument is on, read once from the environment.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ESKER_CATALOG_STATS").is_some())
}

thread_local! {
    /// The version this thread's last view read, so a repeat can be told from a first read.
    ///
    /// Per thread rather than global: the executor runs one statement per blocking thread, so a
    /// thread's previous view is the same session's, and a global would call two sessions'
    /// unrelated reads a repeat of each other.
    static LAST_VERSION: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Views taken. **Not once per transaction**, whatever `Catalog::view_at`'s doc says: the executor
/// reaches `catalog_view` from thirteen call sites, and each one reads the two counters again.
static VIEWS: AtomicU64 = AtomicU64::new(0);
/// Of those, the ones that read **the same version this thread read last time**.
///
/// **This field replaced one that lied.** It used to be "of the store", and the call site passed a
/// literal `true` — so run 111 reported `views == of the store` on all 164 lines and was read as a
/// cache hit rate of exactly zero. That was this constant, not a measurement. What can honestly be
/// seen from here is repetition: `Executor::catalog_view` is reached from thirteen places, so one
/// statement re-reads the two counters several times and every read after the first returns the
/// version the last one did. That repetition is precisely what a cached version with an
/// invalidation would remove, which makes it the number
/// [ADR 0102](../../../../docs/adr/0102-the-catalogs-read-path.md)'s option (a) is about.
static REPEATS: AtomicU64 = AtomicU64::new(0);
/// Total microseconds spent reading the version, and the worst one seen.
static MICROS: AtomicU64 = AtomicU64::new(0);
static WORST: AtomicU64 = AtomicU64::new(0);
/// A coarse histogram: under 100 µs, under 1 ms, under 10 ms, and the rest.
static BUCKETS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Records one catalog version read.
///
/// Called from `Catalog::view_at` with the time the two counter reads took. Cheap enough to leave
/// in the path unconditionally — an `enabled()` load and, when off, nothing else.
pub(super) fn record(took: Duration, version: u64) {
    if !enabled() {
        return;
    }
    VIEWS.fetch_add(1, Ordering::Relaxed);
    LAST_VERSION.with(|last| {
        if last.replace(Some(version)) == Some(version) {
            REPEATS.fetch_add(1, Ordering::Relaxed);
        }
    });
    let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
    MICROS.fetch_add(micros, Ordering::Relaxed);
    WORST.fetch_max(micros, Ordering::Relaxed);
    BUCKETS[bucket_of(micros)].fetch_add(1, Ordering::Relaxed);
    start_reporting();
}

/// Which bucket a reading falls in: under 100 µs, under 1 ms, under 10 ms, and the rest.
///
/// Separate from [`record`] so it can be asserted without the environment: `enabled()` caches its
/// answer for the process, so a test cannot switch the instrument on and off around itself.
fn bucket_of(micros: u64) -> usize {
    match micros {
        0..=99 => 0,
        100..=999 => 1,
        1_000..=9_999 => 2,
        _ => 3,
    }
}

/// Starts the ten-second reporter, once.
fn start_reporting() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    // A thread rather than a task: this crate's catalog is reached from blocking executor threads
    // and knows nothing about the runtime around it.
    let _ = std::thread::Builder::new()
        .name("catalog-stats".to_owned())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(10));
                tracing::info!(target: "esker::catalog::stats", "{}", summary());
            }
        });
}

/// One line, for a log a harness can grep.
#[must_use]
pub fn summary() -> String {
    let views = VIEWS.load(Ordering::Relaxed);
    let repeats = REPEATS.load(Ordering::Relaxed);
    let micros = MICROS.load(Ordering::Relaxed);
    let counts: Vec<u64> = BUCKETS.iter().map(|b| b.load(Ordering::Relaxed)).collect();
    // Integer arithmetic, because a mean in microseconds needs no float and a `u64 as f64` is a
    // lint this workspace refuses on purpose.
    let mean = micros.checked_div(views).unwrap_or(0);
    format!(
        "catalog views {views}, repeats of the same version {repeats}, mean {mean} us, worst {} us, \
         buckets <100us {} <1ms {} <10ms {} rest {}",
        WORST.load(Ordering::Relaxed),
        counts[0],
        counts[1],
        counts[2],
        counts[3],
    )
}

#[cfg(test)]
mod tests {
    use super::{enabled, record, summary};
    use std::time::Duration;

    /// The boundaries, which are the only arithmetic here that can be wrong.
    #[test]
    fn a_reading_falls_in_the_bucket_its_size_says() {
        use super::bucket_of;
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(99), 0);
        assert_eq!(bucket_of(100), 1);
        assert_eq!(bucket_of(999), 1);
        assert_eq!(bucket_of(1_000), 2);
        assert_eq!(bucket_of(9_999), 2);
        assert_eq!(bucket_of(10_000), 3);
        assert_eq!(bucket_of(u64::MAX), 3);
    }

    /// **Off by default, and off costs one atomic load.** The instrument sits on the path of every
    /// statement, so a build that nobody switched on must not pay for it — and must not print.
    #[test]
    fn it_is_off_unless_the_environment_says_otherwise() {
        // The test binary is not run with the variable set; if it ever is, this says so rather
        // than silently measuring the wrong thing.
        if std::env::var_os("ESKER_CATALOG_STATS").is_some() {
            assert!(enabled());
            return;
        }
        assert!(!enabled());
        record(Duration::from_millis(5), 7);
        assert!(
            summary().contains("catalog views 0"),
            "a disabled instrument counted a read: {}",
            summary()
        );
    }
}
