//! `bench --remote`: the same workloads, driven over the network.
//!
//! The point of this mode is one number: **what the wire costs.** Phase 1 measured the engine
//! with the caller inside the process; this measures the same workloads with a frame, a
//! socket, a request id and a server loop in between, and the difference between the two
//! columns in `docs/bench/phase-2.md` is the price of being a database rather than a library.
//!
//! # Why this is not the local driver made generic
//!
//! It would have been fewer lines, and it would have quietly changed the phase-1 numbers. The
//! local path builds one `WriteBatch` and reuses it; a shared abstraction would allocate a
//! vector of pairs per batch, which at the default of one entry per batch is a per-operation
//! allocation the phase-1 measurements did not have. The two paths also genuinely differ: a
//! remote sequential read is a **paged scan**, because that is what a client actually does,
//! where the local one is a single cursor that never leaves the process.
//!
//! So the local driver is untouched, and everything genuinely shared — the workloads, the key
//! and value shapes, the report and its percentiles — is used from [`crate::bench`] rather
//! than copied.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_base::rng::Pcg32;
use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};

use crate::bench::{Report, Run, Workload, key_for, percentile, value_of};
use crate::raw::{BOOTSTRAP_REGION, NO_LEADER_OPINION, resolve};

/// Pairs asked for per round trip in a sequential read.
///
/// A scan has to fit in one frame, so a client reads a range in pages whatever it wants. A
/// thousand is large enough that the round trip is amortised and small enough that a page of
/// hundred-byte values is nowhere near the frame limit.
const SCAN_PAGE: u32 = 1_000;

/// Runs one workload against the store at `addr`.
pub(crate) fn run(options: &Run, addr: &str) -> Result<Report, String> {
    let client = Arc::new(connect(addr)?);

    if options.workload.needs_a_populated_database() {
        // Untimed, exactly as the local driver does it: a read workload has to have something
        // to read, and filling it is not what is being measured.
        populate(&client, options)?;
    }

    let started = Instant::now();
    let latencies = match options.workload {
        Workload::ReadSeq => scan_everything(&client, options)?,
        Workload::ReadRandom => parallel(&client, options, read_random)?,
        Workload::ReadMissing => parallel(&client, options, read_missing)?,
        Workload::FillSeq => parallel(&client, options, write_sequential)?,
        Workload::FillRandom | Workload::Overwrite => parallel(&client, options, write_random)?,
        // Unreachable by construction: `bench::run` refuses a placement-driver workload with
        // `--remote` before it gets here, because this speaks `RawKv` to a store.
        other => return Err(format!("{} does not run over RawKv", other.name())),
    };
    let elapsed = started.elapsed();

    let operations = u64::try_from(latencies.len()).unwrap_or(u64::MAX);
    let key_bytes = u64::try_from(key_for(0).len()).unwrap_or(0);
    let mut latencies = latencies;
    latencies.sort_unstable();

    Ok(Report {
        workload: options.workload,
        operations,
        bytes: operations * (key_bytes + u64::from(options.value_size)),
        elapsed,
        p50: percentile(&latencies, 0.50),
        p99: percentile(&latencies, 0.99),
        // No database in this process, so no tier to report on.
        tier: None,
    })
}

/// Opens one connection and builds a client that routes through it.
fn connect(addr: &str) -> Result<RawClient, String> {
    let addr = resolve(addr)?;
    let stores = TcpStores::connect(addr).map_err(|err| err.to_string())?;
    let store_id = stores
        .only_store()
        .ok_or_else(|| "the store did not say which store it is".to_owned())?;
    let resolver = StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, NO_LEADER_OPINION);
    Ok(RawClient::new(Arc::new(stores), Arc::new(resolver)))
}

/// What one worker does: the client, the options, its index, its first key, and how many
/// operations it performs.
type WorkerBody = fn(&RawClient, &Run, u32, u64, u64) -> Result<Vec<Duration>, String>;

fn parallel(
    client: &Arc<RawClient>,
    options: &Run,
    body: WorkerBody,
) -> Result<Vec<Duration>, String> {
    let threads = options.threads.max(1);
    let per_thread = options.num / u64::from(threads);

    let workers: Vec<_> = (0..threads)
        .map(|worker| {
            let client = Arc::clone(client);
            let options = options.clone();
            std::thread::spawn(move || {
                let start = u64::from(worker) * per_thread;
                body(&client, &options, worker, start, per_thread)
            })
        })
        .collect();

    let mut latencies = Vec::new();
    for worker in workers {
        latencies.extend(
            worker
                .join()
                .map_err(|_| "a worker panicked".to_owned())??,
        );
    }
    Ok(latencies)
}

