//! [`Store`] — the engine, its column families, the regions it hosts — and [`StoreService`],
//! which is what `esker-proto`'s server calls.
//!
//! # Where async stops
//!
//! `esker-engine` is synchronous and stays that way (`CLAUDE.md`, "Toolchain and
//! conventions"). Every engine call from here goes through
//! [`tokio::task::spawn_blocking`], so an `fsync` that takes ten milliseconds blocks a
//! blocking thread and not the reactor that every other connection is being served on. No
//! engine lock is ever held across an `.await`; the whole of a request's engine work happens
//! inside one closure that owns everything it touches.
//!
//! # Concurrency
//!
//! The `Db` is shared behind an `Arc` and its own locking does the work — there is deliberately
//! no global request lock. The single exception is a `RwLock` that every mutation takes the
//! *shared* side of and `CompareAndSwap` takes the *exclusive* side of, which is what makes a
//! read-modify-write atomic against concurrent writers on a single node. It costs a reader
//! acquisition per write and it disappears in phase 3, when the Raft log becomes the
//! serialisation point.

use std::path::Path;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use esker_engine::compaction::CompactionFilter;
use esker_engine::{
    Db, FileSystem, LocalFileSystem, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions,
    cf,
};
use esker_proto::{
    BoxFuture, Peer, PeerRole, ProtoError, RaftBatch, RawKvReq, RawKvResp, Region, Reply, Request,
    RequestHeader, Response, Service, SnapshotRequest, TransportConfig, TxnKvReq, TxnKvResp,
};

use crate::apply::Command;
use crate::driver::DriverPool;
use crate::error::{Result, StoreError};
use crate::gc::{DEFAULT_RETENTION_MS, MvccCollector, RetentionPolicy};
use crate::heartbeat::{Heartbeats, RegionReport, StoreReport};
use crate::meta;
use crate::pd::{PdClient, StoreInfo};
use crate::peer::{Applied, LogCompaction, PeerOptions, RaftPeer, RegionHost};
use crate::raft_log::RaftLogStorage;
use crate::rawkv::{self, Limits};
use crate::region::RegionMeta;
use crate::regions::{RegionMap, RegionState};
use crate::snapshot;
use crate::split::{self, SplitOptions};
use crate::transport::{PeerAddress, StoreAddress, StoreTransport};
use crate::{OPERATOR_TIMEOUT, SNAPSHOT_STREAM_DEPTH, TRANSFER_LAG_ALLOWANCE};

/// How a store is opened.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// This store's id, reported in the handshake.
    pub store_id: u64,
    /// The id of this store's peer in the region it bootstraps.
    pub peer_id: u64,
    /// The region this store bootstraps when its database holds no regions yet, covering the
    /// whole key space (`docs/DESIGN.md` §7).
    ///
    /// Ignored on every later open: what a store hosts is read from its own `'m'` records
    /// ([`crate::meta`]), not re-derived from its configuration.
    pub region_id: u64,
    /// Limits on what one request may return or remove.
    pub limits: Limits,
    /// How the engine underneath is opened.
    pub engine: Options,
    /// The filesystem the engine reads and writes SSTs through.
    ///
    /// [`LocalFileSystem`] by default, which is the ordinary store. The parameter exists so a
    /// caller can hand the engine a **tiered** filesystem — the database directory as a cache
    /// in front of object storage (`docs/DESIGN.md` §13) — without this crate knowing anything
    /// about what is behind it, and so a test can inject faults the way `esker-engine`'s own do.
    pub fs: Arc<dyn FileSystem>,
    /// Replication, when this store is one of several. `None` is a single-node store that writes
    /// straight to the engine — which is what phase 2 built and what the CLI's `server` command
    /// still starts.
    pub raft: Option<RaftOptions>,
    /// The placement driver, when there is one. Setting it means [`Store::open`] must be called
    /// from inside a `tokio` runtime, because the heartbeat schedule is a task.
    ///
    /// `None` is a store that bootstraps its own region from [`StoreOptions::region_id`] and
    /// sends no heartbeats — phase 2's single node and phase 3e's static cluster, both of which
    /// the CLI still starts. With one, the bootstrap question is PD's to answer and this store
    /// reports to it on the schedule of `docs/DESIGN.md` §14.
    pub pd: Option<Arc<dyn PdClient>>,
    /// Where other stores and clients reach this one, as PD should record it. Only read when
    /// [`StoreOptions::pd`] is set.
    pub address: String,
    /// What one heartbeat tick is worth. The intervals below are counted in these
    /// ([`crate::heartbeat::Heartbeats`]), so this is the resolution of the schedule and not its
    /// period.
    pub heartbeat_tick: std::time::Duration,
    /// How often this store reports itself (`docs/DESIGN.md` §14: 10 s).
    pub store_heartbeat: std::time::Duration,
    /// How often each region's leader reports it, absent a change (§14: 60 s).
    ///
    /// It is also the **latency of an operator**: the placement driver answers a region heartbeat
    /// and has no other way to reach a store, so a repair waits at most this long — less whenever
    /// the region has changed, since a change beats immediately. Configurable because a test that
    /// waited sixty seconds for a membership change would not be run.
    pub region_heartbeat: std::time::Duration,
    /// When a region is split, and how finely the boundary is chosen.
    ///
    /// Splitting needs cluster-unique ids, so it needs a placement driver: a store with
    /// [`StoreOptions::pd`] unset never splits, whatever this says.
    pub split: SplitOptions,
}

/// How this store's region is replicated.
#[derive(Debug, Clone)]
pub struct RaftOptions {
    /// The **address book**: every peer this store may have to reach, on every region it hosts,
    /// with the store each is on. The entry for this store is never connected to.
    ///
    /// Not the same thing as a region's membership, and 4c is where the difference started to
    /// matter. A region's peers come from its own `'m'` record and move when a conf change
    /// applies; this list only says where a peer id can be found, and a store must know how to
    /// reach a peer it is *about* to be told it has.
    pub peers: Vec<PeerAddress>,
    /// The voters of the region this store bootstraps, or `None` for "every peer in the address
    /// book".
    ///
    /// A cluster whose stores all start together bootstraps one region across all of them and
    /// leaves this `None`. A store that is the first of a cluster others will *join* sets it to
    /// its own peer, so that its region can commit before the others exist — and they arrive
    /// later as learners, which is what `AddPeer` is for.
    pub bootstrap_voters: Option<Vec<u64>>,
    /// Seed for the election-timeout RNG. A whole cluster may share one: the peer id selects the
    /// stream (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
    pub seed: u64,
    /// What one Raft tick is worth. `esker-raft` counts ticks and never reads a clock; this is
    /// the only place a wall clock touches consensus (`docs/DESIGN.md` §14).
    pub tick: std::time::Duration,
    /// How the connections between stores are configured.
    pub transport: TransportConfig,
    /// When each region's Raft log is compacted.
    pub compaction: LogCompaction,
    /// How many driver threads this store runs. Regions are pinned across them by id
    /// ([`crate::driver`]).
    pub driver_workers: usize,
}

impl RaftOptions {
    /// Replication across `peers`, with the project's defaults.
    #[must_use]
    pub fn new(peers: Vec<PeerAddress>, seed: u64) -> Self {
        Self {
            peers,
            bootstrap_voters: None,
            seed,
            tick: std::time::Duration::from_millis(esker_raft::TICK_MS),
            transport: TransportConfig::new(),
            compaction: LogCompaction::new(),
            driver_workers: crate::driver::DRIVER_WORKERS,
        }
    }
}

impl StoreOptions {
    /// The defaults: store 1, peer 1, region 1, covering everything.
    ///
    /// The engine is opened with **`WalSyncMode::Never`**, which is not what it sounds like: it
    /// means the engine adds no `fsync` of its own, so each write's own `sync` flag decides.
    /// That is the only setting under which `CLAUDE.md` invariant 1 is true as written — "a
    /// write is acknowledged only after its bytes are durable, *unless the caller explicitly
    /// passed `sync = false`*" — because the engine's own default, `PerWrite`, syncs whatever
    /// the caller asked and makes the opt-out unreachable.
    ///
    /// Requests stay durable by default regardless: `RawKvReq`'s write constructors set
    /// `sync: true`, so a caller gets an `fsync` without asking for one and gives it up only by
    /// saying so. This is the same choice phase 1's benchmark driver made — the workload's own
    /// flag decides and the engine adds nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            limits: Limits::new(),
            raft: None,
            pd: None,
            address: String::new(),
            heartbeat_tick: std::time::Duration::from_millis(esker_raft::TICK_MS),
            store_heartbeat: std::time::Duration::from_millis(crate::STORE_HEARTBEAT_MS),
            region_heartbeat: std::time::Duration::from_millis(crate::REGION_HEARTBEAT_MS),
            split: SplitOptions::new(),
            engine: Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            fs: Arc::new(LocalFileSystem::new()),
        }
    }
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// One process, one store id, many regions (`docs/DESIGN.md` §6).
#[derive(Debug)]
// `store_id` names the store this *is*, not a store it points at; `id` alone would read as a
// region's in a type that has several of those too.
#[allow(clippy::struct_field_names)]
pub struct Store {
    db: Arc<Db>,
    /// Every region this store hosts, indexed by id and by range.
    regions: RegionMap,
    store_id: u64,
    limits: Limits,
    /// The MVCC collector, installed on the `write` column family at open and updated in place
    /// as the placement driver publishes safepoints ([`crate::gc`]). It holds the safepoint this
    /// store is working to, so there is no second copy of that number to keep in step.
    collector: Arc<MvccCollector>,
    /// Shared by every mutation, exclusive for `CompareAndSwap`. See the module docs.
    ///
    /// Only used by a store with no Raft peer. Once there is one, the Raft log is the
    /// serialisation point and read-modify-write happens at apply time, on every peer alike.
    write_gate: RwLock<()>,
    /// One connection per store pair, shared by every region. Kept so it can be shut down with
    /// the store; each region's view holds its own reference.
    transport: Option<Arc<StoreTransport>>,
    /// How regions are replicated, kept because a **split** starts a new peer long after `open`
    /// returned and needs the same address book, seed and tick the others got.
    raft: Option<RaftOptions>,
    /// The threads every region's Raft is driven on, each region pinned to one of them
    /// ([`crate::driver`]).
    drivers: Arc<DriverPool>,
    /// The placement driver, kept because an **operator-requested split** needs cluster-unique
    /// ids long after `open` returned (`esker-cli region split`).
    pd: Option<Arc<dyn PdClient>>,
    /// How this store decides to split.
    split: SplitOptions,
    /// The runtime this store was opened on. A split starts the child's peer from the parent's
    /// driver thread, which is a plain OS thread; `tokio::spawn` there is a panic.
    runtime: Option<tokio::runtime::Handle>,
    /// One per replicated region: the timer that feeds its driver thread.
    ///
    /// `TODO(phase-4d)`: one timer per region is one task per region. At fifty regions that is
    /// fifty timers where one wheel would do, which is the same sharding decision as the apply
    /// worker's and belongs with it.
    ///
    /// Behind a lock because a split adds one, from the parent's driver thread.
    tickers: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The store-wide tasks: the heartbeat schedule and the split checker, when there is a
    /// placement driver for either to talk to.
    background: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Regions a snapshot is being fetched for. A leader re-announces every heartbeat, and each
    /// announcement would otherwise start another transfer of the same megabytes.
    receiving: std::sync::Mutex<std::collections::BTreeSet<u64>>,
}

/// Reads the retention policy out of the catalog and hands it to the collector.
///
/// A failure leaves the policy that was working rather than falling back to the default,
/// because the two differ in the direction that matters: an unreadable catalog should make the
/// collector keep *more* than it would have, never less. It is re-read on every safepoint, not
/// cached, because a `retention` DDL writes a record and bumps no version — deliberately
/// ([ADR 0021](../../docs/adr/0021-time-machine.md) decision 4) — so the collector's own next
/// pass is where a change is meant to be noticed.
fn load_retention(db: &Db, collector: &MvccCollector) {
    match RetentionPolicy::load(db, DEFAULT_RETENTION_MS) {
        Ok(policy) => collector.set_policy(policy),
        Err(error) => tracing::warn!(
            %error,
            "could not read the retention policy; keeping the one in force"
        ),
    }
}

/// Opens the engine and checks it has every column family the store needs.
///
/// A missing one is a bootstrap failure rather than a lazily-created family: `Db::open` creates
/// what it is asked for, so a name absent afterwards means the database disagrees with this
/// build about what it holds — and finding that at the first write instead would find it on a
/// path that cannot report it usefully (`docs/DESIGN.md` §4.8).
fn open_engine(
    path: impl AsRef<Path>,
    mut engine: Options,
    fs: Arc<dyn FileSystem>,
    collector: &Arc<MvccCollector>,
) -> Result<Db> {
    // Only on `write`. The collector reads a `WriteRecord` out of every value it is offered, and
    // the other three column families hold something else — it would keep everything it met
    // there, which is safe and pointless (`docs/txn-spec.md` §7).
    let mut write_options = engine.cf_options.clone();
    write_options.compaction_filter = Some(Arc::clone(collector) as Arc<dyn CompactionFilter>);
    engine
        .cf_overrides
        .insert(cf::WRITE.to_string(), write_options);

    let db = Db::open_with(path, engine, fs, &cf::BUILTIN)?;
    for name in cf::BUILTIN {
        if db.cf_id(name).is_none() {
            return Err(StoreError::Bootstrap(format!(
                "the `{name}` column family is missing after open"
            )));
        }
    }
    Ok(db)
}

