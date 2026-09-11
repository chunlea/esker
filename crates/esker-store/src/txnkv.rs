//! The seven `TxnKv` methods, over the engine (`docs/DESIGN.md` §8, `docs/txn-spec.md`).
//!
//! This module owns almost nothing. Every rule of Percolator lives in `esker-txn` as a pure
//! function over a snapshot; what is here is the snapshot — the five questions answered out of
//! the `lock`, `write` and `default` column families — and the translation between
//! `esker-txn`'s decisions and the wire.
//!
//! # Where the decision happens
//!
//! **At apply, never at propose** (`docs/plans/phase-5.md` §10.1). A Percolator operation is a
//! read-modify-write, and a leader that read its engine and then proposed the resulting
//! mutations would leave a window: two prewrites of one key both read "no lock", both propose,
//! both apply, and the second overwrites the first's lock. Apply is already sequential per
//! region and every peer holds identical state at the same log index, so the decision is a
//! pure function of `(state, request)` and every peer reaches the same one — with no gate and
//! no window. `Command::CompareAndSwap` reads inside apply for exactly this reason.
//!
//! # No clock here
//!
//! Nothing in this module reads a clock, and it must stay that way: two peers reading different
//! clocks at the same log index would diverge. The lock-expiry judgement therefore belongs to
//! the client, which holds an oracle timestamp of its own and judges *conservatively* — late in
//! declaring a lock dead, never early (`docs/plans/phase-5.md` §10.2).
//!
//! # And no reading of another region's keys
//!
//! A transaction's primary and its secondaries may live in **different regions on different
//! stores**. So this store never classifies a transaction by reading its primary: that read
//! would answer "missing" for a transaction that is perfectly alive elsewhere, and roll back a
//! committed one. Classification is the client's, made against the primary's own region, and it
//! arrives in the request — as `commit_ts` on a `ResolveLock`, and as the very act of sending a
//! secondary `Commit`.
//!
//! # The `'x'` namespace
//!
//! A client sends raw user keys and gets raw user keys back. The `'x'` prefix, the
//! memcomparable encoding and the version suffix are applied here and only here, by
//! `esker_txn::key` — the same rule that keeps `'r'` out of `RawKv` requests
//! (`docs/DESIGN.md` §10).

use std::collections::BTreeSet;

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, Snapshot, WriteBatch, cf};
use esker_proto::txn::{LockInfo, TxnStatus};
use esker_proto::{ProtoError, TxnKvResp, TxnMutation};
use esker_txn::codec::{LockRecord, WriteRecord};
use esker_txn::mutation::{Cf, Mutation, Mutations};
use esker_txn::snapshot::{TxnSnapshot, Version};
use esker_txn::{Op, Prewrite, PrewriteDecision, ReadOutcome, TxnError, key};

use crate::error::engine_to_proto;

/// Most key-value pairs one transactional `Scan` returns, whatever the caller asked for.
pub const MAX_SCAN_LIMIT: u32 = 8192;

/// Most bytes of keys and values one transactional `Scan` returns. See
/// [`crate::rawkv::MAX_SCAN_BYTES`]: a response that will not fit in a frame is a response the
/// caller never sees at all.
pub const MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;

/// The five questions of `docs/txn-spec.md` §5, answered out of one engine snapshot.
///
/// Pinned for the whole of one request: every read a decision makes must see the same instant,
/// or a prewrite could check the `write` column family against one state and the `lock` column
/// family against another — and the pair of checks is the whole of the isolation guarantee.
#[derive(Debug)]
pub struct EngineSnapshot<'a> {
    db: &'a Db,
    options: ReadOptions,
}

impl<'a> EngineSnapshot<'a> {
    /// Pins `db` at this instant.
    #[must_use]
    pub fn new(db: &'a Db) -> Self {
        Self::at(db, db.snapshot())
    }

    /// Pins `db` at an explicit snapshot.
    #[must_use]
    pub fn at(db: &'a Db, snapshot: Snapshot) -> Self {
        Self {
            db,
            options: ReadOptions {
                snapshot: Some(snapshot),
                ..ReadOptions::default()
            },
        }
    }

