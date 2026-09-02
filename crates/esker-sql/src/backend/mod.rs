//! The seam between the executor and storage, and an in-memory transaction to test against.
//!
//! [`Backend`] and [`Txn`] are the whole of what the executor may ask of the store. They are shaped
//! against `esker-client`'s real `TxnClient` and `Transaction` rather than against a sketch, so
//! wiring the real one in is a matter of writing the impl and nothing above it changes:
//!
//! * `put` and `delete` return nothing, because writes are buffered on the client until commit —
//!   the executor must not be written as though a write can fail where it is issued;
//! * `commit` yields the commit timestamp, or `None` when the transaction wrote nothing;
//! * `get` and `scan` take `&self`, because read-your-writes is served out of the buffer.
//!
//! [`StoreBackend`] is the real one, over `esker-client`'s `TxnClient`; [`MemoryBackend`] below is
//! the in-process fake every unit test runs against.
//!
//! # How a unique index is enforced, with nothing added here to do it
//!
//! There is deliberately no `put_if_absent`. Uniqueness composes out of the two primitives above,
//! and between them they cover both ways a duplicate can arrive:
//!
//! 1. **The executor reads the index key inside the transaction and requires it absent.** The read
//!    is at the transaction's snapshot, so a duplicate that is *already committed* is visible and
//!    is reported as `23505 unique_violation` before anything is written.
//! 2. **Then it writes the index entry like any other key.** A *concurrent* duplicate needs no
//!    further help: both transactions read the key as absent, both prewrite the same key, and
//!    write-write conflict detection (`docs/DESIGN.md` §8) lets exactly one commit. The loser's
//!    `commit` fails, and the executor reports that as `23505` too.
//!
//! The fake below implements the same conflict rule as the real protocol — a commit fails if any
//! key it wrote gained a version after this transaction's snapshot — so an executor test can
//! exercise the race rather than assume it. `a_concurrent_duplicate_loses_at_commit` is that test.

mod store;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::error::{Result, SqlError};

pub use store::{SchemaLease, StoreBackend};

/// Opens transactions. One per SQL node, shared by every session.
pub trait Backend: fmt::Debug + Send + Sync {
    /// Starts a transaction at a fresh snapshot.
    fn begin(&self) -> Result<Box<dyn Txn>>;

    /// Starts a **read-only** transaction at a snapshot the caller chose.
    ///
    /// The whole of ADR 0021's Decision 1: a historical read is a read timestamp and nothing else,
    /// so this is `begin` with the number handed in rather than allocated. Locks, resolution,
    /// read-your-writes and commit are all indifferent to where it came from.
    ///
    /// **Read-only is enforced by the transaction this returns**, not left to the caller.
    /// Committing at `commit_ts > start_ts` against a snapshot that old is a lost update with
    /// extra steps, and it is the one window snapshot isolation does not close: the conflicting
    /// writer committed *after* the snapshot and *before* the write, so Percolator's conflict
    /// check would not catch it.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>>;

    /// How long this node may still serve **writes** from a cached schema, or `None` when it has
    /// no lease at all.
    ///
    /// [ADR 0028](../../../docs/adr/0028-the-schema-lease.md). `None` is **fail closed**: a node
    /// that cannot reach PD holds no lease and refuses to write, which is what lets PD's step
    /// clock advance on a timer rather than on a poll of nodes it may not be able to reach.
    ///
    /// Reads are never gated by it. A reader's snapshot already agrees with the rows it can see
    /// (ADR 0020), so gating reads would add stalls and close no hole — and it would take a node
    /// that has lost PD from *degraded* to *useless*, which is the wrong trade for a bound that
    /// only writers can violate.
    ///
    /// The default is an unexpired lease of unbounded length, which is what an in-process fake
    /// with no cluster to lose contact with means. `StoreBackend` overrides it.
    fn schema_lease_remaining(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::MAX)
    }

    /// The step interval PD publishes, or `None` when nothing publishes one.
    ///
    /// The number a driver waits between the state transitions of a schema change, and the extra
    /// a *removing* change waits before its last step ([`StepInterval`]). It comes from the same
    /// PD answer the lease does, for the same reason: a cluster-wide bound needs one writer, and
    /// a node that kept its own copy would drift — and an interval short by exactly the drift is
    /// unsafe rather than merely wrong (`docs/plans/phase-6e.md` §10).
    ///
    /// `None` is not a default interval, it is the absence of one, and
    /// [`crate::exec::redrive::ReDriver`] will not step a job without it. A node that cannot be
    /// told how long to wait must not guess: guessing short breaks the two-version invariant the
    /// interval exists for. The in-process fake answers `None` because it has no PD, which is
    /// also why re-driving is off by default in tests.
    fn schema_step_interval(&self) -> Option<StepInterval> {
        None
    }

    /// The oracle's current timestamp.
    ///
    /// `CLAUDE.md` invariant 6: no node uses its wall clock for ordering, so "now" is a number from
    /// the timestamp oracle like every other. This is what bounds a historical read from above —
    /// a read at a timestamp that has not happened would see a prefix of it and call it complete.
    fn now(&self) -> Result<u64>;
}

