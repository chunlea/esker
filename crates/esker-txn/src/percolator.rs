//! The protocol, as pure functions (`docs/txn-spec.md` §5).
//!
//! Nothing in this module performs I/O. Each function reads a [`TxnSnapshot`], decides, and
//! returns a [`Mutations`] list for the caller to apply as one atomic batch. That is what
//! makes the whole decision matrix testable against an in-memory fake, and it is what makes
//! the `esker-store` handler that will call these a thin shim: `decode → call → write batch
//! through Raft`.
//!
//! # The three rules that are easy to get wrong
//!
//! **Prewrite checks both column families.** `write` for a commit newer than our snapshot,
//! *and* `lock` for anyone else's lock. Dropping the first breaks first-committer-wins and
//! loses updates; dropping the second lets two live transactions hold one key. Each has its
//! own case in the matrix, alone as well as together.
//!
//! **The primary is the commit point.** A transaction is committed exactly when its primary's
//! `write` record exists, so the primary must be durable before any secondary's is. A
//! secondary committed first is a committed value belonging to a transaction that nothing can
//! yet call committed — and if the client dies in that window, [`resolve`] classifies it
//! *wrongly*: the primary is still locked, its TTL runs out, and a resolver rolls the whole
//! transaction back around a value that is already visible. So [`commit_secondary`] takes a
//! [`PrimaryCommitted`] token, and the only way to get one is to consume the
//! [`PrimaryCommit`] plan that [`commit_primary`] returns — which a caller does after the
//! primary's batch is durable.
//!
//! **A rollback leaves a marker even when there was no lock.** A `Prewrite` whose answer was
//! lost may still arrive after the client gave up; without the marker it would lock a key on
//! behalf of a transaction that has already reported failure.

use bytes::Bytes;

use crate::codec::{Kind, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};
use crate::error::{Result, TxnError};
use crate::mutation::Mutations;
use crate::snapshot::{TxnSnapshot, Version};

/// What a transaction wants to do to one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Write these bytes.
    Put(Bytes),
    /// Remove the key.
    Delete,
    /// **Verify and hold, without writing** — what a SERIALIZABLE transaction does to a key it
    /// only *read*
    /// ([ADR 0062](../../../docs/adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md),
    /// [ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)).
    ///
    /// It takes the same lock a write takes and stages no value, so the conflict check above it is
    /// the *validation* — a key committed since this transaction's snapshot loses — and the lock is
    /// what stops one landing between the check and the commit. It commits as [`Kind::Lock`], the
    /// record kind this format has always reserved for a key held but not written.
    Check,
}

impl Op {
    /// The record kind this becomes.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Put(_) => Kind::Put,
            Self::Delete => Kind::Delete,
            Self::Check => Kind::Lock,
        }
    }

    /// The value, when there is one and it is short enough to inline in the record rather than
    /// spend a `default` CF entry on (`docs/DESIGN.md` §8).
    fn short_value(&self) -> Option<Bytes> {
        match self {
            Self::Put(value) if value.len() <= SHORT_VALUE_MAX_LEN => Some(value.clone()),
            _ => None,
        }
    }

    /// The value, when it is too long to inline and so needs its own `default` entry.
    fn long_value(&self) -> Option<&[u8]> {
        match self {
            Self::Put(value) if value.len() > SHORT_VALUE_MAX_LEN => Some(value),
            _ => None,
        }
    }
}

