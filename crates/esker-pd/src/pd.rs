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
//! # One lock
//!
//! Allocation, timestamp issue and the epoch-guarded upsert are all read-modify-write over
//! state that must not interleave, so they share one mutex. Lookups do not take it: they read
//! the engine, which is concurrent, and a lookup racing an upsert is exactly the staleness the
//! design already tolerates (`docs/DESIGN.md` §7).

use std::path::Path;
use std::sync::{Arc, Mutex};

use esker_engine::{
    Db, LocalFileSystem, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions, cf,
};
use esker_proto::Region;

use crate::alloc::{ALLOC_BATCH, Allocator};
use crate::clock::{Clock, SystemClock};
use crate::error::{PdError, Result};
use crate::keys;
use crate::record::{AllocRecord, ClusterRecord, RegionRecord, StoreRecord, TsoRecord};
use crate::routing::{self, RegionBeat, StoreBeat, Upsert};
use crate::tso::Oracle;

/// How long a store may be silent before it is considered down (`docs/DESIGN.md` §7).
///
/// Recorded and exposed in 4a; acted on in 4c, which is where replica repair lives.
pub const MAX_STORE_DOWN_TIME_MS: u64 = 30_000;

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
    /// How long a store may be silent before [`Pd::down_stores`] reports it.
    pub max_store_down_time_ms: u64,
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
    /// The addresses of the stores hosting the region's peers, in peer order.
    ///
    /// A client addresses a store by id and "resolving one to a socket is PD's job"
    /// (`docs/DESIGN.md` §10); sending them with the region saves the round trip that asking
    /// separately would cost. A peer whose store PD has never heard of is absent from this
    /// list rather than present with an empty address.
    pub stores: Vec<(u64, String)>,
}

/// The placement driver.
#[derive(Debug)]
pub struct Pd {
    db: Arc<Db>,
    cf: u32,
    clock: Arc<dyn Clock>,
    max_store_down_time_ms: u64,
    state: Mutex<State>,
}

