//! The benchmark driver.
//!
//! Five workloads, chosen because between them they exercise every path the engine has:
//! sequential and random writes, an overwrite of keys that already exist, random point reads,
//! and a full scan. They are `LevelDB`'s `db_bench` names on purpose — the numbers are meant
//! to be compared with something.
//!
//! # What is measured, and what is not
//!
//! The timer covers the measured phase only. A read workload has to have something to read, so
//! it fills the database first and that fill is **not** counted; a `flush` at the end of a
//! write workload is not counted either, because it is not part of the write path. Every
//! operation's latency is recorded, so the percentiles are exact rather than sampled: at a
//! million operations that is eight megabytes of timestamps, which is cheaper than being
//! approximately right.
//!
//! # Not a gate
//!
//! `CLAUDE.md` says benchmarks are not optional but are not gates, and that we do not tune
//! before correctness is proven. This exists so that a regression is *visible* in
//! `docs/bench/`, not so that anyone optimises against it today.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_engine::batch::WriteBatch;
use esker_engine::options::{Options, ReadOptions, WalSyncMode, WriteOptions};
use esker_engine::{Db, cf};

/// One of the five workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Workload {
    /// Write every key once, in order.
    FillSeq,
    /// Write random keys.
    FillRandom,
    /// Write random keys over a database that already holds them.
    Overwrite,
    /// Read random keys.
    ReadRandom,
    /// Iterate the whole database.
    ReadSeq,
}

impl Workload {
    /// The name on the command line.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "fillseq" => Some(Self::FillSeq),
            "fillrandom" => Some(Self::FillRandom),
            "overwrite" => Some(Self::Overwrite),
            "readrandom" => Some(Self::ReadRandom),
            "readseq" => Some(Self::ReadSeq),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::FillSeq => "fillseq",
            Self::FillRandom => "fillrandom",
            Self::Overwrite => "overwrite",
            Self::ReadRandom => "readrandom",
            Self::ReadSeq => "readseq",
        }
    }

    /// Whether the database has to be populated before the measured phase.
    fn needs_a_populated_database(self) -> bool {
        matches!(self, Self::Overwrite | Self::ReadRandom | Self::ReadSeq)
    }
}

/// What one run measured.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    pub(crate) workload: Workload,
    pub(crate) operations: u64,
    pub(crate) bytes: u64,
    pub(crate) elapsed: Duration,
    /// Median operation latency.
    pub(crate) p50: Duration,
    /// 99th percentile operation latency.
    pub(crate) p99: Duration,
}

impl Report {
    fn per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)] // Counts here are far below f64's exact range.
        let operations = self.operations as f64;
        operations / seconds
    }

    fn megabytes_per_second(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let bytes = self.bytes as f64;
        bytes / seconds / (1024.0 * 1024.0)
    }

    /// One line per run, in the shape `docs/bench/` records.
    pub(crate) fn print(&self) {
        println!(
            "{:<11} {:>10} ops  {:>10.0} ops/s  {:>8.2} MB/s  p50 {:>8.1} µs  p99 {:>9.1} µs",
            self.workload.name(),
            self.operations,
            self.per_second(),
            self.megabytes_per_second(),
            micros(self.p50),
            micros(self.p99),
        );
    }
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

/// How to run one workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) workload: Workload,
    /// Keys in the database, and operations in the measured phase.
    pub(crate) num: u64,
    pub(crate) value_size: u32,
    /// Entries per write batch. One is a batch per key, which is the honest default.
    pub(crate) batch_size: u32,
    pub(crate) threads: u32,
    /// Whether each write waits for its bytes to be durable.
    pub(crate) sync: bool,
    /// Where to put the database. A fresh temporary directory when `None`.
    pub(crate) dir: Option<PathBuf>,
    /// Stop the measured phase early after this long. Zero runs the whole workload.
    pub(crate) duration_secs: u32,
}

impl Default for Run {
    /// `db_bench`'s defaults where they exist: one thread, one entry per batch, and writes
    /// acknowledged before they are durable. A benchmark whose defaults are ambitious measures
    /// the settings rather than the engine.
    fn default() -> Self {
        Self {
            workload: Workload::FillRandom,
            num: 100_000,
            value_size: 100,
            batch_size: 1,
            threads: 1,
            sync: false,
            dir: None,
            duration_secs: 0,
        }
    }
}

/// A key, formatted the way `db_bench` formats one so the numbers are comparable.
fn key_for(index: u64) -> Vec<u8> {
    format!("key{index:016}").into_bytes()
}

fn value_of(size: u32, seed: u64) -> Vec<u8> {
    let mut value = vec![0u8; usize::try_from(size).unwrap_or(0)];
    let mut rng = Pcg32::from_seed(seed);
    rng.fill_bytes(&mut value);
    value
}