/// One key's worth of a prewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prewrite {
    /// The user key being written.
    pub key: Bytes,
    /// The user key of this transaction's primary. Equal to `key` on the primary itself.
    pub primary: Bytes,
    /// The transaction's snapshot.
    pub start_ts: u64,
    /// **The snapshot the value being written was computed from**, which is `start_ts` unless the
    /// statement that produced it was re-run after waiting for another transaction
    /// ([ADR 0057](../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)
    /// §4).
    ///
    /// First-committer-wins is measured against *this*, per key, and the difference is the whole
    /// of what makes a READ COMMITTED waiter able to commit. A waiter that waits for another
    /// transaction, re-reads at a fresh timestamp and computes the right answer would otherwise
    /// meet that transaction's commit at its own prewrite: its `commit_ts` is above the waiter's
    /// `start_ts` by construction, so the waiter would fail `40001` at `COMMIT` — the failure
    /// moved from the `UPDATE` and nothing else changed.
    ///
    /// A key written by an **earlier** statement keeps its own, older stamp, so a commit that
    /// slipped in between still conflicts. That is why it is per key and not a transaction-wide
    /// "latest read timestamp", and it is the half that stops this being a licence to lose an
    /// update. Nothing else moves: the data version is still written at `start_ts` and the lock
    /// record still carries `start_ts`, so waiters classify this transaction exactly as before.
    pub read_ts: u64,
    /// How long the lock should live without a heartbeat.
    pub ttl_ms: u64,
    /// What to write.
    pub op: Op,
}

impl Prewrite {
    /// A prewrite with the default TTL.
    #[must_use]
    pub fn new(key: Bytes, primary: Bytes, start_ts: u64, op: Op) -> Self {
        Self {
            key,
            primary,
            start_ts,
            // A statement that never waited computed its value from the transaction's own
            // snapshot, which is what every prewrite said before ADR 0057 and what every one that
            // does not say otherwise still means.
            read_ts: start_ts,
            ttl_ms: crate::LOCK_TTL_MS,
            op,
        }
    }

    /// The same prewrite, saying which snapshot its value was computed from.
    #[must_use]
    pub fn read_at(mut self, read_ts: u64) -> Self {
        self.read_ts = read_ts;
        self
    }

    /// Whether this key is its own transaction's primary.
    #[must_use]
    pub fn is_primary(&self) -> bool {
        self.key == self.primary
    }
}

/// What `Prewrite` should do with one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewriteDecision {
    /// Apply these, and the key is locked by this transaction.
    Lock(Mutations),
    /// Our own lock is already here — an earlier attempt got through and its answer was lost.
    /// Apply nothing and report success. This is what makes `Prewrite` idempotent, and it is
    /// why an ambiguous prewrite is recoverable rather than fatal.
    AlreadyLocked,
    /// Someone else holds the key. The caller resolves the lock and retries
    /// (`docs/txn-spec.md` §5.5); it is not an error to report upwards.
    Locked(LockRecord),
    /// A transaction committed after our snapshot. First-committer-wins: this one aborts, and
    /// only a fresh `start_ts` can help.
    Conflict {
        /// When the winner committed.
        commit_ts: u64,
    },
    /// This transaction was already rolled back — its lock expired and someone cleaned it up.
    /// Resurrecting it would commit a transaction a reader has already been told is dead.
    RolledBack,
}

/// What a read at a timestamp found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The value at the read timestamp.
    Value(Bytes),
    /// The key does not exist at the read timestamp — never written, or deleted.
    NotFound,
    /// A lock is in the way. Resolve it and read again.
    Locked(LockRecord),
}

/// What the `write` column family says happened to one transaction, as seen on its primary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryState {
    /// It committed. Every key of it should be rolled forward to this timestamp.
    Committed {
        /// The transaction's commit timestamp.
        commit_ts: u64,
    },
    /// It was rolled back. Every key of it should be rolled back.
    RolledBack,
    /// Its lock is still there and it has not decided yet.
    Locked(LockRecord),
    /// No lock and no record. The transaction never got as far as its primary, or something
    /// removed the lock without leaving a marker.
    Missing,
}

/// What to do with a lock, given the state of its primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The transaction committed: commit this key at the same timestamp.
    RollForward {
        /// The transaction's commit timestamp.
        commit_ts: u64,
    },
    /// The transaction is dead: roll this key back.
    RollBack,
    /// The owner is still alive and inside its lease. Back off and look again.
    Wait,
}