impl Store {
    /// Opens the database in `path`, creating the four built-in column families.
    ///
    /// `default`, `lock`, `write` and `raft` are created at **bootstrap**, all four, even
    /// though this phase writes only to `default`. `docs/DESIGN.md` §4.8 is explicit that the
    /// engine imposes no column family and the store creates the set — so creating three of
    /// them later, when `esker-txn` and `esker-raft` arrive, would mean a format change to
    /// every database made before then.
    ///
    /// Which regions the store hosts comes from its own `'m'` records ([`crate::meta`]). Only a
    /// database with none is a bootstrap, and only then do [`StoreOptions::region_id`] and
    /// [`StoreOptions::peer_id`] — or the placement driver — decide anything.
    ///
    /// **Must be called from inside a `tokio` runtime when [`StoreOptions::raft`] or
    /// [`StoreOptions::pd`] is set**: the peer connections and the heartbeat schedule are tasks.
    /// A store with neither is exactly phase 2's and needs no runtime at all.
    pub fn open(path: impl AsRef<Path>, options: StoreOptions) -> Result<Arc<Self>> {
        let StoreOptions {
            store_id,
            peer_id,
            region_id,
            limits,
            engine,
            fs,
            raft,
            pd,
            address,
            heartbeat_tick,
            store_heartbeat,
            region_heartbeat,
            split,
        } = options;
        // The collector is built before the engine, because the engine has to be opened *with*
        // it: a compaction filter is a column-family setting and this one belongs to `write`
        // alone, whose entries are the only ones it understands ([`crate::gc`]).
        let collector = Arc::new(MvccCollector::new(
            RetentionPolicy::uniform(DEFAULT_RETENTION_MS),
            0,
        ));
        let db = Arc::new(open_engine(path, engine, fs, &collector)?);
        load_retention(&db, &collector);

        // What this store hosts is what its own `'m'` records say — never what its configuration
        // says on a later open, and never what the placement driver currently believes. A
        // database with none is a fresh one, and only then is `options` a bootstrap.
        // A snapshot that was part-way in when this store stopped left keys no region covers.
        // They are cleared before anything else looks at the key space, so the retry finds the
        // range it was promised and `may_receive` does not refuse it
        // (`docs/plans/phase-4.md` §13.1).
        for (region, index) in meta::load_pending_snapshots(&db)? {
            let removed = snapshot::discard_range(&db, &region)?;
            let mut batch = WriteBatch::new();
            let cf_id = db.cf_id(cf::RAFT).ok_or_else(|| {
                StoreError::Bootstrap("the `raft` column family is missing".into())
            })?;
            meta::stage_snapshot_done(&mut batch, cf_id, region.id);
            db.write(batch, &WriteOptions { sync: true })?;
            tracing::warn!(
                region_id = region.id,
                index,
                removed,
                "a snapshot was interrupted; its partial data was discarded"
            );
        }

        let mut hosted = meta::load_regions(&db)?;
        if hosted.is_empty() {
            hosted.extend(bootstrap(
                &db,
                &BootstrapOptions {
                    region_id,
                    store_id,
                    peer_id,
                    address: &address,
                    raft: raft.as_ref(),
                    pd: pd.as_ref(),
                },
            )?);
        }

        // A replicated store needs a runtime: the transport's tasks and the tickers live in one.
        // A store with no Raft options is exactly phase 2's and needs nothing.
        let transport = raft.as_ref().map(|raft| {
            StoreTransport::spawn(
                store_id,
                &StoreAddress::from_peers(&raft.peers),
                raft.transport,
            )
        });

        let drivers = Arc::new(DriverPool::new(
            raft.as_ref().map_or(1, |raft| raft.driver_workers),
        )?);
        let regions = RegionMap::new();
        tracing::info!(
            store_id,
            regions = hosted.len(),
            replicated = transport.is_some(),
            drivers = drivers.workers(),
            column_families = ?cf::BUILTIN,
            "store opened"
        );

        let store = Arc::new(Self {
            db,
            regions,
            store_id,
            limits,
            collector,
            write_gate: RwLock::new(()),
            transport,
            raft,
            drivers,
            pd: pd.clone(),
            split,
            runtime: tokio::runtime::Handle::try_current().ok(),
            tickers: std::sync::Mutex::new(Vec::new()),
            background: std::sync::Mutex::new(Vec::new()),
            receiving: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        });

        // The peers are started only now, because each of them needs a handle back to the store:
        // a split reaches outside the region it happens in, and the driver that applies it has to
        // be able to say so ([`crate::peer::RegionHost`]).
        for region in hosted {
            store.host_region(region)?;
        }

        if let Some(pd) = pd {
            store.spawn_heartbeats(
                Arc::clone(&pd),
                heartbeat_tick,
                store_heartbeat,
                region_heartbeat,
            );
            store.spawn_split_checker(pd, heartbeat_tick);
        }
        Ok(store)
    }

    /// Adds one region to the map, starting its peer when this store replicates.
    ///
    /// A record whose peer list does not name this store is **not** started: that is what a crash
    /// between `RemovePeer` applying and the data being deleted leaves behind
    /// (`docs/plans/phase-4.md` §6, race 3), and starting a peer for it would return a voter to a
    /// group that has already removed it.
    fn host_region(self: &Arc<Self>, region: Region) -> Result<()> {
        if !region
            .peers
            .iter()
            .any(|peer| peer.store_id == self.store_id)
        {
            tracing::warn!(
                store_id = self.store_id,
                region_id = region.id,
                "a region record on this store does not list it as a peer; not started"
            );
            return Ok(());
        }
        let state = match (&self.raft, &self.transport) {
            (Some(raft), Some(transport)) => {
                let host: Arc<dyn RegionHost> = Arc::new(StoreHost {
                    store: Arc::downgrade(self),
                });
                let view = transport.for_region(region.id, region.epoch, &region.peers);
                let peer = start_peer(
                    &self.db,
                    &region,
                    self.store_id,
                    raft,
                    Arc::clone(&view),
                    host,
                    Arc::clone(&self.drivers),
                )?;
                self.spawn_ticker(&peer, raft.tick);
                RegionState::replicated(RegionMeta::new(region), peer, view)
            }
            _ => RegionState::unreplicated(RegionMeta::new(region)),
        };
        self.regions.insert(state)?;
        Ok(())
    }

    /// Starts one peer's ticker, on the runtime this store was opened on.
    fn spawn_ticker(&self, peer: &Arc<RaftPeer>, tick: std::time::Duration) {
        let Some(runtime) = &self.runtime else {
            // A store with no runtime has no transport either, so it has no peers to tick.
            return;
        };
        let ticker = peer.spawn_ticker_on(runtime, tick);
        if let Ok(mut tickers) = self.tickers.lock() {
            tickers.push(ticker);
        }
    }

    /// Carries out one operator from the placement driver, or says why it did not.
    ///
    /// Three ways an operator is declined, and none of them is an error worth failing anything
    /// over — PD re-issues from the next heartbeat, against whatever state it has by then:
    ///
    /// * **the region is not here**, or is not led here. Only a leader proposes;
    /// * **the epoch has moved** since PD decided. The operator was reasoned about against a
    ///   membership or a range that no longer exists, and applying it anyway is how two
    ///   half-informed schedulers take a region below quorum between them;
    /// * **it changes nothing** — the peer is already there, or already gone. The apply path is
    ///   idempotent too, but not proposing at all saves an entry and an epoch bump.
    ///
    /// `TransferLeader` is reserved for 4d and is ignored, loudly enough to notice.
    async fn run_operator(self: &Arc<Self>, operator: &esker_proto::Operator) {
        use esker_proto::Operator;

        let region_id = operator.region_id();
        let Some(state) = self.regions.get(region_id) else {
            tracing::debug!(
                region_id,
                "an operator arrived for a region this store does not host"
            );
            return;
        };
        let current = state.region().epoch;
        if operator.epoch() != current {
            tracing::debug!(
                region_id,
                theirs = ?operator.epoch(),
                ours = ?current,
                "an operator was decided against an epoch this region has moved past"
            );
            return;
        }
        let Some(peer) = state.peer().map(Arc::clone) else {
            return;
        };
        if !peer.is_leader() {
            tracing::debug!(
                region_id,
                "an operator arrived at a peer that does not lead"
            );
            return;
        }

        let (kind, node, store_id) = match operator {
            // **Learner first, and the store finishes the job.** An `AddPeer` for a peer this
            // region has never heard of adds a *learner*: it receives the log and the snapshot
            // without voting, so it never makes a quorum harder to reach while it is catching up.
            // The promotion that follows is the leader's, on
            // [`Store::promote_caught_up_learners`], because "has this learner caught up" is a
            // statement about its match index and the leader is the only party that can see one.
            //
            // 4c put the promotion here instead, as "the same operator for a peer that is already
            // a learner", and that is what phase-4 acceptance stalled on: PD stops re-sending an
            // operator once it can see the learner, so the second step was never asked for. The
            // reasoning that made it look sound is corrected at the promotion itself.
            //
            // Arriving here for a peer that is already a learner is therefore a *repeat* rather
            // than a second step — PD re-deriving after a timeout — and is answered by the same
            // promotion criterion, which is to say by leaving it to the round that checks it.
            Operator::AddPeer {
                store_id, peer_id, ..
            } => match state
                .region()
                .peers
                .iter()
                .find(|peer| peer.peer_id == *peer_id)
            {
                None => (esker_raft::ConfChangeKind::AddLearner, *peer_id, *store_id),
                // Already here. A learner is on its way to being a voter under its own criterion,
                // and a voter is what was asked for: either way there is nothing to propose.
                Some(_) => return,
            },
            Operator::RemovePeer { peer_id, .. } => {
                let Some(existing) = state
                    .region()
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == *peer_id)
                else {
                    return;
                };
                (
                    esker_raft::ConfChangeKind::Remove,
                    *peer_id,
                    existing.store_id,
                )
            }
            // Leadership moves by the core's own `TimeoutNow` path rather than by a conf change,
            // so it takes a different route out of here entirely.
            Operator::TransferLeader { to_peer_id, .. } => {
                self.transfer_leadership(&state, &peer, *to_peer_id).await;
                return;
            }
        };

