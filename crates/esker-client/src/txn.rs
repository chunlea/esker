//! `TxnClient`: Percolator's client half (`docs/DESIGN.md` §8 and §10, `docs/txn-spec.md`).
//!
//! A transaction is `begin()` → buffered `put`/`delete` and read-your-writes `get`/`scan` →
//! `commit()`. Nothing leaves the process until a read needs an answer or the commit starts,
//! so a transaction's whole write set is known before its first `Prewrite` and can be laid out
//! the way two-phase commit needs it: a primary, and everything else grouped by region.
//!
//! Like the rest of this crate it has no I/O and no clock of its own. Bytes leave through
//! [`StoreTransport`], time enters through [`crate::clock::Clock`], and timestamps come from a
//! [`TimestampOracle`] — all three injected, so every rule below is tested against a script
//! and a clock that jumps rather than waits.
//!
//! # The commit, in the order it must happen
//!
//! 1. `start_ts` from the oracle, at `begin()`.
//! 2. **Prewrite the primary**, alone. Its lock is the one every resolver will read.
//! 3. Prewrite the secondaries, grouped by region, the groups **in parallel**: a transaction
//!    across five regions costs one round trip, not five.
//! 4. `commit_ts` from the oracle.
//! 5. **Commit the primary.** This is the instant the transaction commits — before it,
//!    nothing of it is visible and a resolver will roll it back; after it, everything of it is
//!    visible and a resolver will roll it forward.
//! 6. Commit the secondaries. This is cleanup: a reader that gets there first does it instead,
//!    and the answer is the same either way.
//!
//! Steps 2 and 5 are strictly ordered against 3 and 6 by `esker-txn`'s API rather than by this
//! module remembering to: `commit_secondary` demands a `PrimaryCommitted` token which only the
//! primary's applied plan can mint. That crate is not a dependency here — this client speaks to
//! a store over the wire and never links the transaction library — so the ordering it enforces
//! on the store's side is mirrored here by sending the two phases in the order that makes the
//! token obtainable.
//!
//! # Why an ambiguous `Prewrite` is survivable
//!
//! [`crate::Error::AmbiguousResult`] is the case the retry rules are built around: a request
//! went out, no usable answer came back, and it may or may not be in the log
//! (`docs/DESIGN.md` §10). For an ordinary `RawKv` write there is nothing to do but tell the
//! caller.
//!
//! For a `Prewrite` there is. **This is the point of Percolator.** The transaction's fate is
//! one fact in one place — its primary's `write` record — and nothing else can decide it. So:
//!
//! * an ambiguous **secondary** prewrite is not a problem at all. Either it landed (and the
//!   lock is ours, so a retry is an idempotent no-op, `docs/txn-spec.md` §5.2) or it did not
//!   (and there is nothing to clean up). The client simply retries it.
//! * an ambiguous **primary** prewrite leaves a lock that may or may not exist. Retrying is
//!   still safe, for the same reason; and if the client dies here, a *reader* that meets the
//!   lock resolves it by reading the primary — which is not committed, so after the TTL it is
//!   rolled back. Nothing is stranded and nothing is visible.
//! * an ambiguous **primary commit** is the one that matters, and the answer is the same: the
//!   record either exists or it does not, and whichever it is, that is what the transaction
//!   did. The client reports it, and any later reader resolves the secondaries correctly
//!   whichever way it went.
//!
//! A store that acknowledged a prewrite and then lost it would break this; nothing here can.
//! What makes it work is that every one of these operations is idempotent and that the truth
//! lives in one key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, Result, Waiting};
use crate::region_cache::RegionResolver;
use crate::retry::backoff_ms;
use crate::router::{ClientOptions, Router, clamp_end, fan_out, repair_route};
use crate::transport::StoreTransport;
use crate::wire::{
    Body, DEFAULT_SCAN_LIMIT, LockInfo, Method, ProtoError, Response, TxnKvReq, TxnKvResp,
    TxnMutation, TxnStatus,
};

/// Default time-to-live of a lock, in milliseconds (`docs/DESIGN.md` §14).
pub const LOCK_TTL_MS: u64 = 3_000;

/// Bits of the logical counter in a timestamp: `ts = physical_ms << 18 | logical`.
///
/// A copy of `esker_pd::TSO_LOGICAL_BITS`, for the same reason `esker_txn::TSO_LOGICAL_BITS`
/// is one: a client sits above the placement driver in `CLAUDE.md`'s layer table and does not
/// link it, but it has to read a lock's age out of a timestamp to know whether the lease has
/// run out. A copy that drifts would put every lock TTL out by a factor of 2^18, so
/// `tests/txn_ttl.rs` checks this one against its source.
pub const TSO_LOGICAL_BITS: u32 = 18;

/// The physical millisecond a timestamp was minted in.
#[must_use]
pub fn physical_ms(ts: u64) -> u64 {
    ts >> TSO_LOGICAL_BITS
}

/// The first timestamp of the millisecond `physical_ms`, with the logical counter at zero.
///
/// The inverse of [`physical_ms`], and the whole of "wall clock in, timestamp out"
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 1): a timestamp's high 46 bits
/// *are* milliseconds since the Unix epoch, so this is a shift and never a lookup.
///
/// The rounding is deliberate and worth stating, because both readings are defensible until you
/// pick one. A read at this timestamp sees every transaction that committed strictly **before**
/// that millisecond and none that committed within it — so "as of 14:00:00.000" does not
/// include a transaction committing *at* 14:00:00.000, exactly as it does not include one that
/// is one instant from committing.
#[must_use]
pub fn ts_at_ms(physical_ms: u64) -> u64 {
    physical_ms << TSO_LOGICAL_BITS
}

/// Whether a lock minted at `start_ts` with a `ttl_ms` lease is dead as of `now_ts`.
///
/// Both arguments are **timestamps from the oracle**, never wall-clock readings
/// (`CLAUDE.md` invariant 6): no node decides another node's transaction is dead by looking at
/// its own clock. The comparison is on the physical halves alone, so every logical counter
/// inside the last millisecond of the lease is still live.
///
/// The judgement is deliberately **conservative**. `now_ts` comes from this client, which may
/// be behind, so this can be late in declaring a lock dead and never early. Late costs
/// latency; early aborts a transaction that is alive (`docs/plans/phase-5.md` §10.2).
#[must_use]
pub fn is_expired(start_ts: u64, ttl_ms: u64, now_ts: u64) -> bool {
    physical_ms(now_ts) > physical_ms(start_ts).saturating_add(ttl_ms)
}

/// Most regions one scan walks before it answers with what it has.
///
/// A bound rather than a limit on correctness: a scan that has crossed this many regions has
/// read a great deal, and a caller wanting more asks again from where it stopped. Without one, a
/// region cache that kept naming regions would make a scan of a small range unbounded work.
pub const MAX_SCAN_REGIONS: usize = 64;

/// How many times a reader will resolve a lock and try again before giving up.
///
/// Separate from the router's retry budget, which counts *routing* failures. A lock is not a
/// routing failure: each attempt makes progress — the lock is resolved, or its owner is found
/// alive and the reader waits — so the two budgets count different things and sharing one
/// would make a busy key look like a broken cluster.
pub const MAX_LOCK_RESOLUTIONS: u32 = 8;

/// How many times one page of a scan will re-ask the authority where its region ended.
///
/// A page is refused when the split that moved its boundary landed after the route was cached, so
/// each refresh follows a real split and makes real progress — which is why this is a small count
/// and not a deadline. Four, because the measured case is a table splitting under a bulk load and
/// one was demonstrably too few (`Transaction::scan_region`).
pub const SCAN_ROUTE_REFRESHES: usize = 4;

/// Where timestamps come from (`CLAUDE.md` invariant 6).
///
/// The one seam between this client and the placement driver's oracle. It is a trait for the
/// same reason [`StoreTransport`] is: PD's `Tso` is async and lives behind a socket, and a
/// core that called it directly could not be tested without one.
pub trait TimestampOracle: std::fmt::Debug + Send + Sync {
    /// Allocates `count` consecutive timestamps and answers with the first.
    ///
    /// `count` is a hint for batching, not a promise about what the caller will use. Asking
    /// for more than one is how a client amortises the round trip; asking for one is always
    /// correct.
    fn tso(&self, count: u32) -> std::result::Result<u64, ProtoError>;

    /// One timestamp.
    fn timestamp(&self) -> std::result::Result<u64, ProtoError> {
        self.tso(1)
    }
}

impl<T: TimestampOracle + ?Sized> TimestampOracle for Arc<T> {
    fn tso(&self, count: u32) -> std::result::Result<u64, ProtoError> {
        (**self).tso(count)
    }
}

/// The transaction API application code calls.
#[derive(Debug)]
pub struct TxnClient {
    router: Arc<Router>,
    oracle: Arc<dyn TimestampOracle>,
    /// The transactions this client is telling the store are still alive
    /// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
    renewals: Arc<crate::renew::Renewals>,
    /// The snapshots this client still has open — the safepoint's reader floor (ADR 0110).
    active: Arc<crate::active::Active>,
    lock_ttl_ms: u64,
    max_lock_resolutions: u32,
    max_scan_regions: usize,
}

impl TxnClient {
    /// A client with the default options and the real clock.
    #[must_use]
    pub fn new(
        transport: Arc<dyn StoreTransport>,
        resolver: Arc<dyn RegionResolver>,
        oracle: Arc<dyn TimestampOracle>,
    ) -> Self {
        Self::with_options(transport, resolver, oracle, ClientOptions::default())
    }

    /// A client configured explicitly.
    #[must_use]
    pub fn with_options(
        transport: Arc<dyn StoreTransport>,
        resolver: Arc<dyn RegionResolver>,
        oracle: Arc<dyn TimestampOracle>,
        options: ClientOptions,
    ) -> Self {
        let router = Arc::new(Router::with_options(transport, resolver, options));
        Self {
            renewals: crate::renew::Renewals::new(Arc::clone(&router), Arc::clone(&oracle)),
            active: Arc::new(crate::active::Active::default()),
            router,
            oracle,
            lock_ttl_ms: LOCK_TTL_MS,
            max_lock_resolutions: MAX_LOCK_RESOLUTIONS,
            max_scan_regions: MAX_SCAN_REGIONS,
        }
    }

    /// Shares one router — and so one region cache — with an existing client.
    ///
    /// A process usually wants that: the cache is the expensive thing to warm, and a stale
    /// entry costs a redirect whoever holds it. It is also how a test injects a clock:
    /// `Router::with_options(..).with_clock(fake)` and then this.
    #[must_use]
    pub fn on_router(router: Arc<Router>, oracle: Arc<dyn TimestampOracle>) -> Self {
        Self {
            renewals: crate::renew::Renewals::new(Arc::clone(&router), Arc::clone(&oracle)),
            active: Arc::new(crate::active::Active::default()),
            router,
            oracle,
            lock_ttl_ms: LOCK_TTL_MS,
            max_lock_resolutions: MAX_LOCK_RESOLUTIONS,
            max_scan_regions: MAX_SCAN_REGIONS,
        }
    }