    fn get(&self, column: &str, key: &[u8]) -> esker_txn::Result<Option<Bytes>> {
        self.db
            .get(column, key, &self.options)
            .map_err(|error| storage(&error))
    }

    /// Walks one user key's `write` versions, newest first, from `ts` downwards.
    ///
    /// One forward seek and a prefix check, which is the whole reason `enc_ts` is complemented
    /// (`docs/txn-spec.md` §5). `stop_below` bounds the walk: `write_of_txn` needs only the
    /// versions at or above a transaction's `start_ts`, and an unbounded walk over a key with a
    /// long version chain would read the lot.
    fn walk<T>(
        &self,
        user_key: &[u8],
        from_ts: u64,
        stop_below: u64,
        mut take: impl FnMut(Version) -> Option<T>,
    ) -> esker_txn::Result<Option<T>> {
        let prefix = key::prefix(user_key);
        let mut iter = self
            .db
            .iter(cf::WRITE, &self.options)
            .map_err(|error| storage(&error))?;
        iter.seek(&key::seek_write(user_key, from_ts));
        while iter.valid() {
            if !iter.key().starts_with(&prefix) {
                break;
            }
            let (_, commit_ts) = key::split(iter.key())?;
            if commit_ts < stop_below {
                break;
            }
            let record = WriteRecord::decode(iter.value())?;
            if let Some(found) = take(Version::new(commit_ts, record)) {
                return Ok(Some(found));
            }
            iter.next();
        }
        iter.status().map_err(|error| storage(&error))?;
        Ok(None)
    }
}

impl TxnSnapshot for EngineSnapshot<'_> {
    fn get_lock(&self, user_key: &[u8]) -> esker_txn::Result<Option<LockRecord>> {
        self.get(cf::LOCK, &key::lock(user_key))?
            .map(|bytes| LockRecord::decode(&bytes))
            .transpose()
    }

    fn seek_write(&self, user_key: &[u8], ts: u64) -> esker_txn::Result<Option<Version>> {
        self.walk(user_key, ts, 0, Some)
    }

    fn newest_write_after(&self, user_key: &[u8], ts: u64) -> esker_txn::Result<Option<Version>> {
        // From the newest record downwards, and the first record that *changed the key* is the
        // answer: anything at or below `ts` ends the question, a rollback marker is stepped past
        // rather than answered with (ADR 0078), and so is a `Lock` — a transaction that held the
        // key and wrote nothing to it left no value for a later writer to be stale against
        // ([ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md)).
        self.walk(user_key, u64::MAX, ts.saturating_add(1), |version| {
            (!matches!(
                version.record.kind,
                esker_txn::Kind::Rollback | esker_txn::Kind::Lock
            ))
            .then_some(version)
        })
    }

    /// Every `write` record in `[start, end)`, looking for one committed after `ts`.
    ///
    /// A scan of the `write` column family rather than a walk per key: the range is the unit, and
    /// the answer is "somebody committed in here" rather than "which key". The bounds are the
    /// *user* keys' prefixes, so a record for `end` itself is outside — a range is half-open here as
    /// it is everywhere else.
    fn newest_write_in_range(
        &self,
        start: &[u8],
        end: &[u8],
        ts: u64,
    ) -> esker_txn::Result<Option<Version>> {
        if start >= end {
            return Ok(None);
        }
        let mut iter = self
            .db
            .iter(cf::WRITE, &self.options)
            .map_err(|error| storage(&error))?;
        iter.seek(&key::prefix(start));
        let upper = key::prefix(end);
        while iter.valid() {
            if iter.key() >= &upper[..] {
                break;
            }
            let (user_key, commit_ts) = key::split(iter.key())?;
            if commit_ts > ts {
                let record = WriteRecord::decode(iter.value())?;
                // A rollback marker is not a commit, so it is not a phantom either (ADR 0078):
                // a transaction that took a key in this range and died changed nothing, and
                // refusing the read set over it is a `40001` for nothing. A `Lock` record is a
                // different matter — it *is* a commit, just one that wrote no version.
                if record.kind != esker_txn::Kind::Rollback {
                    return Ok(Some(Version::new(commit_ts, record)));
                }
                // A marker was stepped past, and an **older** version of this same key can still
                // be above `ts`. So this one key is walked version by version, which is the case
                // the seek below may not take.
                iter.next();
            } else {
                // Versions sort newest-first under a key's prefix, so this key's newest is at or
                // below `ts` and every older one is too. Nothing under this prefix can be a
                // phantom: past the whole key (#58, the same rule as `scan`'s).
                let (_, past_this_key) = key::version_range(&user_key);
                iter.seek(&past_this_key);
            }
        }
        iter.status().map_err(|error| storage(&error))?;
        Ok(None)
    }

    /// The first lock in `[start, end)` that is not `mine`.
    ///
    /// The mirror of the scan above over the other column family, and the bounds are the same
    /// bytes: a `lock` key is `'x' ++ enc(user_key)` with no timestamp suffix, which is exactly
    /// the prefix a `write` key carries in front of its suffix. **First and not newest** — a lock
    /// has no version to be newest of, and any one of them is enough to refuse the check.
    fn foreign_lock_in_range(
        &self,
        start: &[u8],
        end: &[u8],
        mine: u64,
    ) -> esker_txn::Result<Option<(Vec<u8>, LockRecord)>> {
        if start >= end {
            return Ok(None);
        }
        let mut iter = self
            .db
            .iter(cf::LOCK, &self.options)
            .map_err(|error| storage(&error))?;
        iter.seek(&key::prefix(start));
        let upper = key::prefix(end);
        while iter.valid() {
            if iter.key() >= &upper[..] {
                break;
            }
            let record = LockRecord::decode(iter.value())?;
            // **Our own lock is not a phantom.** The client prewrites its keys before it sends
            // the range checks, so the row this transaction just inserted into the range it read
            // is locked and sitting here — see the trait's note.
            if record.start_ts != mine {
                let user_key = key::split_lock(iter.key())?;
                return Ok(Some((user_key, record)));
            }
            iter.next();
        }
        iter.status().map_err(|error| storage(&error))?;
        Ok(None)
    }

    fn write_of_txn(&self, user_key: &[u8], start_ts: u64) -> esker_txn::Result<Option<Version>> {
        // Bounded below by `start_ts`: a transaction's own record is a commit above it or its
        // rollback marker exactly at it, so there is nothing to find further down.
        self.walk(user_key, u64::MAX, start_ts, |version| {
            (version.record.start_ts == start_ts).then_some(version)
        })
    }

    fn get_value(&self, user_key: &[u8], start_ts: u64) -> esker_txn::Result<Option<Bytes>> {
        self.get(cf::DEFAULT, &key::value(user_key, start_ts))
    }
}

