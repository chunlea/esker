//! What a statement spends its time on, counted at the [`Backend`] seam — debt #109.
//!
//! # Why the seam is here
//!
//! `esker-sql` is written against the [`Backend`] and [`Txn`] traits rather than against
//! `esker-client`'s concrete types, so a decorator placed between the two counts every call the
//! executor makes **without a line of product code changing**. The method boundaries are the
//! phases: a uniqueness probe is a [`Txn::get`], a row or index write is a [`Txn::put`], and every
//! network round trip a statement pays for is inside [`Txn::commit`].
//!
//! # What it can see, and what it cannot
//!
//! * **`put` does not write.** It buffers into the client's write set and returns nothing, so the
//!   microseconds counted against it are memory, not I/O. A report that shows the put phase near
//!   zero is not a broken instrument; it is the reason `commit` is the only phase that can be
//!   large.
//! * **Round trips are not counted *here*, and that is this module's gap rather than the
//!   client's.** An earlier version of this paragraph said `esker-client` keeps no such counter.
//!   It does: `esker_client::stmt_stats::Cost` carries `round_trips`, `regions`, `tso`,
//!   `prewrites`, `commits`, `keys` and `waited`, fed by `record_call` on every wire call, and
//!   `esker-sql`'s own `stmt_stats::Guard` already folds them up per statement. This decorator
//!   times the `Txn` seam and reads none of it. The claim was written after grepping
//!   `esker-client/src/txn.rs` and finding nothing — in a crate whose counter lives in
//!   `stmt_stats.rs`.
//! * **The columnar tee and region routing are below this seam**, inside the store's apply path.
//!   They are not zero here — they are invisible here, which a report must say rather than round
//!   down.
//!
//! # Two traps, both of which this module is written to fall into loudly rather than quietly
//!
//! 1. **[`Backend`] has two methods with default bodies** — `schema_lease_remaining` answers
//!    `Some(Duration::MAX)` and `schema_step_interval` answers `None`. A wrapper that forgets them
//!    compiles, and silently tells the node it holds an eternal schema lease. Both are forwarded
//!    below, and that is the whole reason they appear. ([`Txn`] has no defaulted method, so the
//!    compiler enforces the rest — the lesson ADR 0105 was written about.)
//! 2. **`begin_at` is a second door.** `tests/redrive.rs` wraps `begin` and leaves `begin_at`
//!    forwarding a bare transaction, which is harmless there and would be a wrong answer here: a
//!    transaction opened at an explicit snapshot would be invisible to the counters, and the
//!    report would say the probe reads cost nothing because it never saw them. Both doors are
//!    wrapped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_keys::prefix::{TablePart, split_table};
use esker_sql::backend::{Backend, Buffered, Lock, LockView, Reach, ReadSet, StepInterval, Txn};

/// Microseconds, saturating rather than wrapping.
///
/// `as u64` would be a cast lint under the gate's `-D warnings`, and a silently truncated
/// duration is a wrong number rather than a slow one.
fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

/// Running totals for one cluster, shared by every session on it.
///
/// Always installed — there is no unmeasured build of this harness. A switch that takes the
/// instrument out also takes out a code path, and then the measured cluster is not the cluster
/// the rest of the file tests.
#[derive(Debug, Default)]
pub struct Profile {
    row_get_us: AtomicU64,
    row_get_n: AtomicU64,
    index_get_us: AtomicU64,
    index_get_n: AtomicU64,
    /// Catalog and counters: every key that is not a row or an index entry of a table.
    other_get_us: AtomicU64,
    other_get_n: AtomicU64,
    /// [`Txn::get_without_waiting`], which is its own phase because it is its own door: the
    /// catalog's two version-counter reads go through it and through nothing else
    /// (`catalog/mod.rs`'s `view_at`), so a profile that times only `get` cannot see them at all.
    unwaited_get_us: AtomicU64,
    unwaited_get_n: AtomicU64,
    scan_us: AtomicU64,
    scan_n: AtomicU64,
    put_us: AtomicU64,
    row_put_n: AtomicU64,
    index_put_n: AtomicU64,
    other_put_n: AtomicU64,
    commit_us: AtomicU64,
    commit_n: AtomicU64,
    rollback_n: AtomicU64,
    begin_n: AtomicU64,
}

