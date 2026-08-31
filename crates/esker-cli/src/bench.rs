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
use esker_engine::options::{CfOptions, Options, ReadOptions, WalSyncMode, WriteOptions};
use esker_engine::{Db, cf};

/// One of the workloads: six over the engine, two over the placement driver.
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
    /// Read random keys that are not there, but sort inside the range of the keys that are.
    ///
    /// The workload the bloom filter exists for: the range check cannot rule these out, so
    /// without a filter every one of them costs an index lookup and a block read.
    ReadMissing,
    /// Iterate the whole database.
    ReadSeq,
    /// Take timestamps from the placement driver's oracle ([`crate::bench_pd`]).
    ///
    /// Not an engine workload: phase 5 takes two of these per transaction
    /// (`docs/DESIGN.md` §8), so the oracle is on the critical path of everything above it and
    /// needs a number of its own.
    Tso,
    /// Take cluster-unique ids from the placement driver's allocator.
    AllocId,
    /// One **transaction** per operation, writing `--batch-size` keys
    /// ([`crate::bench_txn`]).
    ///
    /// The comparison that matters is against `fillrandom --remote` on the same store: the gap
    /// is what two-phase commit and MVCC cost, and `docs/bench/phase-5.md` explains it.
    TxnPut,
    /// A snapshot and one transactional read per operation, against `readrandom --remote`.
    TxnGet,
}

impl Workload {
    /// The name on the command line.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "fillseq" => Some(Self::FillSeq),
            "fillrandom" => Some(Self::FillRandom),
            "overwrite" => Some(Self::Overwrite),
            "readrandom" => Some(Self::ReadRandom),
            "readmissing" => Some(Self::ReadMissing),
            "readseq" => Some(Self::ReadSeq),
            "tso" => Some(Self::Tso),
            "allocid" => Some(Self::AllocId),
            "txnput" => Some(Self::TxnPut),
            "txnget" => Some(Self::TxnGet),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::FillSeq => "fillseq",
            Self::FillRandom => "fillrandom",
            Self::Overwrite => "overwrite",
            Self::ReadRandom => "readrandom",
            Self::ReadMissing => "readmissing",
            Self::ReadSeq => "readseq",
            Self::Tso => "tso",
            Self::AllocId => "allocid",
            Self::TxnPut => "txnput",
            Self::TxnGet => "txnget",
        }
    }

    /// Whether this workload speaks `TxnKv` rather than `RawKv`.
    ///
    /// Only over the network: a transaction is a conversation with a *store* — locks, records
    /// and a commit point that apply decides — and there is no in-process shortcut to it the
    /// way there is for an engine workload.
    pub(crate) fn is_transactional(self) -> bool {
        matches!(self, Self::TxnPut | Self::TxnGet)
    }

    /// Whether this workload measures the placement driver rather than the engine.
    ///
    /// The two are different processes with different state, so they cannot share a setup:
    /// one opens a `Db` and the other a `Pd`. This is the fork in [`run`].
    pub(crate) fn is_placement_driver(self) -> bool {
        matches!(self, Self::Tso | Self::AllocId)
    }

    /// Whether the database has to be populated before the measured phase.
    pub(crate) fn needs_a_populated_database(self) -> bool {
        matches!(
            self,
            Self::Overwrite | Self::ReadRandom | Self::ReadMissing | Self::ReadSeq | Self::TxnGet
        )
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
    /// What the SST tier did during the measured phase, when there was one.
    ///
    /// Taken after the clock stops and before the database is dropped: the counters are the
    /// only way to say whether a "cold cache" run was actually cold, and a p99 without them is
    /// a number nobody can interpret six months later.
    pub(crate) tier: Option<esker_engine::fs::tier::TierStats>,
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
        if let Some(tier) = &self.tier {
            let hit_rate = tier
                .hit_rate()
                .map_or_else(|| "n/a".to_string(), |rate| format!("{:.1}%", rate * 100.0));
            println!(
                "{:<11} tier: hit rate {hit_rate}  opens {}/{}  ranged reads {}  \
                 fetches {}  evictions {}",
                "",
                tier.cache_hits,
                tier.cache_hits + tier.cache_misses,
                tier.ranged_reads,
                tier.fetches,
                tier.evictions,
            );
        }
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
    /// Bloom filter bits per key. Zero builds no filter, which is how the filter's own cost
    /// and benefit are measured rather than argued about.
    pub(crate) bloom_bits: u32,
    /// Drive the workload over the network against this `host:port` instead of opening a
    /// database in this process. The engine options above are the server's business then, not
    /// this driver's, and are ignored.
    pub(crate) remote: Option<String>,
    /// Tier the SSTs into `s3://bucket/prefix` instead of leaving them on local disk.
    ///
    /// The endpoint and the credentials come from the environment — `ESKER_S3_ENDPOINT`,
    /// `ESKER_S3_KEY`, `ESKER_S3_SECRET`, `ESKER_S3_REGION` — and not from flags, because a
    /// secret on a command line is a secret in everybody's `ps` output.
    pub(crate) sst_store: Option<String>,
    /// Local SST bytes the tier may keep. `None` keeps everything, which is a warm cache;
    /// `Some(0)` keeps nothing, which is the cold-cache number `docs/bench/phase-6b.md`
    /// records. Ignored without `--sst-store`.
    pub(crate) sst_cache_bytes: Option<u64>,
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
            bloom_bits: 10,
            remote: None,
            sst_store: None,
            sst_cache_bytes: None,
        }
    }
}