/// Stages `mutations` into `batch`, mapping this crate's column families onto the engine's.
pub fn stage(db: &Db, batch: &mut WriteBatch, mutations: &Mutations) -> Result<(), ProtoError> {
    for mutation in mutations {
        let cf_id = cf_id(db, mutation.cf())?;
        match mutation {
            Mutation::Put { key, value, .. } => batch.put(cf_id, key, value),
            Mutation::Delete { key, .. } => batch.delete(cf_id, key),
        }
    }
    Ok(())
}

fn cf_id(db: &Db, column: Cf) -> Result<u32, ProtoError> {
    db.cf_id(column.name()).ok_or_else(|| {
        ProtoError::internal(format!(
            "this database has no {} column family; it was created without one",
            column.name()
        ))
    })
}

/// **The newest `commit_ts` for one key, or `None`** — ADR 0067's read.
///
/// The question a waiter asks instead of guessing: a statement that took a row lock without waiting
/// cannot tell from the lock alone whether the writer in front committed and released between its
/// read and its lock. This is the same seek prewrite's own conflict check makes, without the write.
///
/// A rollback marker is **not** a commit and is skipped: it sits at `commit_ts == start_ts` and
/// says a transaction gave up, so reporting it as the newest commit would tell a waiter its value
/// is stale when nothing replaced it.
pub(crate) fn latest_commit(db: &Db, key: &[u8]) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    // No `kind` filter here: `newest_write_after` answers with the newest *committed* write and
    // steps past markers itself (ADR 0078). Filtering the `Option` afterwards was how this
    // answered `None` — "nothing ever committed this key" — whenever the newest record happened
    // to be a rollback marker, which is what let `changed_since_statement` miss a real commit and
    // leave the statement to be refused at prewrite instead of re-run.
    let newest = snapshot
        .newest_write_after(key, 0)
        .map_err(txn_to_proto)?
        .map(|version| version.commit_ts);
    Ok(TxnKvResp::LatestCommit { newest })
}