fn deadline_of(options: &Run) -> Option<Instant> {
    (options.duration_secs > 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(options.duration_secs)))
}

/// Fills the database without timing it. Unsynced and batched, because this is setup rather
/// than measurement and its only job is to be over quickly.
fn populate(client: &RawClient, options: &Run) -> Result<(), String> {
    let value = Bytes::from(value_of(options.value_size, 1));
    let batch_size = options.batch_size.max(1) as usize;
    let mut batch: Vec<(Bytes, Bytes)> = Vec::with_capacity(batch_size.max(64));

    for index in 0..options.num {
        batch.push((Bytes::from(key_for(index)), value.clone()));
        if batch.len() >= batch_size.max(64) {
            client
                .batch_put_with(std::mem::take(&mut batch), false)
                .map_err(|err| err.to_string())?;
        }
    }
    if !batch.is_empty() {
        client
            .batch_put_with(batch, false)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn write_sequential(
    client: &RawClient,
    options: &Run,
    _worker: u32,
    start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    write_keys(client, options, (start..start + count).collect())
}

fn write_random(
    client: &RawClient,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0xE5E5_0000 + u64::from(worker), u64::from(worker));
    let keys = (0..count)
        .map(|_| rng.range_inclusive(0, options.num.saturating_sub(1)))
        .collect();
    write_keys(client, options, keys)
}

/// One batch is one round trip, so its latency is shared across the keys it carried — the
/// same convention the local driver uses, so the two columns mean the same thing.
fn write_keys(client: &RawClient, options: &Run, keys: Vec<u64>) -> Result<Vec<Duration>, String> {
    let value = Bytes::from(value_of(options.value_size, 2));
    let batch_size = options.batch_size.max(1);
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(keys.len());

    let mut batch: Vec<(Bytes, Bytes)> = Vec::new();
    let mut since = Instant::now();
    for key in keys {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        batch.push((Bytes::from(key_for(key)), value.clone()));
        if u32::try_from(batch.len()).unwrap_or(u32::MAX) < batch_size {
            continue;
        }
        let pending = batch.len();
        flush(client, std::mem::take(&mut batch), options.sync)?;
        let elapsed = since.elapsed();
        let share = elapsed / u32::try_from(pending).unwrap_or(1);
        latencies.extend(std::iter::repeat_n(share, pending));
        since = Instant::now();
    }
    if !batch.is_empty() {
        let pending = batch.len();
        flush(client, batch, options.sync)?;
        let elapsed = since.elapsed();
        let share = elapsed / u32::try_from(pending).unwrap_or(1);
        latencies.extend(std::iter::repeat_n(share, pending));
    }
    Ok(latencies)
}

/// Sends one batch as the request that fits it.
///
/// A single key goes out as a `Put`, which is what the local driver's single-entry
/// `WriteBatch` becomes as well; several go as one atomic `BatchPut`. Sending a one-key
/// `BatchPut` instead would measure a different request than the local column does.
fn flush(client: &RawClient, mut batch: Vec<(Bytes, Bytes)>, sync: bool) -> Result<(), String> {
    if batch.len() == 1
        && let Some((key, value)) = batch.pop()
    {
        return client
            .put_with(&key, &value, sync)
            .map_err(|err| err.to_string());
    }
    client
        .batch_put_with(batch, sync)
        .map_err(|err| err.to_string())
}

fn read_random(
    client: &RawClient,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0x4EAD_0000 + u64::from(worker), u64::from(worker));
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let key = key_for(rng.range_inclusive(0, options.num.saturating_sub(1)));
        let started = Instant::now();
        let found = client.get(&key).map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
        // Reading nothing would make the number meaningless, so say so rather than report it.
        if found.is_none() {
            return Err(format!(
                "readrandom missed {}: the store was not fully populated",
                String::from_utf8_lossy(&key)
            ));
        }
    }
    Ok(latencies)
}

/// Keys that are absent but sort between two that are present, so only the bloom filter can
/// rule them out — the workload the filter exists for, now with a round trip in front of it.
fn read_missing(
    client: &RawClient,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::new(0x4155_0000 + u64::from(worker), u64::from(worker));
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let mut key = key_for(rng.range_inclusive(0, options.num.saturating_sub(1)));
        key.push(b'.');
        let started = Instant::now();
        let found = client.get(&key).map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
        if found.is_some() {
            return Err("readmissing found a key that should not exist".to_owned());
        }
    }
    Ok(latencies)
}