/// One reading of a [`Profile`], and the difference between two of them.
///
/// A statement's cost is `read()` before and after it, subtracted — which is how a per-`INSERT`
/// median is taken without the instrument having to know what a statement is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub row_get_us: u64,
    pub row_get_n: u64,
    pub index_get_us: u64,
    pub index_get_n: u64,
    pub other_get_us: u64,
    pub other_get_n: u64,
    pub unwaited_get_us: u64,
    pub unwaited_get_n: u64,
    pub scan_us: u64,
    pub scan_n: u64,
    pub put_us: u64,
    pub row_put_n: u64,
    pub index_put_n: u64,
    pub other_put_n: u64,
    pub commit_us: u64,
    pub commit_n: u64,
    pub rollback_n: u64,
    pub begin_n: u64,
}

impl Counts {
    /// This reading minus an earlier one. Saturating: a counter never goes backwards, so an
    /// underflow would be a bug in the caller's ordering rather than a negative cost.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            row_get_us: self.row_get_us.saturating_sub(earlier.row_get_us),
            row_get_n: self.row_get_n.saturating_sub(earlier.row_get_n),
            index_get_us: self.index_get_us.saturating_sub(earlier.index_get_us),
            index_get_n: self.index_get_n.saturating_sub(earlier.index_get_n),
            other_get_us: self.other_get_us.saturating_sub(earlier.other_get_us),
            other_get_n: self.other_get_n.saturating_sub(earlier.other_get_n),
            unwaited_get_us: self.unwaited_get_us.saturating_sub(earlier.unwaited_get_us),
            unwaited_get_n: self.unwaited_get_n.saturating_sub(earlier.unwaited_get_n),
            scan_us: self.scan_us.saturating_sub(earlier.scan_us),
            scan_n: self.scan_n.saturating_sub(earlier.scan_n),
            put_us: self.put_us.saturating_sub(earlier.put_us),
            row_put_n: self.row_put_n.saturating_sub(earlier.row_put_n),
            index_put_n: self.index_put_n.saturating_sub(earlier.index_put_n),
            other_put_n: self.other_put_n.saturating_sub(earlier.other_put_n),
            commit_us: self.commit_us.saturating_sub(earlier.commit_us),
            commit_n: self.commit_n.saturating_sub(earlier.commit_n),
            rollback_n: self.rollback_n.saturating_sub(earlier.rollback_n),
            begin_n: self.begin_n.saturating_sub(earlier.begin_n),
        }
    }
}

impl Profile {
    /// Every counter, read at one moment.
    ///
    /// Not an atomic snapshot of all of them together, and it does not need to be: the reading is
    /// taken between statements on the one session that is driving the fill.
    #[must_use]
    pub fn read(&self) -> Counts {
        Counts {
            row_get_us: self.row_get_us.load(Ordering::Relaxed),
            row_get_n: self.row_get_n.load(Ordering::Relaxed),
            index_get_us: self.index_get_us.load(Ordering::Relaxed),
            index_get_n: self.index_get_n.load(Ordering::Relaxed),
            other_get_us: self.other_get_us.load(Ordering::Relaxed),
            other_get_n: self.other_get_n.load(Ordering::Relaxed),
            unwaited_get_us: self.unwaited_get_us.load(Ordering::Relaxed),
            unwaited_get_n: self.unwaited_get_n.load(Ordering::Relaxed),
            scan_us: self.scan_us.load(Ordering::Relaxed),
            scan_n: self.scan_n.load(Ordering::Relaxed),
            put_us: self.put_us.load(Ordering::Relaxed),
            row_put_n: self.row_put_n.load(Ordering::Relaxed),
            index_put_n: self.index_put_n.load(Ordering::Relaxed),
            other_put_n: self.other_put_n.load(Ordering::Relaxed),
            commit_us: self.commit_us.load(Ordering::Relaxed),
            commit_n: self.commit_n.load(Ordering::Relaxed),
            rollback_n: self.rollback_n.load(Ordering::Relaxed),
            begin_n: self.begin_n.load(Ordering::Relaxed),
        }
    }

