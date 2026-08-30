//! The placement driver's two workloads: `tso` and `allocid`.
//!
//! Both measure the same shape — a monotone counter that reserves ahead and fsyncs the
//! reservation *before* it hands out anything the reservation covers (`docs/DESIGN.md` §7,
//! `docs/adr/0010-pd-durable-state.md`). What is interesting about them is not the counter but
//! the **amortisation**: a call that stays inside the current reservation costs a mutex and
//! some arithmetic, and a call that crosses one costs an `fsync`. So the number to read is not
//! a single ops/s, it is how ops/s moves with `--batch-size` and `--threads`.
//!
//! # Why these two exist at all
//!
//! Phase 5 takes a `start_ts` and a `commit_ts` from the oracle for **every transaction**
//! (`docs/DESIGN.md` §8), which puts it on the critical path of everything above it, and 4b
//! takes ids from the allocator for every split. Neither had a recorded number before this,
//! so a regression in either would have shown up first as "phase 5 is slow" with nowhere to
//! look. `CLAUDE.md` is explicit that this exists to make a regression *visible*, not to be
//! optimised against today.
//!
//! # What is measured, and what is not
//!
//! The timer covers the measured phase only: opening PD and bootstrapping a cluster happen
//! first and are not counted. There is **no network here** — `--remote` is refused rather than
//! ignored, because `bench --remote` speaks `RawKv` to a store and a placement driver is not
//! one. The round trip these calls would cost over TCP is a property of the transport and is
//! already recorded in `docs/bench/phase-2.md`; what is recorded here is the ceiling those
//! round trips are subtracted from.
//!
//! `--value-size`, `--bloom-bits` and `--sync` mean nothing to PD and are ignored: it stores
//! counters rather than values, and it acknowledges nothing it has not made durable, so there
//! is no unsynced mode to ask for.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_pd::{Pd, PdOptions};

use crate::bench::{Report, Run, Workload, percentile};

/// What one worker asks for, per call, when `--batch-size` is not given.
///
/// One is the honest default, matching the rest of the driver: it is the call a caller makes
/// when it wants a single timestamp, and it is the one that pays for the reservation most
/// often.
const DEFAULT_BATCH: u32 = 1;

/// Runs `tso` or `allocid` against a placement driver in `dir`.
pub(crate) fn run(options: &Run, dir: &Path) -> Result<Report, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let pd = Pd::open(dir, PdOptions::new())
        .map_err(|err| format!("opening {}: {err}", dir.display()))?;

    // Untimed. A PD nothing has bootstrapped is not a PD anyone would measure, and the first
    // reservation of each counter is paid for here rather than inside the measured phase.
    pd.bootstrap(1, "127.0.0.1:20160")
        .map_err(|err| format!("bootstrapping: {err}"))?;

    let started = Instant::now();
    let latencies = measure(&pd, options)?;
    let elapsed = started.elapsed();

    let operations = u64::try_from(latencies.len()).unwrap_or(u64::MAX);
    let mut latencies = latencies;
    latencies.sort_unstable();
    Ok(Report {
        workload: options.workload,
        operations,
        // A timestamp and an id are both one `u64`, which is the whole of what a call produces.
        // The megabytes-per-second column is therefore honest and uninteresting, which is
        // better than leaving it at zero and looking broken.
        bytes: operations * 8,
        elapsed,
        p50: percentile(&latencies, 0.50),
        p99: percentile(&latencies, 0.99),
    })
}

