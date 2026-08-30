//! The placement driver itself: one process, one database, six operations.
//!
//! Everything here is **synchronous** and takes no runtime. `PdService` ([`crate::service`]) is
//! the only async code in the crate, and it exists to hand these calls to `spawn_blocking` so
//! that an fsync never stalls the reactor — the same shape `esker-store` uses, and the same
//! rule: async at the network edge and nowhere else (`CLAUDE.md`).
//!
//! # What is durable, and when
//!
//! Every write below goes out with `sync = true`, so PD acknowledges nothing it has not made
//! durable (`CLAUDE.md` invariant 1). The engine is opened with `WalSyncMode::Never`, which
//! means the engine adds no fsync of its own and each write's own flag decides — the only
//! setting under which that invariant is true as written (`docs/DESIGN.md` §14).
//!
//! Two of these writes are ordered against the answer rather than merely present in it, and
//! they are what the crash tests exist for: the allocator's batch end
//! ([`crate::alloc`]) and the oracle's high-water mark ([`crate::tso`]) are both persisted
//! *before* the value they cover leaves the process.
//!
//! # Where the scheduling lives
//!
//! Replica repair — observing an operator, retiring it, issuing the next one — is
//! [`repair`], a child module. It is a child rather than a sibling because it reaches into
//! this one's private state on every heartbeat, and because the two halves are one lock's
//! worth of work: the file is split for reading, not for isolation.
//!
//! # One lock
//!
//! Allocation, timestamp issue and the epoch-guarded upsert are all read-modify-write over
//! state that must not interleave, so they share one mutex. Lookups do not take it: they read
//! the engine, which is concurrent, and a lookup racing an upsert is exactly the staleness the
//! design already tolerates (`docs/DESIGN.md` §7).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use esker_engine::{
    Db, LocalFileSystem, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions, cf,
};
use esker_proto::{Operator, Region, StoreInfo};

use crate::alloc::{ALLOC_BATCH, Allocator};
use crate::clock::{Clock, SystemClock};
use crate::error::{PdError, Result};
use crate::keys;
use crate::operator::InFlight;
use crate::record::{AllocRecord, ClusterRecord, RegionRecord, StoreRecord, TsoRecord};
use crate::routing::{self, RegionBeat, StoreBeat, Upsert};
use crate::schedule;
use crate::tso::Oracle;

mod repair;

/// How long a store may be silent before it is considered down (`docs/DESIGN.md` §7).
pub const MAX_STORE_DOWN_TIME_MS: u64 = 30_000;

/// How long an operator may make no observable progress before PD gives up on it.
///
/// Generous on purpose. The slow part of an `AddPeer` is catching the new replica up from a
/// snapshot, and cancelling a transfer that is working throws the work away and starts again
/// with a fresh peer id. The clock runs from the last observed progress rather than from the
/// issue ([`crate::operator`]), so this bounds *being stuck*, not taking long.
pub const OPERATOR_TIMEOUT_MS: u64 = 300_000;

/// How PD is opened.
#[derive(Debug, Clone)]
pub struct PdOptions {
    /// The engine underneath.
    pub engine: Options,
    /// Where physical time comes from. The one wall clock in the system
    /// (`CLAUDE.md` invariant 6, [`crate::clock`]).
    pub clock: Arc<dyn Clock>,
    /// Ids reserved per persist.
    pub alloc_batch: u64,
    /// How far ahead of the clock the oracle's mark is persisted.
    pub tso_save_interval_ms: u64,
    /// How long a store may be silent before it is down, and its regions are repaired.
    pub max_store_down_time_ms: u64,
    /// How long an operator may make no observable progress before it is abandoned.
    pub operator_timeout_ms: u64,
    /// Replicas a region should have. Repair restores this; it does not grow past it.
    pub target_replicas: usize,
}

impl PdOptions {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Options {
                create_if_missing: true,
                // The engine adds no fsync of its own; every write below asks for one. See the
                // module docs.
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            clock: Arc::new(SystemClock),
            alloc_batch: ALLOC_BATCH,
            tso_save_interval_ms: crate::TSO_SAVE_INTERVAL_MS,
            max_store_down_time_ms: MAX_STORE_DOWN_TIME_MS,
            operator_timeout_ms: OPERATOR_TIMEOUT_MS,
            target_replicas: schedule::TARGET_REPLICAS,
        }
    }

    /// The defaults, with `clock` in place of the system clock.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            ..Self::new()
        }
    }
}

