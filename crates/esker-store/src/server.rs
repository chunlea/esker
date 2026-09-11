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
use crate::census;
use crate::columnar::ColumnarOptions;
use crate::columnar::region::ColumnarSlot;
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

/// How long a snapshot ask waits for the conf change that placed its peer to apply here.
///
/// An apply is microseconds behind its commit, so this is a bound on a mistake rather than a
/// latency anyone pays: see [`Store::await_record_of`]. Below the shortest region-heartbeat
/// interval any test uses, so an expired wait is answered by the leader's next announcement
/// rather than stacking behind it.
const RECORD_CATCHUP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// How often that wait looks. The region record is replaced under a lock by an apply, and there is
/// nothing to subscribe to; two milliseconds is far below what it is waiting for.
const RECORD_CATCHUP_POLL: std::time::Duration = std::time::Duration::from_millis(2);

/// How long between attempts, once one has been refused.
///
/// Short, because the expensive waiting already happened inside the attempt: a `RemotePd` spends
/// its own redirect budget before it returns. This is the interval for the in-process driver,
/// where an attempt costs nothing at all.
const PD_LEADER_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How many consecutive leaderless heartbeat rounds make a region worth asking PD about.
///
/// A throttle rather than a bound; see [`Store::sweep_orphaned_regions`] for why the safety is
/// elsewhere. At the default `heartbeat_tick` of one raft tick this is five seconds, which is
/// several election timeouts and still healing in the same breath as a rebalance.
const ORPHAN_PROBE_ROUNDS: u32 = 50;

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
    /// How long the **first** open waits for a placement-driver group to produce a leader.
    ///
    /// Defaults to [`crate::PD_LEADER_WAIT`], and is a field for the reason `region_heartbeat`
    /// is one:
    /// a test that waited thirty seconds for the bound to expire would not be run.
    pub pd_leader_wait: std::time::Duration,
    /// How often each region's peer says what it believes, or `None` for never.
    ///
    /// **Off unless asked for**, and it is a diagnostic rather than a part of how the store
    /// works: nothing reads what it emits, and turning it on changes nothing but the log. See
    /// [`crate::census`] for what one round costs and why it is a cadence and never an event.
    pub region_census: Option<std::time::Duration>,
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
    /// The TLS those connections use, if any.
    ///
    /// Beside `transport` rather than inside it: `TransportConfig` is `Copy` and `Eq` and is
    /// passed by value into every delivery task, and a configuration behind an `Arc` is neither
    /// ([ADR 0055](../../../docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)).
    /// Disabled by default, which is every store that does not ask for it.
    pub tls: esker_proto::transport::RpcTls,
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
            tls: esker_proto::transport::RpcTls::disabled(),
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
            pd_leader_wait: crate::PD_LEADER_WAIT,
            region_census: None,
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
    /// `TODO(debt-c6 #1)`: one timer per region is one task per region. At fifty regions that is
    /// fifty timers where one wheel would do, which is the same sharding decision as the apply
    /// worker's and belongs with it (`docs/plans/debt-c6.md` §4).
    ///
    /// Behind a lock because a split adds one, from the parent's driver thread.
    tickers: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The store-wide tasks: the heartbeat schedule and the split checker, when there is a
    /// placement driver for either to talk to.
    background: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Regions a snapshot is being fetched for. A leader re-announces every heartbeat, and each
    /// announcement would otherwise start another transfer of the same megabytes.
    receiving: std::sync::Mutex<std::collections::BTreeSet<u64>>,
    /// Where this store's data lives. Kept because a columnar copy is written **beside** the
    /// engine rather than inside it: its runs are immutable files of their own format, swept by
    /// their own manifest ([`crate::columnar::runs`]).
    data_dir: std::path::PathBuf,
    /// The filesystem the engine was opened on. A columnar run goes through the same one, so a
    /// store tiering its SSTs tiers these too, and a test injecting faults injects them here.
    fs: Arc<dyn FileSystem>,
    /// One per region that has ever had a peer: the columnar copy a `ColumnarLearner` builds.
    ///
    /// Held by the store rather than by the region map because both sides of the seam need it —
    /// the peer's driver appends to it, the fragment service reads it — and neither owns the
    /// other.
    columnar: std::sync::Mutex<std::collections::BTreeMap<u64, Arc<ColumnarSlot>>>,
}

/// Clears the keys an interrupted snapshot left behind, before anything looks at the key space.
///
/// A snapshot that was part-way in when the store stopped left keys no region covers. They go
/// before the region map is built, so the retry finds the range it was promised and `may_receive`
/// does not refuse it (`docs/plans/phase-4.md` §13.1).
fn discard_interrupted_snapshots(db: &Arc<Db>) -> Result<()> {
    for (region, index) in meta::load_pending_snapshots(db)? {
        let removed = snapshot::discard_range(db, &region)?;
        let mut batch = WriteBatch::new();
        let cf_id = db
            .cf_id(cf::RAFT)
            .ok_or_else(|| StoreError::Bootstrap("the `raft` column family is missing".into()))?;
        meta::stage_snapshot_done(&mut batch, cf_id, region.id);
        db.write(batch, &WriteOptions::synced())?;
        tracing::warn!(
            region_id = region.id,
            index,
            removed,
            "a snapshot was interrupted; its partial data was discarded"
        );
    }
    Ok(())
}

/// Finishes one region's retirement: the range, the columnar tree, and the announcement.
///
/// The whole of what a retirement does to **data**, in one blocking function, because it has two
/// callers that could not otherwise share it: the live path, where a conf change has just removed
/// this store from the region, and [`reclaim_interrupted_retirements`] at open, where a crash
/// interrupted the live path. A second implementation of a delete is a second chance to get a
/// delete wrong.
///
/// # Gate 2: no region this store still hosts may overlap the range
///
/// `overlaps` is the ids of the hosted regions that cover it, and a non-empty one is the case
/// that catches a *stale* record: a parent whose split narrowed it, retired against the range it
/// had before, would delete the child's keys. The announcement is then **forgotten**, because the
/// range is not orphaned — it belongs to a region that is being served — and an announcement kept
/// for it would ask the same refused question at every open for the life of the store.
///
/// # What is idempotent, and why every step has to be
///
/// A crash may land anywhere in here and the next open runs the whole thing again.
/// `snapshot::clear_range` returns early on an empty range and `FileSystem::remove_dir_all` treats
/// an absent directory as done, so a second pass over a finished retirement is a pair of cheap
/// reads and one delete of the announcement.
///
/// # The announcement outlives a failure, deliberately
///
/// It is removed only when `clear_range` reports the range **provably empty**. A clear that failed
/// — a tombstone still above the compaction floor because something holds an engine snapshot open
/// — leaves the keys and leaves the announcement, and the next open tries again. Removing it on
/// the strength of having tried is how a reclamation that failed becomes the leak this record
/// exists to end.
fn finish_retirement(
    db: &Db,
    fs: &Arc<dyn FileSystem>,
    data_dir: &Path,
    region: &Region,
    overlaps: &[u64],
) {
    let region_id = region.id;
    if !overlaps.is_empty() {
        tracing::warn!(
            region_id,
            ?overlaps,
            "a retired region's range is still covered by a region this store hosts, so it is \
             left alone rather than emptied under its owner"
        );
        forget_retirement(db, region_id);
        return;
    }

    let cleared = match snapshot::clear_range(db, region) {
        Ok(()) => {
            tracing::info!(
                region_id,
                "a retired region's range was reclaimed in every column family"
            );
            true
        }
        // Loud and harmless: the range keeps its keys, which is where it was before, and it keeps
        // its announcement, so the next open comes back for them.
        Err(error) => {
            tracing::warn!(region_id, %error, "a retired region's range was not reclaimed");
            false
        }
    };

    // **Unconditionally, and not chained onto the clear's success.** The columnar copy is derived
    // from the range and belongs to a region that is gone either way; a range clear that failed is
    // a reason to keep the *keys*, never a reason to keep a copy of them.
    reclaim_columnar_tree(fs, data_dir, region_id);

    if cleared {
        forget_retirement(db, region_id);
    }
}