/// Evidence that a primary's `write` record is durable.
///
/// The only way to obtain one is [`PrimaryCommit::applied`], which consumes the plan a caller
/// has just applied. Passing it to [`commit_secondary`] is how the type system carries "the
/// primary is committed" from the place that knows it to the place that depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryCommitted {
    start_ts: u64,
    commit_ts: u64,
    primary: Bytes,
}

impl PrimaryCommitted {
    /// The transaction this token belongs to.
    #[must_use]
    pub fn start_ts(&self) -> u64 {
        self.start_ts
    }

    /// The timestamp every key of the transaction commits at.
    #[must_use]
    pub fn commit_ts(&self) -> u64 {
        self.commit_ts
    }

    /// The primary key it was obtained from.
    #[must_use]
    pub fn primary(&self) -> &[u8] {
        &self.primary
    }

    /// Evidence taken **on trust** from a caller that applied the primary's commit elsewhere.
    ///
    /// [`PrimaryCommit::applied`] is the safe constructor and the one any caller that holds
    /// both keys should use. This one exists because a transaction's primary and its
    /// secondaries may live in **different regions, on different stores**: the store applying
    /// a secondary's commit cannot read the record that would witness the primary, so the
    /// evidence has to travel with the request instead of being observed.
    ///
    /// What makes it true is the client: its commit path cannot send a secondary commit until
    /// the primary's own batch was applied, and that ordering is enforced by this very type on
    /// the client's side. The constructor is named to be conspicuous, because a call to it is a
    /// place where the guarantee is being *asserted* rather than *held*.
    #[must_use]
    pub fn on_trust(primary: Bytes, start_ts: u64, commit_ts: u64) -> Self {
        Self {
            start_ts,
            commit_ts,
            primary,
        }
    }
}

/// The primary's commit, before it has been applied.
///
/// Holding this is not the same as having committed. [`PrimaryCommit::applied`] consumes it
/// and yields the token that [`commit_secondary`] demands; call it **after** the mutations are
/// durable, which for `esker-store` means after the batch has been applied through Raft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryCommit {
    mutations: Mutations,
    token: PrimaryCommitted,
}

impl PrimaryCommit {
    /// The batch to apply.
    #[must_use]
    pub fn mutations(&self) -> &Mutations {
        &self.mutations
    }

    /// Takes the batch, leaving the plan behind.
    #[must_use]
    pub fn into_mutations(self) -> Mutations {
        self.mutations
    }

    /// The timestamp the transaction commits at.
    #[must_use]
    pub fn commit_ts(&self) -> u64 {
        self.token.commit_ts
    }

    /// Consumes the plan and yields the evidence its mutations are durable.
    ///
    /// Call this only after applying [`PrimaryCommit::mutations`]. The plan is consumed so
    /// that the token cannot be minted twice from one commit, and it is the only source of a
    /// [`PrimaryCommitted`] anywhere in the crate.
    #[must_use]
    pub fn applied(self) -> PrimaryCommitted {
        self.token
    }
}

/// Whether the primary's commit still has to be applied, or already was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitDecision {
    /// Apply the plan, then call [`PrimaryCommit::applied`].
    Commit(Box<PrimaryCommit>),
    /// An earlier attempt already committed this key; there is nothing to apply and the
    /// evidence is available immediately.
    AlreadyCommitted(PrimaryCommitted),
}

// -- reads -------------------------------------------------------------------------------

/// Reads `user_key` as of `ts` (`docs/txn-spec.md` §5.1).
///
/// A lock with `start_ts <= ts` blocks the read: it may yet commit at a timestamp inside our
/// snapshot, and no reader may guess which way. A lock above `ts` belongs to a later
/// transaction and is invisible.
pub fn read(snapshot: &impl TxnSnapshot, user_key: &[u8], ts: u64) -> Result<ReadOutcome> {
    if let Some(lock) = snapshot.get_lock(user_key)?
        && lock.start_ts <= ts
    {
        return Ok(ReadOutcome::Locked(lock));
    }
    let Some(version) = newest_version_at(snapshot, user_key, ts)? else {
        return Ok(ReadOutcome::NotFound);
    };
    value_of(snapshot, user_key, &version.record)
}