impl Default for PdOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// What a `Bootstrap` call answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrapped {
    /// The cluster's id, minted by whichever store bootstrapped it.
    pub cluster_id: u64,
    /// The region this store is to create, when this call is the one that bootstrapped the
    /// cluster; `None` when the cluster already existed.
    ///
    /// The distinction is the whole answer: exactly one store in the life of a cluster is told
    /// to create region 1, and every other call — including this store's own next restart —
    /// gets `None` and looks to its own disk for the regions it hosts
    /// (`docs/plans/phase-4.md` §5).
    pub region: Option<Region>,
}

/// Where a key lives, and who PD believes leads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionRoute {
    /// The region covering the key.
    pub region: Region,
    /// The peer PD last heard was leading it, if any.
    pub leader_peer_id: Option<u64>,
    /// The stores hosting the region's peers, in peer order.
    ///
    /// A client addresses a store by id and "resolving one to a socket is PD's job"
    /// (`docs/DESIGN.md` §10); sending them with the region saves the round trip that asking
    /// separately would cost. A peer whose store PD has never heard of is absent from this
    /// list rather than present with an empty address.
    pub stores: Vec<StoreInfo>,
}

/// What one region heartbeat did, and what PD wants back from that region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beat {
    /// Whether the heartbeat updated the routing table or was dropped as stale.
    pub upsert: Upsert,
    /// The membership change PD wants this region's leader to propose, if any.
    ///
    /// At most one per region, ever, and the *same* one comes back on every heartbeat until a
    /// heartbeat shows it happened or it is abandoned (`docs/DESIGN.md` §7). A leader that
    /// ignores it loses nothing but time.
    pub operator: Option<Operator>,
}

/// The placement driver.
#[derive(Debug)]
pub struct Pd {
    db: Arc<Db>,
    cf: u32,
    clock: Arc<dyn Clock>,
    max_store_down_time_ms: u64,
    operator_timeout_ms: u64,
    target_replicas: usize,
    state: Mutex<State>,
}

/// The part that must not interleave.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) cluster: Option<ClusterRecord>,
    pub(crate) alloc: Allocator,
    pub(crate) oracle: Oracle,
    /// The operator in flight for each region, keyed by region id — which is what makes
    /// "never two operators for one region" (`docs/DESIGN.md` §7) a property of the type
    /// rather than a rule someone has to remember.
    ///
    /// **Not persisted.** A restart forgets every operator and re-derives what is needed from
    /// the next round of heartbeats ([`crate::schedule`]); persisting them would mean
    /// reconciling a remembered plan with a cluster that moved on while PD was down, which is
    /// strictly harder than recomputing.
    pub(crate) in_flight: BTreeMap<u64, InFlight>,
}

impl Pd {
    /// Opens, or creates, PD's database in `path`.
    ///
    /// A directory with no cluster record is a PD that has not been bootstrapped: it answers
    /// [`PdError::NotBootstrapped`] to everything except [`Pd::bootstrap`], rather than
    /// inventing a cluster of its own.
    pub fn open(path: impl AsRef<Path>, options: PdOptions) -> Result<Arc<Self>> {
        let db = Db::open_with(
            path,
            options.engine.clone(),
            Arc::new(LocalFileSystem::new()),
            &[cf::DEFAULT],
        )?;
        let cf = db
            .cf_id(cf::DEFAULT)
            .ok_or_else(|| PdError::internal("the default column family is missing after open"))?;
        let db = Arc::new(db);

        let cluster = read(&db, &keys::cluster_key())?
            .map(|bytes| ClusterRecord::decode(&bytes))
            .transpose()?;
        let alloc = read(&db, &keys::alloc_key())?
            .map(|bytes| AllocRecord::decode(&bytes))
            .transpose()?;
        let mark = read(&db, &keys::tso_key())?
            .map(|bytes| TsoRecord::decode(&bytes))
            .transpose()?;
        // `max(clock, mark)`, the restart rule, is inside `Oracle::load`.
        let oracle = Oracle::load(mark, options.clock.now_ms(), options.tso_save_interval_ms);

        Ok(Arc::new(Self {
            db,
            cf,
            clock: options.clock,
            max_store_down_time_ms: options.max_store_down_time_ms,
            operator_timeout_ms: options.operator_timeout_ms,
            target_replicas: options.target_replicas,
            state: Mutex::new(State {
                cluster,
                alloc: Allocator::load(alloc, options.alloc_batch),
                oracle,
                in_flight: BTreeMap::new(),
            }),
        }))
    }