/// Runs one workload and returns what it measured.
pub(crate) fn run(options: &Run) -> Result<Report, String> {
    let (dir, temporary) = match &options.dir {
        Some(dir) => (dir.clone(), false),
        None => (temp_dir(), true),
    };
    let result = run_in(options, &dir);
    if temporary {
        // Best effort: a benchmark that leaves a directory behind is untidy, not broken.
        let _unused = std::fs::remove_dir_all(&dir);
    }
    result
}

fn run_in(options: &Run, dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let db = Db::open(
        dir,
        Options {
            create_if_missing: true,
            error_if_exists: false,
            // The workload's own `--sync` decides; the engine adds nothing of its own.
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        },
    )
    .map_err(|err| format!("opening {}: {err}", dir.display()))?;
    let db = Arc::new(db);

    if options.workload.needs_a_populated_database() {
        // Untimed: a read workload has to have something to read, and filling it is not what
        // is being measured.
        populate(&db, options)?;
        db.flush(cf::DEFAULT).map_err(|err| err.to_string())?;
    }

    let started = Instant::now();
    let latencies = match options.workload {
        Workload::ReadSeq => measure_scan(&db, options)?,
        Workload::ReadRandom => measure_parallel(&db, options, read_random)?,
        Workload::FillSeq => measure_parallel(&db, options, write_sequential)?,
        Workload::FillRandom | Workload::Overwrite => measure_parallel(&db, options, write_random)?,
    };
    let elapsed = started.elapsed();

    let operations = u64::try_from(latencies.len()).unwrap_or(u64::MAX);
    // Key plus value either way: a read moves the same bytes a write did.
    let key_bytes = u64::try_from(key_for(0).len()).unwrap_or(0);
    let bytes = operations * (key_bytes + u64::from(options.value_size));

    let mut latencies = latencies;
    latencies.sort_unstable();
    Ok(Report {
        workload: options.workload,
        operations,
        bytes,
        elapsed,
        p50: percentile(&latencies, 0.50),
        p99: percentile(&latencies, 0.99),
    })
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let index = ((sorted.len() as f64 - 1.0) * fraction) as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Fills the database without timing it.
fn populate(db: &Arc<Db>, options: &Run) -> Result<(), String> {
    let value = value_of(options.value_size, 1);
    let write = WriteOptions::unsynced();
    let mut batch = WriteBatch::new();
    for index in 0..options.num {
        batch.put(0, &key_for(index), &value);
        if batch.count() >= options.batch_size.max(1) {
            db.write(std::mem::take(&mut batch), &write)
                .map_err(|err| err.to_string())?;
        }
    }
    if !batch.is_empty() {
        db.write(batch, &write).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// Splits the operation count across threads and collects every latency.
/// What one worker does: the database, the options, its worker index, the first key it owns
/// and how many operations it performs.
type WorkerBody = fn(&Db, &Run, u32, u64, u64) -> Result<Vec<Duration>, String>;

fn measure_parallel(
    db: &Arc<Db>,
    options: &Run,
    body: WorkerBody,
) -> Result<Vec<Duration>, String> {
    let threads = options.threads.max(1);
    let per_thread = options.num / u64::from(threads);
    let deadline = deadline_of(options);

    let handles: Vec<_> = (0..threads)
        .map(|worker| {
            let db = Arc::clone(db);
            let options = options.clone();
            std::thread::spawn(move || {
                let start = u64::from(worker) * per_thread;
                let _ = deadline;
                body(&db, &options, worker, start, per_thread)
            })
        })
        .collect();

    let mut latencies = Vec::new();
    for handle in handles {
        latencies.extend(
            handle
                .join()
                .map_err(|_| "a worker panicked".to_string())??,
        );
    }
    Ok(latencies)
}

fn deadline_of(options: &Run) -> Option<Instant> {
    (options.duration_secs > 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(options.duration_secs)))
}

fn write_options(options: &Run) -> WriteOptions {
    if options.sync {
        WriteOptions::synced()
    } else {
        WriteOptions::unsynced()
    }
}

fn write_sequential(
    db: &Db,
    options: &Run,
    _worker: u32,
    start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    write_keys(db, options, (start..start + count).collect())
}

fn write_random(
    db: &Db,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0xE5E5_0000 + u64::from(worker), u64::from(worker));
    let keys = (0..count)
        .map(|_| rng.range_inclusive(0, options.num.saturating_sub(1)))
        .collect();
    write_keys(db, options, keys)
}

fn write_keys(db: &Db, options: &Run, keys: Vec<u64>) -> Result<Vec<Duration>, String> {
    let value = value_of(options.value_size, 2);
    let write = write_options(options);
    let batch_size = options.batch_size.max(1);
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(keys.len());

    let mut batch = WriteBatch::new();
    let mut pending = 0u32;
    let mut since = Instant::now();
    for key in keys {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        batch.put(0, &key_for(key), &value);
        pending += 1;
        if pending >= batch_size {
            db.write(std::mem::take(&mut batch), &write)
                .map_err(|err| err.to_string())?;
            let elapsed = since.elapsed();
            // One batch is one write, so its latency is shared across the keys it carried.
            for _ in 0..pending {
                latencies.push(elapsed / pending);
            }
            pending = 0;
            since = Instant::now();
        }
    }
    if pending > 0 {
        db.write(batch, &write).map_err(|err| err.to_string())?;
        let elapsed = since.elapsed();
        for _ in 0..pending {
            latencies.push(elapsed / pending);
        }
    }
    Ok(latencies)
}

fn read_random(
    db: &Db,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0x4EAD_0000 + u64::from(worker), u64::from(worker));
    let read = ReadOptions::default();
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let key = key_for(rng.range_inclusive(0, options.num.saturating_sub(1)));
        let started = Instant::now();
        let found = db
            .get(cf::DEFAULT, &key, &read)
            .map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
        // Reading nothing would make the number meaningless, so say so rather than report it.
        if found.is_none() {
            return Err(format!(
                "readrandom missed {}: the database was not fully populated",
                String::from_utf8_lossy(&key)
            ));
        }
    }
    Ok(latencies)
}

