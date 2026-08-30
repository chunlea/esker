//! The threads that drive Raft, and how regions are shared between them.
//!
//! `docs/DESIGN.md` §6 asked for one apply worker per store, "sharded by region id later"; phase 4a
//! shipped one driver thread per region because that is what phase 3e's single region had grown
//! into. Neither extreme survives fifty regions: one thread makes an `fsync` for one region hold up
//! every other region's consensus, and fifty threads is fifty stacks and fifty scheduler entries
//! for work that is mostly idle.
//!
//! This is the middle, and it is the shape `docs/plans/phase-4.md` §14.2 records: **a fixed pool,
//! with each region pinned to one worker by its id**.
//!
//! # Why pinning is the whole correctness argument
//!
//! A region's messages all reach one worker, through one channel, and are handled in the order they
//! arrive. So per-region ordering is exactly what it was when the region had a thread to itself —
//! which is the property `apply_index` rests on, and the `Ready` contract with it. What changes is
//! that a worker holding several regions interleaves *between* them, and nothing depends on that:
//! two regions share no state, no write batch and no apply index.
//!
//! A pool that *scheduled* regions onto whichever worker was free would break this on the first
//! contended moment, and the failure would be a state machine that applied entry 8 before entry 7
//! — silent, rare, and unrecoverable. Pinning costs nothing and removes the question.
//!
//! **By modulo, not by hash.** Region ids come from the placement driver's allocator in order, so
//! `id % workers` spreads them exactly evenly and a hash would only add variance. It is also stable
//! across restarts without being written anywhere, which matters: a region that moved workers
//! between two opens would be a region whose ordering guarantee spanned two threads.
//!
//! # Batching survives
//!
//! A worker drains everything queued before driving anything, then drives each region it touched
//! once. That is the same rule the per-region thread followed — one `Ready` covers a batch of
//! messages rather than one apiece — now applied across the regions a worker holds, which is where
//! a leader's per-tick batching comes from (`docs/DESIGN.md` §6).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use esker_proto::ProtoError;
use tokio::sync::mpsc;

use crate::error::{Result, StoreError};
use crate::peer::{PeerCore, PeerMsg};

/// How many driver threads a store runs by default (`docs/DESIGN.md` §14).
///
/// Four rather than one per core: the work is `fsync`-bound rather than CPU-bound, so what the
/// number buys is *independence between regions* and not throughput. A store that measures a
/// bottleneck here raises it; nothing about correctness moves.
pub const DRIVER_WORKERS: usize = 4;

/// How many jobs may queue for one worker before a caller waits.
///
/// Bounded like everything else that crosses a thread here: an unbounded queue in front of an
/// `fsync` is a memory leak with extra steps (`docs/DESIGN.md` §9).
pub const WORKER_QUEUE_DEPTH: usize = crate::peer::PEER_QUEUE_DEPTH;

/// What a worker is asked to do.
enum Job {
    /// Take on a region, and drive it from now on.
    Register { region_id: u64, core: Box<PeerCore> },
    /// One message for a region this worker holds.
    Deliver { region_id: u64, message: PeerMsg },
    /// Give up a region, failing whatever it still owes its callers.
    Retire {
        region_id: u64,
        /// Signalled once the region is gone, so a caller that is about to flush the database can
        /// know that nothing is still applying into it.
        done: std::sync::mpsc::SyncSender<()>,
    },
    /// End this worker's loop, failing every region it still holds.
    Stop,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Register { region_id, .. } => {
                write!(formatter, "Register({region_id})")
            }
            Self::Deliver { region_id, .. } => write!(formatter, "Deliver({region_id})"),
            Self::Retire { region_id, .. } => write!(formatter, "Retire({region_id})"),
            Self::Stop => formatter.write_str("Stop"),
        }
    }
}