    /// Sets the lock TTL these transactions ask for.
    #[must_use]
    pub fn with_lock_ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.lock_ttl_ms = ttl_ms;
        self
    }

    /// Sets how many times a read will resolve a lock and try again.
    #[must_use]
    pub fn with_max_lock_resolutions(mut self, attempts: u32) -> Self {
        self.max_lock_resolutions = attempts;
        self
    }

    /// Sets how many regions one scan will walk.
    #[must_use]
    pub fn with_max_scan_regions(mut self, regions: usize) -> Self {
        self.max_scan_regions = regions;
        self
    }

    /// The routing machinery underneath, for inspection and for sharing.
    #[must_use]
    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }

    /// Starts a transaction at a fresh snapshot.
    ///
    /// The `start_ts` is the whole of a transaction's identity: it is what its locks are
    /// stamped with, what its `default` values are filed under, and what a resolver looks its
    /// fate up by. It comes from the oracle and from nowhere else (`CLAUDE.md` invariant 6).
    pub fn begin(&self) -> Result<Transaction> {
        crate::stmt_stats::record_tso();
        Ok(self.open(self.oracle.timestamp()?, false))
    }

    /// A **read-only** transaction at a timestamp of the caller's choosing: the time machine
    /// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 1).
    ///
    /// `begin()` is the special case of this with the timestamp taken from the oracle, and that
    /// is the whole of the feature on this side: a transaction's `start_ts` is its snapshot, so
    /// reading the past is only a matter of choosing a different number. Locks, resolution,
    /// read-your-writes and the region walk are all indifferent to where it came from.
    ///
    /// Three rules stand between the caller and a wrong answer, and each is a **refusal**
    /// rather than a clamp — a clamp would answer a question nobody asked:
    ///
    /// * **Read-only.** A write is refused, at commit, with [`Error::ReadOnlyTransaction`].
    /// * **Not below the safepoint.** [`Error::SnapshotTooOld`], carrying the floor, because
    ///   below it the answer would be a state that never existed.
    /// * **Not in the future.** [`Error::SnapshotInTheFuture`].
    ///
    /// The floor costs one round trip, and it is spent here rather than per read: see
    /// [`TxnClient::safepoint`] for what that number is and what it is not.
    pub fn begin_at(&self, start_ts: u64) -> Result<Transaction> {
        crate::stmt_stats::record_tso();
        let now = self.oracle.timestamp()?;
        if start_ts > now {
            return Err(Error::SnapshotInTheFuture {
                requested: start_ts,
                now,
            });
        }
        let floor = self.safepoint()?;
        // At the floor exactly is still answerable: the collector keeps the newest version at
        // or below the safepoint, because that is what a read *at* the safepoint returns
        // (`docs/txn-spec.md` §7). Below it, versions are missing.
        if start_ts < floor {
            return Err(Error::SnapshotTooOld {
                requested: start_ts,
                floor,
            });
        }
        Ok(self.open(start_ts, true))
    }

    /// A read-only transaction as of `how_long` ago.
    ///
    /// The sugar the common case wants, and the one that keeps `CLAUDE.md` invariant 6: "ago"
    /// is measured from a **timestamp the oracle just handed out**, not from this machine's
    /// wall clock. A client that subtracted from its own clock would be ordering itself against
    /// the cluster by a number the cluster never agreed to.
    pub fn begin_ago(&self, how_long: Duration) -> Result<Transaction> {
        self.begin_at(self.ts_ago(how_long)?)
    }

    /// The timestamp `how_long` before now, taken from the oracle and shifted back.
    ///
    /// Saturating: a duration longer than the oracle's clock has been running answers the
    /// bottom of the timestamp space, which [`TxnClient::begin_at`] then refuses as too old —
    /// a refusal naming the window, rather than an overflow.
    pub fn ts_ago(&self, how_long: Duration) -> Result<u64> {
        crate::stmt_stats::record_tso();
        let now = self.oracle.timestamp()?;
        let ago_ms = u64::try_from(how_long.as_millis()).unwrap_or(u64::MAX);
        Ok(ts_at_ms(physical_ms(now).saturating_sub(ago_ms)))
    }

    /// The oldest snapshot this client still has open, or `None` when it holds none.
    ///
    /// **The reader floor of the cluster's safepoint** ([ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md)):
    /// whoever holds the connection to the placement driver reports this number, and PD publishes
    /// `min(now − retention, the minimum across reporters)`. A bare client has no such connection
    /// and a SQL node does, which is why this answers a number rather than sending one.
    ///
    /// **Only the minimum leaves this process.** The rest of the set is nobody else's business,
    /// and a safepoint needs one number.
    ///
    /// `None` is a fact rather than an absence — "nothing open here" — and is what lets the window
    /// half apply. A process that has stopped reporting altogether is a different thing, and PD's
    /// TTL is what decides it.
    #[must_use]
    pub fn oldest_active_read(&self) -> Option<u64> {
        self.active.oldest()
    }

    /// The garbage-collection safepoint now in force: the oldest timestamp a read can be
    /// answered at.
    ///
    /// Asked with a `GcSafepoint` of **zero**, which is a query and not a write: a store's
    /// safepoint only ever rises (`crates/esker-store/src/gc.rs`, `fetch_max`), so publishing
    /// zero cannot lower one, and the response is defined as the safepoint now in force. That
    /// is why this needs no verb of its own.
    ///
    /// **What the number is not.** It is one store's, reached by routing an empty key, where
    /// the placement driver publishes to all of them; a store that has not yet received the
    /// latest safepoint reports a lower one. So a refusal built on it is authoritative — that
    /// history is gone everywhere, since the floor only rises — while an acceptance is a
    /// best-effort: another store may have collected further. The exact per-table floor is
    /// `esker-sql`'s to apply (ADR 0021 decision 2), because it needs the retention records and
    /// the table a key belongs to, and this crate is byte-opaque by `CLAUDE.md` invariant 7.
    pub fn safepoint(&self) -> Result<u64> {
        match self.call(&TxnKvReq::GcSafepoint { safepoint: 0 })? {
            TxnKvResp::GcSafepoint { safepoint } => Ok(safepoint),
            other => Err(unexpected(Method::TxnGcSafepoint, &other)),
        }
    }

    /// Names the present, so a later transaction can read it back — `pg_export_snapshot()`
    /// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 3).
    ///
    /// **It is free, and that is the design.** A checkpoint is a *number*: this takes a
    /// timestamp and writes one nine-byte record. No snapshot, no copy, no flush — the data it
    /// refers to is kept by retention whether anybody named it or not. The catch is exactly
    /// that: a checkpoint older than the retention window names history that is gone, so the
    /// record is a **claim to check** rather than a guarantee, and [`TxnClient::begin_at`] is
    /// where it gets checked. Pinning a checkpoint's timestamp by holding the safepoint back is
    /// the placement driver's half and is not built.
    ///
    /// **Not spelled `CHECKPOINT`.** PostgreSQL owns that keyword for forcing a WAL checkpoint
    /// and this node already answers `0A000` for it; taking the word would be inventing
    /// semantics for one that has some. The spelling is the one PostgreSQL already pairs with
    /// `SET TRANSACTION SNAPSHOT`.
    ///
    /// `name` is a **key**, in whatever space the caller keeps its names in: this crate is
    /// byte-opaque (`CLAUDE.md` invariant 7), so where the name → timestamp map lives is the
    /// decision of the layer above, which for SQL is the catalog (ADR 0021 decision 4).
    pub fn export_snapshot(&self, name: &[u8]) -> Result<u64> {
        crate::stmt_stats::record_tso();
        let at = self.oracle.timestamp()?;
        let mut txn = self.begin()?;
        txn.put(name, &encode_snapshot(at));
        txn.commit()?;
        Ok(at)
    }

    /// The timestamp a name was exported at, or [`Error::NoSuchSnapshot`].
    ///
    /// Read at the present, because a name is looked up now however far back it points.
    pub fn snapshot_at(&self, name: &[u8]) -> Result<u64> {
        let found = self.begin()?.get(name)?;
        let Some(record) = found else {
            return Err(Error::NoSuchSnapshot {
                name: Bytes::copy_from_slice(name),
            });
        };
        decode_snapshot(name, &record)
    }

    /// A read-only transaction as of an exported snapshot: [`TxnClient::begin_at`] with the
    /// timestamp looked up instead of computed, which is all "reading at a checkpoint" is.
    pub fn begin_at_snapshot(&self, name: &[u8]) -> Result<Transaction> {
        self.begin_at(self.snapshot_at(name)?)
    }

    /// One `TxnKv` call that belongs to the client rather than to a transaction.
    fn call(&self, request: &TxnKvReq) -> Result<TxnKvResp> {
        match self.router.call(&Body::Txn(request.clone()))? {
            Response::TxnKv(response) => Ok(response),
            other => Err(Error::UnexpectedResponse {
                expected: request.method(),
                actual: other.method(),
            }),
        }
    }

    fn open(&self, start_ts: u64, read_only: bool) -> Transaction {
        Transaction {
            router: Arc::clone(&self.router),
            oracle: Arc::clone(&self.oracle),
            renewals: Arc::clone(&self.renewals),
            // **Held for the life of the transaction**, so commit, rollback, abort and panic all
            // release it through one `Drop` (ADR 0110, `crate::active`).
            _held: self.active.hold(start_ts),
            start_ts,
            lock_ttl_ms: self.lock_ttl_ms,
            max_lock_resolutions: self.max_lock_resolutions,
            max_scan_regions: self.max_scan_regions,
            buffer: BTreeMap::new(),
            read_ts: BTreeMap::new(),
            statement_ts: None,
            statement_undo: BTreeMap::new(),
            checks: BTreeSet::new(),
            check_ranges: BTreeMap::new(),
            locked: BTreeSet::new(),
            pinned: None,
            read_only,
            refused_write: None,
            state: State::Open,
        }
    }
}

/// What a transaction has buffered for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Write {
    Put(Bytes),
    Delete,
}

/// Where a transaction is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Accepting reads and writes.
    Open,
    /// `commit()` or `rollback()` has run. Kept so a second call is a typed error rather than
    /// a second two-phase commit.
    Finished,
}

/// One transaction: a snapshot, a buffered write set, and the two-phase commit that ends it.
///
/// Not `Clone` and not `Sync`, deliberately. Two handles to one transaction would be two write
/// buffers that each believe they are the whole set, and the one that committed second would
/// prewrite keys the primary's lock never mentioned.
#[derive(Debug)]
pub struct Transaction {
    router: Arc<Router>,
    oracle: Arc<dyn TimestampOracle>,
    /// This transaction's registration in the client's open-snapshot set. Never read — being
    /// alive *is* what it does, and dropping it is what releases the safepoint's reader floor.
    _held: crate::active::Held,
    /// Shared with the client: what keeps this transaction's lock alive while it is doing nothing
    /// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
    renewals: Arc<crate::renew::Renewals>,
    start_ts: u64,
    lock_ttl_ms: u64,
    max_lock_resolutions: u32,
    /// Most regions one scan will walk before it stops and answers with what it has.
    max_scan_regions: usize,
    /// The write set, in key order. A `BTreeMap` rather than a list because a transaction that
    /// writes a key twice must send one mutation, not two — and because the *first* key in
    /// order is a stable choice of primary, which makes a retried commit pick the same one.
    buffer: BTreeMap<Bytes, Write>,
    /// `key -> the read timestamp of the statement that produced this write`, for the keys whose
    /// statement did not read at the transaction's own snapshot
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)
    /// §4).
    ///
    /// Empty for every transaction whose statements never waited, which is why it is a second map
    /// rather than a field on `Write`: the common case pays nothing, and the absence of an entry
    /// *is* "the transaction's own snapshot" rather than a value repeated on every key.
    read_ts: BTreeMap<Bytes, u64>,
    /// The read timestamp of the statement running now, or `None` while it is the transaction's
    /// own. Set by [`Transaction::begin_statement`].
    statement_ts: Option<u64>,
    /// **Keys this transaction only read**, for SERIALIZABLE's commit-time validation
    /// ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md),
    /// [ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)).
    ///
    /// Empty for every transaction at another level, which is what makes this cost nothing to the
    /// two that do not ask for it. A key that is *also* written is not here: its own write lock
    /// already covers the interval, and checking it twice would ask the same question at a
    /// different timestamp.
    checks: BTreeSet<Bytes>,
    /// **Ranges this transaction scanned**, `start -> end`. The phantom half: a row that did not
    /// exist when the scan ran is in no read set, and only the range can name it.
    check_ranges: BTreeMap<Bytes, Bytes>,
    /// Keys this transaction has **already prewritten a lock on**, before its commit
    /// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
    ///
    /// A `SELECT … FOR UPDATE` row, and the difference from `checks` is *when*: a check is staged
    /// and sent at commit, and one of these is on the store from the moment the statement asked for
    /// it, which is the whole point — a lock nobody can see until commit excludes nobody.
    locked: BTreeSet<Bytes>,
    /// The primary, once an eager lock has fixed it.
    ///
    /// **Every lock record of one transaction names the same primary**, and a resolver reads that
    /// primary to decide the fate of all of them. A transaction that locks a row and later writes a
    /// smaller key would move `primary()` under locks already on the store, so the first eager lock
    /// pins it and `primary()` answers this from then on.
    pinned: Option<Bytes>,
    /// What the buffer held for each key **before the statement running now touched it**, so a
    /// statement that has to be re-run can give its writes back.
    ///
    /// `None` against a key means the buffer had nothing there. Empty for every transaction whose
    /// statements never waited, which is every transaction that never blocks.
    statement_undo: BTreeMap<Bytes, Option<Write>>,
    /// Whether this transaction reads a past snapshot and so may not write
    /// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 1).
    read_only: bool,
    /// The first key a write was attempted on, when this transaction is read-only.
    ///
    /// The write is **not** buffered — a historical read that answered a caller with its own
    /// phantom write would be lying about the past, which is the one thing this transaction
    /// exists to tell the truth about — and `commit` refuses, naming this key.
    refused_write: Option<Bytes>,
    state: State,
}

