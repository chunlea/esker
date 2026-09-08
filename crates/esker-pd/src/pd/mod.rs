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
//! Replica repair — observing an operator, retiring it, issuing the next one — is the private
//! `repair` child module. It is a child rather than a sibling because it reaches into this
//! one's private state on every heartbeat, and because the two halves are one lock's worth of
//! work: the file is split for reading, not for isolation.
//!
//! # One lock
//!
//! Allocation, timestamp issue and the epoch-guarded upsert are all read-modify-write over
//! state that must not interleave, so they share one mutex. Lookups do not take it: they read
//! the engine, which is concurrent, and a lookup racing an upsert is exactly the staleness the
//! design already tolerates (`docs/DESIGN.md` §7).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use esker_engine::{Db, FileSystem, LocalFileSystem, Options, WalSyncMode, WriteBatch, cf};
use esker_proto::pd::{ColumnarWish, PdMemberInfo, PdMembership, PdRaftBatch, PdRole};
use esker_proto::{Operator, OperatorProgress, OperatorStatus, Region, ScannedRegion, StoreInfo};
use esker_raft::{ConfChange, ConfChangeKind, Config, NodeId, Term};

use crate::alloc::{ALLOC_BATCH, Allocator};
use crate::clock::{Clock, SystemClock};
use crate::command::Command;
use crate::driver::{Leadership, NoPeers, PdDriver, PdTransport, conf_change_with_address};
use crate::error::{PdError, Result};
use crate::machine::{Answer, Machine};
use crate::member::MemberList;
use crate::operator::{InFlight, Progress};
use crate::raft_log::PdLogStorage;
use crate::record::{ClusterRecord, OperatorEvent, RegionRecord, StoreRecord};
use crate::routing::{self, RegionBeat, StoreBeat, Upsert};
use crate::schedule::{self, LoadDelta};
use crate::tso::Oracle;

mod repair;

/// How long a store may be silent before it is considered down (`docs/DESIGN.md` §7).
pub const MAX_STORE_DOWN_TIME_MS: u64 = 30_000;

/// How many regions one [`Pd::scan_regions`] page carries when the caller names no limit.
///
/// Chosen so the ordinary cluster is one round trip and a large one is a handful: at 96 bytes of
/// region record apiece a page is tens of kilobytes, well under any frame limit, and a thousand
/// regions is eight calls instead of a thousand.
pub const DEFAULT_SCAN_REGIONS: u32 = 128;

/// The most a caller may ask for in one page, however large a limit it sends.
///
/// A caller's limit is a request; the server's cap is the thing that decides. Without one, "give
/// me every region" is a message whose size the caller chose and the server allocated.
pub const MAX_SCAN_REGIONS: u32 = 1024;

/// How long a SQL node may serve **writes** from a cached schema before asking PD again
/// ([ADR 0028](../../../docs/adr/0028-the-schema-lease.md), ADR 0020 as amended).
///
/// Writes only. A reader's snapshot already agrees with the rows it can see, so gating reads would
/// add stalls and close no hole — and a node that cannot renew must **stop writing**, which is what
/// lets [`Pd::schema_lease`]'s step interval be a timer rather than a poll of nodes PD may not be
/// able to reach.
///
/// Five seconds: comfortably above the lock TTL a writer is already bounded by, and short enough
/// that a schema change is tens of seconds rather than minutes.
pub const SCHEMA_LEASE_MS: u64 = 5_000;

/// The schema lease, and the step arithmetic that depends on it.
///
/// The **interval is computed**, never configured: one that somebody could tune is one somebody
/// could tune below the bound it exists to keep (`docs/plans/phase-6e.md` §3). Its *inputs* are a
/// different matter — the lock TTL and the retention window belong to layers above PD, which does
/// not link them, so PD is **told** them ([`PdOptions`]) rather than keeping copies that could
/// drift. ADR 0020 said PD already owned both; it does not, and being told is the honest shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLease {
    /// How long a node may serve writes from a cached schema. [`SCHEMA_LEASE_MS`].
    pub lease_ms: u64,
    /// `lease_ms + lock_ttl_ms` — how long a step in the **adding** direction must wait before the
    /// next one.
    ///
    /// Two terms, and each is a bound on how stale a *writer* can be. The lease is how long a node
    /// may act without hearing from PD; the lock TTL is how long a transaction that has already
    /// started may still commit (`docs/txn-spec.md` §5.2). Past their sum, no writer can still be
    /// acting on a state two steps behind — which is ADR 0020's two-version invariant.
    pub step_interval_ms: u64,
    /// What a **removing** step waits on top of [`SchemaLease::step_interval_ms`]: the MVCC
    /// retention window.
    ///
    /// Separate rather than folded in, because for an *add* it is inert and retention is an hour:
    /// a reader that sees an index as public does so from a snapshot above the backfill, so the
    /// index it reads is complete at any age. For a *remove* it is real — a reader at `public`
    /// reads entries a node at `absent` has already deleted, and retention is what keeps them
    /// readable (ADR 0020, as amended by `docs/plans/phase-6e.md` §1).
    pub removal_extra_ms: u64,
}

/// How long after an operator retires before that region may be moved for **balance** again.
///
/// Balance is an optimisation, and a region that has just been moved has nothing to gain from
/// being moved again immediately. The rule's own threshold ([`crate::balance`]) is what stops
/// it oscillating; this is the weaker second guard, for the case the arithmetic cannot see — a
/// store whose reported counts are lagging several heartbeats behind what PD has already asked
/// for. Repair ignores it: a region a failure away from losing quorum does not wait.
pub const BALANCE_COOLDOWN_MS: u64 = 300_000;