    /// PD's database, for the inspector and the tests.
    #[must_use]
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// The clock this PD reads physical time from.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The cluster record, or `None` if nothing has bootstrapped yet.
    pub fn cluster(&self) -> Result<Option<ClusterRecord>> {
        Ok(self.lock()?.cluster)
    }

    /// The cluster's id, or [`PdError::NotBootstrapped`].
    pub fn cluster_id(&self) -> Result<u64> {
        self.lock()?
            .cluster
            .map(|cluster| cluster.cluster_id)
            .ok_or(PdError::NotBootstrapped)
    }

    /// Refuses a request meant for another cluster.
    ///
    /// Called once at the top of every method except `Bootstrap` — which cannot carry a
    /// cluster id, because asking for one is how a caller learns it. Two clusters sharing an
    /// address is a misconfiguration whose symptom, without this check, is one cluster quietly
    /// answering questions about the other's regions.
    pub fn check_cluster(&self, cluster_id: u64) -> Result<()> {
        let expected = self.cluster_id()?;
        if cluster_id != expected {
            return Err(PdError::ClusterMismatch {
                expected,
                actual: cluster_id,
            });
        }
        Ok(())
    }

    /// Registers a store, and creates the cluster if this is the first one.
    ///
    /// Idempotent, and meant to be called on **every** store start: the cluster id and the
    /// store's address are refreshed by it, and only the first call in the life of a cluster
    /// comes back with a region to create (`docs/DESIGN.md` §7, "the first store to register
    /// receives region 1").
    pub fn bootstrap(&self, store_id: u64, address: &str) -> Result<Bootstrapped> {
        if store_id == 0 {
            return Err(PdError::invalid("store id zero is not a store"));
        }
        let now_ms = self.clock.now_ms();
        let mut state = self.lock()?;

        if let Some(cluster) = state.cluster {
            let mut batch = WriteBatch::new();
            routing::stage_store(
                &mut batch,
                self.cf,
                &self.store_record(store_id, address, now_ms)?,
            );
            self.write(batch)?;
            return Ok(Bootstrapped {
                cluster_id: cluster.cluster_id,
                region: None,
            });
        }

        // Two ids in one reservation: the region and its first peer. The reservation is
        // persisted inside `allocate`, before either id is used for anything.
        let base = {
            let db = Arc::clone(&self.db);
            let cf = self.cf;
            state.alloc.allocate(2, |end| persist_alloc(&db, cf, end))?
        };
        let region = Region::bootstrap(base, store_id, base + 1);
        let cluster = ClusterRecord {
            cluster_id: mint_cluster_id(now_ms, store_id, address),
            first_region_id: region.id,
            created_ms: now_ms,
        };

        let mut batch = WriteBatch::new();
        batch.put(self.cf, &keys::cluster_key(), &cluster.encode());
        routing::stage_store(
            &mut batch,
            self.cf,
            &self.store_record(store_id, address, now_ms)?,
        );
        routing::stage_region(
            &mut batch,
            self.cf,
            &RegionRecord::new(region.clone(), now_ms),
            None,
        );
        self.write(batch)?;

        state.cluster = Some(cluster);
        tracing::info!(
            cluster_id = cluster.cluster_id,
            region_id = region.id,
            store_id,
            "cluster bootstrapped"
        );
        Ok(Bootstrapped {
            cluster_id: cluster.cluster_id,
            region: Some(region),
        })
    }

    /// The first of `count` consecutive cluster-unique ids.
    ///
    /// The batch they come from is persisted before any of them is returned, so a crash skips
    /// ids and never repeats one ([`crate::alloc`]).
    pub fn alloc_id(&self, count: u64) -> Result<u64> {
        let mut state = self.lock()?;
        let db = Arc::clone(&self.db);
        let cf = self.cf;
        state
            .alloc
            .allocate(count, |end| persist_alloc(&db, cf, end))
    }

