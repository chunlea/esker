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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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

/// Which registration a region's core belongs to.
///
/// **A region id names a place; this names an occupant.** Two peers of one region on one store
/// can exist for a moment — one being replaced by a snapshot, one built by a caller that has not
/// yet been told it may not host the region — and every operation that says "this region" has to
/// say *which* of them it meant, or it acts on the wrong one. Monotonic per pool, never reused.
pub(crate) type Token = u64;

/// What a worker is asked to do.
enum Job {
    /// Take on a region, and drive it from now on.
    Register {
        region_id: u64,
        token: Token,
        core: Box<PeerCore>,
    },
    /// One message for a region this worker holds.
    Deliver { region_id: u64, message: PeerMsg },
    /// Give up a region, failing whatever it still owes its callers.
    Retire {
        region_id: u64,
        /// **The registration being retired, not just the region.** A handle that has already
        /// been superseded must not be able to stop the core that took its place.
        token: Token,
        /// Signalled once the region is gone, so a caller that is about to flush the database can
        /// know that nothing is still applying into it.
        done: std::sync::mpsc::SyncSender<()>,
    },
    /// End this worker's loop, failing every region it still holds.
    Stop,
}

impl Job {
    /// Whether this job advances a region's clock, which is the only kind a batch counts.
    ///
    /// A batch of appends or reads is exactly what batching is *for*; it is ticks that must not
    /// pile up ([ADR 0101](../../../docs/adr/0101-a-batch-of-ticks-never-carries-a-whole-election.md)).
    fn is_a_tick(&self) -> bool {
        matches!(
            self,
            Self::Deliver {
                message: PeerMsg::Tick,
                ..
            }
        )
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Register {
                region_id, token, ..
            } => {
                write!(formatter, "Register({region_id}#{token})")
            }
            Self::Deliver { region_id, .. } => write!(formatter, "Deliver({region_id})"),
            Self::Retire {
                region_id, token, ..
            } => write!(formatter, "Retire({region_id}#{token})"),
            Self::Stop => formatter.write_str("Stop"),
        }
    }
}

/// A fixed set of driver threads, with every region pinned to one of them.
#[derive(Debug)]
pub struct DriverPool {
    workers: Vec<mpsc::Sender<Job>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Set once, by [`DriverPool::shutdown`], and read by every worker each time it wakes.
    ///
    /// **This is what makes the shutdown terminate.** `Job::Stop` is delivered with `try_send`,
    /// which loses to a full queue — and a `Stop` dropped there used to be a worker that parked
    /// in `blocking_recv` for ever while `shutdown` blocked in `join` waiting for it. Observed
    /// under saturation: a test process wedged for over an hour, one worker exited on its
    /// `Stop`, the other parked, and a tokio worker thread sat in
    /// `Arc<DriverPool>::drop -> shutdown -> JoinHandle::join` behind it
    /// (`docs/plans/debt-c1.md` section 5).
    ///
    /// The flag closes the gap without a lock on the send path. It is set *before* the `Stop` is
    /// offered, so the two cases are exhaustive: either the queue had room and the `Stop` landed,
    /// or it was full — which means a job is pending, which means the worker wakes, and the first
    /// thing it does on waking is read this.
    stopping: Arc<AtomicBool>,
    /// **Which registration is driving each region, decided here rather than in a worker.**
    ///
    /// `Job::Register` used to be a `BTreeMap::insert` inside the worker: it replaced whatever
    /// core was there and said nothing, so a caller that built a peer and was then refused the
    /// region left the store answering from a handle nobody published into while another core
    /// answered every message ([ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md)).
    /// The claim is taken here, before the job is even queued, because the answer has to be
    /// synchronous: `adopt_split` registers a child from the **parent's driver thread**, which may
    /// be the same worker, so waiting for a worker to answer would be waiting for itself.
    registered: Mutex<BTreeMap<u64, Token>>,
    /// Hands out [`Token`]s. Monotonic, never reused, so a stale retire names nothing.
    next_token: std::sync::atomic::AtomicU64,
}