/// Balance moves PD will start while others are still in flight.
///
/// `prompts/04-multiraft-pd.md` 4d asks for balance to have "in-flight limits", and the reason
/// is sharper than throttling I/O. A region **mid-move sits on two stores and is counted on
/// both**, so every move in flight inflates the very numbers the next decision is taken from.
/// Effective counts ([`crate::schedule::LoadDelta`]) correct for the operator itself; they
/// cannot correct for a replica that genuinely exists in two places until the move finishes.
/// Bounding the moves in flight bounds that inflation, and with it the number of moves made
/// against a picture that is slightly wrong.
///
/// Repair is never capped, and neither is *finishing* a move already begun — a cap that could
/// strand a half-done move would be worse than no cap at all.
pub const MAX_BALANCE_OPERATORS: usize = 4;

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
    /// A writing transaction's lease on its locks — `esker_client::LOCK_TTL_MS`.
    ///
    /// One of the two bounds in [`Pd::schema_lease`]'s step interval, and it belongs to the client,
    /// which sits *above* PD in `CLAUDE.md`'s layer table and which PD therefore does not link. It
    /// is told rather than copied: a copy is a number that can drift, and a step interval short by
    /// exactly the drift is a step interval that is unsafe rather than merely wrong.
    pub lock_ttl_ms: u64,
    /// The MVCC retention window — `esker_sql::catalog::DEFAULT_RETENTION_MS`.
    ///
    /// The other bound, and it is what a **removing** step waits on top of the interval. Told for
    /// the same reason, from a crate that is even further above. A test sets it small, which is the
    /// only way a staged removal is testable at all.
    pub retention_ms: u64,
    /// How long an operator may make no observable progress before it is abandoned.
    pub operator_timeout_ms: u64,
    /// Replicas a region should have. Repair restores this; it does not grow past it.
    pub target_replicas: usize,
    /// How long after an operator retires before that region may be balanced again.
    pub balance_cooldown_ms: u64,
    /// Balance moves that may be *started* while others are in flight.
    pub max_balance_operators: usize,
    /// Whether the balance rules run at all. On by default; a test or an operator wanting a
    /// cluster left exactly as it is turns them off, and repair still runs.
    pub balance: bool,
    /// This placement driver's own member id.
    pub id: NodeId,
    /// The group it belongs to, itself included. One member is the 4a shape and needs no
    /// addresses; three is high availability ([`crate::member`]).
    pub members: MemberList,
    /// Where this member's Raft messages go. `None` is [`NoPeers`], which is right for a group of
    /// one and a dropped message for anything else.
    pub transport: Option<Arc<dyn PdTransport>>,
    /// The seed for the election-timeout generator.
    ///
    /// Shared across a whole group on purpose: the member id selects the stream, so members still
    /// differ while one number still reproduces a run (`esker_raft::Config`).
    pub raft_seed: u64,
    /// Where PD's database keeps its bytes. `None` is the real filesystem.
    ///
    /// The seam exists for the scheduling tests, which drive tens of thousands of heartbeats:
    /// every one of them is a durable write, and on a real disk a thousand rounds over a
    /// hundred regions is eight minutes of `fsync` for a property that has nothing to do with
    /// durability. `esker_engine::memfs` makes the same test seconds. It changes *where* the
    /// bytes go and not whether they are flushed, so nothing about invariant 1 is relaxed —
    /// the crash tests use a real disk, because that is what they are about.
    pub filesystem: Option<Arc<dyn FileSystem>>,
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
            // The defaults name their sources. A cluster that runs a different lock TTL has to say
            // so here, and the consequence of not saying so is written down in ADR 0028.
            lock_ttl_ms: 3_000,
            retention_ms: 60 * 60 * 1_000,
            operator_timeout_ms: OPERATOR_TIMEOUT_MS,
            target_replicas: schedule::TARGET_REPLICAS,
            balance_cooldown_ms: BALANCE_COOLDOWN_MS,
            max_balance_operators: MAX_BALANCE_OPERATORS,
            balance: true,
            id: 1,
            members: MemberList::alone(1),
            transport: None,
            raft_seed: 0x0E5C_0E5C_0E5C_0E5C,
            filesystem: None,
        }
    }

    /// The defaults, for member `id` of `members`.
    pub fn with_members(id: NodeId, members: MemberList) -> Result<Self> {
        if !members.contains(id) {
            return Err(PdError::invalid(format!(
                "placement driver {id} is not in its own member list"
            )));
        }
        Ok(Self {
            id,
            members,
            ..Self::new()
        })
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
    clock: Arc<dyn Clock>,
    /// The replicated state machine every durable write goes through
    /// ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
    machine: Arc<Machine>,
    /// The Raft group underneath. Dropped last, which joins its thread.
    driver: PdDriver,
    /// The group this member believes it is in, and what that group is called.
    ///
    /// **It moves**, which is what dynamic membership means
    /// ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)):
    /// a conf change appended to the log rewrites it, at append, before the messages of that same
    /// `Ready` go out. The lock is its own, taken briefly and never across a propose — the same
    /// rule the applied state follows and for the same reason ([`crate::driver`]).
    members: Arc<RwLock<MemberList>>,
    /// Ids reserved per commit. Kept because a member rebuilds its allocator on taking office.
    alloc_batch: u64,
    /// How far ahead the mark is written. Kept for the same reason.
    tso_save_interval_ms: u64,
    max_store_down_time_ms: u64,
    /// See [`PdOptions::lock_ttl_ms`]. Read only by [`Pd::schema_lease`].
    lock_ttl_ms: u64,
    /// See [`PdOptions::retention_ms`]. Read only by [`Pd::schema_lease`].
    retention_ms: u64,
    operator_timeout_ms: u64,
    target_replicas: usize,
    balance_cooldown_ms: u64,
    max_balance_operators: usize,
    balance: bool,
    state: Mutex<State>,
}

/// The **leader's** working state: the part that must not interleave, and that a member rebuilds
/// when it takes office.
///
/// None of it is durable, and that is the point. The allocator's reservation and the oracle's mark
/// live in the log ([`crate::machine::AppliedState`]); what is here is the position inside a
/// reservation, which a new leader recomputes as `allocated_end + 1` — and which is why a deposed
/// leader that has not noticed can keep handing out ids and timestamps without colliding with its
/// successor's.
#[derive(Debug)]
pub(crate) struct State {
    /// The term this working state was rebuilt for; `0` before this member has ever served.
    ///
    /// Compared against [`Leadership::office_term`] on every write. A member that finds them
    /// different has just taken office and reloads the allocator and the oracle from what it has
    /// applied, which is the whole of the failover rule on this side.
    pub(crate) office_term: Term,
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
    /// When each region becomes eligible for a *balance* move again, by region id.
    ///
    /// Memory, like the in-flight set: a restart forgets it, and the worst that costs is one
    /// move that would otherwise have waited. Entries older than now are pruned as they are
    /// passed, so this does not grow with the number of regions ever balanced.
    pub(crate) cooling: BTreeMap<u64, u64>,
    /// The load of operators that have **finished**, still corrected for because the stores
    /// they moved have not said so themselves yet.
    ///
    /// [`crate::schedule::LoadDelta`] exists because a store's own counts are a heartbeat
    /// behind, and it corrects them for every operator in flight. The correction has to outlive
    /// the operator, and this is the piece that was missing: a store reports every
    /// `store_heartbeat` interval while PD issues operators between two of them, so a move that
    /// landed a moment ago is in neither the in-flight set nor the report — and every region
    /// that heartbeats before the report arrives reads the busy store at its full, unmoved
    /// count and decides, one region at a time, that it should be the next to leave.
    ///
    /// That is a **sweep** rather than an oscillation, which is why the spread threshold cannot
    /// see it: each move on its own strictly reduces the spread, and sixteen of them in a row
    /// still empty a store (`docs/adr/0023-a-retired-operators-load-outlives-it.md`).
    ///
    /// Memory, and bounded twice over: an entry is dropped as soon as every store it names has
    /// reported since the operator retired, and unconditionally once it is older than
    /// `max_store_down_time` — past which a store that has not reported is down and its counts
    /// mean nothing anyway.
    pub(crate) settling: Vec<(LoadDelta, u64)>,
}