    /// A run of `count` consecutive timestamps, starting at the returned one.
    ///
    /// The high-water mark covering them is durable before any of them leaves this call
    /// ([`crate::tso`]). This is the one place in Esker that reads a wall clock, and the one
    /// whose ordering every layer above depends on (`CLAUDE.md` invariant 6).
    pub fn tso(&self, count: u32) -> Result<u64> {
        let now_ms = self.clock.now_ms();
        let mut state = self.lock()?;
        let db = Arc::clone(&self.db);
        let cf = self.cf;
        state
            .oracle
            .allocate(count, now_ms, |mark| persist_tso(&db, cf, mark))
    }

    /// The oracle's high-water mark, for the inspector and the tests.
    pub fn tso_high_water_ms(&self) -> Result<u64> {
        Ok(self.lock()?.oracle.high_water_ms())
    }

    /// The region covering `key`, with the leader PD last heard about and the addresses of the
    /// stores its peers are on.
    ///
    /// `Ok(None)` means the cluster is bootstrapped and no region covers the key, which cannot
    /// happen while the table is a contiguous partition — the case exists so that a hole is
    /// reported rather than papered over. Before bootstrap the answer is
    /// [`PdError::NotBootstrapped`], never an empty one: "there is no cluster" and "no region
    /// owns this key" are different facts.
    pub fn get_region(&self, key: &[u8]) -> Result<Option<RegionRoute>> {
        let _ = self.cluster_id()?;
        let Some(record) = routing::lookup(&self.db, key)? else {
            return Ok(None);
        };
        let mut stores = Vec::with_capacity(record.region.peers.len());
        for peer in &record.region.peers {
            if let Some(store) = routing::read_store(&self.db, peer.store_id)? {
                stores.push(StoreInfo::new(store.store_id, store.address));
            }
        }
        Ok(Some(RegionRoute {
            region: record.region,
            leader_peer_id: (record.leader_peer_id != 0).then_some(record.leader_peer_id),
            stores,
        }))
    }

    /// Records a store's capacity and load, and refreshes its liveness.
    ///
    /// A heartbeat from a store PD has no record of is **refused**, not auto-registered:
    /// registration carries the store's address, and a store record with no address is one a
    /// client cannot be routed to. A store that gets this error should call
    /// [`Pd::bootstrap`], which is registration, and which it is meant to call on every start
    /// anyway.
    pub fn store_heartbeat(&self, beat: &StoreBeat) -> Result<()> {
        let now_ms = self.clock.now_ms();
        let _state = self.lock()?;
        let Some(existing) = routing::read_store(&self.db, beat.store_id)? else {
            return Err(PdError::invalid(format!(
                "store {} has not registered; call Bootstrap first",
                beat.store_id
            )));
        };
        let record = StoreRecord {
            stats: beat.stats,
            last_heartbeat_ms: now_ms,
            ..existing
        };
        let mut batch = WriteBatch::new();
        routing::stage_store(&mut batch, self.cf, &record);
        self.write(batch)
    }

    /// Records what a region's leader reports, unless PD already holds something newer.
    ///
    /// The guard is [`routing::accepts`]; a dropped heartbeat is [`Upsert::Stale`] rather than
    /// an error, because an out-of-order beat is a normal consequence of a leader change and
    /// not something the sender did wrong.
    pub fn region_heartbeat(&self, beat: &RegionBeat) -> Result<Beat> {
        if beat.region.id == 0 {
            return Err(PdError::invalid("region id zero is not a region"));
        }
        if !beat.region.end_key.is_empty() && beat.region.start_key >= beat.region.end_key {
            return Err(PdError::invalid(format!(
                "region {} has an empty or inverted range",
                beat.region.id
            )));
        }
        let now_ms = self.clock.now_ms();
        let mut state = self.lock()?;

        let previous = routing::read_region(&self.db, beat.region.id)?;
        let outdated = previous
            .as_ref()
            .is_some_and(|held| !routing::accepts(held, beat.region.epoch, beat.term));

        // The record PD holds after this beat: the new one, or the one it kept.
        let record = if outdated {
            tracing::debug!(
                region_id = beat.region.id,
                "dropping a heartbeat older than the record it would replace"
            );
            previous.clone().unwrap_or_else(|| unreachable_stale(beat))
        } else {
            let record = RegionRecord {
                region: beat.region.clone(),
                leader_peer_id: beat.leader_peer_id,
                term: beat.term,
                approximate_size: beat.approximate_size,
                applied_index: beat.applied_index,
                last_heartbeat_ms: now_ms,
            };
            let mut batch = WriteBatch::new();
            routing::stage_region(&mut batch, self.cf, &record, previous.as_ref());
            self.write(batch)?;
            record
        };

        // Scheduling happens here, on the heartbeat, and nowhere else. A store going down is
        // noticed by *absence*, so the trigger has to be somebody else's beat: repair latency
        // is therefore bounded by `max_store_down_time` plus one region-heartbeat interval,
        // and PD needs no timer thread to have it (`docs/DESIGN.md` §7, §14).
        let operator = self.schedule(&mut state, &record, now_ms)?;
        Ok(Beat {
            upsert: if outdated {
                Upsert::Stale
            } else {
                Upsert::Applied
            },
            operator,
        })
    }