/// How long a driver waits between the steps of a schema change.
///
/// PD's `SchemaLease` answer, the two fields of it a driver needs. Mirrors
/// `esker_pd::SchemaLease` rather than sharing it: `esker-sql` does not depend on `esker-pd`, and
/// the numbers arrive over the wire in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInterval {
    /// `lease_ms + lock_ttl_ms` — the wait after a state transition, in either direction.
    ///
    /// Each term bounds how stale a *writer* can be: the lease is how long a node may act without
    /// hearing from PD, the lock TTL how long a transaction that has already started may still
    /// commit. Past their sum no writer can be acting on a state two behind, which is ADR 0020's
    /// two-version invariant.
    pub step_ms: u64,
    /// What a **removing** change waits on top of `step_ms`, and only before its final step: the
    /// MVCC retention window.
    ///
    /// Inert for an add, real for a remove — a reader still at `public` reads entries a node at
    /// `absent` has already deleted, and retention is what keeps them readable. Folding it into
    /// `step_ms` would price every `CREATE INDEX` at the retention window
    /// (`crate::exec::verbs`, "why the last step is the expensive one").
    pub removal_extra_ms: u64,
}

/// One transaction's view of storage.
pub trait Txn: fmt::Debug + Send {
    /// Reads one key at this transaction's snapshot, its own buffered writes merged in.
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;

    /// Reads `[start, end)` at this transaction's snapshot, in key order, buffered writes merged
    /// in. `limit` is applied after the merge; 0 means no limit.
    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>>;

    /// Buffers a write. Nothing can fail here — the conflict, if there is one, comes from
    /// [`Txn::commit`].
    fn put(&mut self, key: &[u8], value: &[u8]);

    /// Buffers a delete, with the same rule.
    fn delete(&mut self, key: &[u8]);

    /// The snapshot this transaction reads at.
    ///
    /// What `pg_export_snapshot()` hands out, and it must be **this** transaction's rather than a
    /// fresh one: PostgreSQL's verb exports what the exporting transaction sees, so that a second
    /// session importing the token reads exactly the state the first one was reading. Allocating a
    /// new timestamp would export a moment nobody had looked at.
    fn start_ts(&self) -> u64;

    /// Whether this transaction has already buffered a write.
    ///
    /// **The one rule in ADR 0022 Decision 2 that is about correctness rather than cost**: a
    /// transaction that has written and then reads cannot be answered from a columnar learner at
    /// all, because the learner has not seen an uncommitted write. Read-your-writes is served out
    /// of the buffer this asks about, and a fragment goes to a different machine, which has none
    /// of it.
    ///
    /// **No default**, for the reason [`Txn::is_read_only`] has none, and it is the same lesson: a
    /// default that is right for the in-memory fake and silently wrong for a real cluster is
    /// exactly the shape of a bug that passes every test in this crate and returns a wrong answer
    /// against a store.
    fn has_written(&self) -> bool;

    /// Whether this transaction may write.
    ///
    /// False for one opened by [`Backend::begin_at`]. The executor asks *before* it plans, so that
    /// a write at a past snapshot is `25006` naming the command rather than a write that is
    /// buffered and then quietly dropped. [`Txn::put`] and [`Txn::delete`] cannot report anything —
    /// they are buffered and return nothing — which is exactly why the check has to be here.
    ///
    /// **No default**, deliberately. It had one — `false` — and `StoreBackend` inherited it, so the
    /// executor's `25006` fired against the fake and not against a real cluster: there the write
    /// reached the store, was buffered, and failed at commit under a different code. A default that
    /// is right for one implementor and silently wrong for the other is the shape of that bug, so
    /// there is none.
    fn is_read_only(&self) -> bool;