/// A fixed set of driver threads, with every region pinned to one of them.
#[derive(Debug)]
pub struct DriverPool {
    workers: Vec<mpsc::Sender<Job>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl DriverPool {
    /// Starts `workers` threads, at least one.
    pub fn new(workers: usize) -> Result<Self> {
        let workers = workers.max(1);
        let mut senders = Vec::with_capacity(workers);
        let mut threads = Vec::with_capacity(workers);
        for index in 0..workers {
            let (sender, inbox) = mpsc::channel(WORKER_QUEUE_DEPTH);
            let thread = std::thread::Builder::new()
                .name(format!("raft-driver-{index}"))
                .spawn(move || run(inbox))
                .map_err(|error| {
                    StoreError::Bootstrap(format!("could not start a Raft driver thread: {error}"))
                })?;
            senders.push(sender);
            threads.push(thread);
        }
        Ok(Self {
            workers: senders,
            threads: Mutex::new(threads),
        })
    }

    /// How many workers this pool runs.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Which worker a region is pinned to. See the module docs for why it is a modulo.
    #[must_use]
    pub fn worker_of(&self, region_id: u64) -> usize {
        // The remainder is below the worker count, which is a `usize` already, so this cannot
        // truncate on any target.
        let count = self.workers.len() as u64;
        usize::try_from(region_id % count).unwrap_or(0)
    }

    /// Hands a region's core to the worker it is pinned to.
    pub(crate) fn register(&self, region_id: u64, core: Box<PeerCore>) -> Result<()> {
        self.workers[self.worker_of(region_id)]
            .try_send(Job::Register { region_id, core })
            .map_err(|_| {
                StoreError::Bootstrap(format!(
                    "the driver worker for region {region_id} would not take it"
                ))
            })
    }

    /// Queues one message for a region.
    pub(crate) async fn deliver(
        &self,
        region_id: u64,
        message: PeerMsg,
    ) -> std::result::Result<(), ProtoError> {
        self.workers[self.worker_of(region_id)]
            .send(Job::Deliver { region_id, message })
            .await
            .map_err(|_| ProtoError::not_sent("the Raft driver is not running"))
    }

    /// Gives up a region and waits until its worker has let go of it.
    ///
    /// Waiting matters: a caller that retires a region is usually about to flush or drop the
    /// database, and a worker still holding the core would be applying into it. The wait is
    /// bounded, because a wedged worker must not be able to hold a shutdown open for ever — a
    /// timeout here means the process is going down anyway.
    pub(crate) fn retire(&self, region_id: u64) {
        let (done, waiter) = std::sync::mpsc::sync_channel(1);
        let sender = &self.workers[self.worker_of(region_id)];
        if sender.try_send(Job::Retire { region_id, done }).is_err() {
            return;
        }
        let _ = waiter.recv_timeout(RETIRE_TIMEOUT);
    }

    /// Stops every worker and waits for them, failing whatever is outstanding on every region.
    ///
    /// Taking the threads out first means a second call has nothing to join, so shutting down
    /// twice — which `Drop` after an explicit `stop` does — is a no-op rather than a panic.
    pub fn shutdown(&self) {
        let threads = self
            .threads
            .lock()
            .ok()
            .map(|mut threads| std::mem::take(&mut *threads));
        let Some(threads) = threads else {
            return;
        };
        for sender in &self.workers {
            // A full queue on shutdown must not deadlock the caller: the worker is going away
            // either way, and a closed channel ends its loop just as well.
            let _ = sender.try_send(Job::Stop);
        }
        for thread in threads {
            let _ = thread.join();
        }
    }
}

/// How long [`DriverPool::retire`] waits for a worker to let go of a region.
const RETIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One worker: hold some regions, drive the ones that were touched.
fn run(mut inbox: mpsc::Receiver<Job>) {
    let mut cores: BTreeMap<u64, PeerCore> = BTreeMap::new();

    while let Some(job) = inbox.blocking_recv() {
        // Everything queued travels together, then each region that was touched is driven once.
        // That is the per-region thread's rule, applied across the regions this worker holds.
        let mut touched = BTreeSet::new();
        let mut running = handle(&mut cores, &mut touched, job);
        while running {
            match inbox.try_recv() {
                Ok(next) => running = handle(&mut cores, &mut touched, next),
                Err(_) => break,
            }
        }

        for region_id in touched {
            let Some(core) = cores.get_mut(&region_id) else {
                continue;
            };
            if let Err(error) = core.drive() {
                // A failed write is not something this layer can paper over: the log and the state
                // machine may now disagree. The region is dropped — loudly — and the worker keeps
                // serving the others, because one region's disk is not another's.
                tracing::error!(region_id, %error, "the Raft driver failed for a region");
                if let Some(mut core) = cores.remove(&region_id) {
                    core.fail_outstanding(&ProtoError::internal(
                        "this region's Raft driver stopped",
                    ));
                }
            }
        }
        if !running {
            break;
        }
    }

    let stopping = ProtoError::not_sent("the Raft driver stopped");
    for core in cores.values_mut() {
        core.fail_outstanding(&stopping);
    }
}

/// Applies one job. Returns `false` when a region asked its worker to stop, which only
/// [`PeerMsg::Stop`] does and which now retires that region rather than the whole worker.
fn handle(cores: &mut BTreeMap<u64, PeerCore>, touched: &mut BTreeSet<u64>, job: Job) -> bool {
    match job {
        Job::Register { region_id, core } => {
            cores.insert(region_id, *core);
            touched.insert(region_id);
        }
        Job::Deliver { region_id, message } => {
            let Some(core) = cores.get_mut(&region_id) else {
                // A message for a region this worker has already let go of. Dropping it is right:
                // whoever sent it holds a handle that is on its way out too.
                tracing::debug!(region_id, "a driver job arrived for a region that is gone");
                return true;
            };
            if !core.handle(message) {
                // `Stop`. The region goes; the worker stays, because it holds others.
                if let Some(mut core) = cores.remove(&region_id) {
                    core.fail_outstanding(&ProtoError::not_sent("the Raft peer stopped"));
                }
                touched.remove(&region_id);
                return true;
            }
            touched.insert(region_id);
        }
        Job::Stop => return false,
        Job::Retire { region_id, done } => {
            if let Some(mut core) = cores.remove(&region_id) {
                core.fail_outstanding(&ProtoError::not_sent("the Raft peer stopped"));
            }
            touched.remove(&region_id);
            // Only after the core is gone, so a caller that was about to flush knows nothing is
            // still applying into the database.
            let _ = done.send(());
        }
    }
    true
}

impl Drop for DriverPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{DRIVER_WORKERS, DriverPool};