/// Reads one key at `ts` (`docs/txn-spec.md` §5.1).
pub fn get(db: &Db, user_key: &[u8], ts: u64) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    match esker_txn::read(&snapshot, user_key, ts).map_err(txn_to_proto)? {
        ReadOutcome::Value(value) => Ok(TxnKvResp::Get { value: Some(value) }),
        ReadOutcome::NotFound => Ok(TxnKvResp::Get { value: None }),
        // A lock is a refusal to serve one key, and one key is all this asks about — so it goes
        // out through the error channel, where the client's retry machinery already lives
        // ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
        ReadOutcome::Locked(lock) => Err(lock_info(user_key, &lock).into_error()),
    }
}

/// Reads `[start, end)` at `ts`, in key order.
///
/// Stops at the first lock rather than skipping it: a scan that returned the rows it could read
/// and silently omitted the locked one would be a scan whose result is not a snapshot of
/// anything. The client resolves and asks again.
pub fn scan(
    db: &Db,
    start: &[u8],
    end: &[u8],
    limit: u32,
    ts: u64,
    reverse: bool,
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut pairs = Vec::new();
    let mut bytes = 0usize;
    let limit = limit.min(MAX_SCAN_LIMIT) as usize;

    for user_key in user_keys_in(db, &snapshot, start, end, reverse)? {
        if pairs.len() >= limit || bytes >= MAX_SCAN_BYTES {
            break;
        }
        match esker_txn::read(&snapshot, &user_key, ts).map_err(txn_to_proto)? {
            ReadOutcome::Value(value) => {
                bytes += user_key.len() + value.len();
                pairs.push((Bytes::from(user_key), value));
            }
            ReadOutcome::NotFound => {}
            ReadOutcome::Locked(lock) => {
                return Err(lock_info(&user_key, &lock).into_error());
            }
        }
    }
    Ok(TxnKvResp::Scan { pairs })
}

/// The distinct user keys in `[start, end)` that a read at some timestamp could answer for:
/// every key with a version, **and every key that is only locked**.
///
/// A scan of the `write` column family sees one entry per *version*, so the keys have to be
/// collapsed before they are read at a timestamp — otherwise a key rewritten a hundred times
/// would fill a hundred slots of the caller's limit with one row.
///
/// The `lock` column family is the half that is easy to leave out, and leaving it out is a
/// silent wrong answer. A key prewritten by a transaction that has since **committed its
/// primary** has a lock and no `write` record: the transaction is committed, so the row exists,
/// and until someone resolves that lock there is nothing in the `write` CF to find it by. A
/// scan built from versions alone answers without the row and reports no lock, so the caller
/// has nothing to resolve and no way to notice — which is the failure `scan`'s own header
/// promises not to have. Including the key means `read` meets the lock and refuses, the client
/// resolves it and asks again, and the row appears (or does not, if the transaction was rolled
/// back) for a reason rather than by luck.
fn user_keys_in(
    db: &Db,
    snapshot: &EngineSnapshot<'_>,
    start: &[u8],
    end: &[u8],
    reverse: bool,
) -> Result<Vec<Vec<u8>>, ProtoError> {
    let low = key::prefix(start);
    let high = if end.is_empty() {
        namespace_end()
    } else {
        key::prefix(end)
    };
    // Enough to fill any limit the caller could have asked for, and a bound so a scan of a
    // region with millions of keys cannot be made to collect them all.
    let ceiling = MAX_SCAN_LIMIT as usize;
    // A set rather than a run of adjacent duplicates: the two column families are walked
    // separately and a key can be in both. Memcomparable order is user-key order, so what
    // comes out is still sorted.
    let mut keys: BTreeSet<Vec<u8>> = BTreeSet::new();

    let mut versions = db
        .iter(cf::WRITE, &snapshot.options)
        .map_err(|error| engine_to_proto(&error))?;
    versions.seek(&low);
    while versions.valid() && versions.key() < high.as_slice() && keys.len() < ceiling {
        let (user_key, _) = key::split(versions.key()).map_err(txn_to_proto)?;
        // **Past the whole key, not on to its next version** (#58). Every remaining entry under
        // this prefix is an older version of a key already taken, and the answer is a *set of
        // keys* — so stepping them cost `O(keys × versions)` and produced nothing. The set
        // deduped the answer and hid the work: the rows never grew and the steps grew with every
        // commit in the range's history.
        let (_, past_this_key) = key::version_range(&user_key);
        keys.insert(user_key);
        versions.seek(&past_this_key);
    }
    versions.status().map_err(|error| engine_to_proto(&error))?;

    // The same snapshot, so the two halves are one view and not two.
    let mut locks = db
        .iter(cf::LOCK, &snapshot.options)
        .map_err(|error| engine_to_proto(&error))?;
    locks.seek(&low);
    while locks.valid() && locks.key() < high.as_slice() && keys.len() < ceiling {
        keys.insert(key::split_lock(locks.key()).map_err(txn_to_proto)?);
        locks.next();
    }
    locks.status().map_err(|error| engine_to_proto(&error))?;

    let mut keys: Vec<Vec<u8>> = keys.into_iter().collect();
    if reverse {
        keys.reverse();
    }
    Ok(keys)
}

