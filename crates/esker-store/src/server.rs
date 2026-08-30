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

use esker_engine::{Db, LocalFileSystem, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
use esker_proto::{
    BoxFuture, Peer, PeerRole, ProtoError, RaftBatch, RawKvReq, RawKvResp, Region, Reply, Request,
    RequestHeader, Response, Service, TransportConfig,
};

use crate::apply::Command;
use crate::error::{Result, StoreError};
use crate::heartbeat::{Heartbeats, RegionReport, StoreReport};
use crate::meta;
use crate::pd::{PdClient, StoreInfo};
use crate::peer::{PeerOptions, RaftPeer};
use crate::raft_log::RaftLogStorage;
use crate::rawkv::{self, Limits};
use crate::region::RegionMeta;
use crate::regions::{RegionMap, RegionState};
use crate::transport::{PeerAddress, StoreAddress, StoreTransport};

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
    /// What one heartbeat tick is worth. The intervals of `docs/DESIGN.md` §14 are counted in
    /// these ([`crate::heartbeat::Heartbeats`]), so this is the resolution of the schedule and
    /// not its period.
    pub heartbeat_tick: std::time::Duration,
}

/// How this store's region is replicated.
#[derive(Debug, Clone)]
pub struct RaftOptions {
    /// Every peer of the region, this store's included. The peer with `peer_id ==
    /// `[`StoreOptions::peer_id`] is this one and is never connected to.
    pub peers: Vec<PeerAddress>,
    /// Seed for the election-timeout RNG. A whole cluster may share one: the peer id selects the
    /// stream (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
    pub seed: u64,
    /// What one Raft tick is worth. `esker-raft` counts ticks and never reads a clock; this is
    /// the only place a wall clock touches consensus (`docs/DESIGN.md` §14).
    pub tick: std::time::Duration,
    /// How the connections between stores are configured.
    pub transport: TransportConfig,
}

impl RaftOptions {
    /// Replication across `peers`, with the project's defaults.
    #[must_use]
    pub fn new(peers: Vec<PeerAddress>, seed: u64) -> Self {
        Self {
            peers,
            seed,
            tick: std::time::Duration::from_millis(esker_raft::TICK_MS),
            transport: TransportConfig::new(),
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
            engine: Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
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
    /// Shared by every mutation, exclusive for `CompareAndSwap`. See the module docs.
    ///
    /// Only used by a store with no Raft peer. Once there is one, the Raft log is the
    /// serialisation point and read-modify-write happens at apply time, on every peer alike.
    write_gate: RwLock<()>,
    /// One connection per store pair, shared by every region. Kept so it can be shut down with
    /// the store; each region's view holds its own reference.
    transport: Option<Arc<StoreTransport>>,
    /// One per replicated region: the timer that feeds its driver thread.
    ///
    /// `TODO(phase-4d)`: one timer per region is one task per region. At fifty regions that is
    /// fifty timers where one wheel would do, which is the same sharding decision as the apply
    /// worker's and belongs with it.
    tickers: Vec<tokio::task::JoinHandle<()>>,
    /// The task that reports to the placement driver, when there is one.
    heartbeats: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
            raft,
            pd,
            address,
            heartbeat_tick,
        } = options;
        let db = Db::open_with(path, engine, Arc::new(LocalFileSystem::new()), &cf::BUILTIN)?;

        for name in cf::BUILTIN {
            if db.cf_id(name).is_none() {
                return Err(StoreError::Bootstrap(format!(
                    "the `{name}` column family is missing after open"
                )));
            }
        }

        let db = Arc::new(db);

        // What this store hosts is what its own `'m'` records say — never what its configuration
        // says on a later open, and never what the placement driver currently believes. A
        // database with none is a fresh one, and only then is `options` a bootstrap.
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

        let regions = RegionMap::new();
        let mut tickers = Vec::new();
        for region in hosted {
            // A record whose peer list does not name this store is what a crash between
            // `RemovePeer` applying and this store deleting its data leaves behind
            // (`docs/plans/phase-4.md` §6, race 3). Starting a peer for it would put a voter back
            // into a group that has already removed it.
            if !region.peers.iter().any(|peer| peer.store_id == store_id) {
                tracing::warn!(
                    store_id,
                    region_id = region.id,
                    "a region record on this store does not list it as a peer; not started"
                );
                continue;
            }
            let state = match (&raft, &transport) {
                (Some(raft), Some(transport)) => {
                    let peer = start_peer(&db, &region, store_id, raft, transport)?;
                    tickers.push(peer.spawn_ticker(raft.tick));
                    RegionState::replicated(RegionMeta::new(region), peer)
                }
                _ => RegionState::unreplicated(RegionMeta::new(region)),
            };
            regions.insert(state)?;
        }

        tracing::info!(
            store_id,
            regions = regions.len(),
            replicated = transport.is_some(),
            column_families = ?cf::BUILTIN,
            "store opened"
        );

        let store = Arc::new(Self {
            db,
            regions,
            store_id,
            limits,
            write_gate: RwLock::new(()),
            transport,
            tickers,
            heartbeats: std::sync::Mutex::new(None),
        });
        if let Some(pd) = pd {
            store.spawn_heartbeats(pd, heartbeat_tick);
        }
        Ok(store)
    }