/// A key, formatted the way `db_bench` formats one so the numbers are comparable.
pub(crate) fn key_for(index: u64) -> Vec<u8> {
    format!("key{index:016}").into_bytes()
}

pub(crate) fn value_of(size: u32, seed: u64) -> Vec<u8> {
    let mut value = vec![0u8; usize::try_from(size).unwrap_or(0)];
    let mut rng = Pcg32::from_seed(seed);
    rng.fill_bytes(&mut value);
    value
}

/// Runs one workload and returns what it measured.
///
/// `--remote` drives the same workload over the network instead, against a server that owns
/// its own database; the engine options here are that server's business and are ignored.
pub(crate) fn run(options: &Run) -> Result<Report, String> {
    if let Some(addr) = &options.remote {
        // `--remote` drives a store, and a placement driver is not one. Refused rather than
        // ignored: a flag that quietly measures something else is worse than one that does not
        // work.
        if options.workload.is_placement_driver() {
            return Err(format!(
                "`--remote {addr}` drives a store; the `{}` workload measures a placement \
                 driver in this process",
                options.workload.name()
            ));
        }
        if options.workload.is_transactional() {
            return crate::bench_txn::run(options, addr);
        }
        return crate::bench_remote::run(options, addr);
    }
    // A transaction has no in-process form: its decisions happen at apply, inside a store.
    if options.workload.is_transactional() {
        return Err(format!(
            "the `{}` workload needs a store to talk to; give it `--remote HOST:PORT`",
            options.workload.name()
        ));
    }
    let (dir, temporary) = match &options.dir {
        Some(dir) => (dir.clone(), false),
        None => (temp_dir(), true),
    };
    let result = if options.workload.is_placement_driver() {
        crate::bench_pd::run(options, &dir)
    } else {
        run_in(options, &dir)
    };
    if temporary {
        // Best effort: a benchmark that leaves a directory behind is untidy, not broken.
        let _unused = std::fs::remove_dir_all(&dir);
    }
    result
}

/// Uploads everything the populate phase wrote, then lets the governor settle.
///
/// Loops until a pass moves nothing: one pass handles `batch` files, and a populate can leave
/// many more than that. Does nothing at all when the filesystem has no tier.
fn drain_the_tier(db: &Db) -> Result<(), String> {
    if db.tier_stats().is_none() {
        return Ok(());
    }
    // Bounded so that a store that is refusing every upload ends the bench with a message
    // rather than spinning: each pass that moves nothing still runs the governor, so two
    // consecutive empty passes mean there is nothing left to do or nothing that can be done.
    let mut empty_passes = 0;
    for _ in 0..10_000 {
        let moved = db.tier_maintenance().map_err(|err| err.to_string())?;
        empty_passes = if moved == 0 { empty_passes + 1 } else { 0 };
        if empty_passes >= 2 {
            break;
        }
    }
    Ok(())
}