/// The newest `Put` or `Delete` at or below `ts`, stepping past rollback markers and lock
/// records — which are bookkeeping, not versions.
fn newest_version_at(
    snapshot: &impl TxnSnapshot,
    user_key: &[u8],
    ts: u64,
) -> Result<Option<Version>> {
    let mut at = ts;
    loop {
        let Some(version) = snapshot.seek_write(user_key, at)? else {
            return Ok(None);
        };
        if version.record.kind.is_a_version() {
            return Ok(Some(version));
        }
        // A marker at commit_ts 0 has nothing older beneath it, and stepping below zero would
        // wrap into the newest version and loop for ever.
        let Some(next) = version.commit_ts.checked_sub(1) else {
            return Ok(None);
        };
        at = next;
    }
}

/// Turns a `Put`/`Delete` record into the value a reader sees.
fn value_of(
    snapshot: &impl TxnSnapshot,
    user_key: &[u8],
    record: &WriteRecord,
) -> Result<ReadOutcome> {
    match record.kind {
        // `Delete` says the key is not there; `Rollback` and `Lock` are bookkeeping that
        // `newest_version_at` steps past and never hands here. All three read as absent, and
        // the distinction between them is made where it matters, in `Kind::is_a_version`.
        Kind::Delete | Kind::Rollback | Kind::Lock => Ok(ReadOutcome::NotFound),
        Kind::Put => match &record.short_value {
            Some(value) => Ok(ReadOutcome::Value(value.clone())),
            // No inline value means the value is in the `default` CF. Its absence is
            // corruption and never an absent key: the two mean opposite things, and reporting
            // the wrong one loses a write with nothing to show for it.
            None => snapshot
                .get_value(user_key, record.start_ts)?
                .map(ReadOutcome::Value)
                .ok_or(TxnError::MissingValue {
                    start_ts: record.start_ts,
                }),
        },
    }
}

// -- prewrite ----------------------------------------------------------------------------

/// Decides one key's prewrite (`docs/txn-spec.md` §5.2).
pub fn check_prewrite(snapshot: &impl TxnSnapshot, request: &Prewrite) -> Result<PrewriteDecision> {
    let key = &request.key[..];

    // 1. A commit newer than the snapshot **this value was computed from**, which is the
    //    transaction's own unless the statement waited and re-read (ADR 0057 §4).
    //    First-committer-wins, and we are not it.
    if let Some(winner) = snapshot.newest_write_after(key, request.read_ts)? {
        // Our own commit is not a conflict with ourselves: a retry of a prewrite whose
        // transaction has since committed this key finds its own record here.
        if winner.record.start_ts != request.start_ts {
            return Ok(PrewriteDecision::Conflict {
                commit_ts: winner.commit_ts,
            });
        }
    }

    // 2. Our own fate, if someone else has already decided it. A rollback marker sits at
    //    `commit_ts == start_ts`, below the range checked above, so it needs its own look.
    if let Some(ours) = snapshot.write_of_txn(key, request.start_ts)? {
        return Ok(match ours.record.kind {
            Kind::Rollback => PrewriteDecision::RolledBack,
            // Already committed. Nothing to lock, and reporting success is right: the write
            // this prewrite was going to make is already durable.
            _ => PrewriteDecision::AlreadyLocked,
        });
    }

    // 3. Anyone's lock. Ours means an earlier attempt got through; anyone else's is a
    //    conflict the caller resolves rather than an error it reports.
    if let Some(lock) = snapshot.get_lock(key)? {
        return Ok(if lock.start_ts == request.start_ts {
            PrewriteDecision::AlreadyLocked
        } else {
            PrewriteDecision::Locked(lock)
        });
    }

    let mut mutations = Mutations::new();
    let lock = LockRecord {
        kind: request.op.kind(),
        start_ts: request.start_ts,
        ttl_ms: request.ttl_ms,
        primary: request.primary.clone(),
        short_value: request.op.short_value(),
    };
    mutations.put_lock(key, &lock);
    if let Some(value) = request.op.long_value() {
        mutations.put_value(key, request.start_ts, value);
    }
    Ok(PrewriteDecision::Lock(mutations))
}