/// A scan is one cursor, so it runs on one thread whatever `--threads` says.
fn measure_scan(db: &Arc<Db>, options: &Run) -> Result<Vec<Duration>, String> {
    let deadline = deadline_of(options);
    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|err| err.to_string())?;
    let mut latencies = Vec::with_capacity(usize::try_from(options.num).unwrap_or(0));
    iter.seek_to_first();
    while iter.valid() {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let started = Instant::now();
        let _unused = (iter.key().len(), iter.value().len());
        iter.next();
        latencies.push(started.elapsed());
    }
    iter.status().map_err(|err| err.to_string())?;
    Ok(latencies)
}

/// A fresh directory under the system temporary directory.
fn temp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    std::env::temp_dir().join(format!("esker-bench-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{Run, Workload, key_for, percentile, run};
    use std::time::Duration;

    fn small(workload: Workload) -> Run {
        Run {
            workload,
            num: 200,
            value_size: 16,
            batch_size: 1,
            threads: 2,
            sync: false,
            dir: None,
            duration_secs: 0,
        }
    }

    #[test]
    fn workload_names_round_trip() {
        for name in [
            "fillseq",
            "fillrandom",
            "overwrite",
            "readrandom",
            "readseq",
        ] {
            let workload = Workload::parse(name).expect(name);
            assert_eq!(workload.name(), name);
        }
        assert_eq!(Workload::parse("nonsense"), None);
    }

    #[test]
    fn keys_are_fixed_width_so_sequential_order_is_key_order() {
        assert_eq!(key_for(0).len(), key_for(999_999).len());
        assert!(key_for(1) < key_for(2));
        assert!(
            key_for(9) < key_for(10),
            "zero padding, not lexicographic digits"
        );
    }

    #[test]
    fn percentiles_of_a_known_list() {
        let sorted: Vec<Duration> = (0..100).map(Duration::from_micros).collect();
        assert_eq!(percentile(&sorted, 0.50), Duration::from_micros(49));
        assert_eq!(percentile(&sorted, 0.99), Duration::from_micros(98));
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
    }

    /// Every workload runs end to end against a real database. Small, because this is a test
    /// of the driver and not a measurement.
    #[test]
    fn every_workload_runs() {
        for workload in [
            Workload::FillSeq,
            Workload::FillRandom,
            Workload::Overwrite,
            Workload::ReadRandom,
            Workload::ReadSeq,
        ] {
            let report = run(&small(workload)).unwrap_or_else(|err| panic!("{workload:?}: {err}"));
            assert_eq!(report.workload, workload);
            assert!(report.operations > 0, "{workload:?} measured nothing");
            assert!(report.elapsed > Duration::ZERO);
        }
    }

    /// `--sync` is the difference between a benchmark that measures the disk and one that
    /// measures the page cache, so it has to reach the write path.
    #[test]
    fn a_synced_run_completes() {
        let mut options = small(Workload::FillSeq);
        options.sync = true;
        options.num = 50;
        let report = run(&options).unwrap();
        assert_eq!(report.operations, 50);
    }
}