    /// Starts the task that reports to the placement driver.
    ///
    /// The schedule itself counts ticks and reads no clock ([`crate::heartbeat`]); this is the
    /// edge that turns a `tokio` interval into those ticks — the same shape as a peer's ticker,
    /// and for the same reason. A tick that is late because the process was busy is skipped
    /// rather than replayed: `MissedTickBehavior::Delay` would make a stalled store send a burst
    /// of identical heartbeats the moment it recovered.
    fn spawn_heartbeats(self: &Arc<Self>, pd: Arc<dyn PdClient>, tick: std::time::Duration) {
        let store = Arc::downgrade(self);
        let store_id = self.store_id;
        let task = tokio::spawn(async move {
            let mut beats = Heartbeats::new(pd, store_id, tick);
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(store) = store.upgrade() else {
                    return;
                };
                let report = store.report();
                drop(store);
                beats.tick(&report);
            }
        });
        if let Ok(mut slot) = self.heartbeats.lock() {
            *slot = Some(task);
        }
    }

    /// What this store looks like right now, for one heartbeat round.
    ///
    /// Every number is read without waiting on a driver thread: the regions come from the map and
    /// the Raft numbers from what each driver publishes. A round that asked each peer in turn
    /// would queue behind whatever `fsync` each was in the middle of, and a store with fifty
    /// regions would report a state fifty `fsync`s old.
    #[must_use]
    pub fn report(&self) -> StoreReport {
        let regions = self
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
                        approximate_size: 0,
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
        // TODO(phase-4b): `approximate_size` per region from SST properties plus the memtable,
        // and `applied_bytes` from the same measurement. 4b's split trigger is the first thing
        // that needs a number here rather than a zero.
        StoreReport {
            capacity: 0,
            available: 0,
            applied_bytes: 0,
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
    pub async fn receive_raft(&self, batch: RaftBatch) -> std::result::Result<(), ProtoError> {
        if self.transport.is_none() {
            return Err(ProtoError::invalid(
                "this store does not replicate; it has no Raft peer to receive a batch",
            ));
        }
        for message in batch.messages {
            let Some(state) = self.regions.get(message.region_id) else {
                tracing::debug!(
                    region_id = message.region_id,
                    "dropped a Raft message for a region this store does not host"
                );
                continue;
            };
            // Invariant 5, applied to Raft traffic. A batch carries messages for many regions
            // now, so the epoch it is checked against is the one of the region it names — not
            // the store's, which is not a thing a store has.
            if message.epoch.is_stale_against(state.region().epoch) {
                tracing::debug!(
                    region_id = message.region_id,
                    "dropped a Raft message from a stale epoch"
                );
                continue;
            }
            let Some(peer) = state.peer() else {
                tracing::debug!(
                    region_id = message.region_id,
                    "dropped a Raft message for a region this store does not replicate"
                );
                continue;
            };
            peer.step(message.message).await?;
        }
        Ok(())
    }

    /// Stops replication: the heartbeats, every ticker, every peer's thread, and every store
    /// connection.
    pub fn stop(&self) {
        if let Ok(mut task) = self.heartbeats.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
        for ticker in &self.tickers {
            ticker.abort();
        }
        for state in self.regions.states() {
            if let Some(peer) = state.peer() {
                peer.stop();
            }
        }
        if let Some(transport) = &self.transport {
            transport.shutdown();
        }
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
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            peers: raft
                .peers
                .iter()
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
    transport: &Arc<StoreTransport>,
) -> Result<Arc<RaftPeer>> {
    let voters: Vec<u64> = region
        .peers
        .iter()
        .filter(|peer| peer.role == PeerRole::Voter)
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
        esker_raft::ConfState::from_voters(voters.clone()),
    )?;
    RaftPeer::start(
        PeerOptions {
            region_id: region.id,
            peer_id,
            voters,
            seed: raft.seed,
        },
        storage,
        transport.for_region(region.id, region.epoch, &raft.peers)
            as Arc<dyn crate::peer::RaftTransport>,
    )
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