        // Bounded, because a proposal is answered when it *applies* and a membership change that
        // cannot reach a quorum never does. Without this the heartbeat round that carried the
        // operator would stop, and with it the only channel the placement driver has to correct
        // its own mistake.
        let proposal = peer.propose_conf_change(kind, node, store_id);
        match tokio::time::timeout(OPERATOR_TIMEOUT, proposal).await {
            Ok(Ok(_)) => tracing::info!(region_id, node, ?kind, "an operator applied"),
            Ok(Err(error)) => tracing::debug!(
                region_id,
                node,
                %error,
                "an operator did not apply; the placement driver will issue it again"
            ),
            Err(_) => tracing::warn!(
                region_id,
                node,
                ?kind,
                "an operator did not commit within its timeout; it may still apply later"
            ),
        }
    }

    /// Asks a region's leadership to move, if the target can actually take it.
    ///
    /// No conf change and no epoch bump: who leads is not part of a region's identity, which is
    /// why a client learns it from a `NotLeader` hint rather than from its cache. The core stops
    /// accepting proposals, brings the target up to date, and tells it to campaign — so what
    /// *completes* the transfer is an election, and nothing here can await one. A transfer that
    /// does not happen leaves the current leader in office and PD re-issues from the next
    /// heartbeat.
    async fn transfer_leadership(
        self: &Arc<Self>,
        state: &Arc<RegionState>,
        peer: &Arc<RaftPeer>,
        to_peer_id: u64,
    ) {
        let region_id = state.id();
        let Some(target) = state
            .region()
            .peers
            .iter()
            .find(|peer| peer.peer_id == to_peer_id)
        else {
            tracing::debug!(
                region_id,
                to_peer_id,
                "a transfer named a peer this region does not have"
            );
            return;
        };
        if target.role == PeerRole::Learner {
            // A learner cannot win an election, so the transfer would leave the region without a
            // leader until the old one's timeout brought it back.
            tracing::debug!(
                region_id,
                to_peer_id,
                "a transfer named a learner, which cannot take office"
            );
            return;
        }
        if to_peer_id == peer.peer_id() {
            return;
        }
        // A target that is not caught up would campaign on a short log and either lose or win and
        // then need a snapshot of its own. `RawNode::progress` is what makes this checkable at all
        // (`docs/plans/phase-4.md` §14.1 unit 4).
        if let Ok(progress) = peer.progress().await {
            let own_last = progress
                .iter()
                .find(|entry| entry.id == peer.peer_id())
                .map_or(0, |entry| entry.matched);
            let theirs = progress
                .iter()
                .find(|entry| entry.id == to_peer_id)
                .map_or(0, |entry| entry.matched);
            if own_last.saturating_sub(theirs) > TRANSFER_LAG_ALLOWANCE {
                tracing::debug!(
                    region_id,
                    to_peer_id,
                    behind = own_last - theirs,
                    "a transfer named a peer that is too far behind to take office"
                );
                return;
            }
        }
        if let Err(error) = peer.transfer_leader(to_peer_id).await {
            tracing::debug!(region_id, to_peer_id, %error, "a transfer was not started");
        } else {
            tracing::info!(region_id, to_peer_id, "leadership was asked to move");
        }
    }

    /// Retires a region and **waits** for it, so the caller may reuse its range immediately.
    ///
    /// [`retire_region`](Self::retire_region) spawns and returns, which is right when a conf change
    /// has removed this store and nothing is waiting. It is wrong when the range is about to be
    /// emptied and refilled: a peer still applying into it while `clear_range` runs would write
    /// under the tombstone and survive the discharge, and the emptiness check would then refuse a
    /// transfer that was only racing itself.
    ///
    /// The raft state is left alone. The snapshot about to be adopted overwrites it wholesale, and
    /// destroying it first would turn a failed transfer into a peer that has lost its log as well
    /// as its data.
    async fn retire_region_now(self: &Arc<Self>, region_id: u64) {
        let Some(state) = self.regions.remove(region_id) else {
            return;
        };
        if let Some(peer) = state.peer() {
            let peer = Arc::clone(peer);
            let _ = tokio::task::spawn_blocking(move || peer.stop()).await;
        }
    }

    /// Stops serving a region this store has been removed from, and forgets its Raft state.
    ///
    /// Spawned rather than done here, because this runs on the removed peer's **own driver
    /// thread**: stopping it from inside is a join on itself.
    ///
    /// The region's **data is left in place**. Removing it would mean point deletes over the whole
    /// range — the engine has no range tombstones in v1 (ADR 0006) — and the tombstones that
    /// leaves are keys in the range, which is the state that stops the range ever receiving a
    /// snapshot again (`docs/plans/phase-4.md` §13.4). So the keys stay, no region covers them, and
    /// nothing serves them. `TODO(post-v1)`: reclaim them when the engine can drop a range.
    fn retire_region(self: &Arc<Self>, region_id: u64) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let store = Arc::clone(self);
        runtime.spawn(async move {
            let Some(state) = store.regions.remove(region_id) else {
                return;
            };
            if let Some(peer) = state.peer() {
                let peer = Arc::clone(peer);
                let _ = tokio::task::spawn_blocking(move || peer.stop()).await;
            }
            let db = Arc::clone(&store.db);
            let removed =
                tokio::task::spawn_blocking(move || crate::raft_log::destroy(&db, region_id)).await;
            match removed {
                Ok(Ok(entries)) => tracing::info!(
                    region_id,
                    entries,
                    "this store was removed from a region; its raft state is gone and its data \
                     is left where no region covers it"
                ),
                Ok(Err(error)) => {
                    tracing::warn!(region_id, %error, "could not clear a retired region's log");
                }
                Err(error) => tracing::warn!(region_id, %error, "the retirement task failed"),
            }
        });
    }

    /// Brings a split into effect: narrows the parent and starts the child.
    ///
    /// Called from the **parent's driver thread**, after both halves' records are durable. The map
    /// takes the two changes under one write lock, so no reader ever sees the parent still owning
    /// what the child now owns — the "key space is a contiguous partition" invariant holds through
    /// the split rather than after it.
    fn adopt_split(self: &Arc<Self>, parent: &Region, child: &Region) -> Result<()> {
        let child_state = match (&self.raft, &self.transport) {
            (Some(raft), Some(transport)) => {
                let host: Arc<dyn RegionHost> = Arc::new(StoreHost {
                    store: Arc::downgrade(self),
                });
                // Its log is empty, so `RaftLogStorage::open` writes the configuration it is given
                // as the membership *as of index 0* — which for a region whose log starts there is
                // exactly the split-time membership, and is the anchor rule of `91de89a`.
                let view = transport.for_region(child.id, child.epoch, &child.peers);
                let peer = start_peer(
                    &self.db,
                    child,
                    self.store_id,
                    raft,
                    Arc::clone(&view),
                    host,
                    Arc::clone(&self.drivers),
                )?;
                self.spawn_ticker(&peer, raft.tick);
                RegionState::replicated(RegionMeta::new(child.clone()), peer, view)
            }
            _ => RegionState::unreplicated(RegionMeta::new(child.clone())),
        };
        self.regions.apply_split(parent.clone(), child_state)
    }

    /// Starts the task that reports to the placement driver.
    ///
    /// The schedule itself counts ticks and reads no clock ([`crate::heartbeat`]); this is the
    /// edge that turns a `tokio` interval into those ticks — the same shape as a peer's ticker,
    /// and for the same reason. A tick that is late because the process was busy is skipped
    /// rather than replayed: `MissedTickBehavior::Delay` would make a stalled store send a burst
    /// of identical heartbeats the moment it recovered.
    ///
    /// The round itself runs on a **blocking thread**. [`PdClient`] is synchronous, like
    /// everything else in this store that is not the network edge, so a round that ran on the
    /// reactor would hold a worker for a network round trip to the placement driver — and a
    /// placement driver that had gone away would hold it for the whole timeout, every ten
    /// seconds, on every store. The schedule travels into the closure and back out, because it
    /// is the state that must survive the round.
    fn spawn_heartbeats(
        self: &Arc<Self>,
        pd: Arc<dyn PdClient>,
        tick: std::time::Duration,
        store_every: std::time::Duration,
        region_every: std::time::Duration,
    ) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let store_id = self.store_id;
        let task = runtime.spawn(async move {
            let mut beats = Heartbeats::with_intervals(
                pd,
                store_id,
                Heartbeats::interval_ticks(store_every, tick),
                Heartbeats::interval_ticks(region_every, tick),
            );
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                // A weak reference, so a dropped store ends this task rather than keeping
                // itself alive through the schedule that reports it.
                let Some(store) = weak.upgrade() else {
                    return;
                };
                let report = store.report();
                drop(store);
                // The blocking pool refusing means the process is shutting down.
                let Ok((next, operators)) = tokio::task::spawn_blocking(move || {
                    let operators = beats.tick(&report);
                    (beats, operators)
                })
                .await
                else {
                    return;
                };
                beats = next;
                let Some(store) = weak.upgrade() else {
                    return;
                };
                for operator in operators {
                    store.run_operator(&operator).await;
                }
                // On the same schedule, because it is the second half of the same job: an
                // `AddPeer` is not finished until the replica votes.
                store.promote_caught_up_learners().await;
                drop(store);
            }
        });
        self.remember(task);
    }

    /// Promotes every learner that has caught up, in the regions this store leads.
    ///
    /// **The leader decides, and it has to be the leader.** 4c put this with the placement driver,
    /// on the reasoning — written into this file — that PD "sees every store's region heartbeats,
    /// including the learner's own `applied_index`". That is false, and it is what stalled phase-4
    /// acceptance. A region heartbeat comes from a region's *leader* and only from its leader
    /// (`docs/DESIGN.md` §7), so a learner, which leads nothing, is invisible to PD entirely: PD
    /// could see that a learner had appeared and never that it had caught up. It therefore held
    /// the `AddPeer` open waiting for a promotion nobody was going to ask for, and the region sat
    /// at two voters until the operator timed out and was re-derived into the same wait.
    ///
    /// The leader is the one party that does know — `matched` per peer is exactly the number, and
    /// since 4d it can read it ([`esker_raft::RawNode::progress`]). So an `AddPeer` means "put a
    /// replica here" and the store fulfils it in two steps: add the learner, promote it once it
    /// has caught up. That also makes PD's belief that re-asking "would only earn a refusal" true
    /// rather than merely assumed.
    ///
    /// Three things must hold, and each is a way the 4c deadlock came back:
    ///
    /// * the learner is within [`PROMOTION_LAG_ALLOWANCE`] of the leader's own `matched`;
    /// * it is **not** waiting on a snapshot. A peer being caught up by state has whatever
    ///   `matched` it had before the transfer started, which says nothing about what it holds;
    /// * it has acknowledged something at all. A `matched` of zero on a short log passes a lag
    ///   test that means nothing — the peer has never answered.
    ///
    /// Every learner in v1 is a step toward a voter; nothing creates a permanent one, so a
    /// caught-up learner is always one to promote. `TODO(post-v1)`: read-only replicas would need
    /// PD to say which learners are meant to stay learners.
    async fn promote_caught_up_learners(self: &Arc<Self>) {
        for region in self.regions.regions() {
            let Some(state) = self.regions.get(region.id) else {
                continue;
            };
            let learners: Vec<u64> = region
                .peers
                .iter()
                .filter(|peer| peer.role == PeerRole::Learner)
                .map(|peer| peer.peer_id)
                .collect();
            if learners.is_empty() {
                continue;
            }
            let Some(peer) = state.peer().map(Arc::clone) else {
                continue;
            };
            if !peer.is_leader() {
                continue;
            }
            let Ok(progress) = peer.progress().await else {
                continue;
            };
            let leader_matched = progress
                .iter()
                .find(|entry| entry.id == peer.peer_id())
                .map_or(0, |entry| entry.matched);

            for learner in learners {
                let Some(entry) = progress.iter().find(|entry| entry.id == learner) else {
                    tracing::debug!(
                        region_id = region.id,
                        learner,
                        "not promoting: the leader has no progress for this peer"
                    );
                    continue;
                };
                if entry.pending_snapshot != 0
                    || entry.matched == 0
                    || leader_matched.saturating_sub(entry.matched) > crate::PROMOTION_LAG_ALLOWANCE
                {
                    // Logged rather than silent: "the learner never caught up" is the shape of
                    // every stall this path exists to end, and the three numbers are what
                    // distinguish them.
                    tracing::debug!(
                        region_id = region.id,
                        learner,
                        matched = entry.matched,
                        next = entry.next,
                        leader_matched,
                        pending_snapshot = entry.pending_snapshot,
                        recent_active = entry.recent_active,
                        "not promoting: the learner has not caught up"
                    );
                    continue;
                }
                let Some(store_id) = region
                    .peers
                    .iter()
                    .find(|peer| peer.peer_id == learner)
                    .map(|peer| peer.store_id)
                else {
                    continue;
                };
                tracing::info!(
                    region_id = region.id,
                    learner,
                    store_id,
                    matched = entry.matched,
                    leader_matched,
                    "a learner has caught up; promoting it to voter"
                );
                let proposal = peer.propose_conf_change(
                    esker_raft::ConfChangeKind::AddVoter,
                    learner,
                    store_id,
                );
                match tokio::time::timeout(OPERATOR_TIMEOUT, proposal).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::debug!(
                        region_id = region.id,
                        learner,
                        %error,
                        "a promotion was refused; the next round tries again"
                    ),
                    Err(_) => tracing::warn!(
                        region_id = region.id,
                        learner,
                        "a promotion did not commit within the operator timeout"
                    ),
                }
                // One membership change at a time, per region and per round: the next round sees
                // the result of this one rather than racing it.
                break;
            }
        }
    }

    /// Keeps a store-wide task so [`Store::stop`] can end it.
    fn remember(&self, task: tokio::task::JoinHandle<()>) {
        if let Ok(mut tasks) = self.background.lock() {
            tasks.push(task);
        }
    }

    /// Starts the task that splits regions which have grown past the threshold.
    ///
    /// **One region at a time, per store.** The check itself is an atomic load, but everything
    /// after it — a full scan for the boundary, a round trip to the placement driver for ids, a
    /// Raft round for the entry — is not, and running several at once would turn a store that fell
    /// behind into a store issuing a burst of splits. Sequential also means "is a split already in
    /// flight for this region" needs no bookkeeping: there is one, or there is none.
    ///
    /// Splitting needs cluster-unique ids, so it needs a placement driver. A store without one
    /// never splits, which is exactly what phase 2's single node and phase 3e's static cluster are.
    fn spawn_split_checker(self: &Arc<Self>, pd: Arc<dyn PdClient>, tick: std::time::Duration) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let task = runtime.spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The size at which each region last failed to find a boundary. A region that cannot
            // be split is rescanned only once it has grown by another threshold, so a region of
            // one enormous value does not cost a full scan on every tick for ever.
            let mut refused: std::collections::BTreeMap<u64, u64> =
                std::collections::BTreeMap::new();
            loop {
                interval.tick().await;
                let Some(store) = weak.upgrade() else {
                    return;
                };
                let threshold = store.split.region_split_size;
                // Every led region's size, measured once per round on a blocking thread: the
                // engine walks each region's files and memtables to answer, which is not work
                // for the reactor.
                let sized = {
                    let store = Arc::clone(&store);
                    match blocking(move || Ok(store.region_sizes())).await {
                        Ok(sized) => sized,
                        Err(error) => {
                            tracing::debug!(%error, "could not size this store's regions");
                            continue;
                        }
                    }
                };
                for (state, size) in sized {
                    // A region that had no boundary last time is not rescanned until it has
                    // grown by another threshold: a region of one enormous value would otherwise
                    // cost a full scan on every tick for ever.
                    let held_back = refused
                        .get(&state.id())
                        .is_some_and(|at| size < at + threshold);
                    if size < threshold || held_back {
                        continue;
                    }
                    match store.split_region(&pd, &state).await {
                        Ok(true) => {
                            refused.remove(&state.id());
                        }
                        Ok(false) => {
                            refused.insert(state.id(), size);
                        }
                        Err(error) => tracing::debug!(
                            region_id = state.id(),
                            %error,
                            "a split did not go through; it will be tried again"
                        ),
                    }
                }
                drop(store);
            }
        });
        self.remember(task);
    }

    /// Splits one region: choose a boundary, ask for ids, propose. `Ok(false)` means the region
    /// has no legal boundary and cannot be split, which is not a failure.
    async fn split_region(
        self: &Arc<Self>,
        pd: &Arc<dyn PdClient>,
        state: &Arc<RegionState>,
    ) -> std::result::Result<bool, ProtoError> {
        let region = state.region().clone();
        let Some(peer) = state.peer().map(Arc::clone) else {
            return Ok(false);
        };

        // A full scan of the region, on a blocking thread — `docs/adr/0012-split-key-selection.md`
        // explains why it is a scan and what would replace it.
        let db = Arc::clone(&self.db);
        let scanned = region.clone();
        let max_sampled = self.split.max_sampled_keys;
        let Some(split_key) =
            blocking(move || split::choose_split_key(&db, &scanned, max_sampled)).await?
        else {
            tracing::debug!(
                region_id = region.id,
                "the region is over the split threshold but has no key to split at"
            );
            return Ok(false);
        };

        // One id for the child region and one per peer, in a single block: a round trip per id
        // would put the placement driver on the split path.
        let pd = Arc::clone(pd);
        let count = 1 + region.peers.len() as u64;
        let first = blocking(move || pd.alloc_id(count)).await?;
        let new_region_id = first;
        let new_peer_ids: Vec<u64> = (first + 1..first + count).collect();

        peer.propose(&Command::Split {
            split_key,
            new_region_id,
            new_peer_ids,
        })
        .await?;
        Ok(true)
    }

    /// Answers a follower's request for a region's contents with a stream of its pairs.
    ///
    /// Refused rather than served when the asking peer is not a member of the region: a store
    /// that is not replicating a range has no claim to a copy of it, and a snapshot is the one
    /// request that hands over a region wholesale.
    ///
    /// Refused, too, when this store cannot produce a snapshot at least as recent as the index the
    /// follower was told about. A snapshot older than the announcement would leave a hole between
    /// where the follower's log resumes and where the data actually reaches — the one shape a
    /// follower cannot detect for itself.
    ///
    /// The reading runs on a **blocking thread** and pushes into the stream's bounded channel, so
    /// a snapshot never buffers a region in memory and a slow reader slows the walk rather than
    /// growing a queue.
    async fn send_snapshot(
        self: &Arc<Self>,
        ask: SnapshotRequest,
    ) -> std::result::Result<Reply, ProtoError> {
        let state = self
            .regions
            .get(ask.region_id)
            .ok_or(ProtoError::RegionNotFound {
                region_id: ask.region_id,
            })?;
        if !state
            .region()
            .peers
            .iter()
            .any(|peer| peer.peer_id == ask.peer_id)
        {
            return Err(ProtoError::invalid(format!(
                "peer {} is not a member of region {} and may not have a copy of it",
                ask.peer_id, ask.region_id
            )));
        }
        let peer = state.peer().map(Arc::clone).ok_or_else(|| {
            ProtoError::invalid(format!(
                "region {} is not replicated on this store",
                ask.region_id
            ))
        })?;

        let source = peer.snapshot_source().await?;
        if source.meta.index < ask.index {
            return Err(ProtoError::Unsupported {
                detail: format!(
                    "region {} can offer a snapshot at index {} but {} was asked for",
                    ask.region_id, source.meta.index, ask.index
                ),
            });
        }

        let (sender, stream) = esker_proto::ChunkStream::channel(SNAPSHOT_STREAM_DEPTH);
        let db = Arc::clone(&self.db);
        let header = snapshot::SnapshotHeader {
            region: source.region.clone(),
            meta: source.meta.clone(),
        };
        let to_peer = ask.peer_id;
        let region_id = ask.region_id;
        let reporting = Arc::clone(&peer);
        tokio::spawn(async move {
            let delivered = Self::stream_snapshot(sender, db, header, source.read).await;
            // The core's half of this is `ProgressState::Snapshot`, which is paused until the
            // follower acknowledges — and a transfer the follower never received is never
            // acknowledged. Saying how it went is what ends that wait; the alternative is a
            // replica stranded for the rest of the leader's term (`docs/plans/phase-4.md` §15).
            let status = if delivered {
                esker_raft::SnapshotStatus::Finished
            } else {
                esker_raft::SnapshotStatus::Failed
            };
            if let Err(error) = reporting.report_snapshot(to_peer, status).await {
                tracing::debug!(
                    region_id,
                    to_peer,
                    %error,
                    "a snapshot's outcome could not be reported; the tick timeout covers it"
                );
            }
        });
        Ok(Reply::Stream(stream))
    }

    /// Walks a region into the stream, and says whether every byte of it got there.
    ///
    /// Split out of [`send_snapshot`](Self::send_snapshot) for one reason: every way out of the
    /// walk has to produce an answer, because the caller owes the core a report either way. An
    /// early `return` that skipped it is exactly the bug this whole path exists to fix.
    async fn stream_snapshot(
        sender: esker_proto::ChunkSender,
        db: Arc<Db>,
        header: snapshot::SnapshotHeader,
        read: esker_engine::Snapshot,
    ) -> bool {
        if sender.send(header.encode()).await.is_err() {
            return false;
        }
        // The walk is synchronous engine work and the sending is not, so the two are joined
        // by a channel rather than by one of them pretending to be the other.
        let (chunks, mut chunk_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
        let region = header.region.clone();
        let walk = tokio::task::spawn_blocking(move || {
            snapshot::read_pairs(&db, &region, read, snapshot::CHUNK_TARGET_BYTES, |pairs| {
                chunks
                    .blocking_send(snapshot::encode_pairs(&pairs))
                    .map_err(|_| ProtoError::Closed {
                        detail: "the snapshot's reader has gone".to_owned(),
                    })
            })
        });
        let mut delivered = true;
        while let Some(chunk) = chunk_rx.recv().await {
            if sender.send(chunk).await.is_err() {
                // The receiver has gone. The walk still has to be drained, or its blocking thread
                // stays parked on a channel nobody is reading.
                delivered = false;
                break;
            }
        }
        while chunk_rx.recv().await.is_some() {}
        match walk.await {
            Ok(Ok(())) => delivered,
            Ok(Err(error)) => {
                sender.fail(error).await;
                false
            }
            Err(error) => {
                sender
                    .fail(ProtoError::internal(format!(
                        "the snapshot walk failed: {error}"
                    )))
                    .await;
                false
            }
        }
    }

    /// Answers one operator request (`esker-cli region`).
    ///
    /// Everything here is **addressed to a store and refused by a store**: an operator names a
    /// region, and a store that does not host it, or does not lead it, says so rather than
    /// forwarding. Forwarding would make an operator's "which store did this" unanswerable, which
    /// is the one thing an operator's tool is for.
    async fn serve_admin(
        self: &Arc<Self>,
        request: esker_proto::AdminReq,
    ) -> std::result::Result<esker_proto::AdminResp, ProtoError> {
        use esker_proto::{AdminReq, AdminResp};

        match request {
            AdminReq::Regions => Ok(AdminResp::Regions {
                regions: self.region_statuses(),
            }),
            AdminReq::TransferLeader {
                region_id,
                to_peer_id,
            } => {
                let (state, peer) = self.led_region(region_id)?;
                self.transfer_leadership(&state, &peer, to_peer_id).await;
                Ok(AdminResp::TransferLeader)
            }
            AdminReq::Split {
                region_id,
                split_key,
            } => {
                let (state, peer) = self.led_region(region_id)?;
                let region = state.region().clone();
                if !split::is_legal_boundary(&split_key, &region) {
                    return Err(ProtoError::invalid(format!(
                        "{split_key:?} is not strictly inside region {region_id}'s range \
                         [{:?}, {:?})",
                        region.start_key, region.end_key
                    )));
                }
                let pd = self.pd.clone().ok_or_else(|| {
                    ProtoError::invalid(
                        "a split needs cluster-unique ids and this store has no placement driver",
                    )
                })?;
                let count = 1 + region.peers.len() as u64;
                let first = blocking(move || pd.alloc_id(count)).await?;
                peer.propose(&Command::Split {
                    split_key: split_key.clone(),
                    new_region_id: first,
                    new_peer_ids: (first + 1..first + count).collect(),
                })
                .await?;

                // Read the halves back rather than computing them: what the operator is told is
                // what the store now holds, which is the question they asked.
                let left = self
                    .regions
                    .get(region_id)
                    .map(|state| state.region().clone());
                let right = self.regions.get(first).map(|state| state.region().clone());
                match (left, right) {
                    (Some(left), Some(right)) => Ok(AdminResp::Split { left, right }),
                    _ => Err(ProtoError::internal(format!(
                        "region {region_id} split but this store cannot find both halves"
                    ))),
                }
            }
        }
    }

    /// A region this store hosts **and leads**, or the refusal that says which it is not.
    fn led_region(
        &self,
        region_id: u64,
    ) -> std::result::Result<(Arc<RegionState>, Arc<RaftPeer>), ProtoError> {
        let state = self
            .regions
            .get(region_id)
            .ok_or(ProtoError::RegionNotFound { region_id })?;
        let peer = state.peer().map(Arc::clone).ok_or_else(|| {
            ProtoError::invalid(format!(
                "region {region_id} is not replicated on this store"
            ))
        })?;
        if !peer.is_leader() {
            return Err(peer.not_leader());
        }
        Ok((state, peer))
    }

    /// Every region this store hosts, in key order, as an operator sees it.
    #[must_use]
    pub fn region_statuses(&self) -> Vec<esker_proto::RegionStatus> {
        let mut statuses: Vec<esker_proto::RegionStatus> = self
            .regions
            .states()
            .into_iter()
            .map(|state| {
                let region = state.region().clone();
                let (leader, is_leader) = state.peer().map_or((0, false), |peer| {
                    (peer.leader().unwrap_or(0), peer.is_leader())
                });
                esker_proto::RegionStatus {
                    leader_peer_id: leader,
                    is_leader,
                    approximate_size: split::approximate_size(&self.db, &region).unwrap_or(0),
                    applied_index: state.peer().map_or(0, |peer| peer.applied_index()),
                    region,
                }
            })
            .collect();
        statuses.sort_by(|left, right| left.region.start_key.cmp(&right.region.start_key));
        statuses
    }

    /// Every region this store **leads**, with what the engine says it holds.
    ///
    /// Synchronous and not cheap — the engine walks each region's files and memtables — so it runs
    /// on a blocking thread. A region this store merely hosts is not sized: only a leader splits,
    /// and only a leader's heartbeat reports a size.
    fn region_sizes(&self) -> Vec<(Arc<RegionState>, u64)> {
        self.regions
            .states()
            .into_iter()
            .filter(|state| state.peer().is_some_and(|peer| peer.is_leader()))
            .map(|state| {
                let size = split::approximate_size(&self.db, state.region()).unwrap_or(0);
                (state, size)
            })
            .collect()
    }

    /// What this store looks like right now, for one heartbeat round.
    ///
    /// Every number is read without waiting on a driver thread: the regions come from the map and
    /// the Raft numbers from what each driver publishes. A round that asked each peer in turn
    /// would queue behind whatever `fsync` each was in the middle of, and a store with fifty
    /// regions would report a state fifty `fsync`s old.
    #[must_use]
    pub fn report(&self) -> StoreReport {
        let regions: Vec<RegionReport> = self
            .regions
            .states()
            .into_iter()
            .map(|state| {
                let region = state.region().clone();
                match state.peer() {
                    Some(peer) => RegionReport {
                        leader_peer_id: peer.leader().unwrap_or(0),
                        is_leader: peer.is_leader(),
                        term: peer.term(),
                        applied_index: peer.applied_index(),
                        // From the engine, so every peer of a region agrees on it and PD can
                        // compare one store's regions against another's.
                        approximate_size: split::approximate_size(&self.db, &region).unwrap_or(0),
                        region,
                    },
                    // An unreplicated region has no consensus to lead, and this store is the only
                    // one that can serve it — which is what a leader is for PD's purposes.
                    None => RegionReport {
                        leader_peer_id: region
                            .peers
                            .iter()
                            .find(|peer| peer.store_id == self.store_id)
                            .map_or(0, |peer| peer.peer_id),
                        is_leader: true,
                        term: 0,
                        applied_index: 0,
                        approximate_size: 0,
                        region,
                    },
                }
            })
            .collect();
        // `applied_bytes` is the store's share of the same hint each region publishes. Its
        // limits are `RaftPeer::approximate_size`'s; `capacity` and `available` stay zero until
        // the engine can be asked how large a key range is (`docs/plans/phase-4.md` §12.3).
        let applied_bytes = regions.iter().map(|region| region.approximate_size).sum();
        StoreReport {
            capacity: 0,
            available: 0,
            applied_bytes,
            regions,
        }
    }

    /// Every region this store hosts, indexed by id and by range.
    #[must_use]
    pub fn regions(&self) -> &RegionMap {
        &self.regions
    }

    /// The Raft peer of the region this store hosts, when it hosts exactly one and replicates it.
    ///
    /// A convenience for the single-region tests and for the CLI, which is why it is honest about
    /// its precondition rather than picking one: a store hosting several has no "the" peer, and a
    /// caller that wants one names the region ([`Store::peer_of`]).
    #[must_use]
    pub fn peer(&self) -> Option<Arc<RaftPeer>> {
        let states = self.regions.states();
        match states.as_slice() {
            [only] => only.peer().map(Arc::clone),
            _ => None,
        }
    }

    /// The Raft peer of one region, when this store hosts and replicates it.
    #[must_use]
    pub fn peer_of(&self, region_id: u64) -> Option<Arc<RaftPeer>> {
        self.regions
            .get(region_id)
            .and_then(|state| state.peer().map(Arc::clone))
    }

    /// Feeds a batch of Raft messages from another store into this one's peer.
    ///
    /// A batch for a region this store does not serve is dropped rather than refused: the sender
    /// cannot act on the answer — Raft has no "you sent that to the wrong place" — and answering
    /// would only teach it to retry something that will never work.
    pub async fn receive_raft(
        self: &Arc<Self>,
        batch: RaftBatch,
    ) -> std::result::Result<(), ProtoError> {
        if self.transport.is_none() {
            return Err(ProtoError::invalid(
                "this store does not replicate; it has no Raft peer to receive a batch",
            ));
        }
        for message in batch.messages {
            let Some(state) = self.regions.get(message.region_id) else {
                // Raft traffic for a region this store does not host, which is how a peer added
                // by `AddPeer` first hears of itself. **It asks the sender for the region.**
                //
                // The obvious alternative is to build an empty peer and let Raft discover the gap
                // — the follower rejects, the leader backs off past its own log start and offers
                // a snapshot. It cannot work here: a store with no region has no peer to reject
                // *with*, so it drops the message, the leader sees no rejection, backs off to
                // nothing, and offers nothing. The traffic itself is the signal, so the traffic
                // is what this acts on.
                //
                // A store that has been *removed* from a region and receives a stray message asks
                // too, and is refused: the sender checks the asking peer against the region's
                // membership, which is the right place for that check to live.
                let index = Self::snapshot_announcement(&message.message).unwrap_or(0);
                // The peer id to ask *as* comes from the message's recipient, not from the
                // address book. A region's peer ids are its own — the placement driver allocates
                // one per replica — so the entry a store has for itself in the cluster's address
                // book is a different number, and asking with it is refused by the sender's
                // membership check. Found by `tests/balance.rs`, which uses peer ids that do not
                // happen to equal store ids.
                self.start_snapshot(
                    message.region_id,
                    message.from_store,
                    message.message.recipient(),
                    index,
                );
                continue;
            };
            // **No epoch check here.** Invariant 5 guards *client* requests and admin proposals,
            // where a stale epoch means the caller is addressing a range this store no longer
            // owns. Raft traffic between a region's own peers is a different question: the region
            // id says which region, the peer id says which replica, and staleness within a group
            // is what Raft's own term and index rules exist to settle.
            //
            // Checking it here converts a *transient* disagreement into a permanent partition. A
            // conf change takes effect at different times on different peers by design, so a peer
            // that has not yet applied one stamps the `conf_ver` it has — and a peer that has
            // applied it then drops those messages, which is precisely what stops the first one
            // ever applying it. It cannot catch up until its traffic is accepted, and its traffic
            // is not accepted until it has caught up.
            //
            // Observed, not inferred (`docs/plans/phase-4.md` §19.5): a learner stuck at
            // `applied=0` for the whole run, with its region's log carrying nothing but
            //
            //     dropped a Raft message from a stale epoch
            //         theirs=Epoch { conf_ver: 3, version: 6 }
            //         ours=Epoch { conf_ver: 4, version: 6 }
            //
            // repeated indefinitely — one `conf_ver` apart, from the promotion that peer had not
            // applied and now never could.
            let Some(peer) = state.peer() else {
                tracing::debug!(
                    region_id = message.region_id,
                    "dropped a Raft message for a region this store does not replicate"
                );
                continue;
            };
            // **An `InstallSnapshot` is an announcement and is never stepped.** It carries no data
            // in this design (`docs/DESIGN.md` §6) — the bytes come over their own stream and the
            // receiver does not step the message until they have landed
            // (`docs/plans/phase-4.md` §13.4). Stepping it here let the core `restore` from the
            // metadata alone: the peer's log jumped to the snapshot's index and its membership to
            // the snapshot's `conf_state`, while this store wrote no data and did not move the
            // region record. The `Ready` that carried it then hit a phase-3 stub in `drive` that
            // logged and dropped it, so the core kept a position it had no data for and a
            // membership its own region record disagreed with — which is a peer that answers
            // nothing and stamps an epoch nobody expects (`docs/plans/phase-4.md` §17).
            //
            // A peer that *hosts* the region and has fallen behind past the leader's compaction
            // boundary cannot be caught up by snapshot in v1 at all: `Db::ingest` refuses any
            // overlap, so only a range this store holds nothing in can receive one (§13.2). That
            // limitation is now what happens — the offer is declined and the peer stays behind,
            // visible in PD's heartbeats — rather than something that looked like a repair.
            if let Some(index) = Self::snapshot_announcement(&message.message) {
                // Replacing a region this store already holds is the heaviest thing it can do to
                // one — the peer stops, the range is emptied, and nothing serves that range until
                // the transfer finishes — so it happens only when the log genuinely cannot catch
                // this peer up. Two guards, and both were learned by leaving them out: without
                // them a single stale announcement during a leadership flap tore down a live
                // region and the writes to its range stalled for as long as the refetch took.
                //
                // A peer that **leads** the region is never behind it, and one whose apply index
                // already reaches the announcement has nothing to fetch.
                if peer.is_leader() || peer.applied_index() >= index {
                    tracing::debug!(
                        region_id = message.region_id,
                        index,
                        "ignored a snapshot offer for a region this peer is not behind on"
                    );
                    continue;
                }
                self.start_snapshot(
                    message.region_id,
                    message.from_store,
                    message.message.recipient(),
                    index,
                );
                continue;
            }
            peer.step(message.message).await?;
        }
        Ok(())
    }

    /// Starts fetching a region from the **store** that offered it, unless one is already in
    /// flight.
    ///
    /// Spawned rather than awaited: `receive_raft` answers a `RaftBatch` frame, and holding that
    /// answer open for the length of a snapshot would stall every other region's Raft traffic on
    /// the same connection. The leader keeps announcing until it sees the follower catch up, so
    /// nothing is lost by returning immediately.
    ///
    /// **One at a time per region.** A leader re-announces every heartbeat, and each announcement
    /// would otherwise start another transfer of the same megabytes.
    fn start_snapshot(self: &Arc<Self>, region_id: u64, from_store: u64, as_peer: u64, index: u64) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        {
            let Ok(mut receiving) = self.receiving.lock() else {
                return;
            };
            if !receiving.insert(region_id) {
                return;
            }
        }
        let store = Arc::clone(self);
        runtime.spawn(async move {
            tracing::debug!(region_id, from_store, as_peer, index, "asking for a region");
            match store
                .fetch_snapshot(region_id, from_store, as_peer, index)
                .await
            {
                Ok(()) => tracing::info!(region_id, index, "a region arrived by snapshot"),
                Err(error) => tracing::warn!(
                    region_id,
                    index,
                    %error,
                    "a snapshot did not arrive; the leader will offer it again"
                ),
            }
            if let Ok(mut receiving) = store.receiving.lock() {
                receiving.remove(&region_id);
            }
        });
    }

    /// Pulls a region's contents from `from_peer` and adopts it, in the four durable steps of
    /// `docs/plans/phase-4.md` §13.1.
    ///
    /// Nothing serves the region until the last of them, so a crash anywhere before it leaves a
    /// store that does not host the region at all — and the announcement record it left behind is
    /// what lets the next open clear the keys and the retry start from a clean range.
    async fn fetch_snapshot(
        self: &Arc<Self>,
        region_id: u64,
        from_store: u64,
        as_peer: u64,
        index: u64,
    ) -> std::result::Result<(), ProtoError> {
        let mut stream = self
            .open_snapshot_stream(region_id, from_store, as_peer, index)
            .await?;

        // 1. The header says which region this is and which index it is as of.
        let first = stream
            .next_chunk()
            .await
            .ok_or_else(|| ProtoError::Closed {
                detail: "the snapshot stream ended before its header".to_owned(),
            })??;
        let header = snapshot::SnapshotHeader::decode(&first)?;
        if header.region.id != region_id {
            return Err(ProtoError::invalid(format!(
                "asked for region {region_id} and was sent {}",
                header.region.id
            )));
        }
        // A region this store already hosts is **replaced**, not refused. Refusing it was 4c's
        // limitation and phase-4 acceptance showed it is not an edge case: a peer that falls
        // behind its leader's compaction boundary can only be repaired this way, and until now it
        // could not be repaired at all (`docs/plans/phase-4.md` §18). The old peer is retired
        // first so that nothing is driving the region while its range is emptied and refilled.
        let replacing = self.regions.get(region_id).is_some();
        if replacing {
            tracing::info!(
                region_id,
                index,
                "replacing a region this store already holds with a snapshot of it"
            );
            self.retire_region_now(region_id).await;
        }

        // 2. Announce, before a single key is written.
        self.announce_snapshot(&header).await?;

        // 3. The pairs, chunk by chunk. Each is checked before it is believed.
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk?;
            let db = Arc::clone(&self.db);
            blocking(move || {
                let pairs = snapshot::decode_pairs(&chunk)?;
                snapshot::stage_pairs(&db, &pairs)
            })
            .await?;
        }

        // 4. Adopt, in one batch, so the region becomes complete or stays absent.
        self.adopt_snapshot(&header).await?;
        self.host_region(header.region)?;
        Ok(())
    }

    /// Connects to the peer that offered the snapshot and asks for it.
    async fn open_snapshot_stream(
        &self,
        region_id: u64,
        from_store: u64,
        as_peer: u64,
        index: u64,
    ) -> std::result::Result<esker_proto::StreamResponse, ProtoError> {
        let raft = self
            .raft
            .as_ref()
            .ok_or_else(|| ProtoError::invalid("this store has no peer configuration"))?;
        // By **store**, not by peer: a region's peer ids are allocated per replica and a store's
        // address book knows stores. This is why the batch carries the sender's store id.
        let address = raft
            .peers
            .iter()
            .find(|peer| peer.store_id == from_store)
            .map(|peer| peer.addr)
            .ok_or_else(|| {
                ProtoError::invalid(format!(
                    "no address for store {from_store}, which offered a snapshot"
                ))
            })?;

        // A connection of its own: a snapshot is megabytes and would sit in front of every Raft
        // batch queued behind it on the shared store-pair connection.
        let connection = esker_proto::TcpTransport::connect_with(address, raft.transport).await?;
        connection
            .call_stream(Request::Snapshot(SnapshotRequest {
                region_id,
                index,
                // Which peer this store is **in that region**, taken from the message that
                // announced it. A region's peer ids are its own and are not the store's entry in
                // the cluster's address book.
                peer_id: as_peer,
            }))
            .await
    }

    /// Step 2: the announcement record, written before any key is.
    async fn announce_snapshot(
        &self,
        header: &snapshot::SnapshotHeader,
    ) -> std::result::Result<(), ProtoError> {
        let cf_id = self.raft_cf()?;
        let db = Arc::clone(&self.db);
        let region = header.region.clone();
        let index = header.meta.index;
        blocking(move || {
            // The pending record goes down **first**, and then the range is emptied. A crash
            // between them leaves a record saying this range is mid-replacement, which is what the
            // next open cleans up and retries from; doing it the other way round would leave a
            // cleared range with nothing to say why.
            let mut batch = WriteBatch::new();
            meta::stage_pending_snapshot(&mut batch, cf_id, &region, index);
            db.write(batch, &WriteOptions { sync: true })
                .map_err(|error| crate::error::engine_to_proto(&error))?;
            snapshot::clear_range(&db, &region)
        })
        .await
    }

    /// Step 4: the region's record, the peer's raft state, and the announcement gone — one batch.
    ///
    /// Its log begins after the snapshot's index, and the membership it replays conf changes onto
    /// is the one the snapshot names: the anchor rule of `91de89a`, applied to a peer with no
    /// history at all.
    async fn adopt_snapshot(
        &self,
        header: &snapshot::SnapshotHeader,
    ) -> std::result::Result<(), ProtoError> {
        let cf_id = self.raft_cf()?;
        let db = Arc::clone(&self.db);
        let region = header.region.clone();
        let meta = header.meta.clone();
        blocking(move || {
            let mut batch = WriteBatch::new();
            meta::stage_region(&mut batch, cf_id, &region);
            let state = crate::raft_log::PersistedState {
                hard_state: esker_raft::HardState {
                    term: meta.term,
                    voted_for: None,
                    commit: meta.index,
                },
                conf_state: meta.conf.clone(),
                applied_index: meta.index,
                truncated_index: meta.index,
                truncated_term: meta.term,
            };
            batch.put(
                cf_id,
                &crate::raft_log::state_key(region.id),
                &state.encode(),
            );
            meta::stage_snapshot_done(&mut batch, cf_id, region.id);
            db.write(batch, &WriteOptions { sync: true })
                .map(|_| ())
                .map_err(|error| crate::error::engine_to_proto(&error))
        })
        .await
    }

    /// Serves a plan fragment against this store's columnar copy of a region.
    ///
    /// **The epoch is checked exactly as a row read's is** (invariant 5), from the
    /// [`RequestHeader`] that every request already carries — the fragment body deliberately has
    /// no epoch of its own, because two epochs on one wire is two sources of truth.
    ///
    /// Every negative answer here is a [`FragmentResp::Refused`] and never a `ProtoError`. A
    /// refusal is a *normal* response meaning "fall back to a row scan"; an error frame would
    /// make a stale route or a rolling upgrade look like a fault.
    async fn serve_fragment(
        self: &Arc<Self>,
        header: RequestHeader,
        _request: esker_proto::fragment::FragmentReq,
    ) -> std::result::Result<esker_proto::fragment::FragmentResp, ProtoError> {
        // Routing first, so a fragment addressed to a region this store does not own is answered
        // the same way a row read would be — the epoch check is not optional here.
        let _state = self.regions.route(&header, None)?;
        Ok(esker_proto::fragment::FragmentResp::Refused {
            reason: esker_proto::fragment::RefusalReason::NotColumnar,
            detail: format!(
                "store {} holds region {} as rows, not columns",
                self.store_id(),
                header.region_id
            ),
        })
    }

    /// The `raft` column family's id, or the failure that says the store was opened wrong.
    fn raft_cf(&self) -> std::result::Result<u32, ProtoError> {
        self.db.cf_id(cf::RAFT).ok_or_else(|| {
            ProtoError::internal("the store opened without its `raft` column family")
        })
    }

    /// The index a leader's `InstallSnapshot` names, if this message is one.
    ///
    /// Such a message is an **announcement**, not something to step: the core would restore a
    /// snapshot whose data had not arrived, and the peer would claim an index it did not hold. The
    /// data is fetched first ([`Store::fetch_snapshot`]) and the region is built from it.
    ///
    /// Any other message asks for index zero — "whatever you have" — because a store with no
    /// region has no idea how far behind it is and the sender's own state is the answer.
    fn snapshot_announcement(message: &esker_raft::Message) -> Option<u64> {
        match message {
            esker_raft::Message::InstallSnapshot { snapshot, .. } => Some(snapshot.meta.index),
            _ => None,
        }
    }

    /// Stops replication: the heartbeats, every ticker, every peer's thread, and every store
    /// connection.
    pub fn stop(&self) {
        if let Ok(mut tasks) = self.background.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        if let Ok(tickers) = self.tickers.lock() {
            for ticker in tickers.iter() {
                ticker.abort();
            }
        }
        for state in self.regions.states() {
            if let Some(peer) = state.peer() {
                peer.stop();
            }
        }
        if let Some(transport) = &self.transport {
            transport.shutdown();
        }
        // Last: a worker still holding a region would be applying into a database the caller is
        // about to flush and drop.
        self.drivers.shutdown();
    }

    /// This store's id.
    #[must_use]
    pub fn store_id(&self) -> u64 {
        self.store_id
    }

    /// The region this store hosts, when it hosts exactly one.
    ///
    /// `None` for a store hosting several, for the reason [`Store::peer`] gives: there is no
    /// "the" region to return and choosing one would be a guess. [`Store::regions`] is the
    /// question with an answer in every case.
    #[must_use]
    pub fn region(&self) -> Option<Region> {
        let regions = self.regions.regions();
        match regions.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }

    /// The engine underneath, for the CLI's inspection commands and for tests.
    #[must_use]
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// The limits it applies.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Serves one `RawKv` request, header checks and all.
    ///
    /// Synchronous, because the engine is. [`StoreService`] is what puts it on a blocking
    /// thread; a test can call it directly without a runtime.
    pub fn handle(
        &self,
        header: RequestHeader,
        request: RawKvReq,
    ) -> std::result::Result<RawKvResp, ProtoError> {
        let state = self.regions.route(&header, Some(&request))?;
        let region = state.meta();

        match request {
            // The one request that is a read-modify-write, and so the one that needs the
            // exclusive side of the gate.
            RawKvReq::CompareAndSwap {
                key,
                expected,
                value,
                sync,
            } => {
                region.check_key(&key)?;
                let _gate = self.write_gate.write().map_err(|_| {
                    ProtoError::internal("a thread panicked while holding the write gate")
                })?;
                rawkv::compare_and_swap(&self.db, &key, expected.as_deref(), value.as_deref(), sync)
            }
            other if other.method().is_mutation() => {
                let _gate = self.write_gate.read().map_err(|_| {
                    ProtoError::internal("a thread panicked while holding the write gate")
                })?;
                rawkv::serve(&self.db, region, &self.limits, other)
            }
            read => rawkv::serve(&self.db, region, &self.limits, read),
        }
    }

    /// Serves one `RawKv` request, taking the replicated path when this store has a Raft peer.
    ///
    /// The two paths differ in where a write is ordered and where a read is anchored:
    ///
    /// * **Unreplicated** — the engine is the order, and a read sees whatever is written.
    /// * **Replicated** — only the leader serves. A mutation becomes a command in the Raft log
    ///   and its answer comes from *this peer's own apply*, so a `CompareAndSwap` is decided
    ///   against the state every peer reaches. A read is anchored by a `ReadIndex` round and does
    ///   not run until the state machine has applied through it (`docs/DESIGN.md` §2).
    ///
    /// A follower answers [`ProtoError::NotLeader`] with the peer it believes leads, which is what
    /// the client's region cache learns from.
    pub async fn serve(
        self: &Arc<Self>,
        header: RequestHeader,
        request: RawKvReq,
    ) -> std::result::Result<RawKvResp, ProtoError> {
        let state = self.regions.route(&header, Some(&request))?;

        let Some(peer) = state.peer().map(Arc::clone) else {
            let store = Arc::clone(self);
            return blocking(move || store.handle(header, request)).await;
        };

        // A hint, not an authority: a peer deposed a moment ago still says yes here, and the
        // proposal it accepts on the strength of that is failed by the driver rather than
        // applied. Checking early only saves a round trip through the driver thread.
        if !peer.is_leader() {
            return Err(peer.not_leader());
        }
        state.meta().check_scope(&request)?;

        if let Some(command) = Command::from_request(&request) {
            // An oversized range delete is refused here rather than at apply time: apply must be
            // deterministic, so a refusal there would be a failure of the store on every peer at
            // once (`crate::apply`).
            if let RawKvReq::DeleteRange { start, end, .. } = &request {
                let store = Arc::clone(self);
                let state = Arc::clone(&state);
                let (start, end) = (start.clone(), end.clone());
                blocking(move || {
                    rawkv::count_range(&store.db, state.meta(), &store.limits, &start, &end)
                })
                .await?;
            }
            let applied = peer.propose(&command).await?;
            Ok(crate::apply::response(&request, &applied))
        } else {
            // A linearizable read. `read_index` returns only once the state machine has applied
            // through the index it established, so the engine read below sees at least everything
            // committed when the read was accepted.
            peer.read_index().await?;
            let store = Arc::clone(self);
            blocking(move || rawkv::serve(&store.db, state.meta(), &store.limits, request)).await
        }
    }

    /// Serves one `TxnKv` request (`docs/DESIGN.md` §8, `docs/txn-spec.md`).
    ///
    /// The same two paths as [`Store::serve`], for the same reasons — a read is anchored by a
    /// `ReadIndex` round and a write becomes a command in the log — with one difference that is
    /// the whole of `docs/plans/phase-5.md` §10.1: a transactional write carries the **request**
    /// and is decided at apply, because deciding it here would leave a window two prewrites of
    /// one key could race in.
    ///
    /// `GcSafepoint` takes neither path. It sets a store-local threshold, every store learns it
    /// from the placement driver, and applying it is idempotent and monotonic — so replicating
    /// it would put a number in the log that nothing reads back.
    pub async fn serve_txn(
        self: &Arc<Self>,
        header: RequestHeader,
        request: TxnKvReq,
    ) -> std::result::Result<TxnKvResp, ProtoError> {
        if let TxnKvReq::GcSafepoint { safepoint } = request {
            return Ok(self.set_safepoint(safepoint));
        }

        let state = self
            .regions
            .route_range(&header, Some(crate::region::txn_request_range(&request)))?;
        let Some(peer) = state.peer().map(Arc::clone) else {
            let store = Arc::clone(self);
            return blocking(move || store.handle_txn(&state, request)).await;
        };

        // A hint, not an authority: a peer deposed a moment ago still says yes here, and the
        // proposal it accepts on the strength of that is failed by the driver rather than
        // applied. Checking early only saves a round trip through the driver thread.
        if !peer.is_leader() {
            return Err(peer.not_leader());
        }

        if let Some(command) = crate::txn_command::TxnCommand::from_request(&request) {
            let applied = peer.propose(&Command::Txn(command)).await?;
            return match applied {
                Applied::Txn(response) => Ok(*response),
                // Every other outcome belongs to another command, so reaching one here is a
                // driver that mismatched a proposal with its answer rather than anything a
                // client did.
                other => Err(ProtoError::internal(format!(
                    "a transactional command applied as {other:?}"
                ))),
            };
        }

        // A linearizable read. `read_index` returns only once the state machine has applied
        // through the index it established, so the snapshot below sees at least everything
        // committed when the read was accepted.
        peer.read_index().await?;
        let store = Arc::clone(self);
        blocking(move || store.handle_txn(&state, request)).await
    }

    /// Serves one `TxnKv` request against the engine directly, for a store with no Raft peer
    /// for the region — and for the read half of the replicated path, after its `ReadIndex`.
    ///
    /// Synchronous, because the engine is.
    pub fn handle_txn(
        &self,
        state: &Arc<RegionState>,
        request: TxnKvReq,
    ) -> std::result::Result<TxnKvResp, ProtoError> {
        match request {
            TxnKvReq::Get { key, ts } => {
                state.meta().check_key(&key)?;
                crate::txnkv::get(&self.db, &key, ts)
            }
            TxnKvReq::Scan {
                start,
                end,
                limit,
                ts,
                reverse,
            } => {
                state.meta().check_range(&start, &end)?;
                crate::txnkv::scan(&self.db, &start, &end, limit, ts, reverse)
            }
            TxnKvReq::GcSafepoint { safepoint } => Ok(self.set_safepoint(safepoint)),
            // A write on a store with no peer for this region: no log to put it in, so it is
            // decided and applied here. The decision is the same one apply would make.
            other => {
                let Some(command) = crate::txn_command::TxnCommand::from_request(&other) else {
                    return Err(ProtoError::internal(
                        "a TxnKv request that is neither a read nor a command",
                    ));
                };
                for key in command.keys() {
                    state.meta().check_key(key)?;
                }
                let _gate = self.write_gate.write().map_err(|_| {
                    ProtoError::internal("a thread panicked while holding the write gate")
                })?;
                let mut batch = WriteBatch::new();
                let applied = crate::apply::stage(
                    &self.db,
                    &mut batch,
                    &Command::Txn(command),
                    state.region(),
                )?;
                rawkv::write(&self.db, batch, true)?;
                match applied {
                    Applied::Txn(response) => Ok(*response),
                    other => Err(ProtoError::internal(format!(
                        "a transactional command applied as {other:?}"
                    ))),
                }
            }
        }
    }

    /// Raises this store's garbage-collection safepoint, and answers with the one now in force.
    ///
    /// **Never lowers it.** A safepoint that moved backwards would promise a reader history that
    /// has already been collected, and PD's own safepoint only ever rises — so a lower number
    /// here is a stale message overtaking a fresh one, not a decision.
    fn set_safepoint(&self, safepoint: u64) -> TxnKvResp {
        let now = self.collector.set_published(safepoint);
        // The policy is re-read on every safepoint rather than cached for ever: a `retention`
        // DDL writes a catalog record and bumps nothing, deliberately (ADR 0021 decision 4), so
        // the collector's own next pass is where a change is meant to be picked up. A failure
        // to read it leaves the policy that was working, which keeps more rather than less.
        load_retention(&self.db, &self.collector);
        TxnKvResp::GcSafepoint { safepoint: now }
    }

    /// The garbage-collection safepoint this store is working to.
    #[must_use]
    pub fn safepoint(&self) -> u64 {
        self.collector.published()
    }

    /// The MVCC collector, for a test that wants to see what it is working to.
    #[must_use]
    pub fn collector(&self) -> &Arc<MvccCollector> {
        &self.collector
    }

    /// Compacts the whole `write` column family, so the collector runs over every version now
    /// rather than when the level scores say so.
    ///
    /// For an operator forcing a collection, and for the tests that check one happened: a
    /// safepoint changes nothing until a compaction reads the entries it applies to.
    pub fn compact_write_cf(&self) -> Result<()> {
        self.db.compact_range(cf::WRITE, None, None)?;
        Ok(())
    }

    /// How many `write` records this store holds for one user key.
    ///
    /// The version count MVCC garbage collection is about, which is otherwise invisible from
    /// outside: a read answers with one value however many versions are behind it, so a
    /// collector that ran and a collector that did not look identical through the front door.
    pub fn write_records(&self, user_key: &[u8]) -> Result<u64> {
        let prefix = esker_txn::key::prefix(user_key);
        let mut iter = self.db.iter(cf::WRITE, &ReadOptions::default())?;
        let mut count = 0;
        iter.seek(&prefix);
        while iter.valid() && iter.key().starts_with(&prefix) {
            count += 1;
            iter.next();
        }
        iter.status()?;
        Ok(count)
    }

    /// Flushes every column family, so a caller that is about to stop knows what is on disk.
    pub fn flush(&self) -> Result<()> {
        self.db.flush_all()?;
        Ok(())
    }

    /// A metric from the engine, by the names of `docs/DESIGN.md` §12.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<String> {
        self.db.property(name)
    }
}