    /// Commits, yielding the commit timestamp, or `None` for a transaction that wrote nothing.
    fn commit(self: Box<Self>) -> Result<Option<u64>>;

    /// Abandons the transaction. Buffered writes are discarded and nothing is visible.
    fn rollback(self: Box<Self>) -> Result<()>;
}

/// A buffered write.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Write {
    Put(Bytes),
    Delete,
}

/// Every committed version of every key, newest last, plus the clock that stamps them.
#[derive(Debug)]
struct Versions {
    /// `key -> [(commit_ts, value)]`, ascending by timestamp. `None` is a tombstone.
    keys: BTreeMap<Vec<u8>, Vec<(u64, Option<Bytes>)>>,
    /// Stands in for the timestamp oracle. Monotone, and the only source of timestamps here, which
    /// is `CLAUDE.md` invariant 6 kept true even in a fake.
    ///
    /// **Shaped like a real one**: `ts = physical_ms << 18 | logical` (`esker_pd::tso`), starting
    /// at a plausible instant rather than at zero, and advancing the *logical* half per commit.
    /// The shape is not decoration — it is what a time machine is asked about. A counter starting
    /// at zero puts every version inside the same millisecond of 1970, so an instant a user could
    /// name would not distinguish two of them, and a test written against it would prove the
    /// feature works on data no cluster produces. Tests that want two versions in different
    /// milliseconds ask for that with [`MemoryBackend::advance_ms`].
    clock: u64,
}

impl Default for Versions {
    fn default() -> Self {
        Versions {
            keys: BTreeMap::new(),
            clock: esker_client::ts_at_ms(FAKE_START_MS),
        }
    }
}

/// Where a [`MemoryBackend`]'s clock starts: 2026-08-30 14:00:00 UTC, in Unix milliseconds.
///
/// Checked against the value rather than asserted in a comment: `a_plausible_instant` below is
/// the test, because a constant whose comment says one date and whose bits say another is a
/// trap for whoever reads the next failing assertion.
///
/// Any plausible instant would do. What matters is that it is not zero, so that the physical half
/// of every timestamp the fake hands out is a real date a test can write down.
const FAKE_START_MS: u64 = 1_788_098_400_000;

impl Versions {
    /// The value visible at `ts`: the newest version committed at or before it.
    fn visible(&self, key: &[u8], ts: u64) -> Option<Bytes> {
        self.keys
            .get(key)?
            .iter()
            .rev()
            .find(|(commit_ts, _)| *commit_ts <= ts)
            .and_then(|(_, value)| value.clone())
    }

    /// Whether `key` gained a version after `ts` — the write-write conflict Percolator's prewrite
    /// detects by checking the `write` column family for a commit newer than the snapshot.
    fn written_since(&self, key: &[u8], ts: u64) -> bool {
        self.keys
            .get(key)
            .is_some_and(|versions| versions.iter().any(|(commit_ts, _)| *commit_ts > ts))
    }
}

/// An in-memory transactional store: snapshot reads, buffered writes, and the one conflict rule
/// that matters.
///
/// Good enough to test an executor against, and honest about the thing an executor can get wrong —
/// it really does refuse a commit whose keys moved underneath it, so a test can watch two
/// transactions race for the same unique index entry and see one of them lose.
#[derive(Debug, Clone, Default)]
pub struct MemoryBackend {
    versions: Arc<Mutex<Versions>>,
    /// The largest page [`Txn::scan`] will answer with, and what a `limit` of 0 becomes. Zero
    /// means no ceiling, which is [`Txn`]'s own contract. See [`MemoryBackend::with_scan_limit`].
    max_scan: u32,
}

