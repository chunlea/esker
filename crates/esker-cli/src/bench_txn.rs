//! `bench txnput` / `bench txnget`: what two-phase commit and MVCC cost over `RawKv`.
//!
//! The number this exists to produce is a **ratio**, not a throughput: `txnput` against
//! `fillrandom --remote`, and `txnget` against `readrandom --remote`, on the same machine
//! against the same store in the same run. `docs/bench/phase-5.md` records both columns and
//! explains the gap, which is the point of `prompts/05-txn.md`'s last acceptance line.
//!
//! # What is being counted, exactly
//!
//! One `txnput` operation is one **whole transaction** over `--batch-size` keys: a snapshot, a
//! `Prewrite` of the primary, a `Prewrite` of the rest, and a `Commit` of the primary — the
//! secondaries' commit is cleanup a reader would do (`docs/txn-spec.md` §5.5), and the client
//! sends it without waiting on it for the answer. At the default batch size of one key that is
//! **two round trips and two engine writes** where a `RawKv` put is one of each, plus the
//! `write` and `lock` column families where `RawKv` touches `default` alone.
//!
//! One `txnget` is a snapshot and one `Get` at it: one round trip, and inside the store a seek
//! in `lock`, a seek in `write` and — for a value too long to inline — one in `default`, where
//! `RawKv` does a single point lookup. Both are one round trip, so the difference between the
//! two columns is entirely what MVCC costs *inside* the store.
//!
//! # The timestamps
//!
//! `CountingOracle`, in this process, not PD. It is a correct oracle for one client
//! (`CLAUDE.md` invariant 6) and this driver is one client, so the numbers are honest about
//! everything except the **round trip** to the oracle, which is deliberately excluded: it is a
//! different machine's latency, and the oracle's own cost is already measured by `bench tso`
//! (`docs/bench/phase-4-pd.md`, 13M timestamps a second in-process — it is the wire that costs,
//! not the counter). A reader wanting the production figure adds two of those round trips to a
//! `txnput` and one to a `txnget`. Folding a fake oracle's zero cost into the ratio without
//! saying so would be the dishonest version of the same choice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_client::region_cache::StaticRegion;
use esker_client::{CountingOracle, TcpStores, TxnClient};

use crate::bench::{Report, Run, Workload, key_for, percentile, value_of};
use crate::raw::{BOOTSTRAP_REGION, NO_LEADER_OPINION, resolve};

/// Runs one transactional workload against the store at `addr`.
pub(crate) fn run(options: &Run, addr: &str) -> Result<Report, String> {
    let client = Arc::new(connect(addr)?);

    if options.workload.needs_a_populated_database() {
        // Untimed, and through transactions rather than `RawKv`: a `txnget` has to read rows
        // that have `write` records and, for long values, `default` entries — which is what
        // MVCC reads cost. Filling with `RawKv` would leave nothing for the read path to do
        // and make the ratio flattering.
        populate(&client, options)?;
    }

    let started = Instant::now();
    let latencies = match options.workload {
        Workload::TxnPut => parallel(&client, options, put_random)?,
        Workload::TxnGet => parallel(&client, options, get_random)?,
        other => return Err(format!("{} is not a transactional workload", other.name())),
    };
    let elapsed = started.elapsed();

    let operations = u64::try_from(latencies.len()).unwrap_or(u64::MAX);
    let key_bytes = u64::try_from(key_for(0).len()).unwrap_or(0);
    let keys_per_operation = u64::from(options.batch_size.max(1));
    let mut latencies = latencies;
    latencies.sort_unstable();

    Ok(Report {
        workload: options.workload,
        operations,
        // The bytes a transaction moved, not the bytes it wrote: the `lock` and `write` records
        // it also lays down are the overhead being measured and counting them as payload would
        // hide it in the throughput.
        bytes: operations * keys_per_operation * (key_bytes + u64::from(options.value_size)),
        elapsed,
        p50: percentile(&latencies, 0.50),
        p99: percentile(&latencies, 0.99),
        // No database in this process, so no tier to report on.
        tier: None,
    })
}