// -- commit ------------------------------------------------------------------------------

/// Commits the **primary** key, and yields the evidence secondaries need
/// (`docs/txn-spec.md` §5.3).
///
/// Refuses a key the lock does not name as its own primary: committing a secondary through
/// this door would mint a token for a transaction whose commit point has not been written.
pub fn commit_primary(
    snapshot: &impl TxnSnapshot,
    primary: &[u8],
    start_ts: u64,
    commit_ts: u64,
) -> Result<CommitDecision> {
    if commit_ts <= start_ts {
        return Err(TxnError::Misuse(
            "a commit timestamp must be above the transaction's start timestamp",
        ));
    }
    let token = PrimaryCommitted {
        start_ts,
        commit_ts,
        primary: Bytes::copy_from_slice(primary),
    };

    match snapshot.get_lock(primary)? {
        Some(lock) if lock.start_ts == start_ts => {
            if !lock.is_primary(primary) {
                return Err(TxnError::Misuse(
                    "commit_primary was given a key whose lock names another primary",
                ));
            }
            Ok(CommitDecision::Commit(Box::new(PrimaryCommit {
                mutations: commit_mutations(primary, commit_ts, &lock),
                token,
            })))
        }
        // Another transaction's lock on our primary means ours is long gone; the `write` CF
        // decides, exactly as it does when there is no lock at all.
        Some(_) | None => match settled(snapshot, primary, start_ts)? {
            Settled::Committed { commit_ts: at } if at == commit_ts => {
                Ok(CommitDecision::AlreadyCommitted(token))
            }
            // Committed at a *different* timestamp is not a duplicate: two commit timestamps
            // for one transaction is a contradiction, and answering "fine" would let a caller
            // roll secondaries forward to a timestamp the primary does not agree with.
            Settled::Committed { commit_ts: at } => Err(TxnError::AlreadyCommitted {
                start_ts,
                commit_ts: at,
            }),
            Settled::RolledBack => Err(TxnError::AlreadyRolledBack { start_ts }),
            Settled::Undecided => Err(TxnError::TxnLockNotFound { start_ts }),
        },
    }
}

/// Commits a **secondary** key, against evidence that the primary is durable.
///
/// The token is the whole point: it cannot be constructed, only obtained from
/// [`PrimaryCommit::applied`], so a caller cannot reach this function before the transaction
/// has a commit point (`docs/txn-spec.md` §5.3).
pub fn commit_secondary(
    snapshot: &impl TxnSnapshot,
    user_key: &[u8],
    committed: &PrimaryCommitted,
) -> Result<Mutations> {
    let start_ts = committed.start_ts;
    match snapshot.get_lock(user_key)? {
        Some(lock) if lock.start_ts == start_ts => {
            if lock.primary != committed.primary {
                return Err(TxnError::Misuse(
                    "the lock names a different primary from the token",
                ));
            }
            Ok(commit_mutations(user_key, committed.commit_ts, &lock))
        }
        Some(_) | None => match settled(snapshot, user_key, start_ts)? {
            // Already rolled forward by a resolver — the common case under contention, and a
            // success rather than a race lost.
            Settled::Committed { commit_ts } if commit_ts == committed.commit_ts => {
                Ok(Mutations::new())
            }
            Settled::Committed { commit_ts } => Err(TxnError::AlreadyCommitted {
                start_ts,
                commit_ts,
            }),
            // A secondary rolled back while its primary is committed is a contradiction: the
            // primary is the fact, so this is corruption of the resolution rules and not a
            // race to absorb.
            Settled::RolledBack => Err(TxnError::AlreadyRolledBack { start_ts }),
            Settled::Undecided => Err(TxnError::TxnLockNotFound { start_ts }),
        },
    }
}