/// One past every key in the `'x'` namespace.
fn namespace_end() -> Vec<u8> {
    vec![esker_keys::prefix::TXN + 1]
}

/// Decides one `Prewrite` and stages what it decided (`docs/txn-spec.md` §5.2).
///
/// Answers one status per mutation, positionally, because a batch can collide with several
/// locks and one refusal cannot describe many keys
/// ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md) decision 1). The batch is still one
/// decision: **if any key is refused, nothing is staged**.
pub fn prewrite(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    primary: &Bytes,
    ttl_ms: u64,
    mutations: &[TxnMutation],
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut statuses = Vec::with_capacity(mutations.len());
    let mut staged = Mutations::new();
    let mut refused = false;

    for mutation in mutations {
        // **A range is validated rather than locked**, because there is no key to hold: a phantom
        // is a row that does not exist yet (ADR 0067 §3). Anything committed inside the range since
        // this transaction's snapshot refuses the whole prewrite, which is what makes the read set
        // mean something; what it cannot do is stop an insert that lands *after* this check, and
        // ADR 0062 declares that window.
        if let TxnMutation::CheckRange { start, end } = mutation {
            let winner = snapshot
                .newest_write_in_range(start, end, start_ts)
                .map_err(txn_to_proto)?;
            match winner {
                Some(version) => {
                    refused = true;
                    // The same status a key-level conflict answers, carrying the commit that won —
                    // a client reads them the same way and the caller learns which moment beat it.
                    statuses.push(TxnStatus::Conflict {
                        commit_ts: version.commit_ts,
                    });
                }
                // **A commit is a verdict; a lock is a question** ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §1).
                // Asked in this order because a commit inside the range is decided — this
                // transaction has lost, whatever anyone else is holding — and the lock below is
                // not: its owner may still roll back, in which case nothing was ever in the range
                // and refusing over it would be a `40001` for a phantom that never existed. So a
                // lock is answered the way a key-level check answers one, and the client does with
                // it what it already does: settle the holder, then ask again.
                None => match snapshot
                    .foreign_lock_in_range(start, end, start_ts)
                    .map_err(txn_to_proto)?
                {
                    Some((user_key, lock)) => {
                        refused = true;
                        statuses.push(TxnStatus::Locked(lock_info(&user_key, &lock)));
                    }
                    None => statuses.push(TxnStatus::Ok),
                },
            }
            continue;
        }
        let request = Prewrite {
            key: mutation.key().clone(),
            primary: primary.clone(),
            start_ts,
            // **The snapshot the value was computed from**, and the transaction's own when the
            // mutation does not say (ADR 0057 §4).
            read_ts: match mutation {
                TxnMutation::Put { read_ts, .. } | TxnMutation::Delete { read_ts, .. } => {
                    read_ts.unwrap_or(start_ts)
                }
                // A check validates against the transaction's own snapshot: what it asserts is
                // that the key it *read* has not moved since.
                TxnMutation::Check { .. } | TxnMutation::CheckRange { .. } => start_ts,
            },
            ttl_ms,
            op: match mutation {
                TxnMutation::Put { value, .. } => Op::Put(value.clone()),
                TxnMutation::Delete { .. } => Op::Delete,
                TxnMutation::Check { .. } | TxnMutation::CheckRange { .. } => Op::Check,
            },
        };
        let status = match esker_txn::check_prewrite(&snapshot, &request).map_err(txn_to_proto)? {
            PrewriteDecision::Lock(mutations) => {
                staged.extend(mutations);
                TxnStatus::Ok
            }
            // Our own lock is already there: an earlier attempt got through and its answer was
            // lost. Nothing to stage, and success is the honest answer.
            PrewriteDecision::AlreadyLocked => TxnStatus::Ok,
            PrewriteDecision::Locked(lock) => TxnStatus::Locked(lock_info(mutation.key(), &lock)),
            PrewriteDecision::Conflict { commit_ts } => TxnStatus::Conflict { commit_ts },
            PrewriteDecision::RolledBack => TxnStatus::RolledBack,
        };
        refused |= !status.is_ok();
        statuses.push(status);
    }

    // One decision for the batch. A partially staged prewrite would lock some keys for a
    // transaction that has already been told it lost, and nothing would ever clean them up
    // except the TTL.
    if !refused {
        stage(db, batch, &staged)?;
    }
    Ok(TxnKvResp::Prewrite { keys: statuses })
}