    /// Every region PD knows about, in id order.
    pub fn regions(&self) -> Result<Vec<RegionRecord>> {
        routing::regions(&self.db)
    }

    /// Every store PD knows about, in id order.
    pub fn stores(&self) -> Result<Vec<StoreRecord>> {
        routing::stores(&self.db)
    }

    /// The stores that have not been heard from for longer than the configured limit.
    ///
    /// Exposed, not acted on: replica repair is 4c.
    pub fn down_stores(&self) -> Result<Vec<u64>> {
        let stores = self.stores()?;
        Ok(routing::down_stores(
            &stores,
            self.clock.now_ms(),
            self.max_store_down_time_ms,
        ))
    }

    /// The store record to write for a registering store, keeping the id it already had.
    fn store_record(&self, store_id: u64, address: &str, now_ms: u64) -> Result<StoreRecord> {
        let existing = routing::read_store(&self.db, store_id)?;
        Ok(StoreRecord {
            store_id,
            address: address.to_owned(),
            started_ms: now_ms,
            last_heartbeat_ms: now_ms,
            stats: existing.map(|store| store.stats).unwrap_or_default(),
        })
    }

    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| PdError::internal("pd state lock is poisoned"))
    }

    /// Every write PD makes is durable before it is acknowledged (invariant 1).
    pub(crate) fn write(&self, batch: WriteBatch) -> Result<()> {
        self.db.write(batch, &WriteOptions::synced())?;
        Ok(())
    }
}

/// A heartbeat can only be stale against a record that exists, so this is unreachable — and it
/// builds a record from the beat rather than panicking if the impossible happens
/// (`CLAUDE.md` invariant 9).
fn unreachable_stale(beat: &RegionBeat) -> RegionRecord {
    RegionRecord::new(beat.region.clone(), 0)
}

fn read(db: &Db, key: &[u8]) -> Result<Option<bytes::Bytes>> {
    Ok(db.get(cf::DEFAULT, key, &ReadOptions::default())?)
}

/// Makes the oracle's mark durable. Called *before* a timestamp at or above it is handed out.
fn persist_tso(db: &Db, cf: u32, high_water_ms: u64) -> Result<()> {
    let mut batch = WriteBatch::new();
    batch.put(cf, &keys::tso_key(), &TsoRecord { high_water_ms }.encode());
    db.write(batch, &WriteOptions::synced())?;
    Ok(())
}

/// Makes an id reservation durable. Called by the allocator *before* it hands out an id.
fn persist_alloc(db: &Db, cf: u32, allocated_end: u64) -> Result<()> {
    let mut batch = WriteBatch::new();
    batch.put(
        cf,
        &keys::alloc_key(),
        &AllocRecord { allocated_end }.encode(),
    );
    db.write(batch, &WriteOptions::synced())?;
    Ok(())
}

/// Mints a cluster id from the one-time facts of a bootstrap.
///
/// Its job is to catch a client — or a store — pointed at the wrong cluster, not to be
/// unguessable, so a mix of the bootstrap time, the first store and its address is enough. It
/// is deterministic in those three, which is what lets a test with a fixed clock assert an
/// exact id. Never zero: a zeroed record must not read as a valid cluster.
fn mint_cluster_id(now_ms: u64, store_id: u64, address: &str) -> u64 {
    let mixed = esker_base::hash::mix64(now_ms)
        ^ esker_base::hash::mix64(store_id)
        ^ esker_base::hash::hash64(address.as_bytes());
    if mixed == 0 { 1 } else { mixed }
}