impl MemoryBackend {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        MemoryBackend::default()
    }

    /// Caps every scan at `max` pairs, and reads a `limit` of 0 as `max` rather than as "no
    /// limit".
    ///
    /// **This is the real client's behaviour, not a fault injected for fun.**
    /// `esker_client::Transaction::scan` puts every limit through
    /// `Router::bounded_limit(limit, DEFAULT_SCAN_LIMIT)`, which turns 0 into 1024 and then caps
    /// it at `max_scan_limit`. A fake that answered 0 as "everything" let code that asked for a
    /// whole range look correct here and truncate silently against the real backend — a `DROP
    /// TABLE` that left rows, and a `CREATE INDEX` whose index made a query return *fewer* rows
    /// than the same query without it (`docs/plans/phase-6a.md` §10a).
    ///
    /// So this exists to make that difference *visible to a test*: set it to two and any code
    /// that does not page comes back with two rows. Everything that walks a range goes through
    /// the executor's `for_each_page`, and the tests that pin it set this.
    #[must_use]
    pub fn with_scan_limit(mut self, max: u32) -> Self {
        self.max_scan = max;
        self
    }

    /// Moves the clock's **physical** half on by `millis`, the way time passing does.
    ///
    /// A commit advances the logical half only, so without this every version a test writes lands
    /// in one millisecond — which is what a busy cluster looks like, and which is why the default
    /// is that way. A test about reading *as of an instant* needs its versions in different
    /// milliseconds, because an instant a user can name has millisecond resolution
    /// (`crate::time_machine`), and this is how it says so.
    pub fn advance_ms(&self, millis: u64) {
        let mut versions = self.lock();
        versions.clock = versions
            .clock
            .saturating_add(millis << esker_client::TSO_LOGICAL_BITS);
    }

    /// The value visible at the newest committed timestamp, for assertions in tests.
    #[must_use]
    pub fn peek(&self, key: &[u8]) -> Option<Bytes> {
        let versions = self.lock();
        versions.visible(key, versions.clock)
    }

    /// A poisoned lock means another thread panicked while holding it. The data behind it is a
    /// `BTreeMap` that is still structurally sound, and taking it back is better than propagating
    /// a panic into a session (`CLAUDE.md` invariant 9).
    fn lock(&self) -> std::sync::MutexGuard<'_, Versions> {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Backend for MemoryBackend {
    fn begin(&self) -> Result<Box<dyn Txn>> {
        let start_ts = self.lock().clock;
        Ok(Box::new(MemoryTxn {
            versions: Arc::clone(&self.versions),
            start_ts,
            buffer: BTreeMap::new(),
            max_scan: self.max_scan,
            read_only: false,
        }))
    }

    /// The fake is a real time machine, which is the point of it.
    ///
    /// Every version here is already filed under its `commit_ts` and `Versions::visible` already
    /// answers "the newest at or before `ts`", so a historical read is the same call with a
    /// different number — the same sentence that is true of the store below. That is what lets
    /// the whole feature, and the whole `.slt` corpus, be exercised before the client half lands.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>> {
        Ok(Box::new(MemoryTxn {
            versions: Arc::clone(&self.versions),
            start_ts,
            buffer: BTreeMap::new(),
            max_scan: self.max_scan,
            read_only: true,
        }))
    }

    fn now(&self) -> Result<u64> {
        Ok(self.lock().clock)
    }
}

/// What a scan will actually answer with, given what was asked and the store's ceiling.
///
/// Shaped after `esker_client::Router::bounded_limit`: with a ceiling, a `limit` of 0 means the
/// ceiling rather than "everything", and anything above it is capped. With no ceiling — the
/// default — [`Txn`]'s own contract applies and 0 is unlimited.
fn bounded_limit(limit: u32, max: u32) -> u32 {
    if max == 0 {
        return limit;
    }
    if limit == 0 { max } else { limit.min(max) }
}

/// One transaction against a [`MemoryBackend`].
#[derive(Debug)]
struct MemoryTxn {
    versions: Arc<Mutex<Versions>>,
    start_ts: u64,
    buffer: BTreeMap<Vec<u8>, Write>,
    /// The store's scan ceiling; see [`MemoryBackend::with_scan_limit`].
    max_scan: u32,
    /// Set by [`Backend::begin_at`]. A write here is dropped rather than buffered, and the
    /// executor is what turns the attempt into `25006` before it gets this far.
    read_only: bool,
}

impl MemoryTxn {
    fn lock(&self) -> std::sync::MutexGuard<'_, Versions> {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Txn for MemoryTxn {
    fn start_ts(&self) -> u64 {
        self.start_ts
    }