/// How many times a write set may be **re-cut** against a repaired region cache.
///
/// **Not a retry budget, and the loop does not rely on it.** A round only happens when the cut
/// actually *changed*, and the loop stops the moment it stops changing — that is what terminates
/// it. This is the safety net for a cluster whose regions move on every round forever.
///
/// It is generous because progress is per-*refusal*, not per-region: a refusal teaches the cache
/// about the region it routed by, so a write set spread over *n* regions can need close to *n*
/// rounds to be cut correctly. It was 8, and a 900-row write set over a dozen regions ran out —
/// each round moved a few keys into the right group and the ninth gave up with the same
/// `region epoch does not match` the whole fix is about. A write set that spans more regions than
/// this is a statement whose grouping this client cannot learn in bounded time, which is a
/// different failure and one worth reporting.
const MAX_REGROUPINGS: usize = 64;

/// Whether `error` says **this client's routing was wrong**, as opposed to the cluster being unable
/// to answer.
///
/// The two look alike from a call site and mean opposite things: the first is fixed by asking again
/// with what the refusal taught, and the second is not fixed by asking at all. Both of these arrive
/// as a spent budget, because both are retried — see [`crate::retry::classify`].
fn stale_routing(error: &Error) -> bool {
    let refusal = match error {
        Error::RetriesExhausted { source, .. } => Some(&**source),
        Error::Store(source) => Some(source),
        _ => None,
    };
    matches!(
        refusal,
        Some(ProtoError::EpochNotMatch { .. } | ProtoError::RegionNotFound { .. })
    )
}

impl Transaction {
    /// The snapshot every read of this transaction sees, and its identity.
    #[must_use]
    pub fn start_ts(&self) -> u64 {
        self.start_ts
    }

    /// How many keys are buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the transaction has written anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// The primary key this transaction would commit on: the lowest key it writes.
    ///
    /// Chosen by order rather than at random so that a client which crashes and is replaced
    /// picks the same one, and so that a test can name it.
    #[must_use]
    pub fn primary(&self) -> Option<&Bytes> {
        self.pinned.as_ref().or_else(|| self.buffer.keys().next())
    }

    /// What this transaction has buffered for `key`, in the three states a buffer really has:
    /// `None` for nothing at all, `Some(None)` for a buffered delete, `Some(Some(value))` for a
    /// buffered value.
    ///
    /// **"Nothing" and "a tombstone" are different things**, and the caller that needs this — a
    /// savepoint rollback — leaves a stray delete behind if it cannot tell them apart.
    #[must_use]
    pub fn buffered(&self, key: &[u8]) -> Option<Option<Bytes>> {
        self.buffer.get(key).map(|write| match write {
            Write::Put(value) => Some(value.clone()),
            Write::Delete => None,
        })
    }

    /// Puts `key`'s buffer entry back to what [`Transaction::buffered`] returned earlier, and
    /// **removes it entirely** when that was `None`.
    ///
    /// The removal is the point. Undoing a write by writing its old value back leaves the key in
    /// the write set, so the commit still prewrites it and a concurrent commit on that key refuses
    /// the whole transaction — for a write it no longer intends to make. The key's per-key read
    /// stamp goes with it, so a later write of the same key reads it fresh rather than inheriting
    /// the snapshot of the write that was undone.
    pub fn restore(&mut self, key: &[u8], prior: Option<Option<Bytes>>) {
        let key = Bytes::copy_from_slice(key);
        match prior {
            Some(Some(value)) => {
                self.buffer.insert(key, Write::Put(value));
            }
            Some(None) => {
                self.buffer.insert(key, Write::Delete);
            }
            None => {
                self.buffer.remove(&key);
                self.read_ts.remove(&key);
            }
        }
    }