/// `write` record in, lock out — the same two mutations for a primary and a secondary.
fn commit_mutations(user_key: &[u8], commit_ts: u64, lock: &LockRecord) -> Mutations {
    let mut mutations = Mutations::new();
    mutations.put_write(
        user_key,
        commit_ts,
        &WriteRecord {
            kind: lock.kind,
            start_ts: lock.start_ts,
            short_value: lock.short_value.clone(),
        },
    );
    mutations.delete_lock(user_key);
    mutations
}

// -- rollback ----------------------------------------------------------------------------

/// Rolls one key back (`docs/txn-spec.md` §5.4).
///
/// The marker is written whether or not the lock is there, because the case that matters is
/// the one where it is not: a `Prewrite` whose answer was lost may arrive after the client
/// gave up, and the marker is what makes that late arrival fail.
pub fn rollback(snapshot: &impl TxnSnapshot, user_key: &[u8], start_ts: u64) -> Result<Mutations> {
    match settled(snapshot, user_key, start_ts)? {
        Settled::Committed { commit_ts } => {
            return Err(TxnError::AlreadyCommitted {
                start_ts,
                commit_ts,
            });
        }
        // Already marked. Idempotent: the answer is the same and there is nothing to write.
        Settled::RolledBack => return Ok(Mutations::new()),
        Settled::Undecided => {}
    }

    let mut mutations = Mutations::new();
    // A rollback marker lives at `commit_ts == start_ts`. Nothing else can be there: a real
    // commit is strictly above its start timestamp.
    mutations.put_write(user_key, start_ts, &WriteRecord::rollback(start_ts));

    // Only our own lock and our own value come out. Another transaction's lock is not ours to
    // remove, and its `default` entry is filed under its own start timestamp anyway.
    if let Some(lock) = snapshot.get_lock(user_key)?
        && lock.start_ts == start_ts
    {
        mutations.delete_lock(user_key);
        if lock.short_value.is_none() && lock.kind == Kind::Put {
            mutations.delete_value(user_key, start_ts);
        }
    }
    Ok(mutations)
}

// -- resolution --------------------------------------------------------------------------

/// Reads what the `write` and `lock` column families say about the transaction that owns
/// `lock`, by looking at its **primary** (`docs/txn-spec.md` §5.5).
pub fn primary_state(
    snapshot: &impl TxnSnapshot,
    primary: &[u8],
    start_ts: u64,
) -> Result<PrimaryState> {
    match settled(snapshot, primary, start_ts)? {
        Settled::Committed { commit_ts } => return Ok(PrimaryState::Committed { commit_ts }),
        Settled::RolledBack => return Ok(PrimaryState::RolledBack),
        Settled::Undecided => {}
    }
    Ok(match snapshot.get_lock(primary)? {
        Some(lock) if lock.start_ts == start_ts => PrimaryState::Locked(lock),
        // Someone else's lock on the primary, or none: either way our transaction left no
        // lock and no record, which is `Missing`.
        Some(_) | None => PrimaryState::Missing,
    })
}