impl DriverPool {
    /// Starts `workers` threads, at least one.
    pub fn new(workers: usize) -> Result<Self> {
        let workers = workers.max(1);
        let mut senders = Vec::with_capacity(workers);
        let mut threads = Vec::with_capacity(workers);
        let stopping = Arc::new(AtomicBool::new(false));
        for index in 0..workers {
            let (sender, inbox) = mpsc::channel(WORKER_QUEUE_DEPTH);
            let stopping = Arc::clone(&stopping);
            let thread = std::thread::Builder::new()
                .name(format!("raft-driver-{index}"))
                .spawn(move || run(inbox, &stopping))
                .map_err(|error| {
                    StoreError::Bootstrap(format!("could not start a Raft driver thread: {error}"))
                })?;
            senders.push(sender);
            threads.push(thread);
        }
        Ok(Self {
            workers: senders,
            threads: Mutex::new(threads),
            stopping,
            registered: Mutex::new(BTreeMap::new()),
            next_token: std::sync::atomic::AtomicU64::new(1),
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
    /// Hands a region's core to the worker it is pinned to, **claiming the region first**.
    ///
    /// Refuses a region this pool is already driving, and the refusal is the point: registering is
    /// a claim on a place, not an overwrite of whoever is in it. The token it returns is the
    /// claim's identity — [`DriverPool::retire`] and [`DriverPool::abandon`] both take it, so a
    /// superseded handle can only ever retire itself.
    pub(crate) fn register(&self, region_id: u64, core: Box<PeerCore>) -> Result<Token> {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = registered.get(&region_id) {
            return Err(StoreError::RegionConflict(format!(
                "region {region_id} is already being driven by registration {held}; a store                  drives one core per region"
            )));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.workers[self.worker_of(region_id)]
            .try_send(Job::Register {
                region_id,
                token,
                core,
            })
            .map_err(|_| {
                StoreError::Bootstrap(format!(
                    "the driver worker for region {region_id} would not take it"
                ))
            })?;
        registered.insert(region_id, token);
        Ok(token)
    }

    /// Whether this pool is driving a core for `region_id`.
    ///
    /// The observable the refusal paths are tested against: a caller that was told it may not host
    /// a region must leave nothing behind driving it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn driving(&self, region_id: u64) -> bool {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&region_id)
    }

    /// Releases the claim if `token` still holds it, and says whether it did.
    fn release(&self, region_id: u64, token: Token) -> bool {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registered.get(&region_id) != Some(&token) {
            tracing::debug!(
                region_id,
                token,
                "a superseded registration asked to retire a region it no longer drives"
            );
            return false;
        }
        registered.remove(&region_id);
        true
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
    pub(crate) fn retire(&self, region_id: u64, token: Token) {
        if !self.release(region_id, token) {
            return;
        }
        let (done, waiter) = std::sync::mpsc::sync_channel(1);
        let sender = &self.workers[self.worker_of(region_id)];
        if sender
            .try_send(Job::Retire {
                region_id,
                token,
                done,
            })
            .is_err()
        {
            return;
        }
        let _ = waiter.recv_timeout(RETIRE_TIMEOUT);
    }

    /// Gives a region up **without waiting**, for a peer that was never handed to the store.
    ///
    /// The waiting version exists because a caller that retires a region is usually about to flush
    /// or drop the database. A peer whose reservation was never committed wrote nothing anybody is
    /// about to read, and its canceller may be a **driver thread** — `adopt_split` runs on the
    /// parent's, and the child can be pinned to the same worker, so waiting there would be a
    /// thread waiting for itself.
    pub(crate) fn abandon(&self, region_id: u64, token: Token) {
        if !self.release(region_id, token) {
            return;
        }
        let (done, _waiter) = std::sync::mpsc::sync_channel(1);
        if self.workers[self.worker_of(region_id)]
            .try_send(Job::Retire {
                region_id,
                token,
                done,
            })
            .is_err()
        {
            tracing::error!(
                region_id,
                token,
                "a peer that was never hosted could not be given back to its worker"
            );
        }
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
        // Before the `Stop`s, never after: a worker that wakes to drain a full queue must find
        // this already true. See [`DriverPool::stopping`].
        self.stopping.store(true, Ordering::Release);
        for sender in &self.workers {
            // Best-effort, and that is now safe: a full queue means the worker has a job waiting,
            // so it wakes and reads the flag instead of taking this message.
            let _ = sender.try_send(Job::Stop);
        }
        for thread in threads {
            let _ = thread.join();
        }
    }
}

/// **How many ticks one batch may carry before the worker drives.**
///
/// [ADR 0101](../../../docs/adr/0101-a-batch-of-ticks-never-carries-a-whole-election.md). A batch
/// as wide as the election timeout's own randomisation covers **every draw in it**, so every peer
/// crosses its threshold inside one batch and campaigns in lockstep however carefully it
/// randomised — the vote splits, the pre-vote round ends with nobody, and the next batch does it
/// again. Driving between chunks is what lets the first peer to time out be *heard* before the
/// others fire, which is the whole of what the randomisation is for.
///
/// One below `esker_raft::ELECTION_TIMEOUT_MIN_TICKS`, because the property wanted is that a batch
/// cannot carry a whole election timeout — not that it carries some particular number.
const TICKS_PER_BATCH: u64 = esker_raft::ELECTION_TIMEOUT_MIN_TICKS - 1;

/// Whether a batch that has already carried `ticks` must stop and drive.
///
/// Pure, so the rule can be asserted without a cluster: the arrangement it prevents is measured in
/// `esker-raft`'s `a_batch_of_ticks_flattens_the_randomised_timeout` and on a real cluster in
/// `docs/plans/debts-v1.1.md` #40.
#[must_use]
fn a_whole_election_would_fit(ticks: u64) -> bool {
    ticks >= TICKS_PER_BATCH
}

/// How long [`DriverPool::retire`] waits for a worker to let go of a region.
const RETIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A region's core and the registration it arrived under.
struct Held {
    token: Token,
    core: PeerCore,
}

/// One worker: hold some regions, drive the ones that were touched.
fn run(mut inbox: mpsc::Receiver<Job>, stopping: &AtomicBool) {
    let mut cores: BTreeMap<u64, Held> = BTreeMap::new();

    while let Some(job) = inbox.blocking_recv() {
        // Read on every wake, before the job is even looked at. A worker that woke to a queue
        // too full to have taken a `Stop` finds out here that it is going away, which is the
        // half of the shutdown `try_send` cannot deliver ([`DriverPool::stopping`]). Whatever is
        // still queued is dropped: the pool is being torn down, and every caller waiting on one
        // of those jobs is answered by the `fail_outstanding` below.
        if stopping.load(Ordering::Acquire) {
            break;
        }
        // Everything queued travels together, then each region that was touched is driven once.
        // That is the per-region thread's rule, applied across the regions this worker holds.
        let mut touched = BTreeSet::new();
        let mut ticks = u64::from(job.is_a_tick());
        let mut running = handle(&mut cores, &mut touched, job);
        while running && !a_whole_election_would_fit(ticks) {
            match inbox.try_recv() {
                Ok(next) => {
                    ticks += u64::from(next.is_a_tick());
                    running = handle(&mut cores, &mut touched, next);
                }
                Err(_) => break,
            }
        }

        for region_id in touched {
            let Some(held) = cores.get_mut(&region_id) else {
                continue;
            };
            if let Err(error) = held.core.drive() {
                // A failed write is not something this layer can paper over: the log and the state
                // machine may now disagree. The region is dropped — loudly — and the worker keeps
                // serving the others, because one region's disk is not another's.
                tracing::error!(region_id, %error, "the Raft driver failed for a region");
                if let Some(mut held) = cores.remove(&region_id) {
                    held.core
                        .fail_outstanding("this region's Raft driver stopped");
                }
            }
        }
        if !running {
            break;
        }
    }

    for held in cores.values_mut() {
        held.core.fail_outstanding("the Raft driver stopped");
    }
}

/// Applies one job. Returns `false` when a region asked its worker to stop, which only
/// [`PeerMsg::Stop`] does and which now retires that region rather than the whole worker.
fn handle(cores: &mut BTreeMap<u64, Held>, touched: &mut BTreeSet<u64>, job: Job) -> bool {
    match job {
        Job::Register {
            region_id,
            token,
            core,
        } => {
            // The pool takes the claim before it queues this, so an occupied slot here is a lost
            // `Retire` and not a second host — loud, because it is the state
            // [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md) exists to make
            // impossible, and because the core going out of scope below owed its callers answers.
            if let Some(mut displaced) = cores.insert(region_id, Held { token, core: *core }) {
                tracing::error!(
                    region_id,
                    displaced = displaced.token,
                    token,
                    "a registration displaced one this worker still held"
                );
                displaced
                    .core
                    .fail_outstanding("this region's core was displaced");
            }
            touched.insert(region_id);
        }
        Job::Deliver { region_id, message } => {
            let Some(held) = cores.get_mut(&region_id) else {
                // A message for a region this worker has already let go of. Dropping it is right:
                // whoever sent it holds a handle that is on its way out too.
                tracing::debug!(region_id, "a driver job arrived for a region that is gone");
                return true;
            };
            if !held.core.handle(message) {
                // `Stop`. The region goes; the worker stays, because it holds others.
                if let Some(mut held) = cores.remove(&region_id) {
                    held.core.fail_outstanding("the Raft peer stopped");
                }
                touched.remove(&region_id);
                return true;
            }
            touched.insert(region_id);
        }
        Job::Stop => return false,
        Job::Retire {
            region_id,
            token,
            done,
        } => {
            // **Only the registration that asked.** A retire that names a token this worker no
            // longer holds is a handle that was superseded catching up with itself, and acting on
            // it would stop the core that took its place.
            if cores
                .get(&region_id)
                .is_some_and(|held| held.token == token)
            {
                if let Some(mut held) = cores.remove(&region_id) {
                    held.core.fail_outstanding("the Raft peer stopped");
                }
                touched.remove(&region_id);
            }
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
    use super::{
        DRIVER_WORKERS, DriverPool, Job, PeerMsg, TICKS_PER_BATCH, a_whole_election_would_fit,
        mpsc, run,
    };

    /// **A batch stops before it can carry a whole election timeout**
    /// ([ADR 0101](../../../docs/adr/0101-a-batch-of-ticks-never-carries-a-whole-election.md)).
    ///
    /// The randomisation that keeps two peers from campaigning together draws from
    /// `ELECTION_TIMEOUT_MIN_TICKS..=ELECTION_TIMEOUT_MAX_TICKS`. A batch that carries the floor
    /// covers the bottom of that range for **every** peer at once, so they all time out inside it
    /// and the draw buys nothing — measured as a livelock in `esker-raft`'s
    /// `a_batch_of_ticks_flattens_the_randomised_timeout` and on a real cluster as
    /// `docs/plans/debts-v1.1.md` #40, three voters `PreCandidate` with the term climbing.
    ///
    /// The rule is therefore about the floor and not about a tuned number: below it, no peer can
    /// have timed out on this batch's ticks alone.
    #[test]
    fn a_batch_never_carries_a_whole_election_timeout() {
        assert!(
            TICKS_PER_BATCH < esker_raft::ELECTION_TIMEOUT_MIN_TICKS,
            "a batch of {TICKS_PER_BATCH} ticks can carry a whole election timeout of {}",
            esker_raft::ELECTION_TIMEOUT_MIN_TICKS
        );
        assert!(
            !a_whole_election_would_fit(0),
            "an empty batch drives nothing"
        );
        assert!(!a_whole_election_would_fit(TICKS_PER_BATCH - 1));
        assert!(
            a_whole_election_would_fit(TICKS_PER_BATCH),
            "the batch must stop at the floor, not past it"
        );
    }

    /// A tick is the only job a batch counts: batching appends and reads is what batching is for.
    #[test]
    fn only_a_tick_counts_against_the_batch() {
        let (notify, _answer) = tokio::sync::oneshot::channel();
        assert!(
            Job::Deliver {
                region_id: 1,
                message: PeerMsg::Tick,
            }
            .is_a_tick()
        );
        assert!(
            !Job::Deliver {
                region_id: 1,
                message: PeerMsg::Status(notify),
            }
            .is_a_tick()
        );
    }

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

    /// **A worker stops even when no `Job::Stop` could be queued for it.**
    ///
    /// `Stop` goes out with `try_send`, which fails on a full queue. That failure used to be
    /// swallowed: the worker never got the `Stop`, the sender was still alive inside the pool so
    /// the channel never closed, `blocking_recv` parked for ever, and `shutdown` blocked in
    /// `join` behind it.
    ///
    /// Found by accident under saturation, not by looking: a `balance` test process wedged for
    /// over an hour with `raft-driver-1` exited on its `Stop`, `raft-driver-0` parked in
    /// `blocking_recv`, and a tokio worker thread stuck in
    /// `Arc<DriverPool>::drop -> DriverPool::shutdown -> JoinHandle::join`. It is a production
    /// hang and not only a test one — that is the path a `Store` shutdown takes
    /// (`docs/plans/debt-c1.md` section 5).
    ///
    /// Driven at `run` rather than through the pool, because the condition is "a job was in the
    /// queue ahead of the `Stop`" and one job in a channel of one is that condition exactly —
    /// deterministic, where filling a 4096-deep queue against a draining worker is a race. **The
    /// sender is deliberately kept alive**: a live sender is what stops the channel closing, and
    /// a channel that never closes is what left the old worker parked.
    ///
    /// The assertion is on *termination*, so it is run on a thread with a deadline: without the
    /// fix this hangs rather than fails, and a hanging test tells CI nothing.
    #[test]
    fn a_worker_stops_on_the_flag_when_no_stop_could_be_queued() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, mpsc as std_mpsc};
        use std::time::Duration;

        let (sender, inbox) = mpsc::channel(1);
        let stopping = Arc::new(AtomicBool::new(false));

        // The job that is in the way. A region this worker does not hold, which `handle` logs
        // and ignores — what matters is that it occupies the queue, not what it is.
        sender
            .try_send(Job::Deliver {
                region_id: 7,
                message: PeerMsg::Tick,
            })
            .expect("the channel has room for exactly this");
        // Now the queue is full, so a `Stop` could not be queued. This is what `shutdown` does
        // before it offers one.
        stopping.store(true, Ordering::Release);

        let flag = Arc::clone(&stopping);
        let (done, finished) = std_mpsc::channel();
        std::thread::spawn(move || {
            run(inbox, &flag);
            let _ = done.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(30)).is_ok(),
            "the worker never returned: a `Stop` that could not be queued left it parked in \
             `blocking_recv`, and `shutdown` would wait for it in `join` for ever"
        );
        drop(sender);
    }
}