    /// Which phase a key belongs to.
    ///
    /// `esker_keys::prefix::split_table` answers `None` — not an error — for a key outside the SQL
    /// namespace, which is what the catalog counters and metadata keys are. So a key this harness
    /// has never seen is counted as *other* rather than crashing a measurement run.
    fn part_of(key: &[u8]) -> Option<TablePart> {
        match split_table(key) {
            Ok(Some((_, _, part))) => Some(part),
            Ok(None) | Err(_) => None,
        }
    }

    fn record_get(&self, key: &[u8], elapsed: Duration) {
        let us = micros(elapsed);
        let (time, count) = match Self::part_of(key) {
            Some(TablePart::Row) => (&self.row_get_us, &self.row_get_n),
            Some(TablePart::Index) => (&self.index_get_us, &self.index_get_n),
            None => (&self.other_get_us, &self.other_get_n),
        };
        time.fetch_add(us, Ordering::Relaxed);
        count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_put(&self, key: &[u8], elapsed: Duration) {
        self.put_us.fetch_add(micros(elapsed), Ordering::Relaxed);
        let count = match Self::part_of(key) {
            Some(TablePart::Row) => &self.row_put_n,
            Some(TablePart::Index) => &self.index_put_n,
            None => &self.other_put_n,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// A [`Backend`] that counts what the executor asks of it.
#[derive(Debug)]
pub struct ProfiledBackend {
    inner: Arc<dyn Backend>,
    profile: Arc<Profile>,
}

impl ProfiledBackend {
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>, profile: Arc<Profile>) -> Self {
        Self { inner, profile }
    }
}

impl Backend for ProfiledBackend {
    fn begin(&self) -> esker_sql::Result<Box<dyn Txn>> {
        self.profile.begin_n.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ProfiledTxn {
            inner: self.inner.begin()?,
            profile: Arc::clone(&self.profile),
        }))
    }

    /// **Wrapped, exactly like `begin`.** See the module doc's second trap: a transaction opened at
    /// an explicit snapshot that returned unwrapped would spend its reads outside the counters.
    fn begin_at(&self, start_ts: u64) -> esker_sql::Result<Box<dyn Txn>> {
        self.profile.begin_n.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ProfiledTxn {
            inner: self.inner.begin_at(start_ts)?,
            profile: Arc::clone(&self.profile),
        }))
    }

    fn now(&self) -> esker_sql::Result<u64> {
        self.inner.now()
    }

    /// Forwarded **because it has a default body**: inheriting it would tell the node it holds a
    /// schema lease for `Duration::MAX`, and nothing would fail to compile.
    fn schema_lease_remaining(&self) -> Option<Duration> {
        self.inner.schema_lease_remaining()
    }

    /// Forwarded for the same reason: the default is `None`, which is "this node has no placement
    /// driver" — true of the in-process fake and false of the cluster this harness starts.
    fn schema_step_interval(&self) -> Option<StepInterval> {
        self.inner.schema_step_interval()
    }
}

/// A transaction that times the four calls a write statement spends itself on, and forwards the
/// other twenty-two untouched.
#[derive(Debug)]
struct ProfiledTxn {
    inner: Box<dyn Txn>,
    profile: Arc<Profile>,
}

impl Txn for ProfiledTxn {
    // The four that are measured.