/// What a bootstrap needs to know, so the function that does it takes one argument rather than
/// six positional ones.
struct BootstrapOptions<'a> {
    region_id: u64,
    store_id: u64,
    peer_id: u64,
    address: &'a str,
    raft: Option<&'a RaftOptions>,
    pd: Option<&'a Arc<dyn PdClient>>,
}

/// Decides what a store with no region records of its own should host, and writes the records.
///
/// Two shapes, and the difference between them is who answers the question.
///
/// * **With a placement driver**, the answer is PD's: it registers this store and says whether
///   this call is the one that bootstrapped the cluster. Exactly one store in the life of a
///   cluster is told to create region 1 (`docs/DESIGN.md` §7); every other store gets `None` and
///   hosts **nothing** until PD places a region on it — which is 4c's work, and until then a
///   perfectly honest empty store rather than a second claim to the whole key space.
/// * **Without one**, the answer is the options': region 1 covering everything, which is what
///   phase 2's single node and phase 3e's static cluster are and what the CLI still starts.
///
/// A PD that cannot be reached is a hard failure rather than a fallback to the second shape. A
/// store that bootstrapped its own region 1 because PD was down would be a second claim to every
/// key in the cluster, and the two would not find out until a client asked one of them.
///
/// The records are written **fsynced, before the store serves anything**, because they are what
/// every later open reads. A bootstrap that served a request before the record was durable could
/// acknowledge a write into a region that, after a crash, this store no longer believes it has.
fn bootstrap(db: &Arc<Db>, options: &BootstrapOptions<'_>) -> Result<Option<Region>> {
    let region = match options.pd {
        Some(pd) => {
            let answer = pd.bootstrap(&StoreInfo {
                store_id: options.store_id,
                address: options.address.to_owned(),
            })?;
            tracing::info!(
                store_id = options.store_id,
                cluster_id = answer.cluster_id,
                bootstrapping = answer.region.is_some(),
                "registered with the placement driver"
            );
            let Some(region) = answer.region else {
                tracing::info!(
                    store_id = options.store_id,
                    "the cluster already exists; this store hosts nothing until a region is placed on it"
                );
                return Ok(None);
            };
            region
        }
        None => whole_key_space(options),
    };

    let cf_id = db
        .cf_id(cf::RAFT)
        .ok_or_else(|| StoreError::Bootstrap("the `raft` column family is missing".into()))?;
    let mut batch = WriteBatch::new();
    meta::stage_region(&mut batch, cf_id, &region);
    db.write(batch, &WriteOptions { sync: true })?;
    tracing::info!(
        store_id = options.store_id,
        region_id = region.id,
        peers = region.peers.len(),
        "bootstrapped a region covering the whole key space"
    );
    Ok(Some(region))
}

