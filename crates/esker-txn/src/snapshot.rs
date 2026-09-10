//! The questions the Percolator rules ask of stored state, and an in-memory answer to
//! them for tests.
//!
//! Everything in [`crate::percolator`] is a pure function of a [`TxnSnapshot`], which is what
//! lets the whole protocol matrix be unit-tested without a store, an engine or a socket. The
//! real implementation is `esker-store`'s, over an engine snapshot pinned for the duration of
//! one request; this trait is the seam between them.
//!
//! # Why these
//!
//! They are the smallest set the rules of `docs/txn-spec.md` §5 can be written against, and
//! each is a different shape of engine access. The first five are the protocol's; the two range
//! scans are the read set's, and each looks in the column family the other cannot see:
//!
//! | Question | Engine | Asked by |
//! |---|---|---|
//! | [`get_lock`](TxnSnapshot::get_lock) | point get, `lock` CF | every operation |
//! | [`seek_write`](TxnSnapshot::seek_write) | seek, `write` CF | reads |
//! | [`newest_write_after`](TxnSnapshot::newest_write_after) | seek, `write` CF | prewrite's conflict check |
//! | [`write_of_txn`](TxnSnapshot::write_of_txn) | bounded scan, `write` CF | classifying a transaction's own fate |
//! | [`get_value`](TxnSnapshot::get_value) | point get, `default` CF | reads |
//! | [`newest_write_in_range`](TxnSnapshot::newest_write_in_range) | range scan, `write` CF | a read set's range check |
//! | [`foreign_lock_in_range`](TxnSnapshot::foreign_lock_in_range) | range scan, `lock` CF | the same check, against a phantom still in flight |
//!
//! [`write_of_txn`](TxnSnapshot::write_of_txn) is the only scan, and it is bounded: a
//! transaction's own record sits at some `commit_ts >= start_ts`, so the walk runs from the
//! newest version down to `start_ts` and stops. `TiKV`'s resolver does the same walk for the
//! same reason — there is no index from `start_ts` to `commit_ts`, and adding one would be a
//! second thing to keep consistent with the first.

use bytes::Bytes;

use crate::codec::{LockRecord, WriteRecord};
use crate::error::Result;

/// A `write` record together with the commit timestamp it is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The key it is filed under. A [`crate::Kind::Rollback`] marker's is its `start_ts`.
    pub commit_ts: u64,
    /// What the transaction did.
    pub record: WriteRecord,
}

impl Version {
    /// A version, for tests and for implementations building one.
    #[must_use]
    pub fn new(commit_ts: u64, record: WriteRecord) -> Self {
        Self { commit_ts, record }
    }
}

/// Read access to one region's transactional state, at one instant.
///
/// Every method is fallible because the real implementation reads an engine, and an engine
/// read can fail on a checksum. A failure is not an answer: a rule that treated an I/O error
/// as "no lock" would prewrite over a live transaction.
pub trait TxnSnapshot {
    /// The lock on `user_key`, if any.
    fn get_lock(&self, user_key: &[u8]) -> Result<Option<LockRecord>>;