fn run_in(options: &Run, dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    // `background: false`: a benchmark wants a steady state, not a measurement of the
    // uploader fetching files back while the measured phase reads them. Uploads are driven
    // explicitly by `drain_the_tier` below, before the clock starts.
    let fs = crate::sst_store::filesystem(
        options.sst_store.as_deref(),
        dir,
        options.sst_cache_bytes,
        false,
    )?;
    let db = Db::open_with(
        dir,
        Options {
            create_if_missing: true,
            error_if_exists: false,
            // The workload's own `--sync` decides; the engine adds nothing of its own.
            wal_sync_mode: WalSyncMode::Never,
            cf_options: CfOptions {
                bloom_bits_per_key: usize::try_from(options.bloom_bits).unwrap_or(0),
                ..CfOptions::default()
            },
            ..Options::default()
        },
        fs,
        &cf::BUILTIN,
    )
    .map_err(|err| format!("opening {}: {err}", dir.display()))?;
    let db = Arc::new(db);

    if options.workload.needs_a_populated_database() {
        // Untimed: a read workload has to have something to read, and filling it is not what
        // is being measured.
        populate(&db, options)?;
        db.flush(cf::DEFAULT).map_err(|err| err.to_string())?;
    }
    // Untimed for the same reason, and *before* the clock starts so that the measured phase
    // sees whatever steady state the budget implies: everything local, nothing local, or the
    // mixture in between that makes the hit rate a number worth recording.
    drain_the_tier(&db)?;

    let started = Instant::now();
    let latencies = match options.workload {
        Workload::ReadSeq => measure_scan(&db, options)?,
        Workload::ReadRandom => measure_parallel(&db, options, read_random)?,
        Workload::ReadMissing => measure_parallel(&db, options, read_missing)?,
        Workload::FillSeq => measure_parallel(&db, options, write_sequential)?,
        Workload::FillRandom | Workload::Overwrite => measure_parallel(&db, options, write_random)?,
        // Unreachable by construction: `run` forks the placement-driver workloads into
        // `bench_pd` before a database is opened. An error rather than a panic, because a
        // fork that grew a hole should say so and not abort (`CLAUDE.md` invariant 9).
        other if other.is_placement_driver() => {
            return Err(format!("{} does not run against a database", other.name()));
        }
        other => return Err(format!("{} has no measured phase", other.name())),
    };
    let elapsed = started.elapsed();
    // After the clock, before the drop: the tier's counters describe the measured phase.
    let tier = db.tier_stats();

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
        tier,
    })
}

pub(crate) fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
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

/// Reads keys that are absent but sort between two that are present, so the file's key range
/// cannot rule them out and only the bloom filter can. `db_bench` does the same by appending a
/// character to a key that exists.
fn read_missing(
    db: &Db,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0x_4155_0000 + u64::from(worker), u64::from(worker));
    let read = ReadOptions::default();
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let mut key = key_for(rng.range_inclusive(0, options.num.saturating_sub(1)));
        key.push(b'.');
        let started = Instant::now();
        let found = db
            .get(cf::DEFAULT, &key, &read)
            .map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
        if found.is_some() {
            return Err("readmissing found a key that should not exist".to_string());
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
/// A directory no other run is using.
///
/// The counter is not decoration. `SystemTime::now()` is not nanosecond-granular — on this
/// machine 96% of consecutive reads in a tight loop return the *same* value — so two runs
/// starting at once used to be handed the same path, and the first to finish deleted the
/// other's database out from under it. The process id separates processes and the counter
/// separates threads within one; the timestamp is left in because it makes a leftover
/// directory readable.
fn temp_dir() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    std::env::temp_dir().join(format!(
        "esker-bench-{}-{nanos}-{unique}",
        std::process::id()
    ))
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
            bloom_bits: 10,
            remote: None,
            sst_store: None,
            sst_cache_bytes: None,
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
            "tso",
            "allocid",
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
            Workload::ReadMissing,
            Workload::ReadSeq,
            Workload::Tso,
            Workload::AllocId,
        ] {
            let report = run(&small(workload)).unwrap_or_else(|err| panic!("{workload:?}: {err}"));
            assert_eq!(report.workload, workload);
            assert!(report.operations > 0, "{workload:?} measured nothing");
            assert!(report.elapsed > Duration::ZERO);
        }
    }

    /// Two runs starting at the same instant must not be handed the same directory. They were:
    /// the path was the process id and a timestamp, and the timestamp is not fine-grained
    /// enough to separate two threads, so one run's cleanup deleted the other's database. It
    /// surfaced as a one-in-many failure of the workload sweep once the placement-driver
    /// workloads added three more concurrent runs to this file.
    #[test]
    fn concurrent_runs_never_share_a_directory() {
        let handles: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(|| (0..64).map(|_| super::temp_dir()).collect::<Vec<_>>()))
            .collect();
        let paths: Vec<std::path::PathBuf> = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a worker panicked"))
            .collect();
        let unique: std::collections::BTreeSet<&std::path::PathBuf> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "two runs shared a directory");
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