/// One connection, one region covering everything, one oracle shared by every worker.
fn connect(addr: &str) -> Result<TxnClient, String> {
    let addr = resolve(addr)?;
    let stores = TcpStores::connect(addr).map_err(|err| err.to_string())?;
    let store_id = stores
        .only_store()
        .ok_or_else(|| "the store did not say which store it is".to_owned())?;
    let resolver = StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, NO_LEADER_OPINION);
    // One oracle for every thread of this process. Two of them would hand out the same
    // timestamps to different transactions, which is the failure `CLAUDE.md` invariant 6 names.
    //
    // Started from the wall clock in PD's own layout — `physical_ms << 18` — rather than from
    // one. A benchmark run against a store that has been benchmarked before would otherwise
    // begin below the commit timestamps already in it, and every transaction would be refused
    // as a write conflict against a commit from the previous run. This is the same rule PD's
    // oracle restarts under (`esker_pd::tso::Oracle::load` takes the maximum of its clock and
    // its mark); a benchmark has no mark to keep, so the clock alone is what it has.
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0));
    Ok(TxnClient::new(
        Arc::new(stores),
        Arc::new(resolver),
        Arc::new(CountingOracle::starting_at(
            epoch_ms << esker_client::TSO_LOGICAL_BITS,
        )),
    ))
}

/// What one worker does: the client, the options, its index, its first key, and how many
/// operations it performs.
type WorkerBody = fn(&TxnClient, &Run, u32, u64, u64) -> Result<Vec<Duration>, String>;

fn parallel(
    client: &Arc<TxnClient>,
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

/// Fills the database with committed rows, untimed.
fn populate(client: &TxnClient, options: &Run) -> Result<(), String> {
    /// Keys per filling transaction. Large enough that the setup is quick, and it changes
    /// nothing measured: the read workload sees committed rows either way.
    const PER_TXN: u64 = 500;

    let value = value_of(options.value_size, 1);
    let mut index = 0;
    while index < options.num {
        let mut txn = client.begin().map_err(|err| err.to_string())?;
        for key in index..(index + PER_TXN).min(options.num) {
            txn.put(&key_for(key), &value);
        }
        txn.commit().map_err(|err| err.to_string())?;
        index += PER_TXN;
    }
    Ok(())
}

/// Seeds for the two workloads' key streams. Fixed, so two runs of the same benchmark touch
/// the same keys in the same order and the numbers are comparable; different from each other,
/// so a read run does not simply follow the write run's footprints through the cache.
const PUT_SEED: u64 = 0x7b_5f_10_02;
const GET_SEED: u64 = 0x7b_5f_10_03;

/// One transaction per operation, writing `--batch-size` random keys.
fn put_random(
    client: &TxnClient,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::from_seed(u64::from(worker).wrapping_add(PUT_SEED));
    let value = value_of(options.value_size, 1);
    let keys = u64::from(options.batch_size.max(1));
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let started = Instant::now();
        let mut txn = client.begin().map_err(|err| err.to_string())?;
        for _ in 0..keys {
            txn.put(&key_for(rng.next_u64() % options.num.max(1)), &value);
        }
        txn.commit().map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
    }
    Ok(latencies)
}

/// One snapshot and one read per operation.
fn get_random(
    client: &TxnClient,
    options: &Run,
    worker: u32,
    _start: u64,
    count: u64,
) -> Result<Vec<Duration>, String> {
    let mut rng = Pcg32::from_seed(u64::from(worker).wrapping_add(GET_SEED));
    let deadline = deadline_of(options);
    let mut latencies = Vec::with_capacity(usize::try_from(count).unwrap_or(0));

    for _ in 0..count {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let key = key_for(rng.next_u64() % options.num.max(1));
        let started = Instant::now();
        let txn = client.begin().map_err(|err| err.to_string())?;
        txn.get(&key).map_err(|err| err.to_string())?;
        latencies.push(started.elapsed());
    }
    Ok(latencies)
}