    fn get(&self, key: &[u8]) -> esker_sql::Result<Option<Bytes>> {
        let began = Instant::now();
        let answer = self.inner.get(key);
        self.profile.record_get(key, began.elapsed());
        answer
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> esker_sql::Result<Vec<(Bytes, Bytes)>> {
        let began = Instant::now();
        let answer = self.inner.scan(start, end, limit);
        self.profile
            .scan_us
            .fetch_add(micros(began.elapsed()), Ordering::Relaxed);
        self.profile.scan_n.fetch_add(1, Ordering::Relaxed);
        answer
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        let began = Instant::now();
        self.inner.put(key, value);
        self.profile.record_put(key, began.elapsed());
    }

    /// The only phase that pays for the network: prewrite and commit both happen inside.
    fn commit(self: Box<Self>) -> esker_sql::Result<Option<u64>> {
        let Self { inner, profile } = *self;
        let began = Instant::now();
        let answer = inner.commit();
        profile
            .commit_us
            .fetch_add(micros(began.elapsed()), Ordering::Relaxed);
        profile.commit_n.fetch_add(1, Ordering::Relaxed);
        answer
    }

    // Everything else, forwarded. `Txn` has no defaulted method, so this list is complete or the
    // crate does not build — which is the property that makes this wrapper safe to write.

    fn rollback(self: Box<Self>) -> esker_sql::Result<()> {
        let Self { inner, profile } = *self;
        profile.rollback_n.fetch_add(1, Ordering::Relaxed);
        inner.rollback()
    }

    fn lock(&mut self, key: &[u8], reach: Reach) -> esker_sql::Result<Lock> {
        self.inner.lock(key, reach)
    }

    fn owned_by_session(&mut self, pid: u32) {
        self.inner.owned_by_session(pid);
    }

    fn locks(&self) -> LockView {
        self.inner.locks()
    }

    /// **Measured, and the reason is a bug this profile already had.** The first version of this
    /// wrapper timed the four calls it expected to matter and forwarded the other twenty-two
    /// untouched — and the catalog's two version-counter reads go through *this* door, not through
    /// `get`, so they were invisible. The compiler made the wrapper complete; it could not make it
    /// completely timed.
    fn get_without_waiting(&self, key: &[u8]) -> esker_sql::Result<Option<Bytes>> {
        let began = Instant::now();
        let answer = self.inner.get_without_waiting(key);
        self.profile
            .unwaited_get_us
            .fetch_add(micros(began.elapsed()), Ordering::Relaxed);
        self.profile.unwaited_get_n.fetch_add(1, Ordering::Relaxed);
        answer
    }

    fn validate_reads(&mut self, on: bool) {
        self.inner.validate_reads(on);
    }

    fn changed_since_statement(&self, key: &[u8]) -> esker_sql::Result<bool> {
        self.inner.changed_since_statement(key)
    }

    fn restart_statement(&mut self) -> esker_sql::Result<()> {
        self.inner.restart_statement()
    }

    fn begin_statement(&mut self) -> esker_sql::Result<()> {
        self.inner.begin_statement()
    }

    fn abandon_locks(&mut self) {
        self.inner.abandon_locks();
    }

    fn delete(&mut self, key: &[u8]) {
        self.inner.delete(key);
    }

    fn buffered(&self, key: &[u8]) -> Buffered {
        self.inner.buffered(key)
    }

    fn restore(&mut self, key: &[u8], prior: Buffered) {
        self.inner.restore(key, prior);
    }

    fn stop_waiting(&mut self) {
        self.inner.stop_waiting();
    }

    fn holds(&self, key: &[u8]) -> bool {
        self.inner.holds(key)
    }

    fn unlock(&mut self, key: &[u8]) {
        self.inner.unlock(key);
    }

    fn read_set(&self) -> ReadSet {
        self.inner.read_set()
    }

    fn restore_read_set(&mut self, set: ReadSet) {
        self.inner.restore_read_set(set);
    }

    fn has_read(&self, key: &[u8]) -> bool {
        self.inner.has_read(key)
    }

    fn start_ts(&self) -> u64 {
        self.inner.start_ts()
    }

    fn has_written(&self) -> bool {
        self.inner.has_written()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}