/// What to do with a lock, given its primary's state and a timestamp from the oracle.
///
/// `now_ts` is a **timestamp**, not a wall-clock reading: `CLAUDE.md` invariant 6 says no node
/// decides another node's transaction is dead by looking at its own clock. The caller fetches
/// it from the TSO like any other.
#[must_use]
pub fn resolve(lock: &LockRecord, primary: &PrimaryState, now_ts: u64) -> Resolution {
    match primary {
        PrimaryState::Committed { commit_ts } => Resolution::RollForward {
            commit_ts: *commit_ts,
        },
        // Rolled back, or no lock and no record at all: either way whatever wrote this lock
        // never reached a commit point and nothing ever will now. `Missing` is the weaker
        // evidence of the two, and it is still conclusive — a transaction with no lock has no
        // way left to commit.
        PrimaryState::RolledBack | PrimaryState::Missing => Resolution::RollBack,
        PrimaryState::Locked(primary_lock) => {
            // The primary's own TTL decides, not this key's: they were written at the same
            // start timestamp but the primary is the one being heartbeated.
            if crate::is_expired(primary_lock.start_ts, primary_lock.ttl_ms, now_ts) {
                Resolution::RollBack
            } else {
                let _ = lock;
                Resolution::Wait
            }
        }
    }
}

// -- shared ------------------------------------------------------------------------------

/// Whether the `write` column family has already decided this transaction's fate on this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    Committed { commit_ts: u64 },
    RolledBack,
    Undecided,
}