    /// Buffers a write. No I/O: the whole set goes out at `commit()`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        if self.refuse_write(key) {
            return;
        }
        self.remember(key);
        self.stamp(key);
        self.buffer.insert(
            Bytes::copy_from_slice(key),
            Write::Put(Bytes::copy_from_slice(value)),
        );
    }

    /// The snapshot a **read** is served at: the running statement's where there is one, and the
    /// transaction's own otherwise
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    ///
    /// **Not [`Transaction::start_ts`], which never moves.** The transaction's own timestamp is its
    /// identity — its lock records carry it and its data versions are written at it — while what a
    /// statement *reads* under READ COMMITTED is whatever was committed when that statement began.
    /// Conflating the two would either freeze every statement at `BEGIN` or move the identity of a
    /// transaction under the locks other sessions are classifying it by.
    fn read_ts(&self) -> u64 {
        self.statement_ts.unwrap_or(self.start_ts)
    }

    /// Says which snapshot the value about to be written was computed from
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)
    /// §4).
    ///
    /// **Only when it is not the transaction's own**, which is why the map stays empty for every
    /// transaction whose statements never waited: an absent entry *is* `start_ts`, said once
    /// rather than repeated on every key.
    pub fn reading_at(&mut self, read_ts: u64) {
        self.statement_ts = Some(read_ts);
    }

    /// Hands this transaction the read set its commit must validate (ADR 0062).
    ///
    /// Called once, before `commit`, by the layer that knows which reads *count* — the catalog is
    /// excluded there, not here, because a client that interpreted keys would be deciding SQL
    /// semantics (`CLAUDE.md` invariant 7). A key already in the write buffer is dropped: its write
    /// lock covers the same interval.
    pub fn checking(&mut self, keys: impl IntoIterator<Item = Bytes>, ranges: Vec<(Bytes, Bytes)>) {
        self.checks = keys
            .into_iter()
            .filter(|key| !self.buffer.contains_key(key))
            .collect();
        self.check_ranges = ranges.into_iter().collect();
    }

    /// **Takes a lock on `key` now, that every node can see**
    /// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
    ///
    /// This is `SELECT … FOR UPDATE`'s lock. It prewrites a `Check` mutation — the lock-only
    /// mutation ADR 0067 added as tag 5 — so the store holds a lock record on the key from this
    /// moment rather than from the commit, which is the difference between a lock another
    /// `esker-sql` process is excluded by and one it never sees. No new method and no new tag: the
    /// request is the `Prewrite` a commit sends, sent earlier.
    ///
    /// Idempotent for the holder, like the node-local table it replaces: a key this transaction
    /// already wrote or already locked answers [`Acquired::Taken`] without a round trip.
    ///
    /// **The caller does the waiting.** A live holder comes back as [`Acquired::Held`] and not as
    /// a resolution loop, because what to do about it is a question this layer cannot answer:
    /// `NOWAIT` refuses, `SKIP LOCKED` skips, and a plain wait is bounded by the caller's
    /// `lock_timeout`. What this decides is only the half that must be decided here — whether the
    /// holder is older than this transaction, in which case it is waited for, or younger, in which
    /// case it is wounded.
    ///
    /// # Errors
    ///
    /// Any transport or region failure the router could not retry away, and any fatal verdict the
    /// prewrite came back with — a conflicting commit is `40001` here as it is at commit time.
    pub fn lock(&mut self, key: &[u8]) -> Result<Acquired> {
        let key = Bytes::copy_from_slice(key);
        if self.buffer.contains_key(&key) || self.locked.contains(&key) {
            return Ok(Acquired::Taken);
        }
        if let Some(held) = self.pin_primary(&key)? {
            return Ok(held);
        }
        let primary = self
            .pinned
            .clone()
            .ok_or_else(|| Error::Internal("an eager lock left no primary".to_owned()))?;
        if primary == key {
            self.locked.insert(key);
            return Ok(Acquired::Taken);
        }
        match self.prewrite_once(&primary, &key)? {
            None => {
                self.locked.insert(key);
                Ok(Acquired::Taken)
            }
            Some(lock) => self.wound_or_wait(&lock, &key),
        }
    }

    /// **Gives back locks this transaction placed, and stays running**
    /// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2).
    ///
    /// `ROLLBACK TO SAVEPOINT`, and the deadlock victim inside one. A real server releases a
    /// subtransaction's row locks when it aborts; before this, the SQL layer released its
    /// node-local half and nothing reached the store, so the lock a `SELECT … FOR UPDATE` left
    /// there outlived the savepoint that took it and blocked every other session until the whole
    /// transaction ended.
    ///
    /// Only keys this transaction actually holds a *store* lock on are sent: a buffered write has
    /// no lock yet, and a key we never locked has nothing to give back.
    ///
    /// # The primary
    ///
    /// The primary is what every resolver consults, so releasing it while another of this
    /// transaction's locks still names it would leave those locks pointing at a record that is not
    /// there — a transaction that reads as `Missing` and can be rolled back by anyone.
    ///
    /// **But it cannot simply be refused**, and that is the case this method exists for: the
    /// primary is *the smallest key already buffered, or the key being locked when nothing is* —
    /// `pin_primary`, private to this module and so named rather than linked — and a savepoint
    /// whose first act is `SELECT … FOR UPDATE`
    /// over a transaction that has written nothing — which is precisely Rails' shape — pins that
    /// very row. Refusing it would leave the savepoint's own lock behind, which is the bug.
    ///
    /// So the rule is exact rather than cautious: the primary goes when **nothing else names it**,
    /// and the transaction is unpinned so that the next eager lock or the commit picks a new one by
    /// the ordinary rule. The renewal stops with it — a heartbeat for a lock that is not there
    /// keeps nothing alive.
    ///
    /// # Errors
    ///
    /// Any transport or region failure the router could not retry away. A key whose lock turned out
    /// to be somebody else's is **not** an error: the caller wanted it free of *this* transaction's
    /// lock and it is.
    pub fn release(&mut self, keys: &[Bytes]) -> Result<()> {
        // Everything but the primary first, so that releasing the primary in the same call sees a
        // transaction that no longer holds anything else.
        let pinned = self.pinned.clone();
        let mut others: Vec<Bytes> = keys
            .iter()
            .filter(|key| self.locked.contains(*key) && Some(*key) != pinned.as_ref())
            .cloned()
            .collect();
        others.sort();
        others.dedup();
        if !others.is_empty() {
            self.release_keys(&others)?;
            for key in &others {
                self.locked.remove(key);
            }
        }

        // The primary, if it was asked for and nothing of ours is left to name it.
        let Some(primary) = pinned else {
            return Ok(());
        };
        if !keys.contains(&primary) || !self.locked.contains(&primary) {
            return Ok(());
        }
        if self.locked.iter().any(|held| *held != primary) {
            // Another eager lock still points at it. One row of one transaction stays held to the
            // end of the block, which is the declared remainder — not every row of every savepoint.
            return Ok(());
        }
        self.release_keys(std::slice::from_ref(&primary))?;
        self.locked.remove(&primary);
        self.pinned = None;
        self.renewals.forget(self.start_ts);
        Ok(())
    }

    /// One `ReleaseLock` per region the keys fall in.
    fn release_keys(&self, keys: &[Bytes]) -> Result<()> {
        self.grouped(keys, |group| {
            let request = TxnKvReq::ReleaseLock {
                start_ts: self.start_ts,
                keys: group.to_vec(),
            };
            match self.call(&request)? {
                TxnKvResp::ReleaseLock { .. } => Ok(()),
                other => Err(unexpected(Method::TxnReleaseLock, &other)),
            }
        })
    }

    /// Fixes the primary and puts its lock on the store, which every later lock of this
    /// transaction points at.
    ///
    /// The primary is the smallest key already buffered, or `candidate` when nothing is — the same
    /// rule `primary()` had before eager locks existed, taken once instead of on every call.
    /// Answers `Some` when the primary itself is held by somebody else, which is the caller's
    /// answer too: there is nothing to hang the rest of the transaction on until it clears.
    fn pin_primary(&mut self, candidate: &Bytes) -> Result<Option<Acquired>> {
        if self.pinned.is_some() {
            return Ok(None);
        }
        let primary = self
            .buffer
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| candidate.clone());
        match self.prewrite_once(&primary, &primary)? {
            None => {}
            Some(lock) => match self.wound_or_wait(&lock, &primary)? {
                Acquired::Taken => {}
                held @ Acquired::Held { .. } => return Ok(Some(held)),
            },
        }
        if primary != *candidate {
            // A buffered write, prewritten early so that it can be the primary. It is already in
            // the buffer, so `commit` will send it again and the store will answer `AlreadyLocked`.
            self.locked.insert(primary.clone());
        }
        // **The renewal starts here**, because this is the moment a lock of this transaction's is
        // on the store with a lease running against it. The primary is the only one that has to be
        // told: it is the only lock a resolver consults, and settling it is what kills the rest.
        self.renewals
            .register(self.start_ts, &primary, self.lock_ttl_ms);
        self.pinned = Some(primary);
        Ok(None)
    }

    /// One `Prewrite` of one key, with no resolution loop behind it: the lock it met, or `None`.
    fn prewrite_once(&self, primary: &Bytes, key: &Bytes) -> Result<Option<LockInfo>> {
        let keys = std::slice::from_ref(key);
        let request = TxnKvReq::Prewrite {
            start_ts: self.start_ts,
            primary: primary.clone(),
            ttl_ms: self.lock_ttl_ms,
            mutations: self.mutations_for(keys),
        };
        let statuses = match self.call(&request)? {
            TxnKvResp::Prewrite { keys: statuses } => statuses,
            other => return Err(unexpected(Method::TxnPrewrite, &other)),
        };
        let [status] = &statuses[..] else {
            return Err(Error::Store(ProtoError::invalid(format!(
                "a Prewrite of one key was answered with {} statuses",
                statuses.len()
            ))));
        };
        if status.is_fatal() {
            self.check(status.clone(), Some(key), Waiting::Acquire)?;
        }
        Ok(status.lock().cloned())
    }

    /// **Who dies when two transactions want each other's rows: the younger one.**
    ///
    /// An eager lock is what makes a cross-node cycle possible in the first place — before it, no
    /// transaction here ever held a lock and waited for another, which is the argument
    /// `docs/plans/cross-node-deadlock.md` makes and `tests/prewrite_ordering.rs` pins. So (a')
    /// has to answer "one of you has to die" itself, and it answers it without a graph:
    ///
    /// * the holder is **settled** — committed, or its lease has run out — so it is resolved and
    ///   the lock taken, which is what every other waiter in this client already does;
    /// * the holder is alive and **younger** than this transaction (a larger `start_ts`), so it is
    ///   wounded: its primary is rolled back, its lock here is resolved, and this transaction takes
    ///   the key. It finds out at its own commit, which refuses a transaction whose primary carries
    ///   a rollback marker;
    /// * the holder is alive and **older**, so this transaction waits, and the caller decides for
    ///   how long.
    ///
    /// Wound-wait, and the reason it is this and not a detector: `start_ts` comes from the TSO
    /// (`CLAUDE.md` invariant 6), so every node breaks the tie the same way with no round trip and
    /// no shared state. A cycle needs both sides to wait, and the older side never does.
    ///
    /// **The oldest transaction in a cycle always survives**, so this cannot livelock: the victim
    /// is chosen by an order that does not change while the transactions run.
    fn wound_or_wait(&self, lock: &LockInfo, key: &Bytes) -> Result<Acquired> {
        let lease_ms = match self.classify(lock)? {
            Classified::Settled(verdict) => {
                self.resolve_keys(lock, verdict, key)?;
                return self.retake(lock, key);
            }
            Classified::Alive { lease_ms } => lease_ms,
        };
        if lock.start_ts <= self.start_ts {
            // Older, or the same transaction reaching the same key by two routes. Wait.
            return Ok(Acquired::Held {
                by: lock.start_ts,
                lease_ms,
            });
        }
        // Younger. Roll its primary back — the primary first and alone, because it is the fact
        // every other participant reads — and then this key, which follows it.
        let verdict = self.settle_primary(lock)?;
        self.resolve_keys(lock, verdict, key)?;
        self.retake(lock, key)
    }

    /// Prewrites `key` once more after its holder was settled or wounded.
    ///
    /// A second holder that arrived in between is reported rather than wounded in turn: one wound
    /// per ask keeps this bounded, and the caller is going to come back anyway.
    fn retake(&self, lock: &LockInfo, key: &Bytes) -> Result<Acquired> {
        let primary = self
            .pinned
            .clone()
            .unwrap_or_else(|| self.primary().cloned().unwrap_or_else(|| key.clone()));
        match self.prewrite_once(&primary, key)? {
            None => Ok(Acquired::Taken),
            Some(next) => Ok(Acquired::Held {
                by: next.start_ts,
                lease_ms: lock.ttl_ms.min(next.ttl_ms),
            }),
        }
    }

    /// Rolls one settled transaction's lock on `key` forward or back, its primary already decided.
    fn resolve_keys(&self, lock: &LockInfo, verdict: Verdict, key: &Bytes) -> Result<()> {
        if *key == lock.primary {
            return Ok(());
        }
        let request = TxnKvReq::ResolveLock {
            start_ts: lock.start_ts,
            commit_ts: verdict.commit_ts(),
            keys: vec![key.clone()],
        };
        match self.call(&request)? {
            TxnKvResp::ResolveLock { .. } => Ok(()),
            other => Err(unexpected(Method::TxnResolveLock, &other)),
        }
    }

    /// Begins a statement: a fresh read timestamp, and the previous statement's undo **discarded**.
    ///
    /// The pair to [`Transaction::restart_statement`], and the difference between them is the whole
    /// reason they are two methods: a statement that ended normally keeps its writes, and one that
    /// has to be re-run gives them back.
    pub fn begin_statement(&mut self, read_ts: u64) {
        self.statement_undo.clear();
        self.statement_ts = Some(read_ts);
    }

    /// Gives back everything the statement running now wrote, and takes a fresh read timestamp.
    ///
    /// **A re-run is not a second statement, and the stamps say so.** A key an *earlier* statement
    /// wrote keeps its older timestamp, because the value being written derives from that
    /// statement's read — but the writes of the attempt that *waited*
    /// are discarded and recomputed from a fresh read, so their stamps must move forward with them.
    /// Without this the waiter's prewrite is validated against the snapshot it held **before** the
    /// wait, and it dies with `40001` naming the very commit it waited for: measured against three
    /// real stores as `a commit at 1007 beat this transaction at 1004`.
    ///
    /// Restoring the buffer matters for a second reason a value-only view misses: a re-run may
    /// decide **not** to write a key it wrote the first time — the row it matched has changed and
    /// no longer matches — and a stale write left behind would be committed as if it had.
    pub fn restart_statement(&mut self, read_ts: u64) {
        for (key, before) in std::mem::take(&mut self.statement_undo) {
            if let Some(write) = before {
                self.buffer.insert(key, write);
            } else {
                // Nothing was there before this statement, so the key leaves with its stamp: a
                // re-run that writes it again reads it fresh and stamps it fresh.
                self.buffer.remove(&key);
                self.read_ts.remove(&key);
            }
        }
        self.statement_ts = Some(read_ts);
    }

    /// Records what the buffer held for `key` before this statement wrote it, once per statement.
    fn remember(&mut self, key: &[u8]) {
        if self.statement_ts.is_none() {
            return;
        }
        let key = Bytes::copy_from_slice(key);
        if !self.statement_undo.contains_key(&key) {
            let before = self.buffer.get(&key).cloned();
            self.statement_undo.insert(key, before);
        }
    }

    /// Records the current statement's read timestamp against a key, if there is one to record.
    ///
    /// **The earliest stamp for a key wins.** A key this transaction has already written is read
    /// back from the *buffer* — read-your-writes — so a later statement writing it again computes
    /// from the earlier statement's value and never saw a snapshot of its own. Taking the later
    /// timestamp would claim it did, and the prewrite would then look for conflicts after a moment
    /// too late to find them: a commit over somebody else's version, with no error. Measured in the
    /// SQL layer's own buffer as six lost increments in two hundred and forty (run 66).
    fn stamp(&mut self, key: &[u8]) {
        if let Some(read_ts) = self.statement_ts {
            self.read_ts
                .entry(Bytes::copy_from_slice(key))
                .or_insert(read_ts);
        }
    }

    /// Buffers a delete. No I/O.
    pub fn delete(&mut self, key: &[u8]) {
        if self.refuse_write(key) {
            return;
        }
        self.remember(key);
        self.stamp(key);
        self.buffer
            .insert(Bytes::copy_from_slice(key), Write::Delete);
    }

    /// Whether this transaction may not write, remembering the first key that tried.
    ///
    /// The write is dropped rather than buffered, and the refusal comes at `commit`. Two
    /// alternatives were available and are worse: making `put` return a `Result` puts an error
    /// path on every ordinary transaction's hot loop to serve what is a programming error, and
    /// buffering the write would make [`Transaction::get`] answer with a value that never
    /// existed at this snapshot — a historical read lying about history, which is the one thing
    /// it is for. Nothing is silent: the transaction cannot commit, and the error names the key.
    fn refuse_write(&mut self, key: &[u8]) -> bool {
        if !self.read_only {
            return false;
        }
        if self.refused_write.is_none() {
            self.refused_write = Some(Bytes::copy_from_slice(key));
        }
        true
    }

    /// Whether this transaction reads a past snapshot, and so cannot write.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// **The newest `commit_ts` for `key`, or `None`** — the question, not an acquisition
    /// ([ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)).
    ///
    /// Read-only: no lock, no log entry. What asks it is a statement that took a row lock without
    /// waiting and cannot tell from the lock alone whether the writer in front committed and
    /// released in between.
    ///
    /// # Errors
    ///
    /// Any transport or region failure the router could not retry away.
    pub fn latest_commit(&self, key: &[u8]) -> Result<Option<u64>> {
        let request = TxnKvReq::LatestCommit {
            key: Bytes::copy_from_slice(key),
        };
        match self.call_resolving(&request, Waiting::Read)? {
            TxnKvResp::LatestCommit { newest } => Ok(newest),
            other => Err(unexpected(Method::TxnLatestCommit, &other)),
        }
    }

    /// The snapshot this transaction's reads are served at — the running statement's where there is
    /// one, and the transaction's own otherwise. The public face of the private `read_ts`.
    #[must_use]
    pub fn reading_ts(&self) -> u64 {
        self.read_ts()
    }

    /// Reads one key at this transaction's snapshot, its own buffered writes first.
    ///
    /// **Read-your-writes** is enforced here rather than at the store, because the store has
    /// not seen the buffer: nothing of this transaction exists anywhere else until `commit()`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some(write) = self.buffer.get(key) {
            return Ok(match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            });
        }
        let request = TxnKvReq::Get {
            key: Bytes::copy_from_slice(key),
            ts: self.read_ts(),
        };
        match self.call_resolving(&request, Waiting::Read)? {
            TxnKvResp::Get { value } => Ok(value),
            other => Err(unexpected(Method::TxnGet, &other)),
        }
    }

    /// **The same read, except that it never waits for a lock**
    /// ([ADR 0105](../../../docs/adr/0105-a-catalog-read-never-waits.md)).
    ///
    /// A lock in the way is not resolved and not waited out: this re-reads at
    /// `lock.start_ts - 1`, which is the newest committed state **strictly before** the
    /// transaction holding it. The answer is therefore the value as of a moment just before
    /// somebody else's uncommitted work — which for the catalog is the whole point, because an
    /// uncommitted DDL is not supposed to be visible to anybody.
    ///
    /// # Why this is not a weaker `get`
    ///
    /// It answers a *different question*. `get` asks "what does my snapshot say", waits for
    /// whoever is in the way, and is right for a row. This asks "what was committed before the
    /// transaction in my way", which is only the right question where an in-flight writer must be
    /// invisible rather than waited for. `esker-sql`'s catalog is the one caller, and the key
    /// semantics that make it the right caller live there (`CLAUDE.md` invariant 7) — this crate
    /// only offers the read.
    ///
    /// Terminates by construction: every retry lowers the timestamp strictly, and a timestamp of
    /// zero has nothing below it.
    ///
    /// # Errors
    ///
    /// Any transport or region failure the router could not retry away. **Not** a lock: a lock is
    /// the one thing this cannot fail on.
    pub fn get_without_waiting(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some(write) = self.buffer.get(key) {
            return Ok(match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            });
        }
        let key = Bytes::copy_from_slice(key);
        let mut at = self.read_ts();
        loop {
            let error = match self.call(&TxnKvReq::Get {
                key: key.clone(),
                ts: at,
            }) {
                Ok(TxnKvResp::Get { value }) => return Ok(value),
                Ok(other) => return Err(unexpected(Method::TxnGet, &other)),
                Err(error) => error,
            };
            let Some(lock) = lock_in(&error) else {
                return Err(error);
            };
            // **Just below the holder**, not one lease or one backoff below: the state this read
            // wants is the one that was committed when that transaction began, and every version
            // between the two belongs to transactions that started later than it.
            let Some(below) = lock?.start_ts.checked_sub(1) else {
                // A lock at timestamp zero cannot exist — no transaction starts there — but a
                // store that answered one must not become an unbounded loop here.
                return Ok(None);
            };
            at = below;
        }
    }

    /// Reads `[start, end)` at this transaction's snapshot, its own buffered writes merged in.
    ///
    /// The merge is the read-your-writes rule applied to a range: a buffered `Put` in the
    /// range appears even though no store has it, a buffered `Delete` hides a committed value,
    /// and the result is in key order either way. The `limit` is applied **after** the merge,
    /// so a scan cannot return fewer rows than it would have because the buffer displaced some.
    ///
    /// # A range is not a region
    ///
    /// One request reaches **one** region, and a store answers only for the keys it owns — so a
    /// scan whose range spans a split boundary would come back holding the first region's keys
    /// and nothing else, with no error and nothing to notice. That is the failure this walks
    /// region by region to avoid: it asks, learns from the region cache where that region ended,
    /// and asks again from there until the range is exhausted or the limit is full.
    ///
    /// The cache is a *hint* here as everywhere else (`docs/DESIGN.md` §10). If it does not know
    /// where a region ended, the walk stops rather than guessing — a short answer, which is what
    /// a caller gets from any bounded scan, rather than a wrong one.
    pub fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        let limit = self.router.bounded_limit(limit, DEFAULT_SCAN_LIMIT);
        let mut merged: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        let mut cursor = Bytes::copy_from_slice(start);

        for _ in 0..self.max_scan_regions {
            let (page, next) = self.scan_region(&cursor, end, limit)?;
            merged.extend(page);
            if merged.len() >= limit as usize {
                break;
            }
            // An empty `end_key` is the end of the key space, so a region carrying one is the
            // last there is and the range is exhausted.
            if next.is_empty() {
                break;
            }
            // Past the range the caller asked for, or not moving. The second is the guard that
            // matters: a route naming a region that ends at or before the cursor would otherwise
            // ask the same region for ever.
            if (!end.is_empty() && next.as_ref() >= end) || next <= cursor {
                break;
            }
            cursor = next;
        }

        for (key, write) in self.in_range(start, end) {
            match write {
                Write::Put(value) => merged.insert(key.clone(), value.clone()),
                Write::Delete => merged.remove(key),
            };
        }
        Ok(merged.into_iter().take(limit as usize).collect())
    }

    /// One region's worth of a scan, and where that region ends.
    ///
    /// # The range must not leave the region, and this is what that costs
    ///
    /// A store refuses a `Scan` whose range is not *contained* by the region it reaches —
    /// `RegionMeta::check_range`, and it is right to: invariant 5 says a store never serves a
    /// range it no longer owns, and a scan that came back holding only the first region's keys
    /// with no error would be the silent wrong answer that rule exists to prevent. So the client
    /// is what has to move. This asks who owns `cursor`, clamps the page to that region's end,
    /// and answers the boundary so the walk above can carry on from it.
    ///
    /// Sending the caller's whole `end` to the first region is what made **every** SQL scan across
    /// a boundary fail with `08006 key is not in region 1` the moment a table first grew past the
    /// split threshold ([ADR 0073](../../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
    /// The walk was already here and already correct; what it asked for was not.
    ///
    /// # Why it asks again, and why a *few* times
    ///
    /// The region cache is a hint (`docs/DESIGN.md` §10), and a hint taken before a split names a
    /// region wider than the one that now exists — so the clamp is computed from a boundary that
    /// has moved and the store refuses again. `KeyNotInRegion` is *terminal* for the router,
    /// which is correct, because no amount of waiting fixes routing; what fixes it is asking the
    /// authority, which is what a refreshed attempt does.
    ///
    /// **One refresh is not enough, measured.** The first version allowed exactly one, on the
    /// reasoning that two attempts say "the cache was stale" and a loop would say "the cluster is
    /// splitting faster than we can read". A cluster just past a bulk load *is* splitting that
    /// fast: `cross_region_scan` loaded a table into five regions and the sixth appeared **while
    /// the queries ran**, so a scan could lose its route twice and answer
    /// `08006 key is not in region 7`. Each refresh is authoritative and each one follows a real
    /// split, so the budget is small and fixed rather than a deadline — [`SCAN_ROUTE_REFRESHES`].
    ///
    /// **The timestamp does not move between pages.** `scan_page` reads `self.read_ts()` every
    /// time, and that is the transaction's, so the regions of one scan are read at one snapshot —
    /// a scan that re-stamped per region would be a scan that saw a row twice or not at all.
    fn scan_region(
        &self,
        cursor: &Bytes,
        end: &[u8],
        limit: u32,
    ) -> Result<(Vec<(Bytes, Bytes)>, Bytes)> {
        // The first lookup is repaired too: `Router::route` raises its own `KeyNotInRegion` with
        // `region_id == 0` when the driver does not yet know the key, and under load that is the
        // driver being behind rather than an answer about the cluster.
        let mut boundary = match self.router.route(cursor, None) {
            Ok(route) => route.region.end_key,
            Err(refusal) => repair_route(&self.router, cursor, &refusal)?.region.end_key,
        };
        let mut refreshes = 0;
        loop {
            match self.scan_page(cursor, &clamp_end(end, &boundary), limit) {
                Ok(pairs) => return Ok((pairs, boundary)),
                Err(Error::Store(refusal))
                    if matches!(refusal, ProtoError::KeyNotInRegion { .. })
                        && refreshes < SCAN_ROUTE_REFRESHES =>
                {
                    refreshes += 1;
                    // **The shared repair**, not a copy of it: the fragment dispatch meets the same
                    // stale routing and must resolve it the same way (`router::repair_route`).
                    boundary = repair_route(&self.router, cursor, &refusal)?.region.end_key;
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// One region's worth of a scan.
    fn scan_page(&self, start: &Bytes, end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        let request = TxnKvReq::Scan {
            start: start.clone(),
            end: Bytes::copy_from_slice(end),
            limit,
            ts: self.read_ts(),
            reverse: false,
        };
        match self.call_resolving(&request, Waiting::Read)? {
            TxnKvResp::Scan { pairs } => Ok(pairs),
            other => Err(unexpected(Method::TxnScan, &other)),
        }
    }

    /// The buffered writes inside `[start, end)`; an empty `end` means the end of the key
    /// space (`docs/DESIGN.md` §10).
    fn in_range<'a>(
        &'a self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> impl Iterator<Item = (&'a Bytes, &'a Write)> {
        self.buffer
            .range(Bytes::copy_from_slice(start)..)
            .take_while(move |(key, _)| end.is_empty() || key.as_ref() < end)
    }

    /// Commits, and answers with the commit timestamp — or `None` for a transaction that wrote
    /// nothing, which needs no timestamps and no round trips at all.
    ///
    /// The order is the one in this module's header, and steps 2 and 5 are enforced by
    /// `esker-txn`'s types rather than by this function being read carefully.
    pub fn commit(mut self) -> Result<Option<u64>> {
        if let Some(key) = self.refused_write.clone() {
            return Err(Error::ReadOnlyTransaction {
                start_ts: self.start_ts,
                key,
            });
        }
        self.finish()?;
        let Some(primary) = self.primary().cloned() else {
            return Ok(None);
        };

        // 2. The primary, alone and first. Its lock is what every resolver reads.
        //
        // **A prewrite that fails takes its locks back with it.** Percolator leaves them for the
        // TTL and a reader resolves them, which is correct and slow: the next reader of those keys
        // meets a live lock it may not clear and waits out the whole TTL for a transaction that is
        // already dead. That is bearable when a refusal is rare, and it stopped being rare when
        // SERIALIZABLE started refusing commits on purpose (ADR 0062) — a transaction that loses a
        // validation is a *normal* outcome now. So the loser cleans up after itself, which is what
        // a real server does when it aborts.
        self.prewrite_or_roll_back(&primary, std::slice::from_ref(&primary), &[])?;

        // 3. The secondaries, grouped by region, the groups in parallel.
        //
        // **A checked key is a secondary.** It takes a lock like any other key of this
        // transaction, so it groups, commits and rolls back by the machinery already here — which
        // is the whole reason ADR 0067 put the check on the *mutation* rather than inventing a
        // second kind of request (ADR 0062 §2, §4).
        //
        // **And a key locked eagerly is one too** (ADR 0088): its lock is already on the store, so
        // this prewrite answers `AlreadyLocked` for it — the round trip is what buys the uniform
        // path, where one list of secondaries is committed and rolled back by one piece of code.
        let secondaries: Vec<Bytes> = self
            .buffer
            .keys()
            .chain(self.checks.iter())
            .chain(self.locked.iter())
            .filter(|key| **key != primary)
            .cloned()
            .collect::<BTreeSet<Bytes>>()
            .into_iter()
            .collect();
        self.prewrite_or_roll_back(&primary, &secondaries, std::slice::from_ref(&primary))?;
        // The ranges, which are not keys and so cannot ride that list: each is verified against the
        // region its lower bound falls in, and leaves nothing behind to commit.
        if let Err(error) = self.prewrite_range_checks(&primary) {
            if !matches!(error, Error::AmbiguousResult { .. }) {
                self.undo(&primary, &secondaries);
            }
            return Err(error);
        }

        // 4. and 5. The commit point.
        crate::stmt_stats::record_tso();
        let commit_ts = self.oracle.timestamp()?;
        if let Err(error) = self.commit_keys(commit_ts, std::slice::from_ref(&primary)) {
            // **A transaction that did not commit takes its locks with it**
            // ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)). Percolator's answer is
            // that a later reader resolves them, and that answer is one whole TTL long for every
            // session that wants one of those rows — bearable when reaching here was rare, and it
            // stopped being rare when a transaction could be *wounded*: the loser of a cross-node
            // deadlock arrives here every time, having prewritten every key it holds.
            //
            // The primary is asked what happened rather than guessed at, because the two answers
            // need opposite cleanups and getting it backwards would tear a committed transaction
            // in half. `Rollback` of the primary is the same atomic question a resolver asks: it
            // answers `Committed` if the commit got there first, and otherwise leaves the marker
            // that makes this transaction dead for ever.
            if !matches!(error, Error::AmbiguousResult { .. }) && !secondaries.is_empty() {
                match self.primary_fate(&primary) {
                    Ok(Some(committed_at)) => {
                        let _ = self.commit_grouped(committed_at, &secondaries);
                    }
                    Ok(None) => {
                        let _ = self.rollback_grouped(&secondaries);
                    }
                    // The store could not say. Leaving them is what this did before, and a
                    // resolver still ends them; reporting the original error is what matters.
                    Err(_) => {}
                }
            }
            return Err(error);
        }

        // 6. Cleanup. A secondary that fails here is not a failed transaction: the primary's
        //    record is written, so the transaction *is* committed and a reader that meets one
        //    of these locks will roll it forward (`docs/txn-spec.md` §5.5). Reporting an error
        //    would tell the caller their committed transaction failed, which is worse than
        //    leaving a lock for a reader to clean up.
        if !secondaries.is_empty() {
            let _ = self.commit_grouped(commit_ts, &secondaries);
        }
        Ok(Some(commit_ts))
    }

    /// Abandons the transaction, leaving a rollback marker on every key it prewrote.
    ///
    /// A transaction that never prewrote anything still sends the markers: the client cannot
    /// tell a prewrite that never left from one whose answer was lost, and the marker is
    /// exactly what makes a late arrival of the second kind fail (`docs/txn-spec.md` §5.4).
    pub fn rollback(mut self) -> Result<()> {
        self.finish()?;
        // The eagerly locked keys go back too, and the pinned primary with them: a lock this
        // transaction put on the store at its `SELECT … FOR UPDATE` outlives the transaction by a
        // whole TTL otherwise, and every other session that wants the row waits it out (ADR 0088).
        let keys: Vec<Bytes> = self
            .buffer
            .keys()
            .chain(self.locked.iter())
            .chain(self.pinned.iter())
            .cloned()
            .collect::<BTreeSet<Bytes>>()
            .into_iter()
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        self.rollback_grouped(&keys)
    }

    fn finish(&mut self) -> Result<()> {
        if self.state == State::Finished {
            return Err(Error::Internal(
                "a transaction was committed or rolled back twice".to_owned(),
            ));
        }
        self.state = State::Finished;
        self.renewals.forget(self.start_ts);
        Ok(())
    }

    // -- the phases --------------------------------------------------------------------

    /// Prewrites one region's worth of keys, resolving whatever locks come back.
    ///
    /// A `Prewrite` answers **per key**, so a batch that collides with several locks reports
    /// all of them at once ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
    /// This resolves every one of them **in parallel** and prewrites again — one resolution
    /// round however many keys collided, rather than a round trip per contended key, which is
    /// the shape that matters exactly when the client is already losing races.
    ///
    /// A round is still bounded, because a key can be locked again by a *different*
    /// transaction between the resolution and the retry. That is progress being undone by
    /// someone else rather than a failure to make it, so it gets another round rather than an
    /// error — up to the budget.
    fn prewrite(&self, primary: &Bytes, keys: &[Bytes]) -> Result<()> {
        let request = TxnKvReq::Prewrite {
            start_ts: self.start_ts,
            primary: primary.clone(),
            ttl_ms: self.lock_ttl_ms,
            mutations: self.mutations_for(keys),
        };

        for round in 0..=self.max_lock_resolutions {
            let statuses = match self.call(&request)? {
                TxnKvResp::Prewrite { keys: statuses } => statuses,
                other => return Err(unexpected(Method::TxnPrewrite, &other)),
            };
            // One status per mutation, positionally. A store that answers a different number
            // has said nothing about some key, and reading a short list as "the rest were
            // fine" is exactly the silent wrong answer this codebase refuses.
            let expected = match &request {
                TxnKvReq::Prewrite { mutations, .. } => mutations.len(),
                _ => 0,
            };
            if statuses.len() != expected {
                return Err(Error::Store(ProtoError::invalid(format!(
                    "a Prewrite of {expected} keys was answered with {} statuses",
                    statuses.len()
                ))));
            }

            // A terminal status ends the transaction, and it ends it now: resolving locks on
            // the other keys would be work for a transaction that is already dead.
            //
            // By *position*, not by search: the status list is aligned with the mutations, so
            // the index is which key lost. A caller above needs that — a lost race on a unique
            // index entry is a duplicate key rather than a serialization failure, and only the
            // key tells the two apart (`docs/txn-spec.md` §6.1).
            if let Some(at) = statuses.iter().position(TxnStatus::is_fatal) {
                let lost = keys.get(at);
                return self.check(statuses[at].clone(), lost, Waiting::Acquire);
            }
            let locks: Vec<LockInfo> = statuses
                .iter()
                .filter_map(TxnStatus::lock)
                .cloned()
                .collect();
            if locks.is_empty() {
                return Ok(());
            }
            if round == self.max_lock_resolutions {
                return Err(Error::LockNotCleared {
                    start_ts: locks[0].start_ts,
                    key: locks[0].key.clone(),
                    waiting: Waiting::Acquire,
                });
            }
            // The same rule on the prewrite path: the last look waits the lease out, so a
            // `LockNotCleared` here is never reported about a lease that still had time.
            self.resolve_all(&locks, round, round + 1 == self.max_lock_resolutions)?;
        }
        // The loop above returns on every path; `0..=n` is never empty.
        Err(Error::Internal(
            "the prewrite resolution loop fell through".to_owned(),
        ))
    }

    /// The mutations for `keys`, taken from the write buffer.
    fn mutations_for(&self, keys: &[Bytes]) -> Vec<TxnMutation> {
        keys.iter()
            .map(|key| match self.buffer.get(key) {
                Some(Write::Put(value)) => TxnMutation::Put {
                    key: key.clone(),
                    value: value.clone(),
                    // **The snapshot this value was computed from** (ADR 0057 §4). `None` means
                    // the transaction's own, which is every write a statement that never waited
                    // makes — and every write at all until the framing change lands.
                    read_ts: self.read_ts.get(key).copied(),
                },
                Some(Write::Delete) => TxnMutation::Delete {
                    key: key.clone(),
                    read_ts: self.read_ts.get(key).copied(),
                },
                // Not in the buffer: a key this transaction **read** and is asking the store to
                // verify and hold. `Check` writes no value; what it leaves is the lock that makes
                // the validation and the commit atomic (ADR 0067 §1).
                None => TxnMutation::Check { key: key.clone() },
            })
            .collect()
    }

    /// Resolves every lock a prewrite reported, the groups in parallel.
    ///
    /// Grouped by the transaction that holds them rather than by key: one `ResolveLock` names
    /// a `start_ts` and the keys of *that* transaction, and the common case under contention is
    /// one competitor holding several of the keys we want — which is then one call, not one per
    /// key. Every lock of one transaction names the same primary and the same `start_ts`, so
    /// one of them stands for the group when its owner has to be classified.
    fn resolve_all(&self, locks: &[LockInfo], attempt: u32, last: bool) -> Result<()> {
        let mut by_txn: BTreeMap<u64, (LockInfo, Vec<Bytes>)> = BTreeMap::new();
        for lock in locks {
            by_txn
                .entry(lock.start_ts)
                .or_insert_with(|| (lock.clone(), Vec::new()))
                .1
                .push(lock.key.clone());
        }
        let groups: Vec<(LockInfo, Vec<Bytes>)> = by_txn.into_values().collect();
        for outcome in fan_out(groups.len(), |index| {
            let (lock, keys) = &groups[index];
            self.resolve(lock, keys.clone(), attempt, last, true)
        }) {
            outcome?;
        }
        Ok(())
    }

    fn commit_keys(&self, commit_ts: u64, keys: &[Bytes]) -> Result<()> {
        let request = TxnKvReq::Commit {
            start_ts: self.start_ts,
            commit_ts,
            keys: keys.to_vec(),
        };
        match self.call(&request)? {
            // No key: a `Commit` answers for the batch, not per key, so naming one would be a
            // guess dressed as a fact.
            TxnKvResp::Commit { status } => self.check(status, None, Waiting::Acquire),
            other => Err(unexpected(Method::TxnCommit, &other)),
        }
    }

    fn rollback_keys(&self, keys: &[Bytes]) -> Result<()> {
        let request = TxnKvReq::Rollback {
            start_ts: self.start_ts,
            keys: keys.to_vec(),
        };
        match self.call(&request)? {
            TxnKvResp::Rollback { status } => self.check(status, None, Waiting::Acquire),
            other => Err(unexpected(Method::TxnRollback, &other)),
        }
    }

    fn prewrite_grouped(&self, primary: &Bytes, keys: &[Bytes]) -> Result<()> {
        self.grouped(keys, |group| self.prewrite(primary, group))
    }

    /// Prewrites `keys`, and on failure rolls back everything this transaction has placed.
    ///
    /// `placed` is what earlier steps already locked. The rollback is best effort — its own failure
    /// is not the caller's answer, and what it cannot clean the TTL still will.
    fn prewrite_or_roll_back(
        &self,
        primary: &Bytes,
        keys: &[Bytes],
        placed: &[Bytes],
    ) -> Result<()> {
        match self.prewrite_grouped(primary, keys) {
            Ok(()) => Ok(()),
            // **Only a definite failure is cleaned up after.** An `AmbiguousResult` is a prewrite
            // whose fate the client does not know, and its whole contract is that the *caller*
            // decides what to do about it — rolling back here would answer that question on their
            // behalf and turn "you do not know" into "it is dead", which is a different promise
            // from the one this error makes.
            Err(error) if matches!(error, Error::AmbiguousResult { .. }) => Err(error),
            Err(error) => {
                self.undo(primary, placed);
                Err(error)
            }
        }
    }

    /// Rolls back what a failed commit had already locked, best effort.
    ///
    /// The keys of the attempt itself are included: a group that failed may have been one of
    /// several, and the ones that succeeded hold locks nobody is coming back for.
    fn undo(&self, primary: &Bytes, placed: &[Bytes]) {
        let _ = self.rollback_keys(std::slice::from_ref(primary));
        if !placed.is_empty() {
            let _ = self.rollback_grouped(placed);
        }
    }

    /// Sends the range checks, each to the region its **lower bound** falls in.
    ///
    /// A range is not a key, so it cannot join the grouped list — and it leaves no lock, so there
    /// is nothing to commit or roll back for it either. What it does leave is a verdict: anything
    /// committed inside it since this transaction's snapshot refuses the prewrite.
    ///
    /// **A range that spans a region boundary is checked in the region its start is in and no
    /// further**, which is the same bound `Scan` has and is declared in ADR 0067 §3: a phantom
    /// inserted past the boundary is not seen. Splitting a range check across regions is the same
    /// problem as splitting a scan and is not solved here.
    fn prewrite_range_checks(&self, primary: &Bytes) -> Result<()> {
        for (start, end) in &self.check_ranges {
            let request = TxnKvReq::Prewrite {
                start_ts: self.start_ts,
                primary: primary.clone(),
                ttl_ms: self.lock_ttl_ms,
                mutations: vec![TxnMutation::CheckRange {
                    start: start.clone(),
                    end: end.clone(),
                }],
            };
            // **A lock inside the range is resolved here, and never wounded**
            // ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md)
            // §1). Without this loop the `Locked` the store now answers falls through to
            // `check`, whose `Locked` arm reports `LockNotCleared` on sight — a `40001` for a
            // holder that may be about to roll back, which is a phantom that never existed.
            for round in 0..=self.max_lock_resolutions {
                let status = match self.call_resolving(&request, Waiting::ReadSet)? {
                    TxnKvResp::Prewrite { keys } => keys.into_iter().next(),
                    other => return Err(unexpected(Method::TxnPrewrite, &other)),
                };
                let Some(status) = status else { break };
                let TxnStatus::Locked(lock) = status else {
                    // The range's lower bound is the key the conflict is reported against: it
                    // is what the request routed by, and it is the only key of the range this
                    // client can name.
                    self.check(status, Some(start), Waiting::ReadSet)?;
                    break;
                };
                if round == self.max_lock_resolutions {
                    return Err(Error::LockNotCleared {
                        start_ts: lock.start_ts,
                        key: lock.key.clone(),
                        waiting: Waiting::ReadSet,
                    });
                }
                // **`may_wound: false`, and that is the decision this loop exists to make.** A
                // wound is for an *acquirer* — a transaction that holds locks and wants one more,
                // which is half of a cycle. A range check acquires nothing: it asserts that a
                // range it read has not moved. Killing the holder to make that assertion true
                // would abort a transaction that did nothing wrong and would answer `40P01` where
                // the condition is `40001`. So this waits, the way a reader waits, and asks again
                // — and when the holder commits, the write scan sees it and the answer becomes
                // the `Conflict` it always was.
                let last = round + 1 == self.max_lock_resolutions;
                self.resolve(&lock, vec![lock.key.clone()], round, last, false)?;
            }
        }
        Ok(())
    }

    fn commit_grouped(&self, commit_ts: u64, keys: &[Bytes]) -> Result<()> {
        self.grouped(keys, |group| self.commit_keys(commit_ts, group))
    }

    fn rollback_grouped(&self, keys: &[Bytes]) -> Result<()> {
        self.grouped(keys, |group| self.rollback_keys(group))
    }

    /// Splits `keys` by region and runs `send` once per group, the groups in parallel.
    ///
    /// The grouping is a **hint from the region cache**, never an authority: a key whose region
    /// the cache does not know goes in its own group, and a group that turns out to span a
    /// region boundary is refused with `EpochNotMatch`. Nothing here has to be right for the
    /// result to be, which is the same rule the cache lives under everywhere else
    /// (`docs/DESIGN.md` §10) — but being wrong has to be *survivable*, and for one shape it was
    /// not.
    ///
    /// **A refused group is re-cut here, not re-sent by the router.** This used to say the router
    /// retried it "with a repaired cache", which is true and not enough: the router retries *the
    /// request it was given*, and the request carries the group. So a group built from a stale
    /// cache that spans two regions could never succeed however often it was retried — the first
    /// attempt repaired the cache and reset the budget, every later one learned nothing new, and
    /// the ninth gave up. A transaction over a table that spans regions failed its `COMMIT` with
    /// `gave up after 9 attempts: region epoch does not match`, with the splits long settled;
    /// `esker-sql`'s `tests/multi_region_rows.rs` is where that was found, on a real cluster.
    ///
    /// So the loop below re-cuts the refused keys against the repaired cache and sends the pieces.
    /// It stops when the cut stops changing, which is the difference between a boundary this
    /// client had not seen and a cluster it cannot route to at all.
    fn grouped<F>(&self, keys: &[Bytes], send: F) -> Result<()>
    where
        F: Fn(&[Bytes]) -> Result<()> + Send + Sync,
    {
        if keys.is_empty() {
            return Ok(());
        }
        let mut pending = self.by_region(keys);
        let mut refused = None;
        for _ in 0..MAX_REGROUPINGS {
            let outcomes = fan_out(pending.len(), |index| send(&pending[index]));
            let mut stale: Vec<Vec<Bytes>> = Vec::new();
            for (group, outcome) in pending.iter().zip(outcomes) {
                match outcome {
                    Ok(()) => {}
                    // **A group the routing was wrong about, not a cluster that will not answer.**
                    Err(error) if stale_routing(&error) => {
                        stale.push(group.clone());
                        refused = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            if stale.is_empty() {
                return Ok(());
            }
            // **Re-cut against the cache the refusal repaired.** Every attempt above left the
            // router's cache more correct than it found it, so the same keys now group by the
            // regions that actually exist — which is the whole of the fix. Only the keys of
            // groups that were *refused* are re-sent: a refusal is a refusal, so nothing of
            // theirs was applied (`error::tests::a_spent_retry_budget_never_leaves_a_write_in_doubt`
            // is where that is pinned), and the groups that succeeded are not touched.
            let regrouped = self.by_region(&stale.concat());
            if regrouped == stale {
                // The cut did not change, so sending it again would ask the same question and get
                // the same answer. That is a cluster this client cannot route to, not a boundary
                // it had not seen.
                break;
            }
            pending = regrouped;
        }
        refused.map_or(Ok(()), Err)
    }

    /// One group per region the cache believes in, keys in the order they were given.
    fn by_region(&self, keys: &[Bytes]) -> Vec<Vec<Bytes>> {
        let mut groups: BTreeMap<u64, Vec<Bytes>> = BTreeMap::new();
        for key in keys {
            // Region zero is "the cache does not know", and every such key shares one group:
            // one round trip that gets a redirect teaches the cache, and the retry then goes
            // to the right place. Splitting them would cost a round trip each to learn the
            // same thing.
            let region = self
                .router
                .cached_route(key)
                .map_or(0, |route| route.region.id);
            groups.entry(region).or_default().push(key.clone());
        }
        groups.into_values().collect()
    }

    /// A status that is not `Ok` is the transaction's fate, not a failure of the call.
    ///
    /// `key` is the one the status is about, where the method answered per key. `None` where it
    /// did not — and it stays `None` rather than becoming the batch's first key, because a
    /// caller that reads it as "this key lost" would be reading a guess.
    fn check(&self, status: TxnStatus, key: Option<&Bytes>, waiting: Waiting) -> Result<()> {
        match status {
            TxnStatus::Ok => Ok(()),
            TxnStatus::Conflict { commit_ts } => Err(Error::TxnConflict {
                start_ts: self.start_ts,
                commit_ts,
                key: key.cloned(),
            }),
            TxnStatus::RolledBack => Err(Error::TxnSettled {
                start_ts: self.start_ts,
                detail: "rolled back by a resolver that found its lock expired".to_owned(),
            }),
            TxnStatus::Committed { commit_ts } => Err(Error::TxnSettled {
                start_ts: self.start_ts,
                detail: format!("already committed at {commit_ts}"),
            }),
            TxnStatus::LockNotFound => Err(Error::TxnSettled {
                start_ts: self.start_ts,
                detail: "its lock is gone and no record says what happened to it".to_owned(),
            }),
            // Not a verdict: `prewrite` resolves these and comes back, and no other method can
            // answer with one — the decoder refuses it (ADR 0016 decision 1). Reaching here
            // means the caller skipped the resolution, which is a bug in this crate.
            TxnStatus::Locked(lock) => Err(Error::LockNotCleared {
                start_ts: lock.start_ts,
                key: lock.key.clone(),
                waiting,
            }),
        }
    }

    // -- calls -------------------------------------------------------------------------

    fn call(&self, request: &TxnKvReq) -> Result<TxnKvResp> {
        let method = request.method();
        match self.router.call(&Body::Txn(request.clone()))? {
            Response::TxnKv(response) => Ok(response),
            other => Err(Error::UnexpectedResponse {
                expected: method,
                actual: other.method(),
            }),
        }
    }

    /// [`Transaction::call`], resolving any lock that stands in the way and trying again.
    ///
    /// A `Locked` is **not** an error to report: it is the store saying "another transaction
    /// is here, deal with it". Dealing with it is `docs/txn-spec.md` §5.5 — ask the lock's
    /// primary what happened and finish the job either way — and the loop is bounded, because
    /// a lock whose owner keeps heartbeating never clears and a client that waited for ever
    /// would be indistinguishable from one that hung.
    fn call_resolving(&self, request: &TxnKvReq, waiting: Waiting) -> Result<TxnKvResp> {
        for attempt in 0..=self.max_lock_resolutions {
            let error = match self.call(request) {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let Some(lock) = lock_in(&error) else {
                return Err(error);
            };
            let lock = lock?;
            if attempt == self.max_lock_resolutions {
                return Err(Error::LockNotCleared {
                    start_ts: lock.start_ts,
                    key: lock.key.clone(),
                    waiting,
                });
            }
            // **The last look waits out the lease rather than one more backoff step.**
            //
            // The bound on *looks* is still a count — it has to be, or a heartbeating owner that
            // keeps extending its lease would hold this loop for ever, which is the hang the
            // refusal exists to avoid. What was wrong was the bound on *waiting*: eight
            // exponential backoffs come to 2,550 ms against a three-second lease, so a reader
            // meeting a lock younger than 450 ms reported it uncleared without having waited it
            // out. `esker-sql`'s joint gate saw that twice as a panic on `txn.scan(..).unwrap()`.
            //
            // Spending the final look on the whole remaining lease makes the total cover the
            // lease by construction, whatever the schedule adds up to and whatever the budget is
            // set to — a deadline derived from the lease, with the count left to bound the looks.
            let last = attempt + 1 == self.max_lock_resolutions;
            // **A reader never wounds.** It holds nothing, so it cannot be half of a cycle:
            // `docs/plans/cross-node-deadlock.md` puts it exactly — *"a reader holds no locks:
            // it can wait and cannot be waited for"*. Killing a live transaction because
            // somebody read the row it is holding would abort transactions that are in nobody's
            // way, which is what the first version of this did.
            self.resolve(&lock, vec![lock.key.clone()], attempt, last, false)?;
        }
        // The loop above returns on every path; `0..=n` is never empty.
        Err(Error::Internal(
            "the lock resolution loop fell through".to_owned(),
        ))
    }

    /// Finishes someone else's transaction, the way its primary says
    /// (`docs/txn-spec.md` §5.5).
    ///
    /// **The verdict is this client's**, and it has to be. A store cannot classify a
    /// transaction by reading its primary, because the primary may live in another region on
    /// another store — that read would answer "missing" for a transaction perfectly alive
    /// elsewhere and roll back a committed one (`docs/plans/phase-5.md` §10.6). So
    /// `ResolveLock`'s `commit_ts` is not a question, it is an answer: above zero, commit these
    /// keys there; zero, roll them back. Sending zero without having settled the primary first
    /// tells the store to abandon a transaction that may have committed.
    ///
    /// The order is the mirror of the commit's, and for the same reason: **the primary is
    /// settled first**, and everything else follows the fact it leaves behind.
    /// `may_wound` is whether the caller is **acquiring**: only a transaction that holds locks
    /// and wants another can be half of a cycle, and only it may kill a live holder
    /// ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
    fn resolve(
        &self,
        lock: &LockInfo,
        keys: Vec<Bytes>,
        attempt: u32,
        last: bool,
        may_wound: bool,
    ) -> Result<()> {
        let verdict = match self.classify(lock)? {
            Classified::Settled(verdict) => verdict,
            // Its owner is inside its lease. Waiting is the whole answer: the caller retries,
            // and by then the owner has committed, or its lease has run out and this returns a
            // verdict instead. Killing it here would abort a live transaction.
            //
            // Never longer than the lease has left, because that instant is when the answer
            // can change: sleeping through it would spend a round of the budget on a lock that
            // had become settleable while this thread was asleep. The backoff is still the
            // ceiling — a long lease is waited on the way any other contended resource is.
            Classified::Alive { lease_ms } if !may_wound || lock.start_ts <= self.start_ts => {
                // On the last look, the whole remaining lease: after it the lock is settleable,
                // so the refusal above is only ever reported about a lease that has run out or
                // an owner that extended it. Otherwise a backoff step, capped by the lease for
                // the reason it always was — that instant is when the answer can change.
                let wait = if last {
                    lease_ms
                } else {
                    backoff_ms(attempt).min(lease_ms)
                }
                .max(1);
                crate::stmt_stats::record_wait(Duration::from_millis(wait));
                self.router.clock().sleep(Duration::from_millis(wait));
                return Ok(());
            }
            // **The holder is alive and younger than us, so it loses**
            // ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)). Waiting it out is what
            // this did before eager locks existed, and it was right then: no transaction here held
            // a lock while waiting for another, so a wait always ended. It can now — a
            // `SELECT … FOR UPDATE` puts a lock on the store and the transaction goes on asking for
            // more — and two transactions that want each other's rows would both wait out their
            // budgets and both fail, where one node and PostgreSQL both kill exactly one.
            //
            // Wound-wait picks that one without a graph and without a round trip: `start_ts` comes
            // from the TSO (`CLAUDE.md` invariant 6), so every node breaks the tie the same way,
            // and the oldest transaction in a cycle never waits — which is why this terminates.
            // The victim learns at its own commit, which refuses a transaction whose primary
            // carries a rollback marker.
            // Alive, younger, and this is a transaction **acquiring** — see the note on
            // `may_wound`.
            Classified::Alive { .. } => self.settle_primary(lock)?,
        };
        // The primary is already settled — `classify` settled it — so only the rest is left.
        let rest: Vec<Bytes> = keys
            .into_iter()
            .filter(|key| *key != lock.primary)
            .collect();
        if rest.is_empty() {
            return Ok(());
        }
        let request = TxnKvReq::ResolveLock {
            start_ts: lock.start_ts,
            commit_ts: verdict.commit_ts(),
            keys: rest,
        };
        match self.call(&request)? {
            TxnKvResp::ResolveLock { .. } => Ok(()),
            other => Err(unexpected(Method::TxnResolveLock, &other)),
        }
    }

    /// What the transaction holding `lock` did, settling it if its lease has run out.
    fn classify(&self, lock: &LockInfo) -> Result<Classified> {
        // From the oracle, not from a clock (`CLAUDE.md` invariant 6). A fresh timestamp
        // rather than this transaction's own `start_ts`: both are conservative, but a reader
        // that began long ago would judge every lock alive for ever and never make progress.
        crate::stmt_stats::record_tso();
        let now = self.oracle.timestamp()?;
        if !is_expired(lock.start_ts, lock.ttl_ms, now) {
            return Ok(Classified::alive(lock, now));
        }
        // The lock in hand may be a *secondary's*, and a `Heartbeat` extends the primary's
        // lease alone — so a secondary's TTL can say "dead" about a transaction whose primary
        // is still being kept alive. The primary's lease is the one that decides
        // (`docs/txn-spec.md` §5.5), and this is the round trip that asks for it. It is spent
        // only on the path that is about to declare somebody dead.
        if let Some(primary) = self.lock_on_primary(lock)?
            && !is_expired(primary.start_ts, primary.ttl_ms, now)
        {
            return Ok(Classified::alive(&primary, now));
        }
        self.settle_primary(lock).map(Classified::Settled)
    }

    /// The lock the transaction still holds on its own primary, if it holds one.
    ///
    /// A `Get` of the primary at the lock's own `start_ts` answers `Locked` exactly while the
    /// owner's lock is in the way. Someone *else's* lock there means ours is long gone, which
    /// reads as settled rather than as alive — the same reading `percolator::primary_state`
    /// makes of it.
    fn lock_on_primary(&self, lock: &LockInfo) -> Result<Option<LockInfo>> {
        let request = TxnKvReq::Get {
            key: lock.primary.clone(),
            ts: lock.start_ts,
        };
        let Err(error) = self.call(&request) else {
            // It answered, so nothing is in the way: the owner's lock is gone.
            return Ok(None);
        };
        let Some(found) = lock_in(&error) else {
            return Err(error);
        };
        let found = found?;
        Ok((found.start_ts == lock.start_ts).then_some(found))
    }

    /// What happened to this transaction's own primary: `Some(commit_ts)` if it committed after
    /// all, `None` if it is dead.
    ///
    /// The same atomic question [`Transaction::settle_primary`] asks of somebody else's, asked of
    /// our own after the commit point refused us — a `Rollback` either finds the commit or leaves
    /// the marker, with no window between looking and deciding.
    fn primary_fate(&self, primary: &Bytes) -> Result<Option<u64>> {
        let request = TxnKvReq::Rollback {
            start_ts: self.start_ts,
            keys: vec![primary.clone()],
        };
        match self.call(&request)? {
            TxnKvResp::Rollback { status } => Ok(match status {
                TxnStatus::Committed { commit_ts } => Some(commit_ts),
                _ => None,
            }),
            other => Err(unexpected(Method::TxnRollback, &other)),
        }
    }

    /// Settles the primary, and answers with what the transaction turned out to have done.
    ///
    /// A `Rollback` of it is the verdict *and* the act, in one apply: it either leaves a
    /// rollback marker — so the transaction is dead for ever, and a late `Prewrite` of it will
    /// fail (`docs/txn-spec.md` §5.4) — or it answers `Committed`, because the commit got there
    /// first. There is no window between looking and deciding, which is what would let a
    /// resolver undo a commit.
    fn settle_primary(&self, lock: &LockInfo) -> Result<Verdict> {
        let request = TxnKvReq::Rollback {
            start_ts: lock.start_ts,
            keys: vec![lock.primary.clone()],
        };
        match self.call(&request)? {
            TxnKvResp::Rollback { status } => match status {
                // Ours or someone else's, the marker is there and the answer is the same.
                TxnStatus::Ok | TxnStatus::RolledBack => Ok(Verdict::Dead),
                TxnStatus::Committed { commit_ts } => Ok(Verdict::Committed { commit_ts }),
                // Nothing else can come back from a rollback, and guessing at one would mean
                // guessing at a transaction's fate. `LockNotFound` here means the store could
                // not account for the primary at all, which is not a licence to abandon it.
                other => Err(Error::Store(ProtoError::invalid(format!(
                    "settling the primary of the transaction at {} answered {other:?}",
                    lock.start_ts
                )))),
            },
            other => Err(unexpected(Method::TxnRollback, &other)),
        }
    }
}

/// What [`Transaction::lock`] found.
///
/// The two answers a caller has to tell apart: the key is this transaction's, or somebody else's
/// and they are still alive. There is no third — a settled holder is resolved inside `lock`, and a
/// younger one is wounded, so neither reaches the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// This transaction holds the key now, or held it already.
    Taken,
    /// An **older** live transaction holds it. Waiting is the only move that does not abort
    /// somebody, and how long to wait belongs to the caller.
    Held {
        /// The holder's `start_ts`, which is what a wait-for edge is drawn between and what makes
        /// the holder older than the asker.
        by: u64,
        /// What is left of the holder's lease, in milliseconds.
        lease_ms: u64,
    },
}

impl Drop for Transaction {
    /// **The ending that is not a method call.** A transaction dropped without `commit` or
    /// `rollback` — a session that disconnected, a `?` on the way out — has stopped existing, and a
    /// renewal that outlived it would keep its lock alive for ever: a crashed client's row held
    /// permanently, which is worse than the lease this closes the gap in.
    ///
    /// It only forgets. The locks themselves are left to the lease and the resolver, which is what
    /// happens to any abandoned transaction and is not this method's business to change.
    fn drop(&mut self) {
        self.renewals.forget(self.start_ts);
    }
}

/// What a resolver found when it looked at a lock's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classified {
    /// It is finished, one way or the other, and its keys can be settled.
    Settled(Verdict),
    /// It is inside its lease, with this many milliseconds of it left.
    Alive {
        /// What remains of the lease, in the physical milliseconds of the oracle's timestamps.
        lease_ms: u64,
    },
}

impl Classified {
    /// A live lock, with what is left of its lease measured against `now`.
    fn alive(lock: &LockInfo, now: u64) -> Self {
        let ends_ms = physical_ms(lock.start_ts).saturating_add(lock.ttl_ms);
        Self::Alive {
            lease_ms: ends_ms.saturating_sub(physical_ms(now)).saturating_add(1),
        }
    }
}

/// What the transaction that owns a lock turned out to have done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// It committed, at this timestamp: every key of it rolls **forward**.
    Committed {
        /// Where its `write` records go.
        commit_ts: u64,
    },
    /// It is dead — settled by us, or by whoever got there first.
    Dead,
}

impl Verdict {
    /// The `commit_ts` a `ResolveLock` carries. Zero is "roll back", which is unambiguous
    /// because no transaction commits at timestamp zero.
    fn commit_ts(self) -> u64 {
        match self {
            Self::Committed { commit_ts } => commit_ts,
            Self::Dead => 0,
        }
    }
}

/// Version byte of an exported-snapshot record. An unknown one is a typed error and never a
/// guess (`CLAUDE.md` invariant 2).
const SNAPSHOT_RECORD_VERSION: u8 = 1;

/// `version:u8 ++ start_ts:u64` little-endian — nine bytes.
///
/// Deliberately the same shape as the retention records of
/// [ADR 0021](../../../docs/adr/0021-time-machine.md) decision 4: a version byte and a `u64`, so
/// the two records of one feature read alike and neither needs its own explanation.
fn encode_snapshot(at: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = SNAPSHOT_RECORD_VERSION;
    out[1..].copy_from_slice(&at.to_le_bytes());
    out
}

fn decode_snapshot(name: &[u8], record: &[u8]) -> Result<u64> {
    // Length first: a nine-byte record is the whole format, so anything else is a value that
    // is not one of these rather than a version to interpret.
    let Ok(bytes) = <[u8; 9]>::try_from(record) else {
        return Err(Error::Store(ProtoError::corrupt(
            "snapshot record",
            format!(
                "the snapshot named {name:?} is {} bytes, not 9",
                record.len()
            ),
        )));
    };
    if bytes[0] != SNAPSHOT_RECORD_VERSION {
        return Err(Error::Store(ProtoError::corrupt(
            "snapshot record",
            format!(
                "the snapshot named {name:?} is version {}, and this build reads {SNAPSHOT_RECORD_VERSION}",
                bytes[0]
            ),
        )));
    }
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&bytes[1..]);
    Ok(u64::from_le_bytes(ts))
}

/// The lock inside a `Locked` refusal, if that is what this error is.
///
/// A `Locked` whose payload will not decode is an error and not an absence: treating it as
/// "no lock in the way" would make a client loop against a key it can never read.
fn lock_in(error: &Error) -> Option<Result<LockInfo>> {
    let Error::Store(proto) = error else {
        return None;
    };
    LockInfo::from_error(proto).map(|decoded| {
        decoded.map_err(|error| {
            Error::Store(ProtoError::invalid(format!(
                "a locked key came back with a lock nothing can read: {error}"
            )))
        })
    })
}

fn unexpected(expected: Method, response: &TxnKvResp) -> Error {
    Error::UnexpectedResponse {
        expected,
        actual: response.method(),
    }
}

/// A [`TimestampOracle`] that hands out consecutive numbers from a counter.
///
/// For tests and for the single-process tools: it is a *correct* oracle for one client and a
/// wrong one for two, which is exactly what `CLAUDE.md` invariant 6 is about. Production gets
/// its timestamps from PD.
#[derive(Debug)]
pub struct CountingOracle {
    next: std::sync::atomic::AtomicU64,
}

impl CountingOracle {
    /// An oracle whose first timestamp is `start`.
    #[must_use]
    pub fn starting_at(start: u64) -> Self {
        Self {
            next: std::sync::atomic::AtomicU64::new(start),
        }
    }

    /// The next timestamp this oracle would hand out.
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.next.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for CountingOracle {
    fn default() -> Self {
        Self::starting_at(1)
    }
}

impl TimestampOracle for CountingOracle {
    fn tso(&self, count: u32) -> std::result::Result<u64, ProtoError> {
        let count = u64::from(count.max(1));
        Ok(self
            .next
            .fetch_add(count, std::sync::atomic::Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CountingOracle, SNAPSHOT_RECORD_VERSION, TimestampOracle, decode_snapshot, encode_snapshot,
    };

    /// Timestamps must not repeat: two transactions sharing a `start_ts` would share an
    /// identity, and a resolver could not tell whose lock it had found.
    #[test]
    fn the_counting_oracle_never_repeats() {
        let oracle = CountingOracle::starting_at(10);
        assert_eq!(oracle.timestamp().unwrap(), 10);
        assert_eq!(oracle.tso(5).unwrap(), 11);
        assert_eq!(
            oracle.timestamp().unwrap(),
            16,
            "a batch is not handed out twice"
        );
        assert_eq!(oracle.peek(), 17);
    }

    /// A batch of zero is still one timestamp: answering the same number twice would be worse
    /// than rounding a caller's mistake up.
    #[test]
    fn a_batch_of_zero_still_advances() {
        let oracle = CountingOracle::starting_at(1);
        assert_eq!(oracle.tso(0).unwrap(), 1);
        assert_eq!(oracle.tso(0).unwrap(), 2);
    }

    /// **Frozen bytes.** An exported snapshot is a record other builds will read, so its nine
    /// bytes are pinned here rather than checked against the encoder that produced them — a
    /// round trip passes just as happily when both halves drift together.
    #[test]
    fn a_snapshot_record_is_nine_frozen_bytes() {
        assert_eq!(
            encode_snapshot(0x0102_0304_0506_0708),
            [1, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
            "version 1, then the timestamp little-endian"
        );
        assert_eq!(
            encode_snapshot(0),
            [SNAPSHOT_RECORD_VERSION, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    /// And the decoder reads that pin, so the pair cannot drift together.
    #[test]
    fn a_frozen_record_decodes_to_the_timestamp_it_names() {
        let frozen = [1u8, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
        assert_eq!(
            decode_snapshot(b"n", &frozen).unwrap(),
            0x0102_0304_0506_0708
        );
    }

    /// Every truncation, one byte too many, and a version this build does not know: all
    /// refused, none guessed at.
    #[test]
    fn a_record_that_is_not_one_is_refused_rather_than_read() {
        let good = encode_snapshot(42);
        for length in 0..good.len() {
            assert!(
                decode_snapshot(b"n", &good[..length]).is_err(),
                "a {length}-byte value is not a snapshot record"
            );
        }
        let mut too_long = good.to_vec();
        too_long.push(0);
        assert!(decode_snapshot(b"n", &too_long).is_err());

        let mut wrong_version = good;
        wrong_version[0] = SNAPSHOT_RECORD_VERSION + 1;
        assert!(decode_snapshot(b"n", &wrong_version).is_err());
    }
}