/// Region 1 as a store with no placement driver makes it: `["", "")`, at the initial epoch.
///
/// A replicated region lists every peer in [`RaftOptions::peers`], not just this store's: a
/// `NotLeader` hint names a *peer*, and only the list turns that into the store a client should
/// send to instead (`docs/DESIGN.md` §10).
fn whole_key_space(options: &BootstrapOptions<'_>) -> Region {
    match options.raft {
        None => Region::bootstrap(options.region_id, options.store_id, options.peer_id),
        Some(raft) => Region {
            id: options.region_id,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            // The address book unless the caller named the voters: a store that others will join
            // bootstraps a region of one and grows it, rather than a region that cannot commit
            // until every listed peer exists.
            peers: raft
                .peers
                .iter()
                .filter(|peer| {
                    raft.bootstrap_voters
                        .as_ref()
                        .is_none_or(|voters| voters.contains(&peer.peer_id))
                })
                .map(|peer| Peer::voter(peer.store_id, peer.peer_id))
                .collect(),
            epoch: esker_proto::Epoch::INITIAL,
        },
    }
}

/// Starts one region's Raft peer: its log storage, and its view of the store-pair transport.
///
/// The voters come from the **region's own peer list**, not from the store's configuration. That
/// is the difference a multi-region store makes: two regions on one store have different
/// membership, and a store-wide voter list would give each of them the other's.
fn start_peer(
    db: &Arc<Db>,
    region: &Region,
    store_id: u64,
    raft: &RaftOptions,
    transport: Arc<crate::transport::RegionTransport>,
    host: Arc<dyn RegionHost>,
    pool: Arc<DriverPool>,
) -> Result<Arc<RaftPeer>> {
    let voters: Vec<u64> = region
        .peers
        .iter()
        .filter(|peer| peer.role == PeerRole::Voter)
        .map(|peer| peer.peer_id)
        .collect();
    // **The learners come too.** Dropping them was the last of phase-4 acceptance's stalls: a peer
    // started from a record that already lists learners — a split child inheriting its parent's,
    // a store reopening, a region adopted from a snapshot — built a core configuration of voters
    // only. The region record then said "peer 21 is a learner" while the Raft core had never heard
    // of peer 21, so the leader had no `Progress` for it, sent it nothing, and never promoted it:
    // a learner at `applied = 0` for the life of the cluster (`docs/plans/phase-4.md` §20).
    let learners: Vec<u64> = region
        .peers
        .iter()
        .filter(|peer| peer.role == PeerRole::Learner)
        .map(|peer| peer.peer_id)
        .collect();
    let peer_id = region
        .peers
        .iter()
        .find(|peer| peer.store_id == store_id)
        .map(|peer| peer.peer_id)
        .ok_or_else(|| {
            StoreError::Bootstrap(format!(
                "region {} has no peer on store {store_id}",
                region.id
            ))
        })?;
    let storage = RaftLogStorage::open(
        Arc::clone(db),
        region.id,
        esker_raft::ConfState {
            voters: voters.clone(),
            learners: learners.clone(),
        },
    )?;
    RaftPeer::start(
        PeerOptions {
            region: region.clone(),
            peer_id,
            voters,
            learners,
            seed: raft.seed,
            compaction: raft.compaction,
        },
        storage,
        transport as Arc<dyn crate::peer::RaftTransport>,
        host,
        pool,
    )
}