fn settled(snapshot: &impl TxnSnapshot, user_key: &[u8], start_ts: u64) -> Result<Settled> {
    Ok(match snapshot.write_of_txn(user_key, start_ts)? {
        None => Settled::Undecided,
        Some(version) => match version.record.kind {
            Kind::Rollback => Settled::RolledBack,
            _ => Settled::Committed {
                commit_ts: version.commit_ts,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{
        CommitDecision, Op, Prewrite, PrewriteDecision, ReadOutcome, check_prewrite,
        commit_primary, read, rollback,
    };
    use crate::mutation::Cf;
    use crate::snapshot::MemoryStore;

    /// Prewrites, commits and applies one single-key transaction, as the store would.
    fn commit_one(
        store: &mut MemoryStore,
        key: &[u8],
        value: &[u8],
        start_ts: u64,
        commit_ts: u64,
    ) {
        let request = Prewrite::new(
            Bytes::copy_from_slice(key),
            Bytes::copy_from_slice(key),
            start_ts,
            Op::Put(Bytes::copy_from_slice(value)),
        );
        let PrewriteDecision::Lock(mutations) = check_prewrite(store, &request).unwrap() else {
            panic!("prewrite refused");
        };
        store.apply(&mutations);
        let CommitDecision::Commit(plan) = commit_primary(store, key, start_ts, commit_ts).unwrap()
        else {
            panic!("commit refused");
        };
        store.apply(plan.mutations());
    }

    #[test]
    fn a_committed_write_is_visible_at_and_after_its_commit_ts() {
        let mut store = MemoryStore::new();
        commit_one(&mut store, b"k", b"v", 10, 20);

        assert_eq!(read(&store, b"k", 19).unwrap(), ReadOutcome::NotFound);
        // The boundary: a commit at exactly the read timestamp is visible.
        assert_eq!(
            read(&store, b"k", 20).unwrap(),
            ReadOutcome::Value(Bytes::from_static(b"v"))
        );
        assert_eq!(
            read(&store, b"k", 1_000).unwrap(),
            ReadOutcome::Value(Bytes::from_static(b"v"))
        );
    }

    #[test]
    fn a_read_sees_the_newest_version_at_or_below_its_snapshot() {
        let mut store = MemoryStore::new();
        commit_one(&mut store, b"k", b"first", 10, 20);
        commit_one(&mut store, b"k", b"second", 30, 40);

        assert_eq!(
            read(&store, b"k", 20).unwrap(),
            ReadOutcome::Value(Bytes::from_static(b"first"))
        );
        assert_eq!(
            read(&store, b"k", 39).unwrap(),
            ReadOutcome::Value(Bytes::from_static(b"first"))
        );
        assert_eq!(
            read(&store, b"k", 40).unwrap(),
            ReadOutcome::Value(Bytes::from_static(b"second"))
        );
    }

    /// A value over the inline cutoff goes to the `default` CF and comes back through it.
    #[test]
    fn a_long_value_travels_through_the_default_cf() {
        let mut store = MemoryStore::new();
        let long = vec![7u8; 300];
        commit_one(&mut store, b"k", &long, 10, 20);
        assert_eq!(store.len(Cf::Default), 1, "the value needs its own entry");
        assert_eq!(
            read(&store, b"k", 20).unwrap(),
            ReadOutcome::Value(Bytes::from(long))
        );
    }

    /// A short value is inlined and costs no `default` entry.
    #[test]
    fn a_short_value_is_inlined() {
        let mut store = MemoryStore::new();
        commit_one(&mut store, b"k", b"small", 10, 20);
        assert!(store.is_empty(Cf::Default), "a short value is inline");
    }
    /// Rolls one key back the way the resolver does, leaving the marker at `commit_ts == start_ts`.
    fn roll_back_one(store: &mut MemoryStore, key: &[u8], start_ts: u64) {
        let mutations = rollback(store, key, start_ts).unwrap();
        store.apply(&mutations);
    }

    /// **A rollback marker is not a commit, and first-committer-wins has no business seeing it.**
    ///
    /// `newest_write_after` answers the conflict check, and it used to answer with the newest
    /// record of *any* kind. A marker lives at `commit_ts == start_ts`, so a transaction that took
    /// this key after our snapshot and then died left something above our snapshot that looks, to a
    /// check that does not read `kind`, exactly like a commit that beat us. Nothing committed; the
    /// statement is refused with `40001` anyway.
    ///
    /// This is the same rule reads have always kept: `newest_version_at` steps past a record whose
    /// `Kind::is_a_version` is false rather than answering with it.
    #[test]
    fn a_rollback_marker_is_not_a_commit_and_refuses_nobody() {
        let mut store = MemoryStore::new();
        commit_one(&mut store, b"k", b"v", 10, 20);
        // A writer that took the key at 30 and died. Its marker sits at 30, above the snapshot
        // the next writer read at.
        roll_back_one(&mut store, b"k", 30);

        let request = Prewrite::new(
            Bytes::from_static(b"k"),
            Bytes::from_static(b"k"),
            35,
            Op::Put(Bytes::from_static(b"next")),
        )
        .read_at(25);

        assert!(
            matches!(
                check_prewrite(&store, &request).unwrap(),
                PrewriteDecision::Lock(_)
            ),
            "the only record above the snapshot is a rollback marker, and a rollback committed \
             nothing: refusing here is a 40001 for a conflict that does not exist"
        );
    }

    /// **Stepping past a marker must not step past the commit under it**, and the refusal has to
    /// name the commit that actually won.
    ///
    /// The half of the rule the test above cannot state: a real commit hiding beneath a marker is
    /// still a conflict. Before the marker was stepped past, this refusal reported the *marker's*
    /// timestamp — telling the client "a commit at 50 beat you" about a transaction that rolled
    /// back and committed nothing at all.
    #[test]
    fn a_commit_under_a_rollback_marker_still_refuses_and_names_itself() {
        let mut store = MemoryStore::new();
        commit_one(&mut store, b"k", b"v", 10, 20);
        // Somebody really did commit above our snapshot, at 40 ...
        commit_one(&mut store, b"k", b"theirs", 30, 40);
        // ... and somebody else took the key afterwards and died, leaving a marker above that.
        roll_back_one(&mut store, b"k", 50);

        let request = Prewrite::new(
            Bytes::from_static(b"k"),
            Bytes::from_static(b"k"),
            55,
            Op::Put(Bytes::from_static(b"next")),
        )
        .read_at(25);

        assert_eq!(
            check_prewrite(&store, &request).unwrap(),
            PrewriteDecision::Conflict { commit_ts: 40 },
            "the commit at 40 is the one that beat this transaction; the marker at 50 is not a \
             commit and must not be named as the winner"
        );
    }
}