impl Pd {
    /// Opens, or creates, PD's database in `path`.
    ///
    /// A directory with no cluster record is a PD that has not been bootstrapped: it answers
    /// [`PdError::NotBootstrapped`] to everything except [`Pd::bootstrap`], rather than
    /// inventing a cluster of its own.
    pub fn open(path: impl AsRef<Path>, options: PdOptions) -> Result<Arc<Self>> {
        if !options.members.contains(options.id) {
            return Err(PdError::invalid(format!(
                "placement driver {} is not in its own member list",
                options.id
            )));
        }
        let filesystem = options
            .filesystem
            .clone()
            .unwrap_or_else(|| Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>);
        // Two families, one WAL: the records in `default` and the Raft log in `raft`, so an apply
        // writes the record and the apply index in one atomic batch. A 4a directory has no `raft`
        // family and gains one here; nothing else about it changes
        // ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
        let db = Db::open_with(
            path,
            options.engine.clone(),
            filesystem,
            &[cf::DEFAULT, cf::RAFT],
        )?;
        let cf = db
            .cf_id(cf::DEFAULT)
            .ok_or_else(|| PdError::internal("the default column family is missing after open"))?;
        let db = Arc::new(db);

        let machine = Arc::new(Machine::load(
            Arc::clone(&db),
            cf,
            Arc::clone(&options.clock),
        )?);
        let mut log = PdLogStorage::open(
            Arc::clone(&db),
            esker_raft::ConfState::from_voters(options.members.ids()),
        )?;
        // **Who this member is with, and what the group is called.** The record wins over the
        // command line, which is `esker-raft`'s own rule for membership — after a membership change
        // `--peers` is exactly the stale thing that rule is about
        // ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
        let members = settle_membership(&mut log, &options.members, &db)?;
        let applied = log.applied_index();

        let mut config = Config::new(options.id, members.ids(), options.raft_seed);
        config.applied = applied;
        let alone = members.is_alone();
        let transport = options
            .transport
            .clone()
            .unwrap_or_else(|| Arc::new(NoPeers) as Arc<dyn PdTransport>);
        let shared = Arc::new(RwLock::new(members.clone()));
        let driver = PdDriver::start(
            config,
            log,
            Arc::clone(&machine),
            transport,
            Arc::clone(&shared),
        )?;

        // A group of one wins with a quorum of itself, so it is leading and caught up before this
        // returns — which is what keeps `Pd::open` synchronous, runtime-free and behaviourally
        // identical to the single durable PD it replaces. A larger group elects on ticks.
        if alone {
            driver.campaign()?;
            if !driver.leadership().serving {
                return Err(PdError::internal(
                    "a single-member placement driver did not take office",
                ));
            }
        }

        let (allocated_end, high_water_ms) = {
            let state = machine
                .applied()
                .read()
                .map_err(|_| PdError::internal("pd applied state lock is poisoned"))?;
            (state.allocated_end, state.high_water_ms)
        };
        let office = driver.leadership();
        // `max(clock, mark)`, the restart rule, is inside `Oracle::load`.
        let oracle = Oracle::load(
            (high_water_ms > 0).then_some(crate::record::TsoRecord { high_water_ms }),
            office_clock(&office, options.clock.as_ref()),
            options.tso_save_interval_ms,
        );

        Ok(Arc::new(Self {
            db,
            clock: options.clock,
            machine,
            driver,
            members: shared,
            alloc_batch: options.alloc_batch,
            tso_save_interval_ms: options.tso_save_interval_ms,
            max_store_down_time_ms: options.max_store_down_time_ms,
            lock_ttl_ms: options.lock_ttl_ms,
            retention_ms: options.retention_ms,
            operator_timeout_ms: options.operator_timeout_ms,
            target_replicas: options.target_replicas,
            balance_cooldown_ms: options.balance_cooldown_ms,
            max_balance_operators: options.max_balance_operators,
            balance: options.balance,
            state: Mutex::new(State {
                office_term: office.office_term,
                alloc: Allocator::load(
                    (allocated_end > 0).then_some(crate::record::AllocRecord { allocated_end }),
                    options.alloc_batch,
                ),
                oracle,
                in_flight: BTreeMap::new(),
                cooling: BTreeMap::new(),
                settling: Vec::new(),
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
        Ok(self.applied()?.cluster)
    }

    /// The cluster's id, or [`PdError::NotBootstrapped`].
    pub fn cluster_id(&self) -> Result<u64> {
        self.applied()?
            .cluster
            .map(|cluster| cluster.cluster_id)
            .ok_or(PdError::NotBootstrapped)
    }

    /// What this member believes about who leads PD's own Raft group.
    #[must_use]
    pub fn leadership(&self) -> Leadership {
        self.driver.leadership()
    }

    /// The group this member believes it is in.
    ///
    /// A clone, because it moves: holding a reference across a membership change would be holding
    /// a list that is no longer the group's.
    #[must_use]
    pub fn members(&self) -> MemberList {
        self.member_list()
    }

    /// The membership, or — if the lock is poisoned, which means a thread panicked holding it — a
    /// group of this member alone. Poisoned means this process is finished either way; answering
    /// something rather than panicking is `CLAUDE.md` invariant 9.
    fn member_list(&self) -> MemberList {
        self.members
            .read()
            .map_or_else(|_| MemberList::alone(self.driver.id()), |held| held.clone())
    }

    /// Whether this member may answer. A follower answers nothing but [`PdError::NotLeader`].
    #[must_use]
    pub fn is_serving(&self) -> bool {
        self.driver.leadership().serving
    }

    /// Feeds a batch of this group's Raft messages in.
    ///
    /// **Not leader-only, and not cluster-checked.** Consensus is how a member becomes the
    /// leader, so refusing this on a follower would refuse the only traffic that can end an
    /// election; and the cluster id cannot guard it, because the group elects before `Bootstrap`
    /// has minted one ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
    ///
    /// The **group id** is the guard instead, and it is the one this method exists to check. Two
    /// clusters' placement drivers pointed at each other by a stale flag would otherwise form one
    /// group and replicate one cluster's routing table over the other's, which is the mistake
    /// [ADR 0011](../../../docs/adr/0011-pd-service-and-the-cluster-id.md) was written about with
    /// a worse consequence.
    pub fn step_raft(&self, batch: &PdRaftBatch) -> Result<()> {
        let members = self.member_list();
        let expected = members.group_id();
        if batch.group_id != expected {
            return Err(PdError::invalid(format!(
                "a placement-driver batch from member {} is for group {:#018x}; this is group                  {expected:#018x} — check that every --pd-peers list names the same members",
                batch.from, batch.group_id
            )));
        }
        // A correct group id implies a member this list names, so this can only fire on a
        // deliberate forgery or a hash collision. Refused rather than stepped: a message from
        // outside the configuration is one the core would have to reason about.
        if !members.contains(batch.from) {
            return Err(PdError::invalid(format!(
                "a placement-driver batch claims to come from member {}, which is not in this                  group",
                batch.from
            )));
        }
        for message in &batch.messages {
            self.driver.step(message.clone())?;
        }
        Ok(())
    }

    /// Who is in this member's group, and which member it believes leads.
    ///
    /// Answered by **any** member, and that is the decision rather than an oversight: an operator
    /// asks this exactly when the leader is the thing that is missing, so a version only the
    /// leader could answer would be useless at the moment it was wanted. It reads configuration
    /// and this member's own belief, both of which every member has.
    #[must_use]
    pub fn membership(&self) -> PdMembership {
        let office = self.driver.leadership();
        let members = self.member_list();
        PdMembership {
            group_id: members.group_id(),
            this_id: office.id,
            leader_id: office.leader.unwrap_or(0),
            term: office.term,
            members: members
                .members()
                .iter()
                .map(|member| PdMemberInfo {
                    id: member.id,
                    address: member.address.clone(),
                    // **As this member sees it, and only once it has seen anything.**
                    //
                    // A member's log is *seeded* with the member list it was opened with
                    // (`PdLogStorage::open`), so its conf names every member a voter from birth —
                    // before a single entry has reached it. Reading the conf alone therefore
                    // reports a joiner as configured the moment its own driver publishes for the
                    // first time, which is a race against nothing but the scheduler: three
                    // sightings of `failover.rs`'s
                    // `a_membership_report_says_which_member_is_still_catching_up`, all under a
                    // loaded gate, none in fifteen runs alone.
                    //
                    // *Configured* and *caught up* are two facts, and the seed only carries one of
                    // them. A member that has applied nothing has caught up on nothing, whatever
                    // its log was seeded with — which is what a joiner honestly is until the
                    // group's entries reach it, and is the one report an operator debugging a join
                    // actually needs.
                    role: if office.applied == 0 {
                        PdRole::Unconfigured
                    } else if office.conf.voters.contains(&member.id) {
                        PdRole::Voter
                    } else if office.conf.learners.contains(&member.id) {
                        PdRole::Learner
                    } else {
                        PdRole::Unconfigured
                    },
                })
                .collect(),
        }
    }

    /// How far this member's own log has been applied; `0` means nothing has.
    ///
    /// Published by the driver beside the configuration, and the fact that tells a **seeded** conf
    /// from a caught-up one — see [`Pd::membership`], which is the reason this is not private.
    #[must_use]
    pub fn applied_index(&self) -> esker_raft::Index {
        self.driver.leadership().applied
    }

    /// One step of adding `id` at `address` to this group. Call again until it answers `true`.
    ///
    /// **A reconciliation, not a script**, and the shape is the whole of its correctness. Adding a
    /// member is three things — propose a learner, wait for it to catch up, promote it — so there
    /// are three places for a `kill -9` to land, and an operator who reruns the command after one
    /// must not be told "that member already exists". So this looks at what is *there* and does
    /// what is missing:
    ///
    /// | Found | Done |
    /// |---|---|
    /// | nothing | propose `AddLearner`, with the address in its context |
    /// | a learner, behind | nothing; the answer is "not yet" |
    /// | a learner, caught up | propose `AddVoter` — the promotion |
    /// | a voter | nothing; the answer is "done" |
    ///
    /// **A learner first, because a voter would deadlock the group.** With three members and one
    /// gone, `AddVoter` takes the quorum to three the instant its entry is appended — a
    /// configuration is in force from then, not from when it commits — and the entry that made the
    /// quorum three needs three to commit, while two are live and the new one has an empty log. A
    /// learner is not counted in a quorum, so it commits, catches up, and *then* counts
    /// ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)
    /// §11.4).
    pub fn add_member(&self, id: NodeId, address: &str) -> Result<bool> {
        if id == 0 {
            return Err(PdError::invalid("member id 0 is reserved for 'no member'"));
        }
        address
            .parse::<std::net::SocketAddr>()
            .map_err(|error| PdError::invalid(format!("`{address}` is not an address: {error}")))?;
        let _serving = self.leading()?;
        let conf = self.driver.conf_state()?;
        if conf.voters.contains(&id) {
            return Ok(true);
        }

        if conf.learners.contains(&id) {
            let Some((commit, progress)) = self.driver.progress()? else {
                return Err(self.not_leading());
            };
            let caught_up = progress
                .iter()
                .find(|peer| peer.id == id)
                .is_some_and(|peer| peer.matched >= commit);
            if !caught_up {
                return Ok(false);
            }
            // The promotion carries no address: the one this member already holds for it came in
            // with the `AddLearner`, and re-sending it would let a typo in a *second* command
            // silently move a member that is already in the group.
            self.driver
                .propose_conf_change(ConfChange::new(ConfChangeKind::AddVoter, id))?;
            return Ok(true);
        }

        if self.member_list().len() >= crate::member::MAX_MEMBERS {
            return Err(PdError::invalid(format!(
                "this placement driver's group already has {} members, which is what this build \
                 supports",
                self.member_list().len()
            )));
        }
        self.driver.propose_conf_change(conf_change_with_address(
            ConfChangeKind::AddLearner,
            id,
            address,
        ))?;
        Ok(false)
    }

    /// Removes `id` from this group. Idempotent: removing one that is not there answers `true`.
    ///
    /// Refused for the **last** member, and refused when the group would be left without a quorum
    /// of members that have been heard from — which a leader can see, because `progress` says who
    /// answered recently. Both are refusals rather than warnings: a placement driver that removed
    /// its way out of a quorum could not undo it, because undoing needs the quorum it just lost.
    pub fn remove_member(&self, id: NodeId) -> Result<bool> {
        let _serving = self.leading()?;
        let conf = self.driver.conf_state()?;
        if !conf.voters.contains(&id) && !conf.learners.contains(&id) {
            return Ok(true);
        }
        if conf.voters.len() == 1 && conf.voters.contains(&id) {
            return Err(PdError::invalid(
                "this is the group's last member; removing it would leave no placement driver",
            ));
        }
        if conf.voters.contains(&id) {
            let Some((_, progress)) = self.driver.progress()? else {
                return Err(self.not_leading());
            };
            // This member is live by definition — it is the one answering. For the others,
            // `recent_active` is what a leader knows: `esker-raft` clears it at every
            // election-timeout boundary and sets it when a peer answers, so it means "heard from
            // within the last window".
            //
            // It is **conservative in both directions and that is deliberate**. A leader that has
            // only just taken office has it false for everyone — `Progress::new` starts it that
            // way and the window has not closed yet — so a removal asked in the first moments
            // after an election is refused although the group is healthy. That is the safe
            // direction: the operator retries a second later, where the other direction is a group
            // that has removed its way below a quorum and cannot undo it, because undoing needs
            // the quorum it just lost.
            //
            // The reading it cannot make is "gone for hours" versus "quiet for one window", and it
            // does not need to: either way this member has not heard from it, and a removal
            // decided on a member nobody has heard from is the removal being refused.
            let live_after = 1 + progress
                .iter()
                .filter(|peer| peer.id != self.driver.id() && peer.id != id && peer.recent_active)
                .count();
            let quorum_after = conf.voters.len() / 2 + 1;
            if live_after < quorum_after {
                return Err(PdError::invalid(format!(
                    "removing member {id} would leave {live_after} live of the {quorum_after} a \
                     quorum needs; bring a member back first, or add one"
                )));
            }
        }
        self.driver
            .propose_conf_change(ConfChange::new(ConfChangeKind::Remove, id))?;
        Ok(true)
    }

    /// Waits until the group's Raft core has driven everything posted before this call.
    ///
    /// See [`crate::driver::PdDriver::settle`]. A barrier, for a caller that ticked or stepped and
    /// needs to know what came of it.
    pub fn settle(&self) -> Result<()> {
        self.driver.settle()
    }

    /// How far each member has got, as only a leader can say. For the tools and tests.
    pub fn driver_progress(&self) -> Result<Option<(u64, Vec<esker_raft::PeerProgress>)>> {
        self.driver.progress()
    }

    /// The membership in force — the latest in the log, committed or not. For the tools and tests.
    pub fn conf_state(&self) -> Result<esker_raft::ConfState> {
        self.driver.conf_state()
    }

    /// One logical tick of the group's Raft core.
    ///
    /// Time enters here and nowhere else, sent by an interval at the service edge, so the core
    /// still never reads a clock (`CLAUDE.md` invariant 4). A group of one needs none: it wins
    /// with a quorum of itself inside [`Pd::open`] and has nothing to time out against.
    pub fn tick(&self) -> Result<()> {
        self.driver.tick()
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
        let mut state = self.leading()?;

        // Two ids in one reservation: the region and its first peer. The reservation commits
        // inside `allocate`, before either id is used for anything — and if the cluster turns out
        // to exist, the two are simply skipped, which is what ids being cheap is for.
        let base = {
            let driver = &self.driver;
            state.alloc.allocate(2, |end| reserve(driver, end))?
        };
        // Minted here rather than at apply, so that three members do not mint three
        // ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)). Ignored by an apply that
        // finds a cluster already there.
        let cluster_id = mint_cluster_id(now_ms, store_id, address);
        drop(state);

        match self.propose(Command::Bootstrap {
            store_id,
            address: address.to_owned(),
            base_id: base,
            cluster_id,
            now_ms,
        })? {
            Answer::Bootstrapped(done) => Ok(done),
            other => Err(PdError::internal(format!("bootstrap applied to {other:?}"))),
        }
    }

    /// The first of `count` consecutive cluster-unique ids.
    ///
    /// The batch they come from is persisted before any of them is returned, so a crash skips
    /// ids and never repeats one ([`crate::alloc`]).
    pub fn alloc_id(&self, count: u64) -> Result<u64> {
        let mut state = self.leading()?;
        let driver = &self.driver;
        state.alloc.allocate(count, |end| reserve(driver, end))
    }

    /// A run of `count` consecutive timestamps, starting at the returned one.
    ///
    /// The high-water mark covering them is durable before any of them leaves this call
    /// ([`crate::tso`]). This is the one place in Esker that reads a wall clock, and the one
    /// whose ordering every layer above depends on (`CLAUDE.md` invariant 6).
    pub fn tso(&self, count: u32) -> Result<u64> {
        let now_ms = self.clock.now_ms();
        let mut state = self.leading()?;
        let driver = &self.driver;
        state
            .oracle
            .allocate(count, now_ms, |mark| commit_tso(driver, mark))
    }

    /// The oracle's high-water mark, for the inspector and the tests.
    pub fn tso_high_water_ms(&self) -> Result<u64> {
        Ok(self.applied()?.high_water_ms)
    }

    /// Records which key ranges want columnar replicas, replacing whatever was there.
    ///
    /// **A full assertion, not a delta**, which is what makes it safe for every SQL node to send
    /// and safe to resend. Each reads the same catalog, so each reports the same content and the
    /// last writer is right whoever it was; a delta would need an ordering this service does not
    /// impose. Durable before it answers, for the reason above: PD cannot re-derive it.
    pub fn report_columnar(&self, wishes: Vec<ColumnarWish>) -> Result<()> {
        let _serving = self.leading()?;
        self.propose(Command::Columnar { wishes }).map(|_| ())
    }

    /// How many columnar replicas a region covering `[start, end)` should have, as the SQL layer
    /// last reported ([`crate::record::ColumnarRecord::wanted_for`]).
    ///
    /// Read out of applied state rather than out of `Pd`'s own, because the wishes are replicated:
    /// PD cannot re-derive them from any heartbeat, so they go through the log like every other
    /// record ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
    pub(crate) fn columnar_wanted_for(&self, start: &[u8], end: &[u8]) -> u8 {
        self.applied()
            .map_or(0, |applied| applied.columnar.wanted_for(start, end))
    }

    /// What the SQL layer last said about columnar placement.
    #[must_use]
    pub fn columnar_wishes(&self) -> Vec<ColumnarWish> {
        self.applied()
            .map(|applied| applied.columnar.wishes.clone())
            .unwrap_or_default()
    }

    /// The schema lease and the step arithmetic derived from it
    /// ([ADR 0028](../../../docs/adr/0028-the-schema-lease.md)).
    ///
    /// Infallible and stateless today: three published numbers and one addition. It is a method on
    /// [`Pd`] rather than a free function because that is where it will read a *configured*
    /// retention from when a cluster has one, and because the arithmetic having one home is the
    /// point — a node that held its own opinion about how long it is safe to be behind would be a
    /// node PD cannot reason about.
    #[must_use]
    pub fn schema_lease(&self) -> SchemaLease {
        SchemaLease {
            lease_ms: SCHEMA_LEASE_MS,
            step_interval_ms: SCHEMA_LEASE_MS + self.lock_ttl_ms,
            removal_extra_ms: self.retention_ms,
        }
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

    /// A page of the routing table in key order, and the stores its peers are on.
    ///
    /// What `esker region ls` walks with. The walk it replaces asked `GetRegion` once per region
    /// — correct, and `O(regions)` round trips (`docs/plans/phase-4.md` §14.6, bullet 3).
    ///
    /// `limit` is clamped to [`MAX_SCAN_REGIONS`] and zero means [`DEFAULT_SCAN_REGIONS`]: a
    /// caller's limit is a request, and a response size nobody chose is how a wire format grows a
    /// denial of service. Fewer regions than the limit means the end of the table.
    ///
    /// The store list is **deduplicated across the page** rather than repeated per region, which
    /// is the whole saving on a cluster of many regions and few stores.
    pub fn scan_regions(
        &self,
        start_key: &[u8],
        limit: u32,
    ) -> Result<(Vec<ScannedRegion>, Vec<StoreInfo>)> {
        let _ = self.cluster_id()?;
        let limit = match limit {
            0 => DEFAULT_SCAN_REGIONS,
            asked => asked.min(MAX_SCAN_REGIONS),
        };
        let records = routing::scan_ranges(&self.db, start_key, limit as usize)?;

        let mut store_ids = std::collections::BTreeSet::new();
        let mut regions = Vec::with_capacity(records.len());
        for record in records {
            for peer in &record.region.peers {
                store_ids.insert(peer.store_id);
            }
            regions.push(ScannedRegion {
                region: record.region,
                leader_peer_id: record.leader_peer_id,
            });
        }
        let mut stores = Vec::with_capacity(store_ids.len());
        for store_id in store_ids {
            if let Some(store) = routing::read_store(&self.db, store_id)? {
                stores.push(StoreInfo::new(store.store_id, store.address));
            }
        }
        Ok((regions, stores))
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
        // Refused here, before the propose, and that is the rule: the log carries decisions, so a
        // request PD will not serve never reaches it ([`crate::machine`]). Safe to read outside the
        // log because a store record is never deleted — one that exists now exists at the apply.
        let _serving = self.leading()?;
        if routing::read_store(&self.db, beat.store_id)?.is_none() {
            return Err(PdError::invalid(format!(
                "store {} has not registered; call Bootstrap first",
                beat.store_id
            )));
        }
        self.propose(Command::StoreBeat {
            store_id: beat.store_id,
            stats: beat.stats,
            now_ms,
        })
        .map(|_| ())
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
        // The guard is **not** evaluated here. A leader that read the record, decided, and then
        // proposed would be deciding against a state the log may have moved past by the time the
        // entry lands; letting the log's order settle which beat is newer is what the guard means,
        // and under Raft the log's order is the same on every member
        // ([`crate::machine`], [ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
        let serving = self.leading()?;
        let record = RegionRecord {
            region: beat.region.clone(),
            leader_peer_id: beat.leader_peer_id,
            term: beat.term,
            approximate_size: beat.approximate_size,
            applied_index: beat.applied_index,
            last_heartbeat_ms: now_ms,
        };
        drop(serving);

        let upsert = match self.propose(Command::RegionBeat {
            record: record.clone(),
        })? {
            Answer::Upserted(upsert) => upsert,
            other => {
                return Err(PdError::internal(format!(
                    "a region heartbeat applied to {other:?}"
                )));
            }
        };
        // What PD holds after this beat: the record it just wrote, or the newer one it kept.
        let held = match upsert {
            Upsert::Applied => record,
            Upsert::Stale => routing::read_region(&self.db, beat.region.id)?
                .unwrap_or_else(|| unreachable_stale(beat)),
        };

        // Scheduling happens here, on the heartbeat, and nowhere else. A store going down is
        // noticed by *absence*, so the trigger has to be somebody else's beat: repair latency
        // is therefore bounded by `max_store_down_time` plus one region-heartbeat interval,
        // and PD needs no timer thread to have it (`docs/DESIGN.md` §7, §14).
        //
        // It runs **after** the apply and outside its lock, because it is leader-only memory that
        // no member replicates (ADR 0013) — and because a scheduler that ran while the driver was
        // waiting on this thread would be the deadlock this whole layout avoids.
        //
        // Losing office in the gap costs the *operator*, not the beat. The record is committed and
        // the sender has nothing to do differently, so answering an error here would refuse a
        // heartbeat that plainly succeeded — and an operator is an optimisation a member that no
        // longer leads has no business issuing anyway.
        let Ok(mut state) = self.leading() else {
            return Ok(Beat {
                upsert,
                operator: None,
            });
        };
        let operator = self.schedule(&mut state, &held, now_ms)?;
        Ok(Beat { upsert, operator })
    }

    /// The last few things PD asked for, oldest first.
    ///
    /// A debugging record: no decision reads it, and losing it costs an explanation rather than
    /// a repair ([`crate::record::HistoryRecord`]).
    pub fn history(&self) -> Result<Vec<OperatorEvent>> {
        Ok(self.applied()?.history.events.clone())
    }

    /// Every operator in flight right now, in region order, with PD's clock.
    ///
    /// **The one thing `esker pd inspect` cannot show.** That command opens a *stopped* PD's
    /// database, and the in-flight set is deliberately not in it
    /// ([ADR 0013](../../../docs/adr/0013-repair-operators-are-requests-not-commands.md)): a
    /// restart forgets every operator and re-derives what is needed from the next round of
    /// heartbeats. So the only way to see one is to ask the running process, which is what
    /// `PdReq::Status` is for.
    ///
    /// The clock is taken **under the same lock** as the set, so an age computed from the two
    /// cannot be negative — which is exactly what reading them separately would eventually
    /// produce. That is also why this is not `in_flight()` plus a separate `clock()` call at the
    /// caller: the pairing is the point.
    pub fn status(&self) -> Result<(u64, Vec<OperatorStatus>)> {
        let state = self.lock()?;
        let now_ms = self.clock.now_ms();
        let operators = state
            .in_flight
            .values()
            .map(|entry| OperatorStatus {
                operator: entry.operator.clone(),
                progress: match entry.progress {
                    Progress::Issued => OperatorProgress::Issued,
                    Progress::Started => OperatorProgress::Started,
                },
                issued_ms: entry.issued_ms,
                since_ms: entry.since_ms,
                sends: entry.sends,
            })
            .collect();
        Ok((now_ms, operators))
    }

    /// Appends one event to the history ring.
    ///
    /// Called with `Pd`'s state lock held, which is safe and is worth saying why: the driver
    /// applies into [`crate::machine::AppliedState`], a different lock, and takes this one never
    /// — so a propose made under this lock cannot wait on itself ([`crate::driver`]).
    ///
    /// The write is durable like every other write PD makes, which now costs one Raft round trip
    /// per operator transition — a handful per region per repair, and the price of being able to
    /// answer "what did PD do" after the process is gone.
    pub(crate) fn record_event(&self, event: OperatorEvent) -> Result<()> {
        self.propose(Command::History { event }).map(|_| ())
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

    /// The leader's working state, or a refusal.
    ///
    /// Two things in one call, because they belong together and because doing the second without
    /// the first is the failover bug this phase exists to close:
    ///
    /// * **Refuse unless this member may serve.** A follower — and a leader that has not yet
    ///   applied its own `TakeOffice` — answers [`PdError::NotLeader`] with the hint it has. The
    ///   address comes from the member list, so a caller that was given three endpoints can act
    ///   on the answer without a fourth round trip.
    /// * **Rebuild on a new term.** A member that has just taken office reloads its allocator and
    ///   its oracle from what it has *applied* — `allocated_end + 1` and `max(clock, mark)` — which
    ///   is the same pair of constructors a restart uses, because a failover is a restart that
    ///   kept its socket ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
    fn leading(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        let office = self.driver.leadership();
        if !office.serving {
            return Err(self.not_leader(&office));
        }
        let mut state = self.lock()?;
        if state.office_term == office.office_term {
            return Ok(state);
        }
        let applied = self.applied()?;
        state.alloc = Allocator::load(
            (applied.allocated_end > 0).then_some(crate::record::AllocRecord {
                allocated_end: applied.allocated_end,
            }),
            self.alloc_batch,
        );
        state.oracle = Oracle::load(
            (applied.high_water_ms > 0).then_some(crate::record::TsoRecord {
                high_water_ms: applied.high_water_ms,
            }),
            office_clock(&office, self.clock.as_ref()),
            self.tso_save_interval_ms,
        );
        // **A placement driver that has just taken office is, to the scheduler, one that
        // restarted** (`docs/DESIGN.md` §7, ADR 0013), and this is the line that makes that
        // literally true rather than nearly true. All three of these are leader-only memory about
        // operators *this member* issued, and while it was not leading somebody else was: keeping
        // them would mean acting on a plan formed for a cluster that has moved on. Re-deriving
        // from the next round of heartbeats is the case phase 4 already tests.
        state.in_flight.clear();
        state.cooling.clear();
        state.settling.clear();
        state.office_term = office.office_term;
        tracing::info!(
            id = office.id,
            term = office.office_term,
            next_id = state.alloc.next_id(),
            physical_ms = state.oracle.physical_ms(),
            "took office; the allocator and the oracle resume above what is committed"
        );
        Ok(state)
    }

    /// The refusal a member that cannot serve answers, with the address of the one that can.
    ///
    /// For the service, which checks leadership once at the top of its dispatch rather than
    /// per method.
    #[must_use]
    pub fn not_leading(&self) -> PdError {
        self.not_leader(&self.driver.leadership())
    }

    /// The refusal a member that cannot serve answers, with the address of the one that can.
    fn not_leader(&self, office: &Leadership) -> PdError {
        let leader_id = office.leader.unwrap_or(0);
        PdError::NotLeader {
            leader_id,
            leader_address: self
                .member_list()
                .address_of(leader_id)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// Proposes a command and waits for it to apply. Every durable write goes through here.
    fn propose(&self, command: Command) -> Result<Answer> {
        self.driver.propose(command)
    }

    /// The Raft group underneath, for the child module that also allocates ids.
    pub(crate) fn driver(&self) -> &PdDriver {
        &self.driver
    }

    /// What the state machine has applied, for the reads that come out of memory.
    fn applied(&self) -> Result<std::sync::RwLockReadGuard<'_, crate::machine::AppliedState>> {
        self.machine
            .applied()
            .read()
            .map_err(|_| PdError::internal("pd applied state lock is poisoned"))
    }

    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| PdError::internal("pd state lock is poisoned"))
    }
}

/// A heartbeat can only be stale against a record that exists, so this is unreachable — and it
/// builds a record from the beat rather than panicking if the impossible happens
/// (`CLAUDE.md` invariant 9).
fn unreachable_stale(beat: &RegionBeat) -> RegionRecord {
    RegionRecord::new(beat.region.clone(), 0)
}

/// Commits the oracle's mark. Called *before* a timestamp at or above it is handed out.
///
/// "Durable" now means "applied", and the difference is the whole of
/// [ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md): this returns only once the entry has
/// committed and this member has applied it, so a leader that has quietly lost office fails here
/// rather than handing out a timestamp its successor will hand out again.
fn commit_tso(driver: &PdDriver, high_water_ms: u64) -> Result<()> {
    driver
        .propose(Command::AdvanceTso {
            mark: high_water_ms,
        })
        .map(|_| ())
}

/// Commits an id reservation. Called by the allocator *before* it hands out an id, and durable in
/// the same sense as [`commit_tso`].
fn reserve(driver: &PdDriver, allocated_end: u64) -> Result<()> {
    driver
        .propose(Command::ReserveIds { end: allocated_end })
        .map(|_| ())
}

/// Settles what group this member is in, and what it is called, at open.
///
/// Three cases, and the first is the only one that writes:
///
/// * **a fresh database, or one written before there were group ids.** The id is derived from the
///   configured list — *once* — and written down with the members. Deriving it is what makes the
///   upgrade invisible: every deployment running today computes exactly this id on every start, so
///   the first member upgraded keeps talking to the ones that have not been.
/// * **a member joining an existing group**, which was told the id and the members by
///   [`PdOptions::members`] and writes them before it starts.
/// * **an existing member.** The record answers, and a `--peers` that disagrees is a warning rather
///   than an argument — after a membership change that flag is exactly the out-of-date command line
///   `esker_raft::Config` says not to believe.
fn settle_membership(
    log: &mut PdLogStorage,
    configured: &MemberList,
    db: &Arc<Db>,
) -> Result<MemberList> {
    let recorded = log.group_id();
    let told = configured.recorded_group_id();

    if recorded == 0 {
        let group_id = told.unwrap_or_else(|| configured.derived_group_id());
        let mut batch = WriteBatch::new();
        log.stage_group(&mut batch, group_id, configured.members())?;
        db.write(batch, &esker_engine::WriteOptions::synced())?;
        tracing::info!(
            group_id = format_args!("{group_id:#018x}"),
            members = configured.len(),
            minted = told.is_none(),
            "placement-driver group settled"
        );
        return Ok(configured.clone().with_group_id(group_id));
    }

    if let Some(told) = told
        && told != recorded
    {
        return Err(PdError::invalid(format!(
            "this placement driver belongs to group {recorded:#018x} and was told to join \
             {told:#018x}; a group id is minted once and never changes"
        )));
    }
    if log.members().is_empty() {
        // Recorded id, no address book: a version-1 record whose id was minted on a previous open
        // of *this* build. Take the configured list, which is the only one there is.
        let mut batch = WriteBatch::new();
        log.stage_group(&mut batch, recorded, configured.members())?;
        db.write(batch, &esker_engine::WriteOptions::synced())?;
        return Ok(configured.clone().with_group_id(recorded));
    }

    let held = MemberList::new(log.members().to_vec())?.with_group_id(recorded);
    if held.members() != configured.members() {
        tracing::warn!(
            recorded = ?held.members().iter().map(ToString::to_string).collect::<Vec<_>>(),
            configured = ?configured.members().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "the recorded membership and --peers disagree; believing the record"
        );
    }
    Ok(held)
}

/// The clock a member resumes its oracle against on taking office.
///
/// The term's own `TakeOffice` stamp when there is one, so the resume point is the instant the log
/// records rather than a second, later reading of the same clock; the clock itself before a member
/// has ever taken office. Either way `Oracle::load` takes `max(clock, mark)`, so neither can pull
/// time backwards — this only decides which reading is used.
fn office_clock(office: &Leadership, clock: &dyn Clock) -> u64 {
    if office.office_now_ms > 0 {
        office.office_now_ms
    } else {
        clock.now_ms()
    }
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
mod schema_lease_tests {
    use super::{Pd, PdOptions, SCHEMA_LEASE_MS};

    fn pd(options: PdOptions) -> std::sync::Arc<Pd> {
        let dir = tempfile::tempdir().unwrap();
        let pd = Pd::open(dir.path(), options).unwrap();
        std::mem::forget(dir);
        pd
    }

    /// The interval is **the three inputs**, computed — not a constant. Changing an input changes
    /// it, and that is the assertion: a step interval somebody could tune independently of the
    /// bounds it exists to keep would be one somebody could tune below them.
    #[test]
    fn the_step_interval_is_computed_from_its_inputs() {
        let lease = pd(PdOptions {
            lock_ttl_ms: 3_000,
            retention_ms: 60 * 60 * 1_000,
            ..PdOptions::new()
        })
        .schema_lease();
        assert_eq!(lease.lease_ms, SCHEMA_LEASE_MS);
        assert_eq!(lease.step_interval_ms, SCHEMA_LEASE_MS + 3_000);
        assert_eq!(lease.removal_extra_ms, 60 * 60 * 1_000);

        // A cluster with a longer lock TTL waits longer between steps, by exactly that much.
        let longer = pd(PdOptions {
            lock_ttl_ms: 9_000,
            retention_ms: 60 * 60 * 1_000,
            ..PdOptions::new()
        })
        .schema_lease();
        assert_eq!(longer.step_interval_ms, lease.step_interval_ms + 6_000);
    }

    /// **The removal term is separate, and that is what keeps a schema change from taking an
    /// hour.** For something being added it is inert (ADR 0020 as amended): a reader that sees an
    /// index as public does so from a snapshot above the backfill, so the index is complete at any
    /// age. Folding retention into every step would price every `CREATE INDEX` at the retention
    /// window.
    #[test]
    fn the_removal_term_is_not_in_the_ordinary_interval() {
        let lease = pd(PdOptions {
            lock_ttl_ms: 3_000,
            retention_ms: 60 * 60 * 1_000,
            ..PdOptions::new()
        })
        .schema_lease();
        assert!(
            lease.step_interval_ms < 60_000,
            "an add must not wait out the retention window: {}ms",
            lease.step_interval_ms
        );
        assert_eq!(lease.removal_extra_ms, 60 * 60 * 1_000);
    }

    /// The lease has to exceed nothing in particular, but it must be **positive**: a lease of zero
    /// is a node that may never write, and one that is not above the lock TTL would let a
    /// transaction outlive the deadline that is supposed to bound it.
    #[test]
    fn the_lease_is_positive_and_above_the_lock_ttl() {
        let options = PdOptions::new();
        assert!(SCHEMA_LEASE_MS > 0);
        assert!(
            SCHEMA_LEASE_MS > options.lock_ttl_ms,
            "a lease at or below the lock TTL bounds nothing a transaction does not already bound"
        );
    }
}