/// The store, behind the weak reference a peer's driver holds it by.
///
/// Weak because the store owns the peers and the peers would otherwise own the store. A split
/// arriving after the store has been dropped is a store that is shutting down, and the halves are
/// already durable — the next open reads them.
#[derive(Debug)]
struct StoreHost {
    store: std::sync::Weak<Store>,
}

impl RegionHost for StoreHost {
    fn split_applied(&self, parent: &Region, child: &Region) -> Result<()> {
        let Some(store) = self.store.upgrade() else {
            return Err(StoreError::RegionConflict(format!(
                "region {} split while its store was shutting down",
                parent.id
            )));
        };
        store.adopt_split(parent, child)
    }

    fn conf_change_applied(&self, region: &Region, removed_self: bool) -> Result<()> {
        let Some(store) = self.store.upgrade() else {
            // The store is going away and the record is already durable; the next open reads it.
            return Ok(());
        };
        store.regions.replace(region.clone())?;
        if removed_self {
            store.retire_region(region.id);
        }
        Ok(())
    }
}

/// Runs synchronous engine work on a blocking thread.
///
/// `esker-engine` is synchronous and stays that way, so an `fsync` that takes ten milliseconds
/// must block a blocking thread and not the reactor every other connection is served on.
async fn blocking<T, F>(work: F) -> std::result::Result<T, ProtoError>
where
    F: FnOnce() -> std::result::Result<T, ProtoError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ProtoError::internal(format!("the request task failed: {error}")))?
}