/// The part that must not interleave.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) cluster: Option<ClusterRecord>,
    pub(crate) alloc: Allocator,
    pub(crate) oracle: Oracle,
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
            state: Mutex::new(State {
                cluster,
                alloc: Allocator::load(alloc, options.alloc_batch),
                oracle,
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
                stores.push((store.store_id, store.address));
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
    pub fn region_heartbeat(&self, beat: &RegionBeat) -> Result<Upsert> {
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
        let _state = self.lock()?;

        let previous = routing::read_region(&self.db, beat.region.id)?;
        if let Some(previous) = &previous
            && !routing::accepts(previous, beat.region.epoch, beat.term)
        {
            tracing::debug!(
                region_id = beat.region.id,
                "dropping a heartbeat older than the record it would replace"
            );
            return Ok(Upsert::Stale);
        }

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
        Ok(Upsert::Applied)
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

#[cfg(test)]
mod tests {
    use super::{Pd, PdOptions, RegionBeat, StoreBeat, Upsert};
    use crate::Clock as _;
    use crate::clock::TestClock;
    use crate::error::PdError;
    use esker_proto::{Epoch, Peer, Region};
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Arc<TestClock>, Arc<Pd>) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>),
        )
        .unwrap();
        (dir, clock, pd)
    }

    #[test]
    fn a_fresh_pd_is_not_bootstrapped() {
        let (_dir, _clock, pd) = open();
        assert!(pd.cluster().unwrap().is_none());
        assert!(matches!(
            pd.cluster_id().unwrap_err(),
            PdError::NotBootstrapped
        ));
        assert!(matches!(
            pd.get_region(b"anything").unwrap_err(),
            PdError::NotBootstrapped
        ));
    }

    /// `docs/DESIGN.md` §7: the first store to register receives region 1 covering everything.
    #[test]
    fn the_first_store_receives_the_region_that_covers_everything() {
        let (_dir, _clock, pd) = open();
        let first = pd.bootstrap(1, "127.0.0.1:20160").unwrap();
        let region = first
            .region
            .expect("the first store bootstraps the cluster");

        assert_eq!(region.id, 1, "the first region is region 1");
        assert!(region.start_key.is_empty() && region.end_key.is_empty());
        assert_eq!(region.peers.len(), 1);
        assert_eq!(region.peers[0].store_id, 1);
        assert_ne!(region.peers[0].peer_id, region.id, "a peer is not a region");
        assert_eq!(region.epoch, Epoch::INITIAL);
        assert_ne!(first.cluster_id, 0);

        // And it is routable immediately, at both ends of the key space.
        for key in [&b""[..], b"m", b"\xff\xff\xff\xff"] {
            let route = pd
                .get_region(key)
                .unwrap()
                .expect("a region covers {key:?}");
            assert_eq!(route.region.id, region.id);
            assert_eq!(route.stores, vec![(1, "127.0.0.1:20160".to_owned())]);
            assert_eq!(route.leader_peer_id, None, "no heartbeat has arrived yet");
        }
    }

    /// A second call is a registration, not a second cluster: exactly one store in the life of
    /// a cluster is told to create region 1.
    #[test]
    fn bootstrap_is_idempotent_and_only_one_store_gets_a_region() {
        let (_dir, _clock, pd) = open();
        let first = pd.bootstrap(1, "127.0.0.1:20160").unwrap();

        let again = pd.bootstrap(1, "127.0.0.1:20160").unwrap();
        assert_eq!(again.cluster_id, first.cluster_id);
        assert_eq!(again.region, None);

        let second_store = pd.bootstrap(2, "127.0.0.1:20161").unwrap();
        assert_eq!(second_store.cluster_id, first.cluster_id);
        assert_eq!(second_store.region, None);

        assert_eq!(pd.regions().unwrap().len(), 1, "one region, one bootstrap");
        assert_eq!(pd.stores().unwrap().len(), 2, "both stores are registered");
    }

    /// A store that restarts at a new address is reachable at the new one. This is why
    /// `Bootstrap` is meant to be called on every start.
    #[test]
    fn re_registering_refreshes_the_address() {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "127.0.0.1:20160").unwrap();
        pd.bootstrap(1, "127.0.0.1:29999").unwrap();
        let route = pd.get_region(b"k").unwrap().unwrap();
        assert_eq!(route.stores, vec![(1, "127.0.0.1:29999".to_owned())]);
    }

    #[test]
    fn a_request_for_another_cluster_is_refused() {
        let (_dir, _clock, pd) = open();
        let cluster_id = pd.bootstrap(1, "a").unwrap().cluster_id;
        assert!(pd.check_cluster(cluster_id).is_ok());
        assert!(matches!(
            pd.check_cluster(cluster_id ^ 1).unwrap_err(),
            PdError::ClusterMismatch { .. }
        ));
    }

    #[test]
    fn ids_are_monotone_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);

        let mut handed_out = Vec::new();
        {
            let pd = Pd::open(dir.path(), options()).unwrap();
            pd.bootstrap(1, "a").unwrap();
            for _ in 0..5 {
                handed_out.push(pd.alloc_id(1).unwrap());
            }
            handed_out.push(pd.alloc_id(10).unwrap());
        }
        {
            let pd = Pd::open(dir.path(), options()).unwrap();
            let after = pd.alloc_id(1).unwrap();
            assert!(
                after > *handed_out.last().unwrap() + 9,
                "{after} is inside a batch the previous process had reserved"
            );
            handed_out.push(after);
        }

        let unique: std::collections::BTreeSet<u64> = handed_out.iter().copied().collect();
        assert_eq!(unique.len(), handed_out.len(), "an id was handed out twice");
        assert!(handed_out.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// The cluster id is minted once and never again: a restart must not look like a new
    /// cluster, or every store would be told it is talking to the wrong one.
    #[test]
    fn the_cluster_id_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);

        let minted = {
            let pd = Pd::open(dir.path(), options()).unwrap();
            pd.bootstrap(1, "a").unwrap().cluster_id
        };
        clock.advance(60_000);
        let pd = Pd::open(dir.path(), options()).unwrap();
        assert_eq!(pd.cluster_id().unwrap(), minted);
        assert_eq!(pd.bootstrap(1, "a").unwrap().cluster_id, minted);
        assert_eq!(pd.regions().unwrap().len(), 1);
    }

    /// The oracle's rule, end to end over a real database and a clock that goes backwards
    /// across the reopen: nothing repeats, and nothing goes down.
    #[test]
    fn timestamps_never_repeat_across_a_reopen_with_a_backwards_clock() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);

        let mut seen = Vec::new();
        {
            let pd = Pd::open(dir.path(), options()).unwrap();
            pd.bootstrap(1, "a").unwrap();
            for _ in 0..4 {
                seen.push(pd.tso(16).unwrap());
                clock.advance(1);
            }
        }

        // A day backwards, which is what an NTP correction on a badly-set machine looks like.
        clock.set(1_700_000_000_000 - 86_400_000);
        let pd = Pd::open(dir.path(), options()).unwrap();
        let after = pd.tso(1).unwrap();

        let highest = seen.iter().copied().max().unwrap();
        assert!(
            after > highest,
            "after the restart {after} is not above {highest} from before it"
        );
        let unique: std::collections::BTreeSet<u64> = seen.iter().copied().collect();
        assert_eq!(unique.len(), seen.len());
    }

    /// Every timestamp handed out is strictly below the mark on disk. This is the property the
    /// restart rule leans on; if it ever fails, a restart can repeat a timestamp.
    #[test]
    fn every_timestamp_is_below_the_persisted_mark() {
        let (_dir, clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        for step in 0..8 {
            let ts = pd.tso(4).unwrap();
            let (physical, _) = crate::decompose_ts(ts);
            let mark = pd.tso_high_water_ms().unwrap();
            assert!(
                physical < mark,
                "step {step}: {physical} is not below {mark}"
            );
            clock.advance(500);
        }
        // And the mark on disk is the one in memory, not one still in a buffer somewhere.
        let stored = crate::record::TsoRecord::decode(
            &pd.db()
                .get(
                    esker_engine::cf::DEFAULT,
                    &crate::keys::tso_key(),
                    &esker_engine::ReadOptions::default(),
                )
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored.high_water_ms, pd.tso_high_water_ms().unwrap());
    }

    fn beat(region: Region, leader: u64, term: u64) -> RegionBeat {
        RegionBeat {
            region,
            leader_peer_id: leader,
            term,
            approximate_size: 0,
            applied_index: 0,
        }
    }

    fn ranged(id: u64, start: &'static [u8], end: &'static [u8], epoch: (u64, u64)) -> Region {
        Region {
            id,
            start_key: bytes::Bytes::from_static(start),
            end_key: bytes::Bytes::from_static(end),
            peers: vec![Peer::voter(1, id * 10)],
            epoch: Epoch::new(epoch.0, epoch.1),
        }
    }

    /// The heartbeat that arrives second is not necessarily the one that happened second.
    /// Both orders are tested, because only one of them can be got right by accident.
    #[test]
    fn a_stale_heartbeat_never_overwrites_a_newer_epoch() {
        for reversed in [false, true] {
            let (_dir, _clock, pd) = open();
            pd.bootstrap(1, "a").unwrap();

            let old = ranged(1, b"", b"", (1, 1));
            let new = ranged(1, b"", b"", (1, 2));
            let (first, second) = if reversed {
                (beat(new, 20, 5), beat(old, 10, 4))
            } else {
                (beat(old, 10, 4), beat(new, 20, 5))
            };

            assert_eq!(pd.region_heartbeat(&first).unwrap(), Upsert::Applied);
            let outcome = pd.region_heartbeat(&second).unwrap();
            assert_eq!(
                outcome,
                if reversed {
                    Upsert::Stale
                } else {
                    Upsert::Applied
                },
                "arriving {}",
                if reversed { "newest first" } else { "in order" }
            );

            // Whichever order they arrived in, PD holds the newer epoch and its leader.
            let held = pd.regions().unwrap();
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].region.epoch, Epoch::new(1, 2));
            assert_eq!(held[0].leader_peer_id, 20);
        }
    }

    /// The counters move on different events, so a beat behind in *either* one is stale.
    #[test]
    fn a_heartbeat_behind_in_either_counter_is_stale() {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        pd.region_heartbeat(&beat(ranged(1, b"", b"", (3, 3)), 10, 1))
            .unwrap();

        for stale in [(2, 3), (3, 2), (2, 4)] {
            assert_eq!(
                pd.region_heartbeat(&beat(ranged(1, b"", b"", stale), 99, 9))
                    .unwrap(),
                Upsert::Stale,
                "epoch {stale:?} was accepted over (3, 3)"
            );
        }
        assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 10);
    }

    /// Within one epoch a leader election is invisible, so the term is what says which of two
    /// heartbeats is the newer one.
    #[test]
    fn within_one_epoch_the_newer_term_wins_and_the_older_is_dropped() {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        let region = || ranged(1, b"", b"", (1, 1));

        pd.region_heartbeat(&beat(region(), 10, 7)).unwrap();
        assert_eq!(
            pd.region_heartbeat(&beat(region(), 20, 6)).unwrap(),
            Upsert::Stale,
            "a beat from a leader that has already lost office"
        );
        assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 10);

        // The same leader reporting again, at the same term, is fresher stats.
        assert_eq!(
            pd.region_heartbeat(&beat(region(), 30, 7)).unwrap(),
            Upsert::Applied
        );
        assert_eq!(pd.regions().unwrap()[0].leader_peer_id, 30);
    }

    /// Three regions, including the one that runs to the end of the key space: every key must
    /// land in exactly the region that owns it, and the index must not leave a stale entry
    /// behind when a range changes.
    #[test]
    fn a_lookup_finds_the_region_that_owns_the_key() {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();

        // Region 1 shrinks to ["", "m"), and two more cover the rest. This is the shape a
        // split leaves behind; 4a only ever gets here by heartbeat.
        for region in [
            ranged(1, b"", b"m", (1, 2)),
            ranged(2, b"m", b"t", (1, 2)),
            ranged(3, b"t", b"", (1, 2)),
        ] {
            pd.region_heartbeat(&beat(region, 0, 1)).unwrap();
        }

        for (key, expected) in [
            (&b""[..], 1),
            (b"a", 1),
            (b"l", 1),
            (b"m", 2),
            (b"s", 2),
            (b"t", 3),
            (b"z", 3),
            (b"\xff\xff\xff", 3),
        ] {
            let route = pd
                .get_region(key)
                .unwrap()
                .unwrap_or_else(|| panic!("no region owns {key:?}"));
            assert_eq!(route.region.id, expected, "key {key:?}");
        }

        // The index holds one entry per region and no orphan from region 1's old range.
        let index = crate::routing::range_index(pd.db()).unwrap();
        assert_eq!(index.len(), 3, "the index kept a stale entry: {index:?}");
    }

    /// A store that never registered has no address, so a heartbeat from one is refused
    /// rather than inventing a record a client could be routed to.
    #[test]
    fn a_heartbeat_from_an_unregistered_store_is_refused() {
        let (_dir, clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        let beat = StoreBeat {
            store_id: 9,
            stats: crate::StoreStats::default(),
        };
        assert!(pd.store_heartbeat(&beat).is_err());

        // And a registered one is recorded, liveness included.
        clock.advance(1_000);
        let beat = StoreBeat {
            store_id: 1,
            stats: crate::StoreStats {
                capacity: 100,
                available: 40,
                region_count: 3,
                leader_count: 1,
                applied_bytes: 7,
            },
        };
        pd.store_heartbeat(&beat).unwrap();
        let stored = pd.stores().unwrap();
        assert_eq!(stored[0].stats.available, 40);
        assert_eq!(stored[0].last_heartbeat_ms, clock.now_ms());
        assert_eq!(stored[0].address, "a", "the heartbeat lost the address");
    }

    #[test]
    fn a_malformed_region_is_refused() {
        let (_dir, _clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        assert!(
            pd.region_heartbeat(&beat(ranged(0, b"", b"", (1, 1)), 1, 1))
                .is_err(),
            "region id zero"
        );
        assert!(
            pd.region_heartbeat(&beat(ranged(2, b"z", b"a", (1, 1)), 1, 1))
                .is_err(),
            "an inverted range"
        );
        assert!(
            pd.region_heartbeat(&beat(ranged(2, b"a", b"a", (1, 1)), 1, 1))
                .is_err(),
            "an empty range"
        );
    }

    /// The routing table survives a reopen: it is on disk, not in memory.
    #[test]
    fn the_routing_table_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);
        {
            let pd = Pd::open(dir.path(), options()).unwrap();
            pd.bootstrap(1, "127.0.0.1:1").unwrap();
            pd.region_heartbeat(&beat(ranged(1, b"", b"m", (1, 2)), 10, 3))
                .unwrap();
            pd.region_heartbeat(&beat(ranged(2, b"m", b"", (1, 2)), 20, 3))
                .unwrap();
        }
        let pd = Pd::open(dir.path(), options()).unwrap();
        assert_eq!(pd.regions().unwrap().len(), 2);
        let route = pd.get_region(b"zz").unwrap().unwrap();
        assert_eq!(route.region.id, 2);
        assert_eq!(route.leader_peer_id, Some(20));
    }

    #[test]
    fn a_store_id_of_zero_is_refused() {
        let (_dir, _clock, pd) = open();
        assert!(pd.bootstrap(0, "a").is_err());
    }

    /// Liveness is derived from the last beat and nothing else — and in 4a it is only
    /// reported.
    #[test]
    fn a_silent_store_is_reported_as_down() {
        let (_dir, clock, pd) = open();
        pd.bootstrap(1, "a").unwrap();
        assert!(pd.down_stores().unwrap().is_empty());
        clock.advance(super::MAX_STORE_DOWN_TIME_MS + 1);
        assert_eq!(pd.down_stores().unwrap(), vec![1]);
    }
}
