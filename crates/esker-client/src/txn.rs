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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::region_cache::RegionResolver;
use crate::retry::backoff_ms;
use crate::router::{ClientOptions, Router, fan_out};
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
        Self {
            router: Arc::new(Router::with_options(transport, resolver, options)),
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
        let start_ts = self.oracle.timestamp()?;
        Ok(Transaction {
            router: Arc::clone(&self.router),
            oracle: Arc::clone(&self.oracle),
            start_ts,
            lock_ttl_ms: self.lock_ttl_ms,
            max_lock_resolutions: self.max_lock_resolutions,
            max_scan_regions: self.max_scan_regions,
            buffer: BTreeMap::new(),
            state: State::Open,
        })
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
    start_ts: u64,
    lock_ttl_ms: u64,
    max_lock_resolutions: u32,
    /// Most regions one scan will walk before it stops and answers with what it has.
    max_scan_regions: usize,
    /// The write set, in key order. A `BTreeMap` rather than a list because a transaction that
    /// writes a key twice must send one mutation, not two — and because the *first* key in
    /// order is a stable choice of primary, which makes a retried commit pick the same one.
    buffer: BTreeMap<Bytes, Write>,
    state: State,
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
        self.buffer.keys().next()
    }

    /// Buffers a write. No I/O: the whole set goes out at `commit()`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.buffer.insert(
            Bytes::copy_from_slice(key),
            Write::Put(Bytes::copy_from_slice(value)),
        );
    }

    /// Buffers a delete. No I/O.
    pub fn delete(&mut self, key: &[u8]) {
        self.buffer
            .insert(Bytes::copy_from_slice(key), Write::Delete);
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
            ts: self.start_ts,
        };
        match self.call_resolving(&request)? {
            TxnKvResp::Get { value } => Ok(value),
            other => Err(unexpected(Method::TxnGet, &other)),
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
            let page = self.scan_page(&cursor, end, limit)?;
            merged.extend(page);
            if merged.len() >= limit as usize {
                break;
            }
            // Where this region ended is where the next one starts. An empty `end_key` is the
            // end of the key space, so a region carrying one is the last there is.
            let Some(next) = self
                .router
                .cached_route(&cursor)
                .map(|route| route.region.end_key)
                .filter(|next| !next.is_empty())
            else {
                break;
            };
            // Past the range the caller asked for, or not moving. The second is the guard that
            // matters: a cache entry naming a region that ends at or before the cursor would
            // otherwise ask the same region for ever.
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

    /// One region's worth of a scan.
    fn scan_page(&self, start: &Bytes, end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        let request = TxnKvReq::Scan {
            start: start.clone(),
            end: Bytes::copy_from_slice(end),
            limit,
            ts: self.start_ts,
            reverse: false,
        };
        match self.call_resolving(&request)? {
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
        self.finish()?;
        let Some(primary) = self.primary().cloned() else {
            return Ok(None);
        };

        // 2. The primary, alone and first. Its lock is what every resolver reads.
        self.prewrite(&primary, std::slice::from_ref(&primary))?;

        // 3. The secondaries, grouped by region, the groups in parallel.
        let secondaries: Vec<Bytes> = self
            .buffer
            .keys()
            .filter(|key| **key != primary)
            .cloned()
            .collect();
        self.prewrite_grouped(&primary, &secondaries)?;

        // 4. and 5. The commit point.
        let commit_ts = self.oracle.timestamp()?;
        self.commit_keys(commit_ts, std::slice::from_ref(&primary))?;

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
        let keys: Vec<Bytes> = self.buffer.keys().cloned().collect();
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
        Ok(())
    }

    // -- the phases --------------------------------------------------------------------

    /// Prewrites one region's worth of keys, resolving whatever locks come back.
    ///
    /// A `Prewrite` answers **per key**, so a batch that collides with several locks reports
    /// all of them at once ([ADR 0016](../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
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
                return self.check(statuses[at].clone(), lost);
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
                });
            }
            self.resolve_all(&locks, round)?;
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
                },
                // A key that is not in the buffer cannot be reached: every list here is built
                // from the buffer's own keys. `Delete` is the safe reading if it ever were.
                Some(Write::Delete) | None => TxnMutation::Delete { key: key.clone() },
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
    fn resolve_all(&self, locks: &[LockInfo], attempt: u32) -> Result<()> {
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
            self.resolve(lock, keys.clone(), attempt)
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
            TxnKvResp::Commit { status } => self.check(status, None),
            other => Err(unexpected(Method::TxnCommit, &other)),
        }
    }

    fn rollback_keys(&self, keys: &[Bytes]) -> Result<()> {
        let request = TxnKvReq::Rollback {
            start_ts: self.start_ts,
            keys: keys.to_vec(),
        };
        match self.call(&request)? {
            TxnKvResp::Rollback { status } => self.check(status, None),
            other => Err(unexpected(Method::TxnRollback, &other)),
        }
    }

    fn prewrite_grouped(&self, primary: &Bytes, keys: &[Bytes]) -> Result<()> {
        self.grouped(keys, |group| self.prewrite(primary, group))
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
    /// region boundary is refused with `EpochNotMatch` and retried by the router with a
    /// repaired cache. Nothing here has to be right for the result to be, which is the same
    /// rule the cache lives under everywhere else (`docs/DESIGN.md` §10).
    fn grouped<F>(&self, keys: &[Bytes], send: F) -> Result<()>
    where
        F: Fn(&[Bytes]) -> Result<()> + Send + Sync,
    {
        if keys.is_empty() {
            return Ok(());
        }
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
        let groups: Vec<Vec<Bytes>> = groups.into_values().collect();
        for outcome in fan_out(groups.len(), |index| send(&groups[index])) {
            outcome?;
        }
        Ok(())
    }

    /// A status that is not `Ok` is the transaction's fate, not a failure of the call.
    ///
    /// `key` is the one the status is about, where the method answered per key. `None` where it
    /// did not — and it stays `None` rather than becoming the batch's first key, because a
    /// caller that reads it as "this key lost" would be reading a guess.
    fn check(&self, status: TxnStatus, key: Option<&Bytes>) -> Result<()> {
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
    fn call_resolving(&self, request: &TxnKvReq) -> Result<TxnKvResp> {
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
                });
            }
            self.resolve(&lock, vec![lock.key.clone()], attempt)?;
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
    fn resolve(&self, lock: &LockInfo, keys: Vec<Bytes>, attempt: u32) -> Result<()> {
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
            Classified::Alive { lease_ms } => {
                let wait = backoff_ms(attempt).min(lease_ms).max(1);
                self.router.clock().sleep(Duration::from_millis(wait));
                return Ok(());
            }
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
    use super::{CountingOracle, TimestampOracle};

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
}