/// Removes a retired region's columnar tree: a directory of immutable run files and a manifest of
/// its own (`crate::columnar::runs`), one per region id, written **beside** the engine rather than
/// inside it — so nothing the engine reclaims touches it and it has to be reclaimed by name. Left
/// behind, it is the same unbounded growth as the range, on every store that ever held a columnar
/// learner.
///
/// Per region id, which is what makes it safe without a second look at the region map: PD never
/// reuses an id, and a split child gets its own directory. Sweeping `<data_dir>/columnar` would be
/// a different and much worse operation.
fn reclaim_columnar_tree(fs: &Arc<dyn FileSystem>, data_dir: &Path, region_id: u64) {
    let root = data_dir.join("columnar");
    let dir = root.join(region_id.to_string());
    let removed = fs.remove_dir_all(&dir).and_then(|()| {
        // The removal is not durable until the directory that held the entry is synced. A parent
        // that has never existed is not an error: this store has never written a columnar run.
        match fs.fsync_dir(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    });
    match removed {
        Ok(()) => tracing::debug!(region_id, "a retired region's columnar copy was removed"),
        Err(error) => {
            tracing::warn!(region_id, %error, "a retired region's columnar copy was not removed");
        }
    }
}

/// Drops the announcement that a region's range was being reclaimed.
///
/// Synced, on the same terms as the batch that wrote it: an announcement that survived its own
/// completion costs one wasted sweep at the next open, which is cheap and correct, and losing one
/// that had *not* completed costs the range for ever.
fn forget_retirement(db: &Db, region_id: u64) {
    let Some(cf_id) = db.cf_id(cf::RAFT) else {
        tracing::warn!(region_id, "the `raft` column family is missing");
        return;
    };
    let mut batch = WriteBatch::new();
    meta::stage_retired(&mut batch, cf_id, region_id);
    if let Err(error) = db.write(batch, &WriteOptions::synced()) {
        // The announcement stays, so the next open sweeps a range that is already empty: two
        // reads and a second attempt at this write.
        tracing::warn!(region_id, %error, "a finished retirement was not marked finished");
    }
}

/// Finishes every retirement a crash interrupted, before this store serves anything.
///
/// # The window this closes
///
/// Retiring a region is two durable steps: the batch that destroys its Raft state and its `'m'`
/// record, and the clear of its range. Between them the range's keys belong to nothing — and
/// until the announcement existed, *nothing on disk said which keys they were*, because the `'m'`
/// record that named the range was the first casualty. A crash there orphaned a whole region's
/// data permanently on every one of the three column families, on a path every rebalance takes.
/// The old code read as though this were benign — the state is "recoverable, and never served" —
/// which is true of reads and false of disk.
///
/// # Where it runs, and why after the peers are started
///
/// After [`Store::host_region`], not before, because gate 2 asks the **region map** whether
/// anything this store serves covers the range, and the map is empty until the regions are
/// hosted. Running it first would answer "nothing overlaps" for every retirement and clear a
/// range under its owner, which is the one outcome the gate exists to prevent. Nothing can be
/// written into the range in the meantime: a peer only writes inside its own region, so a peer
/// that could reach these keys is one that makes the gate refuse.
fn reclaim_interrupted_retirements(store: &Arc<Store>) -> Result<()> {
    for region in meta::load_retiring(&store.db)? {
        let overlaps: Vec<u64> = store
            .regions
            .overlapping(&region.start_key, &region.end_key)
            .iter()
            .map(|other| other.id)
            .collect();
        tracing::warn!(
            region_id = region.id,
            "a retirement was interrupted; finishing it before serving"
        );
        finish_retirement(&store.db, &store.fs, &store.data_dir, &region, &overlaps);
    }
    Ok(())
}

/// Reads the retention policy out of the catalog and hands it to the collector.
///
/// A failure leaves the policy that was working rather than falling back to the default,
/// because the two differ in the direction that matters: an unreadable catalog should make the
/// collector keep *more* than it would have, never less. It is re-read on every safepoint, not
/// cached, because a `retention` DDL writes a record and bumps no version — deliberately
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 4) — so the collector's own next
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
            pd_leader_wait,
            region_heartbeat,
            region_census,
            split,
        } = options;
        // The collector is built before the engine, because the engine has to be opened *with*
        // it: a compaction filter is a column-family setting and this one belongs to `write`
        // alone, whose entries are the only ones it understands ([`crate::gc`]).
        let collector = Arc::new(MvccCollector::new(
            RetentionPolicy::uniform(DEFAULT_RETENTION_MS),
            0,
        ));
        let data_dir = path.as_ref().to_path_buf();
        let runs_fs = Arc::clone(&fs);
        let db = Arc::new(open_engine(path, engine, fs, &collector)?);
        load_retention(&db, &collector);

        discard_interrupted_snapshots(&db)?;

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
                    pd_leader_wait,
                },
            )?);
        }

        // A replicated store needs a runtime: the transport's tasks and the tickers live in one.
        // A store with no Raft options is exactly phase 2's and needs nothing.
        let transport = raft.as_ref().map(|raft| {
            StoreTransport::spawn_with_tls(
                store_id,
                &StoreAddress::from_peers(&raft.peers),
                raft.transport,
                &raft.tls,
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
            data_dir,
            fs: runs_fs,
            columnar: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        });

        // The peers are started only now, because each of them needs a handle back to the store:
        // a split reaches outside the region it happens in, and the driver that applies it has to
        // be able to say so ([`crate::peer::RegionHost`]).
        for region in hosted {
            store.host_region(region)?;
        }

        // After the peers, because gate 2 asks the region map what this store serves and the map
        // is only now populated; before the heartbeats, because a store that has not finished
        // reclaiming is not yet telling the placement driver how much disk it has.
        reclaim_interrupted_retirements(&store)?;

        if let Some(pd) = pd {
            store.spawn_heartbeats(
                Arc::clone(&pd),
                heartbeat_tick,
                store_heartbeat,
                region_heartbeat,
            );
            store.spawn_split_checker(pd, heartbeat_tick);
        }
        // **After the peers and independent of the placement driver.** The census reports what
        // this store's own peers believe, which is worth having on a store that has no driver at
        // all — and is worth having most on one whose driver it can no longer reach.
        if let Some(every) = region_census {
            store.spawn_region_census(every);
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
        // **Reserved, not yet hosted.** The map below is what decides whether this store may
        // serve the region at all, and until it has said so the peer is only a claim on its
        // driver ([ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md)): the `?`
        // on the insert drops the reservation, which gives the region straight back.
        let mut reserved = None;
        let state = match (&self.raft, &self.transport) {
            (Some(raft), Some(transport)) => {
                let host: Arc<dyn RegionHost> = Arc::new(StoreHost {
                    store: Arc::downgrade(self),
                });
                let view = transport.for_region(region.id, region.epoch, &region.peers);
                let reservation = start_peer(
                    &self.db,
                    &region,
                    self.store_id,
                    raft,
                    Arc::clone(&view),
                    host,
                    Arc::clone(&self.drivers),
                    Some(self.columnar_slot(region.id)),
                )?;
                let peer = Arc::clone(reservation.peer());
                reserved = Some((reservation, raft.tick));
                RegionState::replicated(RegionMeta::new(region), peer, view)
            }
            _ => RegionState::unreplicated(RegionMeta::new(region)),
        };
        self.regions.insert(state)?;
        if let Some((reservation, tick)) = reserved {
            // The store has it: from here the peer is the region's, and it starts counting time.
            let peer = reservation.commit();
            self.spawn_ticker(&peer, tick);
        }
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

        // Leadership moves by the core's own `TimeoutNow` path rather than by a conf change, so
        // it leaves before the membership vocabulary below.
        if let Operator::TransferLeader { to_peer_id, .. } = operator {
            self.transfer_leadership(&state, &peer, *to_peer_id).await;
            return;
        }
        let Some((kind, node, store_id, role)) = conf_change_for(operator, &state) else {
            return;
        };

        // Bounded, because a proposal is answered when it *applies* and a membership change that
        // cannot reach a quorum never does. Without this the heartbeat round that carried the
        // operator would stop, and with it the only channel the placement driver has to correct
        // its own mistake.
        let proposal = peer.propose_conf_change(kind, node, store_id, role);
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
        if matches!(target.role, PeerRole::Learner | PeerRole::ColumnarLearner) {
            // Neither kind of learner can win an election, so the transfer would leave the region
            // without a leader until the old one's timeout brought it back. A columnar learner is
            // additionally one that must *never* lead: it holds columns, not rows.
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
    ///
    /// **The columnar copy goes**, on the same reasoning as
    /// [`reclaim_retired_range`](Self::reclaim_retired_range): a copy is derived from a range that
    /// is about to be emptied and refilled, and the refill arrives as a snapshot — bytes written
    /// into the column families with no entry to apply, so nothing tees them
    /// ([`crate::columnar::region::ColumnarSlot::saw`]). Left in the map it would be reattached by
    /// `host_region` holding what it held before the transfer, which is a copy of a region that no
    /// longer exists. `saw` catches that at the next entry either way; dropping it here closes it
    /// where it opens.
    async fn retire_region_now(self: &Arc<Self>, region_id: u64) {
        let Some(state) = self.regions.remove(region_id) else {
            return;
        };
        {
            let mut slots = self
                .columnar
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(&region_id);
        }
        if let Some(peer) = state.peer() {
            let peer = Arc::clone(peer);
            let _ = tokio::task::spawn_blocking(move || peer.stop()).await;
        }
    }

    /// Stops serving a region this store has been removed from, forgets its Raft state, and
    /// **reclaims its range and its columnar copy**.
    ///
    /// Spawned rather than done here, because one caller is the removed peer's **own driver
    /// thread**: stopping it from inside is a join on itself.
    ///
    /// # Why the data goes now, when it used to stay
    ///
    /// It used to stay, and this comment used to say why: removing it would mean point deletes
    /// over the whole range, because "the engine has no range tombstones in v1 (ADR 0006)", and
    /// the tombstones those leave are keys in the range — the state that stops the range ever
    /// receiving a snapshot again. Every clause of that was true when it was written and the first
    /// one stopped being true in phase 5: range tombstones are
    /// [ADR 0017](../../../docs/adr/0017-range-tombstones.md), and
    /// [`snapshot::clear_range`](crate::snapshot::clear_range) is this exact operation — delete,
    /// flush, discharge, and *verify* — written for the peer that has to be caught up by a
    /// snapshot. So the reason to keep the keys expired two phases ago and nobody came back to it;
    /// the consequence was unbounded disk growth on any cluster that rebalances, since every
    /// rebalance is a `RemovePeer`.
    ///
    /// # The order, and what a crash between the steps leaves
    ///
    /// The Raft state and the region record go **first**, synced, and the range is emptied after.
    /// A crash in between leaves keys under no record, which is precisely the state this store was
    /// in permanently until now — recoverable, and never served, because there is no region record
    /// to serve them from. Doing it the other way round would leave a *record* pointing at a
    /// half-emptied range, and a peer that restarted into serving a partial region is the one
    /// outcome that must never happen (`docs/plans/phase-4.md` §13.1).
    ///
    /// # The two gates, which are the difference between reclamation and data loss
    ///
    /// Deleting a range this store still owns loses acknowledged writes, so the clear runs only
    /// when both are true:
    ///
    /// 1. **the membership no longer names a peer on this store.** Which record answers that
    ///    depends on who is retiring, and getting it from the map is wrong for one of the two
    ///    callers: a peer that applied its own removal holds the post-change record, but a store
    ///    the sweep found holds the record from *before* the change — it never applied one — so
    ///    its own copy still names it and always will. `membership` is the newer record when the
    ///    caller has one, and the map's when it does not; and
    /// 2. **no region this store still hosts overlaps the range.** This is the one that catches a
    ///    *stale* record: a parent whose split narrowed it, retired against the range it had
    ///    before, would delete the child's keys. The map is the authority on what is served here,
    ///    and it is consulted after the removal so the answer cannot include the region going away.
    ///
    /// Either gate failing is logged and skips the clear. The keys then stay where they were,
    /// which is the old behaviour and costs disk rather than data.
    fn retire_region(self: &Arc<Self>, region_id: u64, membership: Option<Region>) {
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
            // **Gate 1, and it runs here rather than after the destroy because the destroy is
            // what takes its input away.** The batch below deletes the `'m'` record; the answer
            // to "does the membership still name a peer on this store" is computed from a region
            // record, and after that batch there is none.
            let membership = membership.as_ref().unwrap_or(state.region());
            let announce = match membership
                .peers
                .iter()
                .find(|peer| peer.store_id == store.store_id)
            {
                Some(mine) => {
                    tracing::warn!(
                        region_id,
                        peer_id = mine.peer_id,
                        "a region was retired while its record still names a peer on this store; \
                         its range is left alone"
                    );
                    None
                }
                // The **range** is always this store's own record: it is what this store actually
                // wrote keys into, and a newer record from elsewhere may describe a narrower one.
                // Gate 2, inside the reclamation, is what keeps the pair honest when the two
                // records disagree about the range as well.
                None => Some(state.region().clone()),
            };

            let db = Arc::clone(&store.db);
            let announced = announce.clone();
            let removed = tokio::task::spawn_blocking(move || {
                crate::raft_log::destroy(&db, region_id, announced.as_ref())
            })
            .await;
            match removed {
                Ok(Ok(entries)) => tracing::info!(
                    region_id,
                    entries,
                    "this store was removed from a region; its raft state is gone"
                ),
                Ok(Err(error)) => {
                    tracing::warn!(region_id, %error, "could not clear a retired region's log");
                    // The record may still be on disk, so the range may still be served after a
                    // restart. Emptying it now would be emptying a range this store can still
                    // come back into.
                    return;
                }
                Err(error) => {
                    tracing::warn!(region_id, %error, "the retirement task failed");
                    return;
                }
            }
            let Some(region) = announce else {
                return;
            };
            store.reclaim_retired_range(&region).await;
        });
    }

    /// Empties a retired region's range, once it is provably nobody's here.
    ///
    /// Split out of [`retire_region`](Self::retire_region) because it is the part that deletes
    /// data, and the gates in front of it are the whole reason it is safe to. Gate 1 is that
    /// method's; gate 2 is [`finish_retirement`]'s, and it is here rather than there because the
    /// region map is the authority on what this store serves and it is consulted *after* the
    /// removal, so its answer cannot include the region going away.
    ///
    /// The in-memory columnar slot is dropped first and on this thread, because it owns the
    /// `RunSet` that owns the manifest: a fragment arriving mid-removal would otherwise reopen
    /// the table and write a manifest back into a directory being deleted.
    async fn reclaim_retired_range(self: &Arc<Self>, region: &Region) {
        {
            let mut slots = self
                .columnar
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(&region.id);
        }
        let db = Arc::clone(&self.db);
        let fs = Arc::clone(&self.fs);
        let data_dir = self.data_dir.clone();
        let region = region.clone();
        let overlaps: Vec<u64> = self
            .regions
            .overlapping(&region.start_key, &region.end_key)
            .iter()
            .map(|other| other.id)
            .collect();
        // On a blocking thread: `clear_range` is a delete, a flush and a compaction of the range.
        if let Err(error) = tokio::task::spawn_blocking(move || {
            finish_retirement(&db, &fs, &data_dir, &region, &overlaps);
        })
        .await
        {
            tracing::warn!(%error, "the reclamation task failed");
        }
    }

    /// Brings a split into effect: narrows the parent and starts the child.
    ///
    /// Called from the **parent's driver thread**, after both halves' records are durable. The map
    /// takes the two changes under one write lock, so no reader ever sees the parent still owning
    /// what the child now owns — the "key space is a contiguous partition" invariant holds through
    /// the split rather than after it.
    fn adopt_split(self: &Arc<Self>, parent: &Region, child: &Region) -> Result<()> {
        // Reserved until `apply_split` below has taken it, exactly as `host_region` does: three of
        // that call's refusals — the parent not here, the child already here, the parent's start
        // key moved — used to leave the child's core driving a region this store does not host
        // ([ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md)).
        let mut reserved = None;
        let child_state = match (&self.raft, &self.transport) {
            (Some(raft), Some(transport)) => {
                let host: Arc<dyn RegionHost> = Arc::new(StoreHost {
                    store: Arc::downgrade(self),
                });
                // Its log is empty, so `RaftLogStorage::open` writes the configuration it is given
                // as the membership *as of index 0* — which for a region whose log starts there is
                // exactly the split-time membership, and is the anchor rule of `91de89a`.
                let view = transport.for_region(child.id, child.epoch, &child.peers);
                let reservation = start_peer(
                    &self.db,
                    child,
                    self.store_id,
                    raft,
                    Arc::clone(&view),
                    host,
                    Arc::clone(&self.drivers),
                    Some(self.columnar_slot(child.id)),
                )?;
                let peer = Arc::clone(reservation.peer());
                reserved = Some((reservation, raft.tick));
                // **The child stands for election at once, if this store led the parent**
                // ([ADR 0094](../../../docs/adr/0094-a-split-childs-leader-is-the-parents-leader.md)).
                //
                // Without it every replica of the child is a follower and the group waits out an
                // election timeout before anyone stands — a median of **62 ms** measured across 132
                // splits, once per split, and a tail that reached half a minute. The store that led
                // the parent knows everything an election would establish: that it is the leader,
                // that the child's membership is the parent's, and that the child's log is empty.
                //
                // **It is still an election**, and that is the point of doing it this way rather
                // than starting the child *as* leader: the other replicas grant or refuse by the
                // ordinary rules and ADR 0085's guard still decides who may win, so nothing here
                // fabricates leadership. What it removes is the waiting.
                //
                // Only on the parent's leader, because two replicas campaigning at once is a split
                // vote that costs another timeout — the thing this exists to avoid.
                //
                // **After the map has taken the child**, below, and not here: a campaign is the
                // one thing a peer does entirely on its own initiative, so a child this store
                // turns out not to host would otherwise campaign — and go on campaigning — for a
                // region nothing on this store can serve. That is the `campaigns_pre: 1108`
                // beside `campaigns_real: 91` in `docs/plans/debts-v1.1.md` #9.
                RegionState::replicated(RegionMeta::new(child.clone()), peer, view)
            }
            _ => RegionState::unreplicated(RegionMeta::new(child.clone())),
        };
        let leads_the_parent = self
            .regions
            .get(parent.id)
            .and_then(|state| state.peer().map(|peer| peer.is_leader()))
            .unwrap_or(false);
        self.regions.apply_split(parent.clone(), child_state)?;
        let Some((reservation, tick)) = reserved else {
            return Ok(());
        };
        let peer = reservation.commit();
        self.spawn_ticker(&peer, tick);
        if leads_the_parent {
            // **The store's own runtime handle, not `tokio::spawn`.** This runs on the parent's
            // *driver* thread, which is a plain thread from `DriverPool` and not a reactor worker
            // — `tokio::spawn` there panics for want of a runtime context, and a panic on the
            // driver thread stops the region applying anything. The first version of this did
            // exactly that: the same load split twice instead of a hundred and thirty times, and
            // the writer got seventy-nine `08006`s because the regions it wanted had stopped
            // moving.
            //
            // Spawned rather than awaited because the child's driver is another thread: awaiting
            // here would have the parent's apply loop wait on the child's.
            if let Some(runtime) = &self.runtime {
                runtime.spawn(async move {
                    Self::campaign_the_child(&peer, tick).await;
                });
            }
        }
        Ok(())
    }

    /// Stands the child for election until somebody can vote for it
    /// ([ADR 0094](../../../docs/adr/0094-a-split-childs-leader-is-the-parents-leader.md)).
    ///
    /// **One campaign is not enough, and the reason is the shape of a split.** Every replica creates
    /// the child when *it* applies the split entry, and the leader applies first — so a campaign fired
    /// the instant the leader adopts its child reaches stores that do not serve that region yet, and a
    /// Raft batch for a region a store does not serve is **dropped** rather than refused. The child
    /// then waits out the timeout it was supposed to skip. Measured: campaigning once left the median
    /// where it was, at 63 ms.
    ///
    /// So it asks again, briefly, and stops the moment there is a leader — which is also what makes it
    /// safe to be wrong about: every attempt is an ordinary pre-vote, it changes no term when it fails,
    /// and if none of them lands the region elects on its timeout exactly as it did before.
    async fn campaign_the_child(peer: &Arc<RaftPeer>, tick: std::time::Duration) {
        // A handful of ticks: long enough for the followers to have applied the same entry, far short
        // of the election timeout this exists to beat.
        for _ in 0..8 {
            if peer.campaign().await.is_err() {
                return;
            }
            tokio::time::sleep(tick).await;
            match peer.status().await {
                Ok(status) if status.leader.is_some() => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    /// Starts the task that reports to the placement driver.
    ///
    /// The schedule itself counts ticks and reads no clock ([`crate::heartbeat`]); this is the
    /// edge that turns a `tokio` interval into those ticks. A round that is late because the
    /// process was busy is **skipped rather than replayed**: `MissedTickBehavior::Burst` — the
    /// default, and the one that replays — would make a stalled store send a run of identical
    /// heartbeats the moment it recovered, and the next round carries the same content anyway.
    ///
    /// **Not the same shape as a peer's ticker, and this comment used to say it was.** A report
    /// and a clock want opposite policies: dropping a missed report costs nothing, while dropping
    /// a missed raft tick is elapsed time the cluster never counts. The peer's ticker therefore
    /// catches up on what it slept through and this deliberately does not
    /// ([`crate::peer::drive_ticks`], [ADR 0081](../../../docs/adr/0081-the-tick-driver-catches-up.md)).
    ///
    /// The round itself runs on a **blocking thread**. [`PdClient`] is synchronous, like
    /// everything else in this store that is not the network edge, so a round that ran on the
    /// reactor would hold a worker for a network round trip to the placement driver — and a
    /// placement driver that had gone away would hold it for the whole timeout, every ten
    /// seconds, on every store. The schedule travels into the closure and back out, because it
    /// is the state that must survive the round.
    /// Starts the region census, on the runtime this store was opened on.
    ///
    /// One `info` event per region per period ([`crate::census`]). Nothing reads it and nothing
    /// depends on it: it exists so that a run which stops serving can be diagnosed from the
    /// stores' own beliefs rather than from a client's refusals.
    ///
    /// A weak reference, like every other schedule here, so a dropped store ends the task rather
    /// than being kept alive by the thing that reports it.
    fn spawn_region_census(self: &Arc<Self>, every: std::time::Duration) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let task = runtime.spawn(async move {
            let mut interval = tokio::time::interval(every);
            // Skip and not burst: a round that ran long because a driver would not answer must
            // not be followed by a catch-up flurry of rounds asking the same wedged driver.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(store) = weak.upgrade() else {
                    return;
                };
                for census in store.take_region_census(every).await {
                    census.emit();
                }
            }
        });
        if let Ok(mut tickers) = self.tickers.lock() {
            tickers.push(task);
        }
    }

    /// One census round: every region this store hosts, as its peer for it believes it to be.
    ///
    /// `budget` bounds the whole round, and [`census::ANSWER_WITHIN`] bounds each region's share
    /// of it. Regions the budget did not reach are still reported — with `unanswered` saying so —
    /// because a census that skipped them would read as a store that had stopped hosting them.
    async fn take_region_census(&self, budget: std::time::Duration) -> Vec<census::RegionCensus> {
        let started = std::time::Instant::now();
        let mut taken = Vec::new();
        for state in self.regions.states() {
            let region = state.region();
            let record_peers = region.peers.iter().map(|peer| peer.peer_id).collect();
            let Some(peer) = state.peer() else {
                // An unreplicated region has no core to ask and no election to have. It is still
                // reported, because "this store hosts a region with no consensus" is a fact a
                // reader of this log would otherwise have to infer from an absence.
                taken.push(census::RegionCensus {
                    store_id: self.store_id,
                    region_id: region.id,
                    epoch: region.epoch,
                    handle_peer: 0,
                    answered_by: None,
                    term: 0,
                    role: None,
                    is_leader: true,
                    believes_leader: None,
                    voted_for: None,
                    applied: 0,
                    commit: None,
                    last_index: None,
                    core_voters: Vec::new(),
                    core_learners: Vec::new(),
                    record_peers,
                    elections: None,
                    unanswered: Some("this region is not replicated".to_owned()),
                });
                continue;
            };
            // The published half first, and without waiting on anything: these are what the
            // request path itself reads, so they are the store's answer even when its driver
            // cannot give one.
            let mut census = census::RegionCensus {
                store_id: self.store_id,
                region_id: region.id,
                epoch: region.epoch,
                handle_peer: peer.peer_id(),
                answered_by: None,
                term: peer.term(),
                role: None,
                is_leader: peer.is_leader(),
                believes_leader: peer.leader(),
                voted_for: None,
                applied: peer.applied_index(),
                commit: None,
                last_index: None,
                core_voters: Vec::new(),
                core_learners: Vec::new(),
                record_peers,
                elections: None,
                unanswered: None,
            };
            let Some(left) = census::left_of(budget, started) else {
                census.unanswered = Some("the census round ran out of time".to_owned());
                taken.push(census);
                continue;
            };
            let within = left.min(census::ANSWER_WITHIN);
            match tokio::time::timeout(within, peer.status()).await {
                Ok(Ok(status)) => {
                    census.answered_by = Some(status.id);
                    census.role = Some(format!("{:?}", status.role));
                    census.voted_for = status.voted_for;
                    census.commit = Some(status.commit);
                    census.last_index = Some(status.last_index);
                    census.core_voters.clone_from(&status.conf.voters);
                    census.core_learners.clone_from(&status.conf.learners);
                }
                Ok(Err(error)) => census.unanswered = Some(format!("the peer answered: {error}")),
                Err(_) => {
                    census.unanswered =
                        Some(format!("the driver did not answer within {within:?}"));
                }
            }
            if census.unanswered.is_none()
                && let Some(left) = census::left_of(budget, started)
            {
                let within = left.min(census::ANSWER_WITHIN);
                match tokio::time::timeout(within, peer.counters()).await {
                    Ok(Ok(counters)) => census.elections = Some(counters),
                    Ok(Err(error)) => {
                        census.unanswered = Some(format!("the counters: {error}"));
                    }
                    Err(_) => {
                        census.unanswered =
                            Some(format!("the counters did not arrive within {within:?}"));
                    }
                }
            }
            taken.push(census);
        }
        taken
    }

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
            // How many consecutive rounds each region has had no leader, which is the only state
            // `sweep_orphaned_regions` keeps. It lives here rather than on the store because this
            // task is its only reader and its only writer.
            let mut leaderless: std::collections::BTreeMap<u64, u32> =
                std::collections::BTreeMap::new();
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
                // And the same for the other operator: a `RemovePeer` is not finished until the
                // store it removed has heard about it. See `sweep_orphaned_regions`.
                store.sweep_orphaned_regions(&mut leaderless).await;
                drop(store);
            }
        });
        self.remember(task);
    }

    /// Retires every region this store hosts that the cluster has removed it from.
    ///
    /// # The gap this closes: nobody tells a removed peer
    ///
    /// `retire_region` is reached from one place — a peer applying the conf change that removed
    /// it — and a removed peer does not apply that entry, because it never receives it. A
    /// configuration change takes effect **when it is appended** (§4.1, and `esker-raft`'s
    /// `conf::tests::a_leader_removed_by_a_committed_change_steps_down` asserts exactly that), so
    /// the instant the leader appends `Remove(n)` its own `Progress` no longer has an entry for
    /// `n`, and the entry that says `n` is gone is the first one `n` is not sent. The commit needs
    /// a quorum of the *new* configuration, which `n` is not in, so it commits without `n` ever
    /// hearing of it. Nothing after that is addressed to `n` either.
    ///
    /// So without this sweep a removed store keeps a peer, a region record and every key of the
    /// range for the life of the process. It is not only a disk leak: that peer has no leader, so
    /// it campaigns for ever against a group that has replaced it. A restart does not heal it —
    /// `docs/plans/phase-4.md` §6 race 3 tombstones a record whose peer list does not name this
    /// store, and this record still names it, because it is the record from before the change.
    ///
    /// # Why the placement driver is asked, and why its answer is safe to delete data on
    ///
    /// PD holds the routing table (`docs/DESIGN.md` §7) and learns a region's membership from its
    /// **leader's** region heartbeats. So an answer that names a newer `conf_ver` than this store
    /// holds is an applied membership from a peer that is in the group this store thinks it is in.
    /// Three conditions together, and all three are needed:
    ///
    /// * the answer is about the **same region** — a different id means the range moved under a
    ///   split or a merge, which is a different question and not this one's to act on;
    /// * its `conf_ver` is **strictly greater** than the one here, so it describes a change this
    ///   store has not applied rather than the state it already knows; and
    /// * it names **no peer on this store**.
    ///
    /// Every other answer, and every failure to get one, leaves the region alone. That is the
    /// fail-closed direction: a store that keeps a region it has been removed from wastes disk
    /// and campaigns, and a store that drops one it still holds loses acknowledged writes.
    ///
    /// # The trigger is a throttle, not a safety bound
    ///
    /// Only a region whose peer has had **no leader** for [`ORPHAN_PROBE_ROUNDS`] consecutive
    /// rounds is asked about, and the counter resets when it is. Being leaderless is what a
    /// removed peer is permanently and what an ordinary election is for a moment, so the count
    /// keeps the question rare rather than making it correct — a probe during a real election is
    /// answered "you are still a member" and costs one round trip. Safety is entirely in the
    /// three conditions above.
    async fn sweep_orphaned_regions(
        self: &Arc<Self>,
        leaderless: &mut std::collections::BTreeMap<u64, u32>,
    ) {
        let Some(pd) = self.pd.clone() else {
            return;
        };
        let hosted = self.regions.regions();
        leaderless.retain(|region_id, _| hosted.iter().any(|region| region.id == *region_id));

        for region in hosted {
            let Some(peer) = self.peer_of(region.id) else {
                continue;
            };
            if peer.leader().is_some() {
                leaderless.remove(&region.id);
                continue;
            }
            let rounds = leaderless.entry(region.id).or_default();
            *rounds += 1;
            if *rounds < ORPHAN_PROBE_ROUNDS {
                continue;
            }
            *rounds = 0;

            let pd = Arc::clone(&pd);
            let start_key = region.start_key.clone();
            let Ok(Ok(Some(route))) =
                tokio::task::spawn_blocking(move || pd.get_region(&start_key)).await
            else {
                // Unreachable, or PD has never heard of the range. Neither is evidence of a
                // removal, and this runs again next round.
                continue;
            };
            if route.region.id != region.id
                || route.region.epoch.conf_ver <= region.epoch.conf_ver
                || route
                    .region
                    .peers
                    .iter()
                    .any(|peer| peer.store_id == self.store_id)
            {
                continue;
            }
            tracing::info!(
                region_id = region.id,
                ours = ?region.epoch,
                theirs = ?route.region.epoch,
                "the placement driver holds a newer membership for a region this store hosts, \
                 and it does not name this store; retiring it"
            );
            self.retire_region(region.id, Some(route.region));
        }
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
                // **Exhaustive on purpose.** A role added later must not fall into either
                // default: promoting it silently would make a replica vote that was never meant
                // to, and skipping it silently would strand a replica that was. The compiler
                // asks the question instead.
                .filter(|peer| match peer.role {
                    PeerRole::Learner => true,
                    // Never promoted — that is the whole of ADR 0022 Decision 1. A promoted
                    // columnar replica votes, counts toward a quorum, and is asked for row reads
                    // it holds no rows for.
                    PeerRole::ColumnarLearner | PeerRole::Voter => false,
                })
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
                    PeerRole::Voter,
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
    /// # A peer the core has and the record does not is **waited for**, not refused and not served
    ///
    /// A conf change takes effect in the core when it is *appended* and in the region record when
    /// it is *applied*, and between those two this leader already knows about the new peer while
    /// its own record does not. That gap is precisely where the new peer asks, because the traffic
    /// that tells it the region exists is the traffic this leader started sending the moment it
    /// appended. Checking only the record therefore refused the one caller the check was written
    /// to admit, with a correct sentence about the wrong membership
    /// (`docs/plans/phase-8-learner.md` §close bullet 4).
    ///
    /// **Answering it from the core's membership alone is worse than the refusal**, and this is
    /// measured rather than argued. The snapshot's header carries `source.region` — the *applied*
    /// record — so a snapshot served on the strength of the core's membership arrives with a
    /// region record that does not list the peer receiving it. The receiver writes that record,
    /// writes every byte of the region, and then [`host_region`](Self::host_region) declines to
    /// start it, because a record that does not name this store is one it must not serve. Nothing
    /// fails: the transfer "succeeds", the region is not hosted, the leader announces again and
    /// the whole thing repeats. `tests/promotion.rs` fails three times out of three with a learner
    /// stranded for the length of the test — the phase-4 acceptance stall, reintroduced by the fix
    /// for a retry.
    ///
    /// So the ask is held instead. The core's membership decides whether the caller is a stranger
    /// or a member this store has not caught up to; a member is waited for, up to
    /// [`RECORD_CATCHUP_WAIT`], and served the moment the record lists it — which is when the
    /// change has **committed and applied**, and therefore when it can no longer be rolled back.
    /// A wait that expires refuses, and says that it expired: that is where this was before, and
    /// the leader's next announcement asks again.
    ///
    /// The record is consulted first and answers on its own, so a region with no core to ask
    /// behaves as it did, and a store in neither membership is still refused.
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
        let in_record = state
            .region()
            .peers
            .iter()
            .any(|peer| peer.peer_id == ask.peer_id);
        let peer = state.peer().map(Arc::clone).ok_or_else(|| {
            ProtoError::invalid(format!(
                "region {} is not replicated on this store",
                ask.region_id
            ))
        })?;
        if !in_record {
            // The configuration in force — the latest in the log, committed or not — which is
            // where a conf change lands one step before the record does.
            let conf = peer.status().await?.conf;
            if !conf.is_voter(ask.peer_id) && !conf.is_learner(ask.peer_id) {
                return Err(ProtoError::invalid(format!(
                    "peer {} is not a member of region {} and may not have a copy of it",
                    ask.peer_id, ask.region_id
                )));
            }
            self.await_record_of(ask.region_id, ask.peer_id).await?;
        }

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

    /// Waits for this store's region record to list `peer_id`, so a snapshot of it can name the
    /// peer it is being sent to.
    ///
    /// Called only when the core's configuration already has the peer and the record does not, so
    /// what is being waited for is one apply: the conf change is appended, and this returns when
    /// it has committed and been applied here. That is the right thing to wait for and not merely
    /// the convenient one — an appended change can still be rolled back by a new leader, and a
    /// region shipped to a peer a rollback removes is a copy of a range with no owner.
    ///
    /// Bounded, and the bound is what keeps a design mistake from becoming a deadlock. Adding a
    /// **voter** to a group that then needs it for a quorum cannot commit until that voter has the
    /// region, which is what this call is trying to give it; nothing in this system does that —
    /// `AddPeer` adds a learner and promotes it once it is caught up, and a learner's addition
    /// commits on the existing voters alone — but a wait with no end would turn the day somebody
    /// tries into a hang instead of a retry.
    async fn await_record_of(
        &self,
        region_id: u64,
        peer_id: u64,
    ) -> std::result::Result<(), ProtoError> {
        let deadline = tokio::time::Instant::now() + RECORD_CATCHUP_WAIT;
        loop {
            let listed = self.regions.get(region_id).is_some_and(|state| {
                state
                    .region()
                    .peers
                    .iter()
                    .any(|peer| peer.peer_id == peer_id)
            });
            if listed {
                tracing::debug!(
                    region_id,
                    peer_id,
                    "a snapshot ask waited for the conf change that placed its peer to apply"
                );
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProtoError::Unsupported {
                    detail: format!(
                        "peer {peer_id} is in region {region_id}'s configuration but the change \
                         that put it there has not applied here within {RECORD_CATCHUP_WAIT:?}, \
                         so a snapshot of it would carry a record that does not name the peer \
                         receiving it"
                    ),
                });
            }
            tokio::time::sleep(RECORD_CATCHUP_POLL).await;
        }
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
            snapshot::read_pairs(
                &db,
                &region,
                read,
                snapshot::CHUNK_TARGET_BYTES,
                |cf_tag, pairs| {
                    chunks
                        .blocking_send(snapshot::encode_pairs(cf_tag, &pairs))
                        .map_err(|_| ProtoError::Closed {
                            detail: "the snapshot's reader has gone".to_owned(),
                        })
                },
            )
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
            // ADR 0109. Both answer only when the work is done: `Db::flush_all` blocks on the
            // flush job and `compact_range` on the compaction it schedules, so neither adds a
            // cadence of its own — and an operator's flush that returned early would be useless
            // to the measurement it exists for.
            AdminReq::Flush => {
                self.flush()?;
                Ok(AdminResp::Flushed {
                    families: self.sst_files()?,
                })
            }
            AdminReq::Compact { cf } => {
                if cf.is_empty() {
                    for name in self.cf_names() {
                        self.compact_cf(&name)?;
                    }
                } else {
                    // Named and absent is an operator's typo, and it is worth saying which names
                    // there are rather than handing back the engine's "no such column family".
                    if !self.cf_names().contains(&cf) {
                        return Err(ProtoError::invalid(format!(
                            "no column family named `{cf}`; this store holds {}",
                            self.cf_names().join(", ")
                        )));
                    }
                    self.compact_cf(&cf)?;
                }
                Ok(AdminResp::Compacted {
                    families: self.sst_files()?,
                })
            }
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

    /// **Which peer's core actually answers for this region**, asked through the handle the
    /// request path holds.
    ///
    /// The two are the same peer or the store is broken in the way
    /// [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md) describes: the handle
    /// publishes what the request path reads, so a handle whose core has been displaced answers
    /// `NotLeader` for ever while another core leads the region.
    #[cfg(test)]
    async fn answered_by(&self, region_id: u64) -> Option<u64> {
        let peer = self.peer_of(region_id)?;
        peer.status().await.ok().map(|status| status.id)
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
        // **Before a byte is written.** A record that does not name this store is one
        // `host_region` refuses to start — correctly — so adopting the snapshot behind it would
        // write the whole region, host nothing, and leave the leader announcing into a transfer
        // that can only repeat. Refusing here costs the same retry and leaves nothing behind, and
        // it is the loud version of a failure that was silent.
        if !header
            .region
            .peers
            .iter()
            .any(|peer| peer.store_id == self.store_id)
        {
            return Err(ProtoError::Unsupported {
                detail: format!(
                    "the snapshot offered for region {region_id} carries a record that does not \
                     name store {}, so it could be stored but never served",
                    self.store_id
                ),
            });
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
                let (cf_tag, pairs) = snapshot::decode_pairs(&chunk)?;
                snapshot::stage_pairs(&db, cf_tag, &pairs)
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
            db.write(batch, &WriteOptions::synced())
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
            db.write(batch, &WriteOptions::synced())
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
    ///
    /// # The two halves of Decision 4, and why they are separate
    ///
    /// `min_apply_index` is a **catch-up** bound, satisfied before evaluation by a `ReadIndex`
    /// round to the leader — a columnar learner may run one, and the leader answers any forwarder
    /// (ADR 0022 Decision 4). `ts` is **visibility**, applied *during* evaluation. They fail
    /// differently: one refuses with `TooFarBehind`, the other would silently answer from an older
    /// state. A build that derived one from the other would answer from a state it had not
    /// reached.
    /// Answers another store's ask for a table's columnar record, out of this store's own catalog.
    ///
    /// The record is an ordinary transactional value in the `'m'` key space, so "do I have it" is
    /// "do I host the region that covers its key" — and this store does not have to work that out:
    /// reading it is the test. A store that does not hold the range reads nothing and answers
    /// nothing, which is the same answer as a table that never asked for a columnar copy, and the
    /// asker treats the two the same way ([`esker_proto::schema::SchemaResp`] says why).
    ///
    /// Read at `u64::MAX` — whatever is committed — for the same reason
    /// `columnar::region::published_schema` is: a reader on an older snapshot would hand back a
    /// schema that later rows are not written under.
    async fn serve_schema(
        self: &Arc<Self>,
        ask: esker_proto::schema::SchemaReq,
    ) -> std::result::Result<esker_proto::schema::SchemaResp, ProtoError> {
        let db = Arc::clone(&self.db);
        blocking(move || {
            let record = crate::columnar::region::published_record(&db, ask.tenant, ask.table_id)
                .map_err(|error| {
                ProtoError::internal(format!("reading a columnar record: {error}"))
            })?;
            Ok(esker_proto::schema::SchemaResp { record })
        })
        .await
    }

    /// Makes sure this store can decode `table`, fetching the record from whoever holds it.
    ///
    /// # Why this is on the request path and never on the apply path
    ///
    /// `crate::columnar::decode`'s module header states the rule and the reason: *"the apply path
    /// may not fetch — a schema lookup on the log's critical path makes apply latency depend on
    /// another region's availability, and a lookup that fails stalls the log rather than failing a
    /// request"*. So a learner whose schema has not arrived stops advancing its applied index,
    /// which the heartbeat already reports and which `RefusalReason::TooFarBehind` already turns
    /// into a row-scan fallback. This is the other end of that: the **fragment** is a request, it
    /// may wait, and it is the moment the schema is actually needed.
    ///
    /// # Asked on every fragment, not once
    ///
    /// A store that can read the record locally re-reads it per fragment already — `table` clears
    /// the miss cache and `ensure` reads through, on the rule `columnar::region` states in as many
    /// words: *"a fragment is rare enough to pay a point read and must never answer `NotColumnar`
    /// from a stale 'no'"*. A store that **cannot** read it locally has the same problem one step
    /// worse: it sees no catalog writes for that table at all, so nothing tells it when the schema
    /// moves. Fetching once and caching froze such a store at the version it first saw, and after
    /// an `ADD COLUMN` its copy refused the table for ever — the same never-ending refusal
    /// [ADR 0037](../../../docs/adr/0037-a-columnar-learner-fetches-the-schema-it-cannot-read.md)
    /// was written to remove, one step later (`docs/plans/debt-c3.md` §7b).
    ///
    /// So the remote read happens on the same schedule the local one does. The cost is one round
    /// trip per fragment for a table whose record is elsewhere, which is the same order as the
    /// point read the local path already pays, and `install_record` drops an answer whose version
    /// has not moved — so the *copy* is rebuilt only when the schema actually changed.
    ///
    /// # The three steps, and what each is allowed to fail at
    ///
    /// 1. **Can this store read it itself?** Then nothing here: its own read is already
    ///    per-fragment and cannot be stale. On a single-region cluster this is every table, which
    ///    is why the whole path was invisible for a phase.
    /// 2. **Where does it live?** `PdClient::get_region` on the record's own key — the placement
    ///    driver is the authority on which region covers a key and which stores host it, and it is
    ///    the same answer a client's routing rests on. No PD, no fetch.
    /// 3. **Ask a store that hosts it.** Every peer's store in turn, because the first may be
    ///    down; the first store that answers with a record wins.
    ///
    /// Every failure here is silent and leaves the schema unknown, on purpose: the caller's next
    /// step is `slot.table(..)`, which answers `NotColumnar`, which is a refusal the planner
    /// already falls back from. A fetch that could fail a fragment would make a columnar read less
    /// available than the row read it is an optimisation of.
    async fn ensure_schema(self: &Arc<Self>, slot: &Arc<ColumnarSlot>, table: (u64, u64)) {
        let (tenant, table_id) = table;
        if slot.reads_schema_locally(&self.db, tenant, table_id) {
            return;
        }
        let Some(pd) = self.pd.clone() else {
            return;
        };
        let key = esker_keys::columnar::key(tenant, table_id);
        let Ok(Ok(Some(route))) = tokio::task::spawn_blocking(move || pd.get_region(&key)).await
        else {
            return;
        };

        for (store_id, address) in &route.stores {
            if *store_id == self.store_id {
                // Already asked, by reading. Asking this store over a socket would answer the
                // same nothing a round trip later.
                continue;
            }
            let Ok(address) = address.parse::<std::net::SocketAddr>() else {
                continue;
            };
            match self.ask_for_schema(address, tenant, table_id).await {
                Ok(Some(record)) => {
                    if let Err(error) = slot.install_record(tenant, table_id, &record) {
                        tracing::warn!(
                            tenant,
                            table_id,
                            %error,
                            "a columnar record fetched from another store did not decode"
                        );
                        return;
                    }
                    tracing::debug!(
                        tenant,
                        table_id,
                        from = store_id,
                        region_id = route.region.id,
                        "fetched a table's columnar record from the store that holds the catalog"
                    );
                    return;
                }
                // That store does not have it either, which for a peer of the region PD named
                // means it is behind rather than wrong. Try the next.
                Ok(None) => {}
                Err(error) => tracing::debug!(
                    tenant,
                    table_id,
                    store_id,
                    %error,
                    "a store could not be asked for a columnar record"
                ),
            }
        }
    }

    /// One `Schema::Fetch` round trip to one store.
    async fn ask_for_schema(
        &self,
        address: std::net::SocketAddr,
        tenant: u64,
        table_id: u64,
    ) -> std::result::Result<Option<Bytes>, ProtoError> {
        let transport = self
            .raft
            .as_ref()
            .map_or_else(TransportConfig::new, |raft| raft.transport);
        let connection = esker_proto::TcpTransport::connect_with(address, transport).await?;
        // Through the `Transport` trait, which numbers the request itself: ids zero and one are
        // the keepalive's and the handshake's, and a caller that picks its own starts by colliding
        // with the `Hello` it just sent.
        match esker_proto::Transport::call(
            &connection,
            Request::Schema(esker_proto::schema::SchemaReq { tenant, table_id }),
        )
        .await?
        {
            Response::Schema(response) => Ok(response.record),
            other => Err(ProtoError::invalid(format!(
                "a schema fetch was answered with {}",
                other.method().name()
            ))),
        }
    }

    async fn serve_fragment(
        self: &Arc<Self>,
        header: RequestHeader,
        request: esker_proto::fragment::FragmentReq,
    ) -> std::result::Result<esker_proto::fragment::FragmentResp, ProtoError> {
        use esker_proto::fragment::RefusalReason;

        // Routing first, so a fragment addressed to a region this store does not own is answered
        // the same way a row read would be — the epoch check is not optional here.
        let state = self.regions.route(&header, None)?;

        let fragment = match esker_columnar::fragment::decode(&request.fragment) {
            Ok(fragment) => fragment,
            // A fragment this build cannot read is not this region's fault, and the planner's
            // answer to it is the same as to any other refusal: read the rows.
            Err(error) => {
                return Ok(refused(
                    RefusalReason::Unsupported,
                    format!("this store cannot read the fragment: {error}"),
                ));
            }
        };

        // A voter holds rows and answers so. The role is the region record's, which is the same
        // fact PD scheduled on and the same one the peer's apply reads (ADR 0022 Decision 1).
        if !state
            .region()
            .peers
            .iter()
            .any(|peer| peer.store_id == self.store_id && peer.role == PeerRole::ColumnarLearner)
        {
            return Ok(refused(
                RefusalReason::NotColumnar,
                format!(
                    "store {} holds region {} as rows, not columns",
                    self.store_id(),
                    header.region_id
                ),
            ));
        }

        // **The catch-up first, and this order is the point.** A learner placed a moment ago has
        // the region and not yet its data; asked for a table then, it would answer "no columnar
        // copy" — a refusal that says *look elsewhere* — when the truth is "not yet", which says
        // *wait*. `min_apply_index` is what tells the two apart, so it is satisfied before
        // anything is read (ADR 0022 Decision 4). Found by the differential, which asked a learner
        // the instant PD placed it.
        //
        // **Unconditional, including for `min_apply_index = 0`.** It was guarded by `> 0` until
        // milestone 4, which made the default value the *unsafe* one: a caller with no Raft index
        // to name got no round at all and was answered from whatever the learner happened to hold.
        // A SQL node is exactly that caller — it holds a snapshot `ts` and a region id, and the
        // leader's commit index is not a number it can compute — and it does not need to be. The
        // round is the mechanism: a transaction the client has been told committed was committed
        // on the leader *before* this statement's `ts` was allocated, so the leader's commit index
        // when the fragment arrives is at or past that entry, and waiting for it covers every
        // commit visible at `ts`. ADR 0022 Decision 4 states the round unconditionally for this
        // reason (`docs/plans/phase-10-routing.md` §2).
        if let Some(refusal) = self.catch_up(&state, request.min_apply_index).await {
            return Ok(refusal);
        }

        let slot = self.columnar_slot(header.region_id);
        // **Before the slot is asked, not after it refuses.** A store holding this table's rows
        // and not the `'m'` region cannot read the record the decoder is built from, and until
        // this call existed it answered `NotColumnar` for ever — a refusal that says *look
        // elsewhere* where the truth was *nobody here can read the schema*
        // ([ADR 0037](../../../docs/adr/0037-a-columnar-learner-fetches-the-schema-it-cannot-read.md)).
        self.ensure_schema(&slot, (fragment.table.tenant, fragment.table.table_id))
            .await;
        let runs = match slot.table(&self.db, fragment.table.tenant, fragment.table.table_id) {
            Ok(Some(runs)) => runs,
            Ok(None) => {
                return Ok(refused(
                    RefusalReason::NotColumnar,
                    format!(
                        "store {} holds region {} without a columnar copy of table {}",
                        self.store_id(),
                        header.region_id,
                        fragment.table.table_id
                    ),
                ));
            }
            Err(error) => {
                return Ok(refused(
                    RefusalReason::NotColumnar,
                    format!("this store's columnar copy is not readable: {error}"),
                ));
            }
        };

        match evaluate(
            slot.fs().as_ref(),
            &runs,
            &fragment,
            request.ts,
            state.region(),
        ) {
            Ok(answer) => Ok(answer),
            Err(error) => Ok(refused(
                RefusalReason::Unsupported,
                format!("this store could not evaluate the fragment: {error}"),
            )),
        }
    }

    /// Brings this peer up to the leader's commit index, and then to `min_apply_index` on top of
    /// it, or says why it will not.
    ///
    /// **The round always runs.** `min_apply_index` is a *floor a caller can name*, not the
    /// trigger: the freshness a fragment needs comes from the `ReadIndex` round itself, which
    /// `read_index_as_learner` returns from only once the state machine has applied through the
    /// index it established. A caller with a number asks for at least that much as well; a caller
    /// with none — a SQL node, which has a snapshot `ts` and no way to turn it into a Raft index —
    /// passes zero and is still answered from a state that includes every commit it could have
    /// been told about.
    ///
    /// `read_index_as_learner` rather than `read_index`, because the row path's version refuses on
    /// a peer that does not lead — right for a row read, which only a leader serves, and wrong for
    /// a learner serving a fragment (`crate::peer::RaftPeer::read_index_as_learner`).
    async fn catch_up(
        &self,
        state: &Arc<RegionState>,
        min_apply_index: u64,
    ) -> Option<esker_proto::fragment::FragmentResp> {
        use esker_proto::fragment::RefusalReason;

        // **A named limit, not an oversight.** A region this store holds the record for but does
        // not replicate has no peer to run a round with, so there is no freshness to establish and
        // the copy answers with whatever it holds. `TooFarBehind` would be the wrong word for it —
        // that reason means *another replica may be closer*, and this store is not behind a stream,
        // it is not receiving one — and no reason on the wire means "not part of the group". A
        // placed learner is never in this state for long; what constructs it deliberately is a
        // harness with no consensus in it (`esker-store/tests/schema_fetch.rs`, which says so).
        // Named in `docs/plans/phase-10-routing.md` §5 rather than left here to be discovered.
        let peer = state.peer()?;
        let index = match peer.read_index_as_learner().await {
            Ok(index) => index,
            Err(error) => {
                return Some(refused(
                    RefusalReason::TooFarBehind,
                    format!("could not reach the leader to catch up: {error}"),
                ));
            }
        };
        // The round has already waited for the index it established. What is left is the caller's
        // own floor, which can be higher than the leader's commit index when the caller learned it
        // somewhere this peer has not caught up to.
        if peer.applied_index() >= min_apply_index {
            return None;
        }
        Some(refused(
            RefusalReason::TooFarBehind,
            format!(
                "applied {} of the {min_apply_index} asked for, at read index {index}",
                peer.applied_index()
            ),
        ))
    }

    /// This region's columnar copy, created on first ask.
    ///
    /// Every region gets one because a peer can be *told* it is a columnar learner long after it
    /// starts — a conf change is how one is placed — and a slot that has never been opened costs a
    /// map entry and touches no disk.
    fn columnar_slot(&self, region_id: u64) -> Arc<ColumnarSlot> {
        let mut slots = self
            .columnar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(slots.entry(region_id).or_insert_with(|| {
            Arc::new(ColumnarSlot::new(
                Arc::clone(&self.fs),
                self.data_dir.join("columnar").join(region_id.to_string()),
                region_id,
                ColumnarOptions::default(),
            ))
        }))
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
        // **Taken out, not just aborted.** A handle left in place would be aborted again by the
        // next `stop`, and — more to the point — this function has to *wait* for these, so it
        // needs to own them.
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Ok(mut background) = self.background.lock() {
            tasks.append(&mut background);
        }
        if let Ok(mut tickers) = self.tickers.lock() {
            tasks.append(&mut tickers);
        }
        for task in &tasks {
            task.abort();
        }
        for state in self.regions.states() {
            if let Some(peer) = state.peer() {
                peer.stop();
            }
        }
        if let Some(transport) = &self.transport {
            transport.shutdown();
        }
        // Before the wait: a worker still holding a region would be applying into a database the
        // caller is about to flush and drop, and `DriverPool::shutdown` joins its threads.
        self.drivers.shutdown();
        // Aborted and not waited for: `abort` is a request, and the runtime drops the future
        // when it next gets to it. What that costs — a database still held for a moment after
        // `stop()` returns — is answered where it lands, in `Db::open`, which waits for a claim
        // rather than refusing the first time it meets one.
        drop(tasks);
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
        // **Local work, but region-addressed.** `route_range` above has checked the epoch, which
        // is what invariant 5 asks of every request; the clearing itself needs no leader and no
        // log entry, and it is disk work — a delete, a flush and a compaction — so it goes to the
        // blocking pool rather than the driver thread.
        if matches!(request, TxnKvReq::ReclaimRange { .. }) {
            let store = Arc::clone(self);
            return blocking(move || store.handle_txn(&state, request)).await;
        }

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
            // **A read, and it is answered here for the same reason `Get` is**: it asks what the
            // engine already knows and changes nothing (ADR 0067 §2).
            TxnKvReq::LatestCommit { key } => {
                state.meta().check_key(&key)?;
                crate::txnkv::latest_commit(&self.db, &key)
            }
            TxnKvReq::GcSafepoint { safepoint } => Ok(self.set_safepoint(safepoint)),
            TxnKvReq::ReclaimRange {
                start,
                end,
                below_ts,
            } => self.reclaim_range(&start, &end, below_ts),
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

    /// Clears one chunk of a dropped database's key range, and says how far it got
    /// ([ADR 0069](../../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md)).
    ///
    /// # The record goes down before a byte comes out
    ///
    /// A range being reclaimed is written to the `raft` family, synced, **before** anything is
    /// deleted, and the cursor advances after — so a crash re-clears a chunk that is already
    /// empty, which is a no-op. The other order would skip a chunk and leave its keys with nothing
    /// coming back for them, which is the leak
    /// [ADR 0034](../../../docs/adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md) names.
    ///
    /// A repeat for a range this store has already finished finds no record and no hosted region
    /// overlapping it, and answers `finished` after one pass with no writes. That is the
    /// idempotence a resuming caller needs, and it costs a scan of the `'D'` prefix.
    fn reclaim_range(
        &self,
        start: &Bytes,
        end: &Bytes,
        below_ts: u64,
    ) -> std::result::Result<TxnKvResp, ProtoError> {
        let safepoint = self.safepoint();
        let cf_id = self.db.cf_id(cf::RAFT).ok_or_else(|| {
            ProtoError::internal("the store opened without its `raft` column family")
        })?;
        let held = crate::reclaim::load(&self.db)
            .map_err(|error| ProtoError::internal(format!("reading the reclaim records: {error}")))?
            .into_iter()
            .find(|record| record.start == start && record.end == end);
        let record = if let Some(record) = held {
            record
        } else {
            let fresh = crate::reclaim::Reclaim::new(start.clone(), end.clone(), below_ts);
            // Synced, and before `advance` deletes anything: this record is the only thing on disk
            // that says which range a restart has to finish.
            let mut batch = WriteBatch::new();
            crate::reclaim::stage(&mut batch, cf_id, &fresh);
            self.db
                .write(batch, &WriteOptions::synced())
                .map_err(|error| crate::error::engine_to_proto(&error))?;
            fresh
        };
        let hosted: Vec<Region> = self
            .regions
            .states()
            .into_iter()
            .map(|state| state.region().clone())
            .collect();
        let progress = crate::reclaim::advance(&self.db, &record, &hosted, safepoint)?;
        Ok(match progress {
            crate::reclaim::Progress::Blocked { safepoint, .. } => TxnKvResp::ReclaimRange {
                cursor: record.cursor,
                finished: false,
                safepoint,
            },
            crate::reclaim::Progress::Advanced { cursor } => TxnKvResp::ReclaimRange {
                cursor,
                finished: false,
                safepoint,
            },
            crate::reclaim::Progress::Finished => TxnKvResp::ReclaimRange {
                cursor: end.clone(),
                finished: true,
                safepoint,
            },
        })
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
        self.compact_cf(cf::WRITE)
    }

    /// Compacts one column family, end to end, and returns when it has finished
    /// ([ADR 0109](../../../docs/adr/0109-an-operator-can-ask-a-store-to-flush-and-to-compact.md)).
    ///
    /// The general form of [`Store::compact_write_cf`], which is the one `esker bench --compact`
    /// has always meant: `write` is where MVCC versions are.
    pub fn compact_cf(&self, cf: &str) -> Result<()> {
        self.db.compact_range(cf, None, None)?;
        Ok(())
    }

    /// Every column family this store holds, with the SSTs in each (ADR 0109).
    ///
    /// The **level** is carried and not only a count, because that is the distinction the
    /// measurement this exists for turns on: one file in L0 and one in L1 are the difference
    /// between versions that are all still there and versions that were merged away.
    pub fn sst_files(&self) -> Result<Vec<esker_proto::CfFiles>> {
        let mut families = Vec::new();
        for cf in self.db.cf_names() {
            let mut files = Vec::new();
            for (level, number) in self.db.files_by_level(&cf)? {
                let level = u32::try_from(level).map_err(|_| {
                    StoreError::Bootstrap(format!("{cf} reports a level of {level}"))
                })?;
                files.push((level, number));
            }
            families.push(esker_proto::CfFiles { cf, files });
        }
        Ok(families)
    }

    /// The column families this store holds, for a request that names one.
    pub fn cf_names(&self) -> Vec<String> {
        self.db.cf_names()
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
    pd_leader_wait: std::time::Duration,
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
            let answer = register_with_the_driver(pd.as_ref(), options)?;
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
    db.write(batch, &WriteOptions::synced())?;
    tracing::info!(
        store_id = options.store_id,
        region_id = region.id,
        peers = region.peers.len(),
        "bootstrapped a region covering the whole key space"
    );
    Ok(Some(region))
}

/// Registers this store with the placement driver, waiting out an election rather than exiting.
///
/// # Why the open waits at all
///
/// A store that exited on [`ProtoError::PdNotLeader`] turned a one-second election into a dead
/// node, and only on a machine slow enough to lose the race: four ADR 0108 tests failed this way
/// on a loaded gate and were green on a quiet one, each after `esker cluster start` reported
/// `node 1 exited with exit status: 1` (#59). The refusal itself is exactly the one worth waiting
/// on — the member **provably did nothing**, and its answer cannot change until the election ends.
///
/// The client side of this already had its rule, in `esker_proto::LeaderBook`; what it does not
/// have is a store's question, which is asked once, before anything is serving, and has no caller
/// above it to retry.
///
/// # And why it does not wait for everything
///
/// A driver that was never dialled is **not** an election. `esker cluster start` orders the driver
/// before the stores on purpose — *"a store whose PD is not up yet fails to open, which is the
/// behaviour that makes a cluster's start order matter here and nowhere else"* — and a mistyped
/// `--pd` should say so in a second rather than in half a minute. So an unreachable member is
/// waited on **only after some member has said an election is running**, which is the point at
/// which the group is known to exist. That is the sequence a store started before the rest of its
/// group actually meets: one member up and leaderless, the others not yet listening.
fn register_with_the_driver(
    pd: &dyn PdClient,
    options: &BootstrapOptions<'_>,
) -> Result<crate::pd::Bootstrapped> {
    let info = StoreInfo {
        store_id: options.store_id,
        address: options.address.to_owned(),
    };
    let began = std::time::Instant::now();
    // Set by the first `PdNotLeader`, and the whole of what separates "a group is electing" from
    // "there is nothing there".
    let mut electing = false;
    let mut said = false;
    loop {
        let refusal = match pd.bootstrap(&info) {
            Ok(answer) => return Ok(answer),
            Err(error) => error,
        };
        let mid_election = matches!(refusal, ProtoError::PdNotLeader { .. });
        electing |= mid_election;
        let worth_waiting_on = mid_election || (electing && esker_proto::is_unreachable(&refusal));
        if !worth_waiting_on {
            // Returned exactly as it arrived: this is the path an operator with a mistyped
            // `--pd` takes, and the socket's own words are what tells them so.
            return Err(StoreError::from(refusal));
        }
        if began.elapsed() >= options.pd_leader_wait {
            // The bound, and it says it *is* one — a bare "placement driver is not the leader"
            // after half a minute of waiting reads like the refusal came back instantly.
            return Err(StoreError::Bootstrap(format!(
                "no placement driver led the group within {:?}: {refusal}",
                began.elapsed()
            )));
        }
        if !said {
            said = true;
            tracing::info!(
                store_id = options.store_id,
                wait_ms = options.pd_leader_wait.as_millis(),
                "waiting for a placement driver to lead before registering"
            );
        }
        std::thread::sleep(PD_LEADER_POLL);
    }
}

/// The membership change an operator asks for, or `None` when there is nothing to propose.
///
/// Split out of [`Store::apply_operator`] because it is the whole of the operator *vocabulary* —
/// what each one means as a change of membership — and that reads better as one thing than as a
/// preamble to the proposal carrying it. `TransferLeader` is deliberately not here: it moves
/// leadership by the core's own `TimeoutNow` path rather than by a conf change, so it leaves
/// `apply_operator` before this is reached.
/// Whether this region already has the peer an `AddPeer` or `AddLearner` would place.
///
/// **By store, and not only by peer id.** A store hosts at most one peer per region — `Regions` is
/// keyed by region id and the transport drops a message it would have to address to itself — so
/// "already here" is a question about the *store*, and asking it by peer id answers "no" to every
/// repeat PD issues.
///
/// It answers "no" because PD mints a **fresh peer id every time**: `esker-pd`'s `repair.rs` says
/// so in as many words — *"a fresh peer id, from the persisted allocator, every time an `AddPeer`
/// is issued ... reusing the id of an operator PD has forgotten would risk two peers with one
/// id"* — and burning ids is the right call there. It is the check here that was reading the wrong
/// half of the pair.
///
/// # What that cost, found under load
///
/// An `AddPeer` is answered when its conf change *applies*, and a leader that steps down with the
/// proposal in its log answers "it may still commit" — which is not "it did not". PD sees nothing,
/// times the operator out, re-derives the same plan onto the same store with a new peer id, and
/// the second change commits beside the first. The region then has two peers on one store, and the
/// second one can never exist: while the record catches up the leader logs *"no store known for a
/// peer of this region; the message was dropped"*, and once it has, *"a message was addressed to a
/// peer on this store; it was dropped"* — for ever. The peer sits at `matched=0`,
/// `recent_active=false`, is never promoted, and PD, counting a peer that is only on paper, no
/// longer sees the region as short and never repairs it.
///
/// The same family as `balance::is_mid_repair`'s "voters, not peers": an identity read off the
/// wrong field, turning a transient ambiguity into a permanent state
/// (`docs/plans/phase-14-flakes.md` U2).
fn already_placed(state: &RegionState, peer_id: u64, store_id: u64) -> bool {
    state
        .region()
        .peers
        .iter()
        .any(|peer| peer.peer_id == peer_id || peer.store_id == store_id)
        // **And the record is not the whole membership.** A conf change is in force from the
        // moment its entry is on disk; the region record only moves when it applies. The
        // re-derived operator arrives at the *new* leader in exactly that window — the old one
        // stepped down with the first change in its log, which is why PD re-derived at all — and
        // a check that reads only the record sees a region that is still short and adds a second
        // peer beside the first. Asked against the record alone this guard let
        // `region 3 has peers 16 and 27 both on store 3` through on the very next run.
        || state.hosts_store(store_id)
}

fn conf_change_for(
    operator: &esker_proto::Operator,
    state: &RegionState,
) -> Option<(esker_raft::ConfChangeKind, u64, u64, PeerRole)> {
    Some(match operator {
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
        esker_proto::Operator::AddPeer {
            store_id, peer_id, ..
        } => {
            // Already here. A learner is on its way to being a voter under its own criterion,
            // and a voter is what was asked for: either way there is nothing to propose.
            if already_placed(state, *peer_id, *store_id) {
                return None;
            }
            (
                esker_raft::ConfChangeKind::AddLearner,
                *peer_id,
                *store_id,
                PeerRole::Learner,
            )
        }
        // **A columnar replica: a learner that is never promoted** (ADR 0022 Decision 1).
        //
        // The same `ConfChangeKind::AddLearner` a row replica gets — `esker-raft` has one
        // notion of learner and the ADR leaves it that way — with the *role* carried in the
        // conf change's **context**, which raft replicates and never interprets. That is what
        // makes the distinction land on every peer including the leader, which matters
        // because promotion is a decision the leader takes from the region record.
        //
        // `AddPeer` cannot serve here: it completes when the peer becomes a **voter**, and a
        // columnar replica never does, so it would be a repair that never finishes.
        esker_proto::Operator::AddLearner {
            store_id, peer_id, ..
        } => {
            // Already here, in whatever role it was added as. Nothing to propose, and
            // certainly not a change of role: that would be a promotion or a demotion and
            // neither is what this operator asks for.
            if already_placed(state, *peer_id, *store_id) {
                return None;
            }
            (
                esker_raft::ConfChangeKind::AddLearner,
                *peer_id,
                *store_id,
                PeerRole::ColumnarLearner,
            )
        }
        esker_proto::Operator::RemovePeer { peer_id, .. } => {
            let existing = state
                .region()
                .peers
                .iter()
                .find(|peer| peer.peer_id == *peer_id)?;
            (
                esker_raft::ConfChangeKind::Remove,
                *peer_id,
                existing.store_id,
                existing.role,
            )
        }
        // Handled by the caller, which is the only place that can await the transfer.
        esker_proto::Operator::TransferLeader { .. } => return None,
    })
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
#[allow(
    clippy::too_many_arguments,
    reason = "each is a distinct collaborator the peer needs — its log, its region, its \
              transport, its host, its driver pool, its columnar copy — and a bag type would \
              move the list rather than shorten it"
)]
fn start_peer(
    db: &Arc<Db>,
    region: &Region,
    store_id: u64,
    raft: &RaftOptions,
    transport: Arc<crate::transport::RegionTransport>,
    host: Arc<dyn RegionHost>,
    pool: Arc<DriverPool>,
    columnar: Option<Arc<ColumnarSlot>>,
) -> Result<crate::peer::Reservation> {
    // The one reader of the region record's membership, so the core's configuration and the
    // peer's `applied_conf` cannot answer the same question differently
    // ([`crate::region::membership`] carries why the learners come too).
    let conf = crate::region::membership(region);
    let (voters, learners) = (conf.voters, conf.learners);
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
            columnar,
        },
        storage,
        transport as Arc<dyn crate::peer::RaftTransport>,
        host,
        pool,
    )
}

/// A refusal, which is a normal answer.
fn refused(
    reason: esker_proto::fragment::RefusalReason,
    detail: String,
) -> esker_proto::fragment::FragmentResp {
    esker_proto::fragment::FragmentResp::Refused { reason, detail }
}

/// Evaluates one fragment over a table's runs, at one visibility timestamp.
///
/// **Every live run at once**, and that is the load-bearing part rather than an optimisation:
/// resolving versions per run answers "the newest version of this key *in this run*", which is a
/// different question — a run whose only word on a key is a tombstone resolves it to nothing, and
/// an older run's live-looking row then survives a delete (`esker-store/tests/columnar_differential`
/// was built for exactly that regression).
/// A region bound in the byte form a run's `__key` column holds.
///
/// **Measured, because the obvious form is wrong.** A region owns a range of the **user** key
/// space and its record stores the bound raw (`crate::split`: a split key "knows nothing of a `'x'`
/// prefix or a timestamp suffix"). A run's key is what `columnar::region::versioned` wrote:
/// `esker_txn::key::write` minus its namespace byte, which is `enc(user_key) ++ !ts` — the
/// **memcomparable group encoding** (`esker_keys::encode_bytes`), not the raw bytes. Comparing one
/// against the other dropped rows the region owned, and the two forms differ visibly:
///
/// ```text
/// __key  [116, 0,0,0,0,0,0,0, 255, 1, 0,0,0,0,0,0,0, 255, 1, 114, 128, …]
/// bound  [116, 0,0,0,0,0,0,0,      1, 0,0,0,0,0,0,0,      1, 114, 128, …]
/// ```
///
/// The marker byte after each eight-byte group is the whole of the difference, and it is what makes
/// the encoding **prefix-free** — which is also why the comparison is exact once both sides are in
/// it: `enc(k)` is never a prefix of `enc(b)` unless `k == b`, so the `!ts` suffix can never carry a
/// key across a bound. A key equal to the bound sorts *after* it and is excluded, which is right —
/// a region's end is the next region's first key.
///
/// An empty bound stays empty: it means unbounded, and encoding it would produce a group that
/// bounds something.
fn as_run_key(bound: &[u8]) -> Vec<u8> {
    if bound.is_empty() {
        return Vec::new();
    }
    let mut encoded = esker_txn::key::prefix(bound);
    encoded.remove(0);
    encoded
}

fn evaluate(
    fs: &dyn FileSystem,
    runs: &crate::columnar::region::TableRuns,
    fragment: &esker_columnar::Fragment,
    ts: u64,
    region: &Region,
) -> std::result::Result<esker_proto::fragment::FragmentResp, esker_columnar::Error> {
    use esker_columnar::scan::visible::Visibility;
    use esker_columnar::{Reader, ScanOptions};

    // A table with a copy but no runs has committed nothing this fragment could return, and the
    // evaluator refuses an empty reader list rather than inventing an empty answer.
    if runs.paths.is_empty() {
        return Ok(esker_proto::fragment::FragmentResp::Result {
            result: esker_proto::fragment::result::encode(
                &esker_proto::fragment::result::Body::Rows {
                    types: Vec::new(),
                    rows: Vec::new(),
                },
            )
            .map_err(|error| esker_columnar::Error::InvalidArgument(error.to_string()))?
            .into(),
            stats: esker_proto::fragment::ScanStats::default(),
        });
    }

    let readers: Vec<Reader> = runs
        .paths
        .iter()
        .map(|path| Reader::open(fs, path))
        .collect::<esker_columnar::Result<_>>()?;
    let (key_column, ts_column, deleted_column) = runs.visibility;
    let result = esker_columnar::evaluate_merged(
        &readers,
        fragment,
        &ScanOptions {
            prune: true,
            // **Every run read as the table is now**, which is what an `ADD COLUMN` needs and
            // what this passed as `None` until the joint gate's widened corpus asked for it.
            // Without it the target schema is `readers[0]`'s — the *oldest* run's — and the two
            // ways that is wrong are the two halves of the same defect: a newer, wider run is
            // refused outright ("written under a newer schema"), and if the widest run happened
            // to sort first, the older ones would be padded with `NULL` where the row store pads
            // with the column's `DEFAULT`. The first is loud; the second is the silent
            // disagreement ADR 0022 calls the worst failure this feature can have.
            //
            // `esker-store`'s own `columnar_differential.rs` proved the mechanism by building
            // this `Widening` **by hand in the test**, which is precisely how a module can be
            // right while the path that uses it is not.
            widening: Some(esker_columnar::Widening {
                schema: runs.schema.clone(),
                missing: runs.missing.clone(),
            }),
            // **This region's own key range**, so an answer covers the shard the caller was
            // routed to and nothing else. Not `fragment.range` — that is the client's field and
            // is still refused; which rows a copy may answer for is a property of the *region*,
            // so it is applied here where the region record is, and a caller cannot get it wrong.
            //
            // Scoping the *build* (`columnar::region::convert`) stops a copy being written with
            // another region's rows; this makes the answer right whatever a run already holds —
            // a parent's runs after a split, which nothing prunes (ADR 0040). The two cover
            // different halves and `docs/plans/phase-8-learner.md` §store unit 4 says why neither
            // is sufficient alone.
            range: Some(esker_columnar::KeyRange {
                start: as_run_key(&region.start_key),
                end: as_run_key(&region.end_key),
            }),
            // MVCC at **read** time (ADR 0022 Decision 4): the runs hold every version as
            // committed, and which of them a caller may see is a property of when it is reading.
            visibility: Some(Visibility {
                key_columns: vec![key_column],
                ts_column,
                deleted_column,
                ts: i64::try_from(ts).unwrap_or(i64::MAX),
            }),
        },
    )?;

    let body = crate::columnar::wire::body_of(&result.output)
        .map_err(esker_columnar::Error::InvalidArgument)?;
    let bytes = esker_proto::fragment::result::encode(&body)
        .map_err(|error| esker_columnar::Error::InvalidArgument(error.to_string()))?;
    Ok(esker_proto::fragment::FragmentResp::Result {
        result: bytes.into(),
        stats: esker_proto::fragment::ScanStats {
            stripes_considered: result.stats.stripes_considered,
            stripes_read: result.stats.stripes_read,
            chunks_decoded: result.stats.chunks_decoded,
            rows_scanned: result.stats.rows_scanned,
            rows_matched: result.stats.rows_matched,
        },
    })
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
            // No newer record to pass: this peer applied the change itself, so the record the map
            // now holds is the post-change one.
            store.retire_region(region.id, None);
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
                // A fragment, answered from this store's columnar copy of the region — or
                // **refused**, which is a normal answer and never an error frame: `NotColumnar`
                // means "the caller's routing is stale, or placement moved — fall back to a row
                // scan", and the planner's fallback is not an error path (ADR 0022 Decision 4,
                // `esker_proto::fragment`). Making it an error would turn every rolling upgrade
                // into a fault.
                Request::Fragment { header, request } => {
                    return store
                        .serve_fragment(header, request)
                        .await
                        .map(|response| Reply::Unary(Response::Fragment(response)));
                }
                // A table's columnar record, read out of this store's own catalog for a store
                // that cannot read it — see `Store::serve_schema` and [`esker_proto::schema`].
                // "I do not have it" is an answer and not an error, for the same reason a
                // fragment's refusal is one: the asker's next move is to ask somewhere else.
                Request::Schema(request) => {
                    return store
                        .serve_schema(request)
                        .await
                        .map(|response| Reply::Unary(Response::Schema(response)));
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
    use super::{RaftOptions, Store, StoreOptions, conf_change_for};
    use crate::region::RegionMeta;
    use crate::regions::RegionState;
    use bytes::Bytes;
    use esker_proto::{
        Epoch, Operator, Peer, ProtoError, RawKvReq, RawKvResp, Region, RequestHeader,
    };
    use std::sync::Arc;

    /// A store that replicates, on a runtime, with nobody to talk to.
    ///
    /// The address is unreachable on purpose: every test below is about what this store does with
    /// its **own** region map and driver pool before a message would ever leave it.
    fn replicated_store(dir: &tempfile::TempDir) -> Arc<Store> {
        Store::open(
            dir.path(),
            StoreOptions {
                raft: Some(RaftOptions::new(
                    vec![crate::PeerAddress::new(
                        1,
                        1,
                        "127.0.0.1:1".parse().unwrap(),
                    )],
                    7,
                )),
                ..StoreOptions::new()
            },
        )
        .unwrap()
    }

    /// A region record naming a peer on store 1, for the refusals below to be refused *on their
    /// own merits* rather than for not naming this store.
    fn record(id: u64, start: &'static [u8], end: &'static [u8]) -> Region {
        Region {
            id,
            start_key: Bytes::from_static(start),
            end_key: Bytes::from_static(end),
            peers: vec![Peer::voter(1, id + 100)],
            epoch: Epoch::INITIAL,
        }
    }

    /// **Every way the region map can refuse a peer this store has already built, and the one
    /// thing all of them have to leave behind: nothing.**
    ///
    /// [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md). `start_peer` hands the
    /// core to the driver pool before the map has said whether this store may host the region at
    /// all — that is unavoidable, the peer has to exist to be offered — so what matters is that
    /// every refusal gives it straight back. Before the reservation, each of these left a live
    /// core driving the region: ticking, campaigning, answering every Raft message, while the
    /// request path read a handle nobody published into and answered `NotLeader` for ever.
    ///
    /// One test rather than five, and it **collects** rather than stopping at the first: the
    /// answer wanted is *which* branches leak, and a run that panics on branch one says nothing
    /// about the other four.
    ///
    /// Red before the fix, in the shape the field had — the ticker spawned while the peer was
    /// still only reserved, so its `Arc` outlived the refusal and nothing else would give the
    /// region back: `an overlapping range left a core driving region 41: region 41 [b"a", b"b")
    /// overlaps region 1 [b"", b"") already on this store`.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_refusal_leaves_nothing_driving_the_region_it_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = replicated_store(&dir);
        let hosted = store.regions().regions()[0].clone();
        let mine = store
            .peer_of(hosted.id)
            .expect("the bootstrapped region is replicated")
            .peer_id();
        assert!(
            store.drivers.driving(hosted.id),
            "the bootstrapped region is driven by its own peer"
        );
        let mut leaked: Vec<String> = Vec::new();

        // 1. `host_region`, a range that overlaps one this store already holds. The bootstrapped
        //    region is the whole key space, so any range at all overlaps it.
        let refused = store
            .host_region(record(41, b"a", b"b"))
            .expect_err("an overlapping range");
        if store.drivers.driving(41) {
            leaked.push(format!(
                "an overlapping range left region 41 driven ({refused})"
            ));
        }

        // 2. `adopt_split`, a parent this store does not host.
        let refused = store
            .adopt_split(&record(42, b"", b"m"), &record(43, b"m", b""))
            .expect_err("a parent this store does not host");
        if store.drivers.driving(43) {
            leaked.push(format!(
                "a split of a region this store does not host left its child 43 driven ({refused})"
            ));
        }

        // 3. `adopt_split`, a parent whose start key a split appears to have moved.
        let moved = Region {
            start_key: Bytes::from_static(b"a"),
            ..hosted.clone()
        };
        let refused = store
            .adopt_split(&moved, &record(44, b"m", b""))
            .expect_err("a parent whose start key moved");
        if store.drivers.driving(44) {
            leaked.push(format!(
                "a split that moved its parent's start key left its child 44 driven ({refused})"
            ));
        }

        // 4. `host_region`, a region id this store already hosts, and
        // 5. `adopt_split`, a child this store already hosts. Neither of these can leave an
        //    *unhosted* region driven — the id is one this store serves — so what they are checked
        //    for is the other face of the same defect: the region's own peer must still be the
        //    core that answers for it.
        let _ = store
            .host_region(record(hosted.id, b"", b""))
            .expect_err("a region already hosted");
        if store.answered_by(hosted.id).await != Some(mine) {
            leaked.push(format!(
                "a second host of region {}: its peer is no longer the core that answers for it",
                hosted.id
            ));
        }
        let _ = store
            .adopt_split(&hosted, &record(hosted.id, b"m", b""))
            .expect_err("a child already hosted");
        if store.answered_by(hosted.id).await != Some(mine) {
            leaked.push(format!(
                "a split into region {}, which this store already hosts: its peer is no longer \
                 the core that answers for it",
                hosted.id
            ));
        }

        assert!(leaked.is_empty(), "{}", leaked.join("\n  "));
        assert!(
            store.drivers.driving(hosted.id),
            "the region this store does host stopped being driven at all"
        );
    }

    /// A region with the peers named, and nothing else: what `conf_change_for` reads.
    fn placed(peers: Vec<Peer>) -> RegionState {
        RegionState::unreplicated(RegionMeta::replicated(1, peers))
    }

    /// A re-issued `AddPeer` for a store that already has a peer of this region proposes nothing.
    ///
    /// **PD mints a fresh peer id every time it issues one** — `repair.rs`, "a fresh peer id, from
    /// the persisted allocator, every time an `AddPeer` is issued ... reusing the id of an operator
    /// PD has forgotten would risk two peers with one id". So "is this already done?" cannot be
    /// asked by peer id: the id is different by construction on every re-derivation, and the check
    /// that asked by id answered "no" to every repeat.
    ///
    /// What that produced, found under load in `tests/promotion.rs`: an `AddPeer` whose proposal
    /// was answered *ambiguously* — the leader stepped down with it in its log and it committed
    /// anyway — was re-derived by PD onto the same store, and the second one committed too. The
    /// region then had two peers on one store, and a store hosts one peer per region, so the second
    /// could never be created: `no store known for a peer of this region` while the record caught
    /// up, then `a message was addressed to a peer on this store; it was dropped` for ever after.
    /// A learner at `matched=0`, `recent_active=false`, never promoted, with PD no longer seeing
    /// the region as short and so never repairing it.
    #[test]
    fn a_re_issued_add_peer_does_not_place_a_second_peer_on_a_store_that_has_one() {
        let state = placed(vec![Peer::voter(1, 10), Peer::learner(3, 20)]);
        // Store 3 already has peer 20. PD, whose view has not caught up, asks again with a new id.
        assert!(
            conf_change_for(
                &Operator::AddPeer {
                    region_id: 1,
                    epoch: Epoch::INITIAL,
                    store_id: 3,
                    peer_id: 27,
                },
                &state
            )
            .is_none(),
            "a second peer was placed on a store that already hosts one, which no store can \
             create and nothing can undo"
        );
        // And the same store with no peer of this region is still added, or the guard would have
        // turned a repeat into a refusal to repair at all.
        assert!(
            conf_change_for(
                &Operator::AddPeer {
                    region_id: 1,
                    epoch: Epoch::INITIAL,
                    store_id: 4,
                    peer_id: 27,
                },
                &state
            )
            .is_some(),
            "a store with no peer of this region was refused"
        );
    }

    /// The peer id is still a repeat when it *is* reused, which is the case the guard was written
    /// for and which must not be lost while widening it.
    #[test]
    fn an_add_peer_for_a_peer_id_already_here_still_proposes_nothing() {
        let state = placed(vec![Peer::voter(1, 10), Peer::learner(3, 20)]);
        assert!(
            conf_change_for(
                &Operator::AddPeer {
                    region_id: 1,
                    epoch: Epoch::INITIAL,
                    store_id: 3,
                    peer_id: 20,
                },
                &state
            )
            .is_none()
        );
    }

    /// A columnar learner is placed by the same rule and breaks the same way.
    ///
    /// ADR 0022 puts a columnar replica on a store *without* a peer — "three voters need a fourth"
    /// — so a second one on a store that already has any peer of the region is the same
    /// unroutable state, arriving through the other operator.
    #[test]
    fn a_re_issued_add_learner_does_not_place_a_second_peer_on_a_store_that_has_one() {
        let state = placed(vec![
            Peer::voter(1, 10),
            Peer::voter(2, 11),
            Peer::voter(3, 12),
        ]);
        assert!(
            conf_change_for(
                &Operator::AddLearner {
                    region_id: 1,
                    epoch: Epoch::INITIAL,
                    store_id: 2,
                    peer_id: 30,
                },
                &state
            )
            .is_none(),
            "a columnar learner was placed on a store that already hosts a peer of this region"
        );
    }

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

    /// **A region bound and a run's key must be the same byte form**, and the raw bound is not it.
    ///
    /// This is the assertion the unit tests behind the region-scoping fix did not have, and its
    /// absence is why they passed while a real cluster answered 52 rows for 200. They compared
    /// synthetic keys against synthetic bounds — both raw, both agreeing — where the product
    /// compares a run's `enc(user_key) ++ !ts` against a region record's *raw* bound.
    ///
    /// So it asserts the relationship rather than an answer: the encoded bound is a **prefix** of
    /// every version of that key, and the raw bound is not a prefix of anything. The second half is
    /// what fails on the old code.
    #[test]
    fn a_region_bound_is_a_prefix_of_every_run_key_for_that_row() {
        // A SQL row key: `'t' ++ tenant ++ table_id ++ …`, which is what a region bound holds.
        let row = esker_keys::row::row_key(1, 1, &[esker_keys::value::Datum::Int8(7)])
            .expect("a row key");

        let bound = super::as_run_key(&row);
        for ts in [1_u64, 42, u64::MAX] {
            let mut run_key = esker_txn::key::write(&row, ts);
            // What `columnar::region::versioned` stores: the same bytes without the namespace byte.
            run_key.remove(0);
            assert!(
                run_key.starts_with(&bound),
                "a version at {ts} does not start with its own region bound\n  key   {run_key:?}\n  bound {bound:?}"
            );
            assert!(
                !run_key.starts_with(&row),
                "the RAW bound is not a prefix — comparing it against a run key is the defect"
            );
        }

        // Unbounded stays unbounded: encoding an empty bound would bound something.
        assert!(super::as_run_key(b"").is_empty());
    }
}