/// Commits keys this transaction prewrote (`docs/txn-spec.md` §5.3).
///
/// The **primary must be first in `keys`** when it is present, and `esker-txn`'s token is what
/// enforces it: `commit_secondary` cannot be called without the evidence `commit_primary`
/// produces. A batch of secondaries alone — the client's cleanup phase — carries the evidence
/// implicitly, because the client only sends it after the primary's own batch was applied.
pub fn commit(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    commit_ts: u64,
    keys: &[Bytes],
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut staged = Mutations::new();

    for user_key in keys {
        // Each key is committed on its own evidence: the lock names the primary, and a lock
        // whose primary is itself is the commit point. `esker-txn` refuses a secondary through
        // `commit_primary`, so the two are told apart by the record rather than by position.
        let is_primary = match snapshot.get_lock(user_key).map_err(txn_to_proto)? {
            Some(lock) => lock.is_primary(user_key),
            // No lock: the `write` column family decides, and either answer is the same for a
            // primary and a secondary.
            None => true,
        };
        let outcome = if is_primary {
            esker_txn::commit_primary(&snapshot, user_key, start_ts, commit_ts).map(|decision| {
                match decision {
                    esker_txn::CommitDecision::Commit(plan) => plan.into_mutations(),
                    esker_txn::CommitDecision::AlreadyCommitted(_) => Mutations::new(),
                }
            })
        } else {
            secondary(&snapshot, user_key, start_ts, commit_ts)
        };
        match outcome {
            Ok(mutations) => staged.extend(mutations),
            Err(error) => {
                return Ok(TxnKvResp::Commit {
                    status: status_of(&error),
                });
            }
        }
    }
    stage(db, batch, &staged)?;
    Ok(TxnKvResp::Commit {
        status: TxnStatus::Ok,
    })
}

/// Commits one secondary, against the primary's commit timestamp.
///
/// The token is asserted rather than witnessed, because **the primary may be in another
/// region on another store** and this one cannot read the record that would witness it. What
/// makes the assertion true is the client: its commit path cannot send a secondary commit until
/// the primary's own batch was applied, and `esker-txn`'s token is what enforces that ordering
/// on the side that *can* hold both (`docs/plans/phase-5.md` §10.2).
fn secondary(
    snapshot: &EngineSnapshot<'_>,
    user_key: &Bytes,
    start_ts: u64,
    commit_ts: u64,
) -> esker_txn::Result<Mutations> {
    let primary = snapshot
        .get_lock(user_key)?
        .map_or_else(|| user_key.clone(), |lock| lock.primary);
    let token = esker_txn::PrimaryCommitted::on_trust(primary, start_ts, commit_ts);
    esker_txn::commit_secondary(snapshot, user_key, &token)
}