    /// Pinning is by modulo, so consecutive region ids land on consecutive workers and the
    /// mapping is the same on every open — a region that moved workers between two opens would be
    /// one whose ordering guarantee spanned two threads.
    #[test]
    fn regions_are_pinned_to_workers_evenly_and_stably() {
        let pool = DriverPool::new(4).unwrap();
        assert_eq!(pool.workers(), 4);
        for id in 0..100u64 {
            assert_eq!(pool.worker_of(id), (id % 4) as usize);
            assert_eq!(pool.worker_of(id), pool.worker_of(id), "not stable");
        }

        // Every worker gets a quarter of a hundred consecutive ids: exactly even, which is what a
        // hash would have given up for nothing.
        let mut counts = [0usize; 4];
        for id in 1..=100u64 {
            counts[pool.worker_of(id)] += 1;
        }
        assert_eq!(counts, [25, 25, 25, 25]);
        pool.shutdown();
    }

    /// A pool of one is legal and is what a single-region test wants; a pool of zero is not a pool
    /// and is rounded up rather than refused, because nothing useful comes of failing an open over
    /// a configuration typo.
    #[test]
    fn a_pool_always_has_at_least_one_worker() {
        for asked in [0, 1] {
            let pool = DriverPool::new(asked).unwrap();
            assert_eq!(pool.workers(), 1);
            assert_eq!(pool.worker_of(7), 0);
            pool.shutdown();
        }
        assert!(DRIVER_WORKERS >= 2, "the default must actually be a pool");
    }

    #[test]
    fn shutting_down_twice_is_not_a_panic() {
        let pool = DriverPool::new(2).unwrap();
        pool.shutdown();
        pool.shutdown();
    }
}