    /// The newest `write` record with `commit_ts <= ts`.
    ///
    /// One forward seek to `key::seek_write(user_key, ts)` plus a prefix check. **The bound is
    /// inclusive**: a version committed at exactly `ts` is visible at `ts`.
    fn seek_write(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>>;

    /// The newest **committed** write with `commit_ts > ts`, if there is one.
    ///
    /// Prewrite's conflict check: any answer at all means a transaction committed after our
    /// snapshot, and first-committer-wins says we lose.
    ///
    /// **A rollback marker is stepped past, not answered with**
    /// ([ADR 0078](../../../docs/adr/0078-a-marker-is-not-a-commit.md)). A marker lives at
    /// `commit_ts == start_ts`, so a transaction that took this key after our snapshot and then
    /// died leaves a record above it that an implementation reading only the timestamp cannot tell
    /// from a commit — and the refusal it produces names a `commit_ts` at which nothing committed.
    ///
    /// **A `Kind::Lock` record is stepped past too**, and it was not until
    /// [ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md) made one reachable. The rule
    /// this check exists for is first-committer-wins: *somebody wrote a version of this key after
    /// my snapshot, so the value I computed is stale*. A lock record is the transaction saying it
    /// held the key and **wrote nothing to it** — no value moved, so nothing a later writer holds
    /// is stale against it, and refusing that writer is a `40001` for a race nobody ran.
    ///
    /// It was written the other way when `Kind::Lock` was reserved and unreachable, and the
    /// measurement that changed it is in ADR 0088: with a `FOR UPDATE` leaving one of these on
    /// every locked row, the old rule made every completed locking read refuse the next write of
    /// that row by any older transaction — and it told a deadlock victim it had lost a race
    /// rather than that it had been killed.
    fn newest_write_after(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>>;

    /// **Anything committed inside `[start, end)` after `ts`** — the phantom test
    /// ([ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)).
    ///
    /// A key-level check names keys that *existed* when a transaction read them; a row inserted
    /// afterwards is in nobody's read set, and only the range it would have appeared in can name it.
    /// Any answer at all refuses the prewrite.
    ///
    /// The default answers `None`, which is right for a snapshot with no range access and honest
    /// rather than silently weaker: a store that cannot scan a range cannot claim a range is
    /// unchanged, and every implementation in this workspace overrides it.
    fn newest_write_in_range(&self, start: &[u8], end: &[u8], ts: u64) -> Result<Option<Version>> {
        let _ = (start, end, ts);
        Ok(None)
    }

    /// **A lock inside `[start, end)` that belongs to somebody other than `mine`** — the half of
    /// the phantom test that [`newest_write_in_range`](TxnSnapshot::newest_write_in_range) cannot
    /// see ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md)
    /// §1).
    ///
    /// A commit inside a read range is a phantom that already happened; a **lock** inside one is a
    /// phantom in flight, and the range check that looks only at the `write` CF walks straight
    /// past it. Two transactions that both scan a range and both insert into it therefore both
    /// prewrite — different keys, so nothing collides — and both commit, which is the write skew
    /// PostgreSQL's SSI refuses.
    ///
    /// **`mine` is excluded, and it has to be.** The client prewrites its own keys *before* it
    /// sends the range checks, so by the time this is asked the checking transaction's own new row
    /// is locked inside the very range it read. Reporting that would refuse every transaction that
    /// writes into a range it scanned — which is all of them.
    ///
    /// Answers the key as well as the record: the lock is not on the range's bound, and a client
    /// that has to resolve it needs the key it actually sits on.
    ///
    /// The default answers `None` for the same reason the write scan's does — a snapshot with no
    /// range access cannot claim a range is unlocked — and every implementation here overrides it.
    fn foreign_lock_in_range(
        &self,
        start: &[u8],
        end: &[u8],
        mine: u64,
    ) -> Result<Option<(Vec<u8>, LockRecord)>> {
        let _ = (start, end, mine);
        Ok(None)
    }

    /// The record the transaction at `start_ts` left on this key — a commit, or the rollback
    /// marker at `commit_ts == start_ts`.
    ///
    /// There is at most one: a transaction commits or rolls back a key, never both. This is
    /// how a resolver classifies a primary and how a commit tells "already done" from
    /// "someone killed us".
    fn write_of_txn(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Version>>;

    /// The `default` CF entry written by the transaction at `start_ts`.
    ///
    /// Only consulted for a `Put` whose record has no inline value; absence in that case is
    /// corruption, not an empty value, and the caller reports it as such.
    fn get_value(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Bytes>>;
}

impl<T: TxnSnapshot + ?Sized> TxnSnapshot for &T {
    fn get_lock(&self, user_key: &[u8]) -> Result<Option<LockRecord>> {
        (**self).get_lock(user_key)
    }
    fn seek_write(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>> {
        (**self).seek_write(user_key, ts)
    }
    fn newest_write_in_range(&self, start: &[u8], end: &[u8], ts: u64) -> Result<Option<Version>> {
        (**self).newest_write_in_range(start, end, ts)
    }
    fn foreign_lock_in_range(
        &self,
        start: &[u8],
        end: &[u8],
        mine: u64,
    ) -> Result<Option<(Vec<u8>, LockRecord)>> {
        (**self).foreign_lock_in_range(start, end, mine)
    }
    fn newest_write_after(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>> {
        (**self).newest_write_after(user_key, ts)
    }
    fn write_of_txn(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Version>> {
        (**self).write_of_txn(user_key, start_ts)
    }
    fn get_value(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Bytes>> {
        (**self).get_value(user_key, start_ts)
    }
}

#[cfg(any(test, feature = "testing"))]
pub use memory::MemoryStore;

#[cfg(any(test, feature = "testing"))]
mod memory {
    use std::collections::BTreeMap;

    use bytes::Bytes;

    use super::{TxnSnapshot, Version};
    use crate::codec::{Kind, LockRecord, WriteRecord};
    use crate::error::Result;
    use crate::mutation::{Cf, Mutation, Mutations};

    /// The three column families as three `BTreeMap`s, keyed by the same engine keys the real
    /// store uses.
    ///
    /// Keyed by the **encoded** key rather than by `(user_key, ts)` on purpose: the ordering
    /// this store iterates in is then the ordering the engine iterates in, so a seek that is
    /// wrong here is wrong there. A map keyed by the decoded pair would sort by ascending
    /// timestamp and hide every mistake in `enc_ts`'s direction.
    #[derive(Debug, Default, Clone)]
    pub struct MemoryStore {
        lock: BTreeMap<Vec<u8>, Vec<u8>>,
        write: BTreeMap<Vec<u8>, Vec<u8>>,
        default: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    impl MemoryStore {
        /// An empty store.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Applies a decision's mutations, as the store would apply one `WriteBatch`.
        pub fn apply(&mut self, mutations: &Mutations) {
            for mutation in mutations {
                match mutation {
                    Mutation::Put { cf, key, value } => {
                        self.cf_mut(*cf).insert(key.clone(), value.clone());
                    }
                    Mutation::Delete { cf, key } => {
                        self.cf_mut(*cf).remove(key);
                    }
                }
            }
        }

        /// How many entries one column family holds — for tests that assert a lock was
        /// removed rather than merely overwritten.
        #[must_use]
        pub fn len(&self, cf: Cf) -> usize {
            self.cf(cf).len()
        }

        /// Whether a column family is empty.
        #[must_use]
        pub fn is_empty(&self, cf: Cf) -> bool {
            self.cf(cf).is_empty()
        }

        /// Puts a raw byte string into a column family, for tests that need a record no
        /// encoder would produce.
        pub fn put_raw(&mut self, cf: Cf, key: Vec<u8>, value: Vec<u8>) {
            self.cf_mut(cf).insert(key, value);
        }

        fn cf(&self, cf: Cf) -> &BTreeMap<Vec<u8>, Vec<u8>> {
            match cf {
                Cf::Lock => &self.lock,
                Cf::Write => &self.write,
                Cf::Default => &self.default,
            }
        }

        fn cf_mut(&mut self, cf: Cf) -> &mut BTreeMap<Vec<u8>, Vec<u8>> {
            match cf {
                Cf::Lock => &mut self.lock,
                Cf::Write => &mut self.write,
                Cf::Default => &mut self.default,
            }
        }

        /// Walks one key's `write` versions newest-first, as a forward engine iterator would.
        fn versions<'a>(&'a self, user_key: &[u8]) -> impl Iterator<Item = Result<Version>> + 'a {
            let (start, end) = crate::key::version_range(user_key);
            self.write.range(start..end).map(|(key, value)| {
                let (_, commit_ts) = crate::key::split(key)?;
                Ok(Version::new(commit_ts, WriteRecord::decode(value)?))
            })
        }
    }

    impl TxnSnapshot for MemoryStore {
        fn get_lock(&self, user_key: &[u8]) -> Result<Option<LockRecord>> {
            self.lock
                .get(&crate::key::lock(user_key))
                .map(|bytes| LockRecord::decode(bytes))
                .transpose()
        }

        fn foreign_lock_in_range(
            &self,
            start: &[u8],
            end: &[u8],
            mine: u64,
        ) -> Result<Option<(Vec<u8>, LockRecord)>> {
            if start >= end {
                return Ok(None);
            }
            // A lock key is `'x' ++ enc(user_key)` with no timestamp suffix, so the CF is ordered
            // by the same encoded user key the bounds are built from and the range is the
            // half-open one the caller asked for.
            for (key, value) in self
                .lock
                .range(crate::key::lock(start)..crate::key::lock(end))
            {
                let record = LockRecord::decode(value)?;
                if record.start_ts != mine {
                    return Ok(Some((crate::key::split_lock(key)?, record)));
                }
            }
            Ok(None)
        }

        fn seek_write(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>> {
            // A forward range from the seek key: the first entry is the newest version at or
            // below `ts`, which is the whole reason `enc_ts` is complemented.
            let (_, end) = crate::key::version_range(user_key);
            let start = crate::key::seek_write(user_key, ts);
            match self.write.range(start..end).next() {
                None => Ok(None),
                Some((key, value)) => {
                    let (_, commit_ts) = crate::key::split(key)?;
                    Ok(Some(Version::new(commit_ts, WriteRecord::decode(value)?)))
                }
            }
        }

        fn newest_write_after(&self, user_key: &[u8], ts: u64) -> Result<Option<Version>> {
            // Newest first, so the walk ends at the first record at or below `ts`: nothing
            // under it can be above `ts` either. Rollback markers are stepped past on the way
            // down, and so is a `Lock` record — it changed no value, so no later writer is stale
            // against it (ADR 0088).
            for version in self.versions(user_key) {
                let version = version?;
                if version.commit_ts <= ts {
                    return Ok(None);
                }
                if !matches!(version.record.kind, Kind::Rollback | Kind::Lock) {
                    return Ok(Some(version));
                }
            }
            Ok(None)
        }

        fn write_of_txn(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Version>> {
            for version in self.versions(user_key) {
                let version = version?;
                // The walk is bounded: our record is at some commit_ts >= start_ts, so once
                // the versions drop below start_ts there is nothing left to find.
                if version.commit_ts < start_ts {
                    return Ok(None);
                }
                if version.record.start_ts == start_ts {
                    return Ok(Some(version));
                }
            }
            Ok(None)
        }

        fn get_value(&self, user_key: &[u8], start_ts: u64) -> Result<Option<Bytes>> {
            Ok(self
                .default
                .get(&crate::key::value(user_key, start_ts))
                .map(|value| Bytes::copy_from_slice(value)))
        }
    }
}