/// Rolls keys back, leaving a marker on each (`docs/txn-spec.md` §5.4).
pub fn rollback(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    keys: &[Bytes],
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut staged = Mutations::new();
    for user_key in keys {
        match esker_txn::rollback(&snapshot, user_key, start_ts) {
            Ok(mutations) => staged.extend(mutations),
            Err(error) => {
                return Ok(TxnKvResp::Rollback {
                    status: status_of(&error),
                });
            }
        }
    }
    stage(db, batch, &staged)?;
    Ok(TxnKvResp::Rollback {
        status: TxnStatus::Ok,
    })
}

/// Gives **this** transaction's own locks on `keys` back, leaving it running
/// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2).
///
/// `ROLLBACK TO SAVEPOINT`, and the deadlock victim inside one: a real server releases a
/// subtransaction's row locks when it aborts and keeps the transaction alive. Until this existed
/// the node released the node-local half and nothing reached the store, so a `SELECT … FOR UPDATE`
/// taken inside a savepoint held its row until the whole transaction ended — and the survivor of
/// a deadlock met that lock at its own commit and was told `40001`.
///
/// **Not [`rollback`]**, which leaves a marker and so kills the transaction on those keys for
/// ever; a savepoint's victim very often writes the row it locked once its `rescue` is done.
/// Nothing is written down here: the lock record goes and the key is as it was.
///
/// The count is how many locks were actually this transaction's. A key held by somebody else, or
/// no longer held at all, is left alone and not counted — the caller wanted the keys free of *its*
/// lock and they are, which is the same reading [`resolve_lock`]'s count gets.
pub fn release_lock(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    keys: &[Bytes],
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut staged = Mutations::new();
    let mut released = 0u64;

    for user_key in keys {
        let (mutations, gave_back) =
            esker_txn::release(&snapshot, user_key, start_ts).map_err(txn_to_proto)?;
        if gave_back {
            staged.extend(mutations);
            released += 1;
        }
    }

    stage(db, batch, &staged)?;
    Ok(TxnKvResp::ReleaseLock { released })
}

/// Applies a verdict about someone else's transaction to the keys of it that live here
/// (`docs/txn-spec.md` §5.5).
///
/// **This store does not classify the transaction, and must not try.** A transaction's primary
/// may live in another region on another store, so a `primary_state` read here would answer
/// `Missing` for a transaction that is perfectly alive elsewhere — and roll back a committed
/// one. The classification is the *client's*, made against the primary's own region, and it
/// arrives as `commit_ts`: above zero, commit these keys there; zero, roll them back.
///
/// The client gets that verdict atomically rather than by inspection: it sends a `Rollback` of
/// the stuck transaction's **primary**, which either leaves a marker (so the transaction is now
/// dead) or answers `Committed { commit_ts }` (so it was already alive). There is no window
/// between looking and deciding, which is what would let a resolver undo a commit.
///
/// Whether the lease had expired is also the client's judgement, made from `LockInfo` and its
/// own oracle timestamp, because nothing here may read a clock.
pub fn resolve_lock(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    commit_ts: u64,
    keys: &[Bytes],
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let mut staged = Mutations::new();
    let mut resolved = 0u64;

    for user_key in keys {
        let held = snapshot
            .get_lock(user_key)
            .map_err(txn_to_proto)?
            .is_some_and(|lock| lock.start_ts == start_ts);
        if !held {
            // Someone else resolved it already, or it was never locked here. The caller wanted
            // the lock gone and it is gone, which is a success and not a race lost — and
            // counting it as resolved would overstate what this call did.
            continue;
        }
        let outcome = if commit_ts == 0 {
            esker_txn::rollback(&snapshot, user_key, start_ts)
        } else {
            let primary = snapshot
                .get_lock(user_key)
                .map_err(txn_to_proto)?
                .map_or_else(|| user_key.clone(), |lock| lock.primary);
            let token = esker_txn::PrimaryCommitted::on_trust(primary, start_ts, commit_ts);
            esker_txn::commit_secondary(&snapshot, user_key, &token)
        };
        // One key that cannot be resolved does not stop the rest: the caller asked about a set,
        // and a key already settled the other way is information rather than a failure of the
        // call. The count is what says how much was done.
        if let Ok(mutations) = outcome {
            staged.extend(mutations);
            resolved += 1;
        }
    }
    stage(db, batch, &staged)?;
    Ok(TxnKvResp::ResolveLock { resolved })
}