/// Splits the operations across threads and collects every latency.
///
/// The threads are the point. PD serialises allocation behind one mutex — the oracle and the
/// allocator are read-modify-write over state that must not interleave — so this is where the
/// cost of that decision becomes a number rather than an argument.
fn measure(pd: &Arc<Pd>, options: &Run) -> Result<Vec<Duration>, String> {
    let threads = options.threads.max(1);
    let batch = options.batch_size.max(DEFAULT_BATCH);
    // `--num` counts *values*, exactly as it counts keys for every other workload, so that two
    // runs at different batch sizes are the same amount of work and their numbers can be
    // compared. What the batch changes is how many calls that work takes.
    let per_thread = (options.num / u64::from(threads) / u64::from(batch)).max(1);
    let deadline = (options.duration_secs > 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(options.duration_secs)));

    let handles: Vec<_> = (0..threads)
        .map(|_worker| {
            let pd = Arc::clone(pd);
            let workload = options.workload;
            std::thread::spawn(move || worker(&pd, workload, batch, per_thread, deadline))
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

/// One worker's calls, and one latency per value handed out.
///
/// A call for `batch` values is one operation on PD and `batch` values for the caller, so its
/// latency is shared across them — the same accounting `bench::write_keys` uses for a write
/// batch, and the reason `--batch-size` moves the per-operation number at all.
fn worker(
    pd: &Pd,
    workload: Workload,
    batch: u32,
    calls: u64,
    deadline: Option<Instant>,
) -> Result<Vec<Duration>, String> {
    let mut latencies = Vec::with_capacity(usize::try_from(calls).unwrap_or(0));
    for _ in 0..calls {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let started = Instant::now();
        match workload {
            Workload::Tso => {
                pd.tso(batch).map_err(|err| err.to_string())?;
            }
            Workload::AllocId => {
                pd.alloc_id(u64::from(batch))
                    .map_err(|err| err.to_string())?;
            }
            other => {
                return Err(format!(
                    "{} is not a placement-driver workload",
                    other.name()
                ));
            }
        }
        let elapsed = started.elapsed();
        for _ in 0..batch {
            latencies.push(elapsed / batch);
        }
    }
    Ok(latencies)
}

#[cfg(test)]
mod tests {
    use crate::bench::{Run, Workload, run};
    use std::time::Duration;

    fn small(workload: Workload) -> Run {
        Run {
            workload,
            num: 200,
            threads: 2,
            ..Run::default()
        }
    }

    /// Both workloads run end to end against a real placement driver. Small, because this is a
    /// test of the driver and not a measurement.
    #[test]
    fn both_placement_driver_workloads_run() {
        for workload in [Workload::Tso, Workload::AllocId] {
            let report = run(&small(workload)).unwrap_or_else(|err| panic!("{workload:?}: {err}"));
            assert_eq!(report.workload, workload);
            assert!(report.operations > 0, "{workload:?} measured nothing");
            assert!(report.elapsed > Duration::ZERO);
        }
    }

    /// `--num` is values, not calls, so two runs at different batch sizes do the same amount of
    /// work and their numbers can be compared. That is the one comparison these workloads exist
    /// to make, and it is meaningless if the batch silently multiplies the work.
    #[test]
    fn the_batch_size_changes_the_calls_and_not_the_work() {
        let mut options = small(Workload::Tso);
        options.num = 256;
        options.threads = 1;

        options.batch_size = 1;
        assert_eq!(run(&options).unwrap().operations, 256);

        options.batch_size = 16;
        assert_eq!(run(&options).unwrap().operations, 256, "16 calls of 16");

        // A batch larger than the whole run still makes one call rather than none.
        options.batch_size = 1_024;
        assert_eq!(run(&options).unwrap().operations, 1_024);
    }

    /// The values really are handed out, and really are distinct: a benchmark whose counter
    /// repeated itself would be measuring nothing worth having, and would say so nowhere.
    #[test]
    fn the_values_a_run_hands_out_are_all_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let pd = esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).unwrap();
        pd.bootstrap(1, "a:1").unwrap();

        let mut timestamps = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let start = pd.tso(4).unwrap();
            for offset in 0..4 {
                assert!(
                    timestamps.insert(start + offset),
                    "timestamp {start}+{offset}"
                );
            }
            let first = pd.alloc_id(2).unwrap();
            for offset in 0..2 {
                assert!(ids.insert(first + offset), "id {first}+{offset}");
            }
        }
        assert_eq!(timestamps.len(), 64 * 4);
        assert_eq!(ids.len(), 64 * 2);
    }

    /// PD is not a store, and `bench --remote` speaks `RawKv` to one. Refusing says so; ignoring
    /// the flag would silently measure something else.
    #[test]
    fn a_remote_placement_driver_workload_is_refused() {
        let mut options = small(Workload::Tso);
        options.remote = Some("127.0.0.1:2379".to_owned());
        let error = run(&options).unwrap_err();
        assert!(error.contains("--remote"), "{error}");
    }
}