    fn has_written(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        // Read-your-writes: the buffer wins, and a buffered delete hides a committed value.
        if let Some(write) = self.buffer.get(key) {
            return Ok(match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            });
        }
        Ok(self.lock().visible(key, self.start_ts))
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        let versions = self.lock();
        let mut merged: BTreeMap<Vec<u8>, Bytes> = BTreeMap::new();
        for key in versions.keys.keys() {
            if key.as_slice() >= start && key.as_slice() < end {
                if let Some(value) = versions.visible(key, self.start_ts) {
                    merged.insert(key.clone(), value);
                }
            }
        }
        drop(versions);
        // The buffer is applied over the snapshot, so a row this transaction wrote is in its own
        // range scan and one it deleted is not.
        for (key, write) in &self.buffer {
            if key.as_slice() >= start && key.as_slice() < end {
                match write {
                    Write::Put(value) => {
                        merged.insert(key.clone(), value.clone());
                    }
                    Write::Delete => {
                        merged.remove(key);
                    }
                }
            }
        }
        let rows = merged.into_iter().map(|(k, v)| (Bytes::from(k), v));
        // The limit is applied after the merge, or a buffered row could displace a committed one
        // and the scan would return fewer rows than it should.
        let limit = bounded_limit(limit, self.max_scan);
        Ok(if limit == 0 {
            rows.collect()
        } else {
            rows.take(limit as usize).collect()
        })
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        if self.read_only {
            return;
        }
        self.buffer
            .insert(key.to_vec(), Write::Put(Bytes::copy_from_slice(value)));
    }

    fn delete(&mut self, key: &[u8]) {
        if self.read_only {
            return;
        }
        self.buffer.insert(key.to_vec(), Write::Delete);
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn commit(self: Box<Self>) -> Result<Option<u64>> {
        if self.buffer.is_empty() {
            // Nothing was written, so nothing needs a timestamp.
            return Ok(None);
        }
        let mut versions = self.lock();

        // Prewrite, in one step because there is no network here: every key this transaction wrote
        // must be untouched since its snapshot. This is the check that makes a unique index work
        // without a dedicated method — two inserts of the same index key both reach here, and the
        // second one finds the first one's version.
        for key in self.buffer.keys() {
            if versions.written_since(key, self.start_ts) {
                return Err(SqlError::SerializationFailure {
                    message: format!(
                        "key {} was written after this transaction's snapshot",
                        String::from_utf8_lossy(key)
                    ),
                    // The fake answers per key like a real `Prewrite` does, so the executor's
                    // translation of a lost race into a `23505` is exercised here the same way it
                    // will be against the store rather than only there.
                    key: Some(key.clone()),
                });
            }
        }

        versions.clock += 1;
        let commit_ts = versions.clock;
        for (key, write) in &self.buffer {
            let value = match write {
                Write::Put(value) => Some(value.clone()),
                Write::Delete => None,
            };
            versions
                .keys
                .entry(key.clone())
                .or_default()
                .push((commit_ts, value));
        }
        Ok(Some(commit_ts))
    }

    fn rollback(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Backend, MemoryBackend, Txn};
    use crate::error::SqlError;
    use crate::sqlstate;

    /// The composition the executor performs for a unique index: read, require absent, then write.
    /// Returns the error the executor would report.
    fn insert_unique(txn: &mut Box<dyn Txn>, index_key: &[u8], row: &[u8]) -> crate::Result<()> {
        if txn.get(index_key)?.is_some() {
            return Err(SqlError::UniqueViolation {
                constraint: "g_b_key".into(),
                key: None,
            });
        }
        txn.put(index_key, row);
        Ok(())
    }

    #[test]
    fn a_committed_value_is_visible_to_the_next_transaction_and_not_to_an_older_one() {
        let backend = MemoryBackend::new();
        let before = backend.now().unwrap();
        let older = backend.begin().unwrap();

        let mut writer = backend.begin().unwrap();
        writer.put(b"k", b"v");
        // A commit takes the next timestamp, which is the one after the snapshot every open
        // transaction holds. Written as a *relation* to `before` rather than as a literal,
        // because the clock starts at a plausible instant rather than at zero.
        assert_eq!(writer.commit().unwrap(), Some(before + 1));

        // The transaction that started first still sees its own snapshot.
        assert_eq!(older.get(b"k").unwrap(), None);
        let newer = backend.begin().unwrap();
        assert_eq!(newer.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn a_transaction_reads_its_own_writes_and_its_own_deletes() {
        let backend = MemoryBackend::new();
        let mut setup = backend.begin().unwrap();
        setup.put(b"a", b"1");
        setup.commit().unwrap();

        let mut txn = backend.begin().unwrap();
        txn.put(b"b", b"2");
        txn.delete(b"a");
        assert_eq!(txn.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(txn.get(b"a").unwrap(), None, "a buffered delete hides it");

        let rows = txn.scan(b"a", b"z", 0).unwrap();
        assert_eq!(rows.len(), 1, "the scan sees the buffer too");
        assert_eq!(rows[0].0.as_ref(), b"b");
    }

    #[test]
    fn a_rolled_back_transaction_leaves_nothing_behind() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        txn.put(b"k", b"v");
        txn.rollback().unwrap();
        assert_eq!(backend.peek(b"k"), None);
    }

    #[test]
    fn a_transaction_that_wrote_nothing_needs_no_timestamp() {
        let backend = MemoryBackend::new();
        let txn = backend.begin().unwrap();
        assert_eq!(txn.commit().unwrap(), None);
    }

    /// The first of the two ways a duplicate arrives: it is already committed, so the in-transaction
    /// read finds it and the executor refuses before writing anything.
    #[test]
    fn a_committed_duplicate_is_caught_by_the_read() {
        let backend = MemoryBackend::new();
        let mut first = backend.begin().unwrap();
        insert_unique(&mut first, b"i/alice", b"row1").unwrap();
        first.commit().unwrap();

        let mut second = backend.begin().unwrap();
        let error = insert_unique(&mut second, b"i/alice", b"row2").unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::UNIQUE_VIOLATION);
    }

    /// The second way, and the one no read can catch: both transactions look, both see nothing,
    /// both write. Exactly one commits, and the loser's failure is what the executor turns into
    /// `23505`. This is why there is no `put_if_absent` — the conflict check already is one.
    #[test]
    fn a_concurrent_duplicate_loses_at_commit() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();

        // Both read the index key at their own snapshot; neither sees anything.
        assert_eq!(left.get(b"i/alice").unwrap(), None);
        assert_eq!(right.get(b"i/alice").unwrap(), None);
        insert_unique(&mut left, b"i/alice", b"left").unwrap();
        insert_unique(&mut right, b"i/alice", b"right").unwrap();

        assert!(left.commit().is_ok(), "the first to commit wins");
        let loser = right.commit().expect_err(
            "the second must lose the write-write conflict, or the index is not unique",
        );
        assert_eq!(
            loser.sqlstate(),
            sqlstate::SERIALIZATION_FAILURE,
            "a lost race is 40001 here; the executor is what turns it into 23505 for an index key"
        );
        assert_eq!(backend.peek(b"i/alice").as_deref(), Some(&b"left"[..]));
    }

    /// A conflict is about the keys a transaction *wrote*, not the ones it read past. Two
    /// transactions touching different keys must both commit, or every concurrent insert would
    /// fail and the fake would be useless for testing anything else.
    #[test]
    fn transactions_that_write_different_keys_both_commit() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();
        left.put(b"a", b"1");
        right.put(b"b", b"2");
        assert!(left.commit().is_ok());
        assert!(right.commit().is_ok(), "different keys do not conflict");
    }

    /// A scan must not see a version committed after its snapshot, or a query would return rows
    /// that did not exist when it started.
    #[test]
    fn a_scan_reads_only_its_own_snapshot() {
        let backend = MemoryBackend::new();
        let mut setup = backend.begin().unwrap();
        setup.put(b"k1", b"a");
        setup.commit().unwrap();

        let reader = backend.begin().unwrap();
        let mut writer = backend.begin().unwrap();
        writer.put(b"k2", b"b");
        writer.commit().unwrap();

        let rows = reader.scan(b"k", b"l", 0).unwrap();
        assert_eq!(rows.len(), 1, "k2 was committed after the reader started");
    }

    #[test]
    fn a_scan_respects_its_range_and_limit() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        for key in [&b"a"[..], b"b", b"c", b"d"] {
            txn.put(key, b"v");
        }
        txn.commit().unwrap();

        let reader = backend.begin().unwrap();
        assert_eq!(
            reader.scan(b"b", b"d", 0).unwrap().len(),
            2,
            "end is exclusive"
        );
        assert_eq!(
            reader.scan(b"a", b"z", 2).unwrap().len(),
            2,
            "limit applies"
        );
        assert_eq!(reader.scan(b"a", b"z", 0).unwrap().len(), 4);
    }
}