/// Extends a live transaction's lock TTL.
///
/// Never *shortens* one: a heartbeat that arrived out of order behind a longer one would
/// otherwise pull the lease in under a resolver that has already read the longer value.
pub fn heartbeat(
    db: &Db,
    batch: &mut WriteBatch,
    start_ts: u64,
    primary: &Bytes,
    ttl_ms: u64,
) -> Result<TxnKvResp, ProtoError> {
    let snapshot = EngineSnapshot::new(db);
    let Some(mut lock) = snapshot
        .get_lock(primary)
        .map_err(txn_to_proto)?
        .filter(|lock| lock.start_ts == start_ts)
    else {
        // No lock of ours to extend. Not an error: the transaction may have committed while
        // the heartbeat was in flight, and the answer a client acts on is the TTL it now has,
        // which is none.
        return Ok(TxnKvResp::Heartbeat { ttl_ms: 0 });
    };
    if ttl_ms > lock.ttl_ms {
        lock.ttl_ms = ttl_ms;
        let mut staged = Mutations::new();
        staged.put_lock_record(primary, &lock);
        stage(db, batch, &staged)?;
    }
    Ok(TxnKvResp::Heartbeat {
        ttl_ms: lock.ttl_ms,
    })
}

/// The `LockInfo` a refusal carries: the key, and what a resolver needs to settle it.
fn lock_info(user_key: &[u8], lock: &LockRecord) -> LockInfo {
    LockInfo {
        key: Bytes::copy_from_slice(user_key),
        primary: lock.primary.clone(),
        start_ts: lock.start_ts,
        ttl_ms: lock.ttl_ms,
    }
}

/// The response status a protocol refusal from `esker-txn` belongs in.
///
/// One-to-one by construction ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md) decision 1),
/// which is what makes this a `match` and not a translation with judgement in it.
///
/// The four that map are the transaction's *fate*. Corruption, a missing value and caller
/// misuse are not a fate and have no status — they reach the caller through the error channel,
/// from the `?` on every read above this, so nothing here has to invent a status for them.
fn status_of(error: &TxnError) -> TxnStatus {
    match error {
        TxnError::WriteConflict { commit_ts, .. } => TxnStatus::Conflict {
            commit_ts: *commit_ts,
        },
        TxnError::AlreadyRolledBack { .. } => TxnStatus::RolledBack,
        TxnError::AlreadyCommitted { commit_ts, .. } => TxnStatus::Committed {
            commit_ts: *commit_ts,
        },
        // `LockNotFound` is the honest answer for the four that map *and* for the rest: a
        // transaction whose fate cannot be read is one whose lock this store cannot account
        // for. Corruption, a missing value and caller misuse also reach the caller through the
        // error channel, from the `?` on every read above this — this arm is what the response
        // says when one of them is caught after a decision was already made for another key.
        TxnError::TxnLockNotFound { .. }
        | TxnError::Corrupt { .. }
        | TxnError::KeyIsLocked { .. }
        | TxnError::MissingValue { .. }
        | TxnError::Misuse(_) => TxnStatus::LockNotFound,
    }
}

fn txn_to_proto(error: TxnError) -> ProtoError {
    match error {
        TxnError::Corrupt { what, detail } => ProtoError::corrupt(what, detail),
        TxnError::KeyIsLocked { lock_info, .. } => ProtoError::Locked { lock_info },
        TxnError::MissingValue { start_ts } => ProtoError::corrupt(
            "default cf",
            format!("the value written at {start_ts} is not there"),
        ),
        other => ProtoError::internal(other.to_string()),
    }
}

fn storage(error: &esker_engine::Error) -> TxnError {
    TxnError::corrupt("engine", error.to_string())
}