/// The store behind the wire.
#[derive(Debug)]
pub struct StoreService {
    store: Arc<Store>,
}

impl StoreService {
    /// Serves `store`.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self { store })
    }

    /// The store it serves.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }
}

impl Service for StoreService {
    fn call(&self, request: Request) -> BoxFuture<'_, std::result::Result<Reply, ProtoError>> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let (header, request) = match request {
                Request::RawKv { header, request } => (header, request),
                Request::Raft(batch) => {
                    store.receive_raft(batch).await?;
                    return Ok(Reply::Unary(Response::Raft));
                }
                // The connection answers `Hello` itself; one reaching a service means the
                // transport changed underneath us, which is worth an error rather than a
                // shrug.
                Request::Hello(_) => {
                    return Err(ProtoError::invalid(
                        "Hello is handled by the connection, not by the store",
                    ));
                }
                // A follower asking for a region's contents. The answer is a stream, so it
                // returns from here rather than falling through to the `RawKv` path below.
                Request::Snapshot(ask) => {
                    return store.send_snapshot(ask).await;
                }
                // An operator asking for one specific thing (`esker-cli region`).
                Request::Admin(request) => {
                    return store
                        .serve_admin(request)
                        .await
                        .map(|response| Reply::Unary(Response::Admin(response)));
                }
                // A fragment for a region this store holds no columnar copy of. **A refusal, not
                // an error**: `NotColumnar` is a normal answer meaning "the caller's routing is
                // stale, or placement moved — fall back to a row scan", and the planner's
                // fallback is not an error path (ADR 0022 Decision 4, `esker_proto::fragment`).
                // Making it an error frame would make every rolling upgrade look like a fault.
                //
                // Until the apply target is placed on regions (phase 8 unit 3), this is the only
                // answer this store has, and it is the correct one rather than a placeholder.
                Request::Fragment { header, request } => {
                    return store
                        .serve_fragment(header, request)
                        .await
                        .map(|response| Reply::Unary(Response::Fragment(response)));
                }
                // A store is not a placement driver. Answering anything but a refusal — even a
                // helpful-looking one — would let a misconfigured client believe it had reached
                // PD and route the whole cluster from a store's opinion.
                Request::Pd { request, .. } => {
                    return Err(ProtoError::invalid(format!(
                        "{} is a placement-driver method; this is store {}",
                        request.method().name(),
                        store.store_id()
                    )));
                }
                // Transactions take their own path: the decision is made at apply, on every
                // peer alike, so the answer comes back from there rather than from a handler
                // run here (`docs/plans/phase-5.md` §10.1).
                Request::TxnKv { header, request } => {
                    let response = store.serve_txn(header, request).await?;
                    return Ok(Reply::Unary(Response::TxnKv(response)));
                }
            };

            store
                .serve(header, request)
                .await
                .map(|response| Reply::Unary(Response::RawKv(response)))
        })
    }

    fn store_id(&self) -> u64 {
        self.store.store_id()
    }
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreOptions};
    use bytes::Bytes;
    use esker_proto::{Epoch, ProtoError, RawKvReq, RawKvResp, RequestHeader};
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
        (dir, store)
    }

    fn header() -> RequestHeader {
        RequestHeader::new(1, Epoch::INITIAL, 0)
    }

    fn call(store: &Store, request: RawKvReq) -> RawKvResp {
        store.handle(header(), request).unwrap()
    }

    /// `docs/DESIGN.md` §4.8: the store creates the set, at bootstrap, all four. Creating them
    /// as each phase needs them would make every earlier database a migration.
    #[test]
    fn all_four_column_families_exist_after_bootstrap() {
        let (_dir, store) = open();
        for name in esker_engine::cf::BUILTIN {
            assert!(
                store.db().cf_id(name).is_some(),
                "the `{name}` column family was not created"
            );
        }
        assert_eq!(store.db().cf_names().len(), 4);
    }

    /// And they survive a reopen, which is what makes the bootstrap a format decision rather
    /// than a runtime one.
    #[test]
    fn the_column_families_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
            call(&store, RawKvReq::put(&b"k"[..], &b"v"[..]));
        }
        let store = Store::open(dir.path(), StoreOptions::new()).unwrap();
        for name in esker_engine::cf::BUILTIN {
            assert!(store.db().cf_id(name).is_some(), "{name} was lost");
        }
        assert_eq!(
            call(&store, RawKvReq::get(&b"k"[..])),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"v"))
            },
            "a durable write did not survive a reopen"
        );
    }

    #[test]
    fn a_value_written_can_be_read_back() {
        let (_dir, store) = open();
        assert_eq!(
            call(&store, RawKvReq::get(&b"absent"[..])),
            RawKvResp::Get { value: None }
        );

        call(&store, RawKvReq::put(&b"k"[..], &b"v"[..]));
        assert_eq!(
            call(&store, RawKvReq::get(&b"k"[..])),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"v"))
            }
        );

        call(&store, RawKvReq::delete(&b"k"[..]));
        assert_eq!(
            call(&store, RawKvReq::get(&b"k"[..])),
            RawKvResp::Get { value: None }
        );
    }

    /// The client sends raw user bytes; the `'r'` prefix is added here. The proof is that a
    /// key which *is* the namespace byte round trips, and that the stored key is not the one
    /// the client sent.
    #[test]
    fn the_namespace_prefix_is_the_stores_business() {
        let (_dir, store) = open();
        call(&store, RawKvReq::put(&b"r"[..], &b"namespace-shaped"[..]));

        assert_eq!(
            call(&store, RawKvReq::get(&b"r"[..])),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"namespace-shaped"))
            }
        );

        // Stored under `'r' ++ "r"`, and nothing is stored under the bare key the client sent.
        let stored = store
            .db()
            .get(
                esker_engine::cf::DEFAULT,
                b"rr",
                &esker_engine::ReadOptions::default(),
            )
            .unwrap();
        assert_eq!(stored, Some(Bytes::from_static(b"namespace-shaped")));
        let unprefixed = store
            .db()
            .get(
                esker_engine::cf::DEFAULT,
                b"r",
                &esker_engine::ReadOptions::default(),
            )
            .unwrap();
        assert_eq!(unprefixed, None, "the client's key was stored unprefixed");
    }

    #[test]
    fn a_batch_put_is_one_atomic_write_and_batch_get_reads_it_back() {
        let (_dir, store) = open();
        let pairs = vec![
            (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
            (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
        ];
        assert_eq!(
            call(&store, RawKvReq::batch_put(pairs)),
            RawKvResp::BatchPut
        );

        assert_eq!(
            call(
                &store,
                RawKvReq::BatchGet {
                    keys: vec![
                        Bytes::from_static(b"a"),
                        Bytes::from_static(b"missing"),
                        Bytes::from_static(b"b"),
                    ],
                },
            ),
            RawKvResp::BatchGet {
                values: vec![
                    Some(Bytes::from_static(b"1")),
                    None,
                    Some(Bytes::from_static(b"2")),
                ],
            },
            "answers must line up with the keys that were asked, absences included"
        );
    }

    #[test]
    fn a_scan_returns_user_keys_in_order() {
        let (_dir, store) = open();
        for key in ["a", "b", "c", "d"] {
            call(
                &store,
                RawKvReq::put(Bytes::from(key.to_owned()), Bytes::from(key.to_uppercase())),
            );
        }

        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b""[..], &b""[..], 0)) else {
            panic!("not a scan response");
        };
        let keys: Vec<&[u8]> = pairs.iter().map(|(key, _)| &key[..]).collect();
        assert_eq!(
            keys,
            [b"a", b"b", b"c", b"d"],
            "keys came back prefixed or out of order"
        );

        // Bounded, and the end is exclusive.
        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b"b"[..], &b"d"[..], 0))
        else {
            panic!("not a scan response");
        };
        let keys: Vec<&[u8]> = pairs.iter().map(|(key, _)| &key[..]).collect();
        assert_eq!(keys, [b"b", b"c"]);

        // A limit truncates rather than failing.
        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b""[..], &b""[..], 2)) else {
            panic!("not a scan response");
        };
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn a_reverse_scan_walks_down_from_its_upper_bound() {
        let (_dir, store) = open();
        for key in ["a", "b", "c", "d"] {
            call(
                &store,
                RawKvReq::put(Bytes::from(key.to_owned()), &b"v"[..]),
            );
        }

        let request = RawKvReq::Scan {
            start: Bytes::from_static(b"d"),
            end: Bytes::from_static(b"a"),
            limit: 0,
            reverse: true,
        };
        let RawKvResp::Scan { pairs } = call(&store, request) else {
            panic!("not a scan response");
        };
        let keys: Vec<&[u8]> = pairs.iter().map(|(key, _)| &key[..]).collect();
        assert_eq!(
            keys,
            [b"c", b"b", b"a"],
            "a reverse scan is [low, high) walked downwards, with the upper bound excluded"
        );
    }

    /// A scan of the whole region must not leave the `'r'` namespace, even when the next
    /// namespace has keys in it. This is the test that fails if the upper bound is left open.
    #[test]
    fn a_scan_stops_at_the_end_of_the_namespace() {
        let (_dir, store) = open();
        call(&store, RawKvReq::put(&b"z"[..], &b"raw"[..]));

        // A transactional key, written straight to the engine as `esker-txn` will in phase 5.
        let mut batch = esker_engine::WriteBatch::new();
        batch.put(
            store.db().cf_id(esker_engine::cf::DEFAULT).unwrap(),
            &esker_keys::prefix::txn_key(b"a", 1),
            b"not raw",
        );
        store
            .db()
            .write(batch, &esker_engine::WriteOptions::synced())
            .unwrap();

        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b""[..], &b""[..], 0)) else {
            panic!("not a scan response");
        };
        assert_eq!(pairs.len(), 1, "the scan escaped its namespace: {pairs:?}");
        assert_eq!(pairs[0].0, Bytes::from_static(b"z"));
    }

    #[test]
    fn delete_range_removes_exactly_its_range() {
        let (_dir, store) = open();
        for key in ["a", "b", "c", "d", "e"] {
            call(
                &store,
                RawKvReq::put(Bytes::from(key.to_owned()), &b"v"[..]),
            );
        }

        assert_eq!(
            call(&store, RawKvReq::delete_range(&b"b"[..], &b"d"[..])),
            RawKvResp::DeleteRange { deleted: 2 }
        );

        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b""[..], &b""[..], 0)) else {
            panic!("not a scan response");
        };
        let keys: Vec<&[u8]> = pairs.iter().map(|(key, _)| &key[..]).collect();
        assert_eq!(
            keys,
            [b"a", b"d", b"e"],
            "DeleteRange removed the wrong keys — the engine's own range tombstone would have \
             removed only `b`"
        );
    }

    /// ADR 0006: the range is bounded, and going past it is a typed limitation rather than a
    /// silent partial delete.
    #[test]
    fn an_oversized_delete_range_is_refused_as_a_limitation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                limits: crate::rawkv::Limits {
                    max_delete_range_keys: 4,
                    ..crate::rawkv::Limits::new()
                },
                ..StoreOptions::new()
            },
        )
        .unwrap();

        for index in 0..8u8 {
            call(&store, RawKvReq::put(vec![b'k', index], &b"v"[..]));
        }

        let error = store
            .handle(header(), RawKvReq::delete_range(&b""[..], &b""[..]))
            .unwrap_err();
        assert!(matches!(error, ProtoError::Unsupported { .. }), "{error:?}");

        // And nothing was deleted: the refusal happens before the batch is written.
        let RawKvResp::Scan { pairs } = call(&store, RawKvReq::scan(&b""[..], &b""[..], 0)) else {
            panic!("not a scan response");
        };
        assert_eq!(pairs.len(), 8, "a refused DeleteRange deleted something");
    }

    #[test]
    fn compare_and_swap_writes_only_on_a_match() {
        let (_dir, store) = open();

        // Absent, and the caller says so: the swap happens.
        assert_eq!(
            call(
                &store,
                RawKvReq::compare_and_swap(&b"k"[..], None, Some(Bytes::from_static(b"first"))),
            ),
            RawKvResp::CompareAndSwap {
                swapped: true,
                previous: None
            }
        );

        // Present, and the caller guesses wrong: nothing is written, and it is told what is
        // actually there.
        assert_eq!(
            call(
                &store,
                RawKvReq::compare_and_swap(
                    &b"k"[..],
                    Some(Bytes::from_static(b"wrong")),
                    Some(Bytes::from_static(b"second")),
                ),
            ),
            RawKvResp::CompareAndSwap {
                swapped: false,
                previous: Some(Bytes::from_static(b"first")),
            }
        );
        assert_eq!(
            call(&store, RawKvReq::get(&b"k"[..])),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"first"))
            }
        );

        // A `None` value deletes on a match.
        assert_eq!(
            call(
                &store,
                RawKvReq::compare_and_swap(&b"k"[..], Some(Bytes::from_static(b"first")), None),
            ),
            RawKvResp::CompareAndSwap {
                swapped: true,
                previous: Some(Bytes::from_static(b"first")),
            }
        );
        assert_eq!(
            call(&store, RawKvReq::get(&b"k"[..])),
            RawKvResp::Get { value: None }
        );
    }

    /// The property the exclusive gate exists for: many threads incrementing one counter with
    /// compare-and-swap must not lose an update.
    #[test]
    fn concurrent_compare_and_swap_does_not_lose_an_update() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 25;

        let (_dir, store) = open();
        call(&store, RawKvReq::put(&b"counter"[..], &b"0"[..]));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let store = Arc::clone(&store);
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        loop {
                            let RawKvResp::Get { value } = store
                                .handle(header(), RawKvReq::get(&b"counter"[..]))
                                .unwrap()
                            else {
                                panic!("not a get response");
                            };
                            let current = value.expect("the counter vanished");
                            let number: u64 =
                                std::str::from_utf8(&current).unwrap().parse().unwrap();
                            let next = Bytes::from((number + 1).to_string());

                            let response = store
                                .handle(
                                    header(),
                                    RawKvReq::compare_and_swap(
                                        &b"counter"[..],
                                        Some(current),
                                        Some(next),
                                    )
                                    .unsynced(),
                                )
                                .unwrap();
                            if matches!(response, RawKvResp::CompareAndSwap { swapped: true, .. }) {
                                break;
                            }
                        }
                    }
                });
            }
        });

        let RawKvResp::Get { value } = call(&store, RawKvReq::get(&b"counter"[..])) else {
            panic!("not a get response");
        };
        let total: usize = std::str::from_utf8(&value.unwrap())
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            total,
            THREADS * PER_THREAD,
            "an update was lost: compare-and-swap is not atomic against concurrent writers"
        );
    }

    /// Invariant 5, on every request, even with one region that never moves.
    #[test]
    fn a_stale_epoch_is_refused_with_the_current_region() {
        let (_dir, store) = open();
        let stale = RequestHeader::new(1, Epoch::new(1, 0), 0);
        let error = store.handle(stale, RawKvReq::get(&b"k"[..])).unwrap_err();
        match error {
            ProtoError::EpochNotMatch { current_regions } => {
                assert_eq!(current_regions[0].epoch, Epoch::INITIAL);
                assert_eq!(current_regions[0].id, 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_request_for_another_region_is_refused() {
        let (_dir, store) = open();
        let elsewhere = RequestHeader::new(42, Epoch::INITIAL, 0);
        assert_eq!(
            store
                .handle(elsewhere, RawKvReq::get(&b"k"[..]))
                .unwrap_err(),
            ProtoError::RegionNotFound { region_id: 42 }
        );
    }

    /// A write acknowledged with `sync = true` is on disk; the default is durable, and the
    /// un-durable path is the opt-in (`CLAUDE.md` invariant 1).
    #[test]
    fn writes_default_to_durable() {
        assert!(RawKvReq::put(&b"k"[..], &b"v"[..]).is_sync());
        assert!(!RawKvReq::put(&b"k"[..], &b"v"[..]).unsynced().is_sync());
    }

    /// The engine must not sync behind the request's back, or `sync = false` is a wire field
    /// with no reachable behaviour and invariant 1's opt-out is a promise the store cannot
    /// keep. `WalSyncMode::PerWrite` — the engine's own default — does exactly that, which is
    /// why the store overrides it.
    ///
    /// This is a configuration pin rather than a behavioural test on purpose. Nothing
    /// observable in-process distinguishes the two settings: a clean reopen finds an unsynced
    /// write too, because a clean exit leaves the log intact whether or not it was flushed.
    /// What distinguishes them is a `kill -9` (the crash lane's) and the benchmark's write
    /// throughput, where the ratio is about three orders of magnitude.
    #[test]
    fn the_engine_adds_no_sync_of_its_own() {
        assert_eq!(
            StoreOptions::new().engine.wal_sync_mode,
            esker_engine::WalSyncMode::Never,
            "the engine would sync regardless of the request's `sync` flag"
        );
    }

    /// And an unsynced write is still a write: the flag changes when it is acknowledged, never
    /// whether it happened.
    #[test]
    fn an_unsynced_write_is_still_applied() {
        let (_dir, store) = open();
        call(&store, RawKvReq::put(&b"fast"[..], &b"v"[..]).unsynced());
        assert_eq!(
            call(&store, RawKvReq::get(&b"fast"[..])),
            RawKvResp::Get {
                value: Some(Bytes::from_static(b"v"))
            }
        );
    }

    /// The stall metrics `docs/DESIGN.md` §12 names have to be reachable, because the phase's
    /// load test asserts `ServerIsBusy` corresponds to a real one.
    #[test]
    fn the_engine_metrics_are_reachable_through_the_store() {
        let (_dir, store) = open();
        for name in [
            "esker.write-stalls",
            "esker.write-slowdowns",
            "esker.last-sequence",
        ] {
            assert!(store.property(name).is_some(), "{name} is not reported");
        }
    }
}