/// A sequential read, in pages.
///
/// A remote scan cannot be one cursor held open across the whole database — a response has to
/// fit in a frame — so a client pages through it, which is what this measures. One page is one
/// round trip, and its latency is shared across the pairs it returned.
fn scan_everything(client: &RawClient, options: &Run) -> Result<Vec<Duration>, String> {
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(options.num).unwrap_or(0));
    let mut cursor: Vec<u8> = Vec::new();

    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let started = Instant::now();
        let page = client
            .scan(&cursor, b"", SCAN_PAGE)
            .map_err(|err| err.to_string())?;
        let elapsed = started.elapsed();
        // An empty page is the end of the scan, and it is also the only thing that stops this
        // loop — so taking the last key and testing for the end are the same step.
        let Some((last_key, _)) = page.last() else {
            break;
        };
        let share = elapsed / u32::try_from(page.len()).unwrap_or(1);
        latencies.extend(std::iter::repeat_n(share, page.len()));

        // The next page starts just after the last key returned. Appending a zero byte is the
        // immediate successor of any key, so nothing is skipped and nothing is read twice.
        cursor.clear();
        cursor.extend_from_slice(last_key);
        cursor.push(0);
    }
    Ok(latencies)
}

#[cfg(test)]
mod tests {
    use super::{SCAN_PAGE, connect, run};
    use crate::bench::{Run, Workload};
    use crate::testserver::TestServer;

    /// Every workload, over a real socket to a real store. Small on purpose: this is a test of
    /// the driver, not a measurement — the numbers for `docs/bench/phase-2.md` come from a
    /// deliberate run, not from `cargo test`.
    #[test]
    fn every_workload_runs_against_a_real_server() {
        let server = TestServer::start();
        for workload in [
            Workload::FillSeq,
            Workload::FillRandom,
            Workload::Overwrite,
            Workload::ReadRandom,
            Workload::ReadMissing,
            Workload::ReadSeq,
        ] {
            let options = Run {
                workload,
                num: 200,
                value_size: 16,
                batch_size: 1,
                threads: 2,
                remote: Some(server.addr()),
                ..Run::default()
            };
            let report = run(&options, &server.addr())
                .unwrap_or_else(|err| panic!("{workload:?} over the wire: {err}"));
            assert_eq!(report.workload, workload);
            assert!(report.operations > 0, "{workload:?} measured nothing");
        }
    }

    /// A batch is one round trip, and batching has to actually reduce them — otherwise
    /// `--batch-size` measures nothing and the phase-2 column is comparing the wrong things.
    #[test]
    fn a_batched_write_still_counts_every_key() {
        let server = TestServer::start();
        let options = Run {
            workload: Workload::FillSeq,
            num: 100,
            value_size: 8,
            batch_size: 10,
            threads: 1,
            remote: Some(server.addr()),
            ..Run::default()
        };
        let report = run(&options, &server.addr()).expect("a batched fill");
        assert_eq!(
            report.operations, 100,
            "a batch's latency is shared across its keys, so every key is still an operation"
        );
    }

    /// A sequential read pages through the range, and the paging must not skip a key or read
    /// one twice — the cursor arithmetic is where that would go wrong.
    #[test]
    fn a_paged_scan_reads_every_key_exactly_once() {
        let server = TestServer::start();
        let options = Run {
            workload: Workload::ReadSeq,
            num: 2_500,
            value_size: 8,
            batch_size: 100,
            threads: 1,
            remote: Some(server.addr()),
            ..Run::default()
        };
        let report = run(&options, &server.addr()).expect("a paged scan");
        assert_eq!(
            report.operations, 2_500,
            "the scan read {} pairs across pages of {SCAN_PAGE}",
            report.operations
        );
    }

    /// `--sync` has to reach the wire, or the durable column measures the page cache.
    #[test]
    fn a_synced_remote_write_completes() {
        let server = TestServer::start();
        let options = Run {
            workload: Workload::FillSeq,
            num: 50,
            value_size: 8,
            sync: true,
            remote: Some(server.addr()),
            ..Run::default()
        };
        let report = run(&options, &server.addr()).expect("a synced fill");
        assert_eq!(report.operations, 50);
    }

    /// A benchmark that cannot reach a store must say so, not panic and not report a zero.
    #[test]
    fn an_unreachable_store_is_an_error() {
        let error = connect("127.0.0.1:1").expect_err("nothing is listening");
        assert!(
            error.contains("not sent") || error.contains("refused"),
            "{error}"
        );
        assert!(connect("not a host:port").is_err());
    }

    /// A page has to fit in a frame with room to spare, or a sequential read fails on values
    /// the local driver handles without noticing.
    #[test]
    fn a_page_of_large_values_still_fits_a_frame() {
        let widest_value = 4096;
        let page_bytes = SCAN_PAGE as usize * (widest_value + 32);
        assert!(page_bytes < esker_client::wire::MAX_FRAME_SIZE);
    }
}
