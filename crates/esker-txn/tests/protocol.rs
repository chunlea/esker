//! The Percolator decision matrix, against an in-memory three-column-family store.
//!
//! Every rule of `docs/txn-spec.md` §5 has a case here, and the three traps of the module
//! header have more than one:
//!
//! * prewrite's two checks are tested **each alone** as well as together, because a rule that
//!   only ever fires alongside another is a rule nobody has tested;
//! * the primary decides, so roll-forward and roll-back are driven by the primary's state and
//!   never by the secondary's;
//! * the TTL boundary is tested on both sides.
//!
//! This is the file the `esker-store` handler will be judged against when phase 4 closes: if
//! it decodes a request, calls one of these functions, and writes the answer through Raft,
//! everything below is already true of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_txn::codec::{Kind, LockRecord, WriteRecord};
use esker_txn::mutation::{Cf, Mutation};
use esker_txn::snapshot::{MemoryStore, TxnSnapshot};
use esker_txn::{
    CommitDecision, LOCK_TTL_MS, Mutations, Op, Prewrite, PrewriteDecision, PrimaryCommitted,
    PrimaryState, ReadOutcome, Resolution, TxnError, check_prewrite, commit_primary,
    commit_secondary, primary_state, read, release, resolve, rollback,
};

/// The largest logical counter that fits beside a physical millisecond.
const MAX_LOGICAL: u64 = (1 << esker_txn::TSO_LOGICAL_BITS) - 1;

/// PD's timestamp layout: physical milliseconds in the high bits, a logical counter below.
fn ts(physical_ms: u64, logical: u64) -> u64 {
    assert!(
        logical <= MAX_LOGICAL,
        "the logical counter would overflow into the clock"
    );
    (physical_ms << esker_txn::TSO_LOGICAL_BITS) | logical
}

fn key(bytes: &'static [u8]) -> Bytes {
    Bytes::from_static(bytes)
}

fn prewrite(k: &'static [u8], primary: &'static [u8], start_ts: u64, op: Op) -> Prewrite {
    Prewrite::new(key(k), key(primary), start_ts, op)
}

fn put(value: &'static [u8]) -> Op {
    Op::Put(Bytes::from_static(value))
}

/// Prewrites one key and applies the result, panicking if it was refused.
fn lock_key(store: &mut MemoryStore, request: &Prewrite) {
    match check_prewrite(store, request).unwrap() {
        PrewriteDecision::Lock(mutations) => store.apply(&mutations),
        other => panic!("prewrite refused: {other:?}"),
    }
}

/// A whole single-key transaction, applied.
fn commit_one(
    store: &mut MemoryStore,
    k: &'static [u8],
    value: &'static [u8],
    start: u64,
    commit: u64,
) {
    lock_key(store, &prewrite(k, k, start, put(value)));
    let CommitDecision::Commit(plan) = commit_primary(store, k, start, commit).unwrap() else {
        panic!("commit refused");
    };
    store.apply(plan.mutations());
}

// -- prewrite: the two checks, each alone ------------------------------------------------

/// Check 1 alone: a commit landed after our snapshot and there is **no lock** anywhere. Only
/// the `write` CF can catch this, and missing it loses an update.
#[test]
fn a_commit_after_the_snapshot_is_a_conflict_even_with_no_lock_in_sight() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"winner", 30, 40);
    assert!(store.is_empty(Cf::Lock), "the winner left no lock behind");

    let decision = check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"loser"))).unwrap();
    assert_eq!(decision, PrewriteDecision::Conflict { commit_ts: 40 });
}

/// Check 2 alone: a live lock and **nothing at all** in the `write` CF. Only the `lock` CF can
/// catch this, and missing it lets two transactions hold one key.
#[test]
fn a_live_lock_is_a_conflict_even_with_an_empty_write_cf() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"first")));
    assert!(
        store.is_empty(Cf::Write),
        "a prewrite writes no commit record"
    );

    let decision = check_prewrite(&store, &prewrite(b"k", b"k", 20, put(b"second"))).unwrap();
    match decision {
        PrewriteDecision::Locked(lock) => assert_eq!(lock.start_ts, 10),
        other => panic!("expected a lock conflict, got {other:?}"),
    }
}

/// A commit *below* our snapshot is what snapshot isolation is for: we read it, and writing
/// over it is exactly right.
#[test]
fn a_commit_before_the_snapshot_is_not_a_conflict() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"old", 10, 20);

    let decision = check_prewrite(&store, &prewrite(b"k", b"k", 30, put(b"new"))).unwrap();
    assert!(matches!(decision, PrewriteDecision::Lock(_)));
}

/// The boundary. A commit at exactly our `start_ts` is inside our snapshot, not after it.
#[test]
fn a_commit_at_exactly_the_snapshot_is_not_a_conflict() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"old", 10, 30);

    assert!(matches!(
        check_prewrite(&store, &prewrite(b"k", b"k", 30, put(b"new"))).unwrap(),
        PrewriteDecision::Lock(_)
    ));
    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 29, put(b"new"))).unwrap(),
        PrewriteDecision::Conflict { commit_ts: 30 },
        "one below the boundary is a conflict"
    );
}

/// Both checks fire at once. Reported as a conflict rather than a lock: the transaction has to
/// abort either way, and telling the caller to go and resolve a lock it cannot benefit from
/// resolving would cost a round trip for nothing.
#[test]
fn a_conflict_and_a_lock_together_report_the_conflict() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"winner", 30, 40);
    lock_key(&mut store, &prewrite(b"k", b"k", 50, put(b"other")));

    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"loser"))).unwrap(),
        PrewriteDecision::Conflict { commit_ts: 40 }
    );
}

/// Idempotence: a prewrite whose answer was lost is safe to send again. This is what makes an
/// ambiguous prewrite resolvable rather than fatal, which is the point of Percolator.
#[test]
fn re_prewriting_our_own_lock_writes_nothing_and_succeeds() {
    let mut store = MemoryStore::new();
    let request = prewrite(b"k", b"k", 10, put(b"v"));
    lock_key(&mut store, &request);

    assert_eq!(
        check_prewrite(&store, &request).unwrap(),
        PrewriteDecision::AlreadyLocked
    );
}

/// A rollback marker at `commit_ts == start_ts` is below the range the conflict check looks
/// at, so it needs its own lookup — and without it a transaction someone declared dead would
/// quietly lock a key.
#[test]
fn a_prewrite_after_our_own_rollback_is_refused() {
    let mut store = MemoryStore::new();
    store.apply(&rollback(&store, b"k", 10).unwrap());

    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"v"))).unwrap(),
        PrewriteDecision::RolledBack
    );
    // And a different transaction is unaffected by our marker.
    assert!(matches!(
        check_prewrite(&store, &prewrite(b"k", b"k", 11, put(b"v"))).unwrap(),
        PrewriteDecision::Lock(_)
    ));
}

/// A prewrite that lands after the transaction already committed this key is a duplicate, not
/// a conflict with itself.
#[test]
fn a_prewrite_after_our_own_commit_is_a_duplicate() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);

    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"v"))).unwrap(),
        PrewriteDecision::AlreadyLocked
    );
}

/// The prewrite of a value over the inline cutoff is two mutations in one batch. Splitting
/// them would leave a value no reader can see or a lock pointing at nothing.
#[test]
fn a_long_value_is_locked_and_stored_in_one_batch() {
    let store = MemoryStore::new();
    let long = Bytes::from(vec![3u8; 300]);
    let request = Prewrite::new(key(b"k"), key(b"k"), 10, Op::Put(long));
    let PrewriteDecision::Lock(mutations) = check_prewrite(&store, &request).unwrap() else {
        panic!("refused");
    };
    let families: Vec<Cf> = mutations.iter().map(Mutation::cf).collect();
    assert_eq!(families, vec![Cf::Lock, Cf::Default]);
}

// -- commit ------------------------------------------------------------------------------

#[test]
fn commit_writes_the_record_and_removes_the_lock() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"v")));
    let CommitDecision::Commit(plan) = commit_primary(&store, b"k", 10, 20).unwrap() else {
        panic!("refused");
    };

    let mutations: Vec<&Mutation> = plan.mutations().iter().collect();
    assert_eq!(mutations.len(), 2);
    assert!(matches!(mutations[0], Mutation::Put { cf: Cf::Write, .. }));
    assert!(matches!(
        mutations[1],
        Mutation::Delete { cf: Cf::Lock, .. }
    ));

    store.apply(plan.mutations());
    assert!(store.is_empty(Cf::Lock), "the lock is gone");
    assert_eq!(
        read(&store, b"k", 20).unwrap(),
        ReadOutcome::Value(key(b"v"))
    );
}

/// A commit whose lock is gone and whose record is there is a duplicate — the answer to the
/// first attempt was lost. It yields the token without asking for anything to be written.
#[test]
fn committing_twice_is_a_duplicate_not_an_error() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);

    match commit_primary(&store, b"k", 10, 20).unwrap() {
        CommitDecision::AlreadyCommitted(token) => {
            assert_eq!(token.commit_ts(), 20);
            assert_eq!(token.start_ts(), 10);
        }
        other @ CommitDecision::Commit(_) => panic!("expected a duplicate, got {other:?}"),
    }
}

/// Two commit timestamps for one transaction is a contradiction, and answering "fine" would
/// let a caller roll secondaries forward to a timestamp the primary does not agree with.
#[test]
fn committing_at_a_second_timestamp_is_refused() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);

    assert_eq!(
        commit_primary(&store, b"k", 10, 21).unwrap_err(),
        TxnError::AlreadyCommitted {
            start_ts: 10,
            commit_ts: 20
        }
    );
}

#[test]
fn committing_a_rolled_back_transaction_is_refused() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"v")));
    store.apply(&rollback(&store, b"k", 10).unwrap());

    assert_eq!(
        commit_primary(&store, b"k", 10, 20).unwrap_err(),
        TxnError::AlreadyRolledBack { start_ts: 10 }
    );
}

/// No lock and no record: something removed the lock without saying why, and a commit that
/// guessed either way would be inventing a fact.
#[test]
fn committing_with_no_lock_and_no_record_is_refused() {
    let store = MemoryStore::new();
    assert_eq!(
        commit_primary(&store, b"k", 10, 20).unwrap_err(),
        TxnError::TxnLockNotFound { start_ts: 10 }
    );
}

/// A commit timestamp at or below the start timestamp is not a timestamp ordering can be read
/// out of; the rollback marker lives at `commit_ts == start_ts`, so it is also a collision.
#[test]
fn a_commit_ts_below_the_start_ts_is_misuse() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"v")));
    assert!(matches!(
        commit_primary(&store, b"k", 10, 10).unwrap_err(),
        TxnError::Misuse(_)
    ));
    assert!(matches!(
        commit_primary(&store, b"k", 10, 9).unwrap_err(),
        TxnError::Misuse(_)
    ));
}

/// `commit_primary` on a key whose lock names someone else as the primary would mint a token
/// for a transaction that has no commit point.
#[test]
fn commit_primary_refuses_a_secondary() {
    let mut store = MemoryStore::new();
    lock_key(
        &mut store,
        &prewrite(b"secondary", b"primary", 10, put(b"v")),
    );
    assert!(matches!(
        commit_primary(&store, b"secondary", 10, 20).unwrap_err(),
        TxnError::Misuse(_)
    ));
}

// -- the primary-first rule ---------------------------------------------------------------

/// The whole shape of the commit phase: the token cannot be conjured, only taken from a plan
/// that has been consumed, and `commit_secondary` will not move without one.
#[test]
fn a_secondary_commits_only_against_the_primary_s_token() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"a", b"a", 10, put(b"va")));
    lock_key(&mut store, &prewrite(b"b", b"a", 10, put(b"vb")));

    let CommitDecision::Commit(plan) = commit_primary(&store, b"a", 10, 20).unwrap() else {
        panic!("refused");
    };
    // Applying the primary's batch is what makes the token honest; `applied()` consumes the
    // plan so it cannot be minted twice from one commit.
    store.apply(plan.mutations());
    let token = plan.applied();

    let mutations = commit_secondary(&store, b"b", &token).unwrap();
    store.apply(&mutations);

    assert!(store.is_empty(Cf::Lock));
    assert_eq!(
        read(&store, b"a", 20).unwrap(),
        ReadOutcome::Value(key(b"va"))
    );
    assert_eq!(
        read(&store, b"b", 20).unwrap(),
        ReadOutcome::Value(key(b"vb"))
    );
}

/// A secondary already rolled forward by a resolver is a success, not a race lost — that is
/// the common case under contention.
#[test]
fn a_secondary_already_rolled_forward_commits_to_nothing() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"a", b"a", 10, put(b"va")));
    lock_key(&mut store, &prewrite(b"b", b"a", 10, put(b"vb")));

    let CommitDecision::Commit(plan) = commit_primary(&store, b"a", 10, 20).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());
    let token = plan.applied();

    // A reader resolves `b` first.
    store.apply(&commit_secondary(&store, b"b", &token).unwrap());
    // The owner arrives late and finds its work done.
    assert_eq!(
        commit_secondary(&store, b"b", &token).unwrap(),
        Mutations::new()
    );
}

/// A token from one transaction must not commit another's lock.
#[test]
fn a_token_from_another_transaction_is_refused() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"a", b"a", 10, put(b"va")));
    lock_key(&mut store, &prewrite(b"b", b"b", 30, put(b"vb")));

    let CommitDecision::Commit(plan) = commit_primary(&store, b"a", 10, 20).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());
    let token: PrimaryCommitted = plan.applied();

    // `b` is locked at start_ts 30; the token says 10, so the write CF is consulted and says
    // nothing has happened to `b` at 10.
    assert_eq!(
        commit_secondary(&store, b"b", &token).unwrap_err(),
        TxnError::TxnLockNotFound { start_ts: 10 }
    );
}

// -- rollback ----------------------------------------------------------------------------

#[test]
fn rollback_removes_our_lock_and_value_and_leaves_a_marker() {
    let mut store = MemoryStore::new();
    let long = Bytes::from(vec![1u8; 300]);
    let request = Prewrite::new(key(b"k"), key(b"k"), 10, Op::Put(long));
    match check_prewrite(&store, &request).unwrap() {
        PrewriteDecision::Lock(mutations) => store.apply(&mutations),
        other => panic!("{other:?}"),
    }
    assert_eq!(store.len(Cf::Default), 1);

    store.apply(&rollback(&store, b"k", 10).unwrap());
    assert!(store.is_empty(Cf::Lock), "the lock is gone");
    assert!(store.is_empty(Cf::Default), "the value is gone");
    assert_eq!(
        store.write_of_txn(b"k", 10).unwrap(),
        Some(esker_txn::Version::new(10, WriteRecord::rollback(10))),
        "the marker sits at commit_ts == start_ts"
    );
}

/// The case the marker exists for: rolling back a key that was never prewritten still leaves
/// the marker, so a `Prewrite` that arrives after the client gave up is refused.
#[test]
fn rolling_back_an_unlocked_key_still_leaves_a_marker() {
    let mut store = MemoryStore::new();
    store.apply(&rollback(&store, b"k", 10).unwrap());

    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"late"))).unwrap(),
        PrewriteDecision::RolledBack
    );
}

/// Another transaction's lock is not ours to remove, but our own marker still goes down.
#[test]
fn rollback_leaves_another_transaction_s_lock_alone() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 30, put(b"theirs")));

    store.apply(&rollback(&store, b"k", 10).unwrap());
    assert_eq!(
        store.get_lock(b"k").unwrap().map(|lock| lock.start_ts),
        Some(30),
        "the other transaction still holds its lock"
    );
    assert_eq!(
        check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"late"))).unwrap(),
        PrewriteDecision::RolledBack
    );
}

#[test]
fn rolling_back_twice_writes_nothing_the_second_time() {
    let mut store = MemoryStore::new();
    store.apply(&rollback(&store, b"k", 10).unwrap());
    assert_eq!(rollback(&store, b"k", 10).unwrap(), Mutations::new());
}

// -- release -----------------------------------------------------------------------------

/// **A release takes the lock and the value and writes nothing down**
/// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2).
///
/// The whole difference from `rollback`, in one assertion: no marker. A rollback makes the
/// transaction dead on the key for ever, and a savepoint's victim goes on to write the row it
/// locked.
#[test]
fn release_removes_our_lock_and_value_and_leaves_no_marker() {
    let mut store = MemoryStore::new();
    let long = Bytes::from(vec![1u8; 300]);
    let request = Prewrite::new(key(b"k"), key(b"k"), 10, Op::Put(long));
    match check_prewrite(&store, &request).unwrap() {
        PrewriteDecision::Lock(mutations) => store.apply(&mutations),
        other => panic!("{other:?}"),
    }
    assert_eq!(store.len(Cf::Default), 1);

    let (mutations, released) = release(&store, b"k", 10).unwrap();
    assert!(released, "the lock was ours");
    store.apply(&mutations);
    assert!(store.is_empty(Cf::Lock), "the lock is gone");
    assert!(store.is_empty(Cf::Default), "the value went with it");
    assert!(
        store.is_empty(Cf::Write),
        "and nothing was written down: a release is not a rollback"
    );
}

/// **The consequence that makes it a release and not a rollback**: the same transaction may lock
/// the key again. After a rollback it could never touch it again, which is what a savepoint's
/// victim does the moment its `rescue` is over.
#[test]
fn a_released_key_can_be_taken_again_by_the_same_transaction() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"first")));

    let (mutations, _) = release(&store, b"k", 10).unwrap();
    store.apply(&mutations);

    match check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"again"))).unwrap() {
        PrewriteDecision::Lock(mutations) => store.apply(&mutations),
        other => panic!("a released key is free, not rolled back: {other:?}"),
    }
    assert_eq!(
        store.get_lock(b"k").unwrap().map(|lock| lock.start_ts),
        Some(10)
    );
}

/// Somebody else's lock is not ours to give away — the same rule `rollback` keeps, and here it is
/// the only rule, because there is no marker to leave behind either.
#[test]
fn release_leaves_another_transaction_s_lock_alone() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 30, put(b"theirs")));

    let (mutations, released) = release(&store, b"k", 10).unwrap();
    assert!(!released, "not ours, so not counted");
    assert_eq!(mutations, Mutations::new(), "and nothing is staged");
    store.apply(&mutations);
    assert_eq!(
        store.get_lock(b"k").unwrap().map(|lock| lock.start_ts),
        Some(30)
    );
}

/// Idempotent: releasing a key we do not hold writes nothing and says so. What makes it safe on a
/// retry, and safe from a destructor.
#[test]
fn releasing_a_key_we_do_not_hold_writes_nothing() {
    let store = MemoryStore::new();
    let (mutations, released) = release(&store, b"k", 10).unwrap();
    assert!(!released);
    assert_eq!(mutations, Mutations::new());
}

/// A committed key has no lock left to take, and a release must not pretend otherwise: the write
/// record is every reader's now.
#[test]
fn releasing_a_committed_key_takes_nothing() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);
    let (mutations, released) = release(&store, b"k", 10).unwrap();
    assert!(!released);
    assert_eq!(mutations, Mutations::new());
    assert_eq!(
        read(&store, b"k", 30).unwrap(),
        ReadOutcome::Value(Bytes::from_static(b"v")),
        "the commit is untouched"
    );
}

#[test]
fn rolling_back_a_committed_transaction_is_refused() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);
    assert_eq!(
        rollback(&store, b"k", 10).unwrap_err(),
        TxnError::AlreadyCommitted {
            start_ts: 10,
            commit_ts: 20
        }
    );
}

// -- reads and locks ----------------------------------------------------------------------

/// A lock at or below the read's snapshot blocks it: it may still commit inside the snapshot.
#[test]
fn a_lock_at_or_below_the_snapshot_blocks_a_read() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"k", b"k", 10, put(b"v")));

    match read(&store, b"k", 10).unwrap() {
        ReadOutcome::Locked(lock) => assert_eq!(lock.start_ts, 10),
        other => panic!("expected Locked, got {other:?}"),
    }
    assert!(matches!(
        read(&store, b"k", 50).unwrap(),
        ReadOutcome::Locked(_)
    ));
}

/// A lock from a *later* transaction is invisible: it cannot commit below our snapshot.
#[test]
fn a_lock_above_the_snapshot_does_not_block_a_read() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"old", 5, 8);
    lock_key(&mut store, &prewrite(b"k", b"k", 20, put(b"new")));

    assert_eq!(
        read(&store, b"k", 10).unwrap(),
        ReadOutcome::Value(key(b"old"))
    );
}

#[test]
fn a_delete_hides_the_older_version() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);
    lock_key(&mut store, &prewrite(b"k", b"k", 30, Op::Delete));
    let CommitDecision::Commit(plan) = commit_primary(&store, b"k", 30, 40).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());

    assert_eq!(
        read(&store, b"k", 39).unwrap(),
        ReadOutcome::Value(key(b"v"))
    );
    assert_eq!(read(&store, b"k", 40).unwrap(), ReadOutcome::NotFound);
}

/// A rollback marker is bookkeeping, not a version: a read steps past it to the value beneath.
/// Treating it as a version would make a committed write disappear when an unrelated
/// transaction aborted on the same key.
#[test]
fn a_read_steps_past_a_rollback_marker_to_the_value_beneath() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 10, 20);
    // Another transaction starts later, gets nowhere, and is rolled back at 30.
    store.apply(&rollback(&store, b"k", 30).unwrap());

    assert_eq!(
        read(&store, b"k", 50).unwrap(),
        ReadOutcome::Value(key(b"v"))
    );
}

/// A `Put` whose value is not in the `default` CF is corruption, and reporting it as an absent
/// key would lose a write silently.
#[test]
fn a_put_with_no_value_anywhere_is_corruption_not_an_absent_key() {
    let mut store = MemoryStore::new();
    let record = WriteRecord::new(Kind::Put, 10);
    store.put_raw(Cf::Write, esker_txn::key::write(b"k", 20), record.encode());

    assert_eq!(
        read(&store, b"k", 20).unwrap_err(),
        TxnError::MissingValue { start_ts: 10 }
    );
}

// -- resolution ---------------------------------------------------------------------------

fn locked_secondary(store: &mut MemoryStore, start_ts: u64) -> LockRecord {
    lock_key(store, &prewrite(b"a", b"a", start_ts, put(b"va")));
    lock_key(store, &prewrite(b"b", b"a", start_ts, put(b"vb")));
    store.get_lock(b"b").unwrap().unwrap()
}

/// The primary committed: roll the secondary forward to the same timestamp.
#[test]
fn a_committed_primary_rolls_a_secondary_forward() {
    let mut store = MemoryStore::new();
    let lock = locked_secondary(&mut store, 10);

    let CommitDecision::Commit(plan) = commit_primary(&store, b"a", 10, 20).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());

    let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
    assert_eq!(state, PrimaryState::Committed { commit_ts: 20 });
    assert_eq!(
        resolve(&lock, &state, ts(1, 0)),
        Resolution::RollForward { commit_ts: 20 }
    );

    // And the roll-forward is exactly a secondary commit at that timestamp.
    let token = plan.applied();
    store.apply(&commit_secondary(&store, b"b", &token).unwrap());
    assert_eq!(
        read(&store, b"b", 20).unwrap(),
        ReadOutcome::Value(key(b"vb"))
    );
}

/// The primary was rolled back: roll the secondary back too.
#[test]
fn a_rolled_back_primary_rolls_a_secondary_back() {
    let mut store = MemoryStore::new();
    let lock = locked_secondary(&mut store, 10);
    store.apply(&rollback(&store, b"a", 10).unwrap());

    let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
    assert_eq!(state, PrimaryState::RolledBack);
    assert_eq!(resolve(&lock, &state, ts(1, 0)), Resolution::RollBack);

    store.apply(&rollback(&store, b"b", 10).unwrap());
    assert!(store.is_empty(Cf::Lock));
    assert_eq!(read(&store, b"b", 100).unwrap(), ReadOutcome::NotFound);
}

/// The primary is still locked and inside its lease: wait. Rolling back here would abort a
/// healthy transaction that is simply slower than the reader.
#[test]
fn a_live_primary_makes_a_reader_wait() {
    let mut store = MemoryStore::new();
    lock_key(
        &mut store,
        &Prewrite {
            key: key(b"a"),
            primary: key(b"a"),
            start_ts: ts(1_000, 0),
            read_ts: ts(1_000, 0),
            ttl_ms: LOCK_TTL_MS,
            op: put(b"va"),
        },
    );
    lock_key(
        &mut store,
        &Prewrite {
            key: key(b"b"),
            primary: key(b"a"),
            start_ts: ts(1_000, 0),
            read_ts: ts(1_000, 0),
            ttl_ms: LOCK_TTL_MS,
            op: put(b"vb"),
        },
    );
    let lock = store.get_lock(b"b").unwrap().unwrap();
    let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
    assert!(matches!(state, PrimaryState::Locked(_)));

    // The boundary, both sides. A lock lives for exactly its TTL.
    assert_eq!(
        resolve(&lock, &state, ts(1_000 + LOCK_TTL_MS, MAX_LOGICAL)),
        Resolution::Wait,
        "still inside the lease"
    );
    assert_eq!(
        resolve(&lock, &state, ts(1_000 + LOCK_TTL_MS + 1, 0)),
        Resolution::RollBack,
        "one millisecond past it"
    );
}

/// The TTL that decides is the **primary's**, because that is the lock a live client
/// heartbeats. A secondary's own TTL is irrelevant, and using it would abort a transaction
/// whose owner is demonstrably alive.
#[test]
fn the_primary_s_ttl_decides_not_the_secondary_s() {
    let mut store = MemoryStore::new();
    let start = ts(1_000, 0);
    lock_key(
        &mut store,
        &Prewrite {
            key: key(b"a"),
            primary: key(b"a"),
            start_ts: start,
            read_ts: start,
            // The client heartbeated the primary out to a minute.
            ttl_ms: 60_000,
            op: put(b"va"),
        },
    );
    lock_key(
        &mut store,
        &Prewrite {
            key: key(b"b"),
            primary: key(b"a"),
            start_ts: start,
            read_ts: start,
            ttl_ms: LOCK_TTL_MS,
            op: put(b"vb"),
        },
    );

    let secondary = store.get_lock(b"b").unwrap().unwrap();
    let state = primary_state(&store, &secondary.primary, secondary.start_ts).unwrap();
    // Past the secondary's own 3 s, well inside the primary's 60 s.
    assert_eq!(
        resolve(&secondary, &state, ts(1_000 + 10_000, 0)),
        Resolution::Wait
    );
    assert_eq!(
        resolve(&secondary, &state, ts(1_000 + 60_001, 0)),
        Resolution::RollBack
    );
}

/// A lock whose primary has neither a lock nor a record: nothing will ever commit it.
#[test]
fn a_missing_primary_rolls_back() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"b", b"a", 10, put(b"vb")));
    let lock = store.get_lock(b"b").unwrap().unwrap();

    let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
    assert_eq!(state, PrimaryState::Missing);
    assert_eq!(resolve(&lock, &state, ts(1, 0)), Resolution::RollBack);
}

/// Someone else's lock on our primary means ours is long gone — `Missing`, not `Locked`, or a
/// resolver would sit waiting on a lease that belongs to another transaction.
#[test]
fn another_transaction_s_lock_on_our_primary_reads_as_missing() {
    let mut store = MemoryStore::new();
    lock_key(&mut store, &prewrite(b"a", b"a", 30, put(b"theirs")));
    assert_eq!(
        primary_state(&store, b"a", 10).unwrap(),
        PrimaryState::Missing
    );
}

// -- the whole crash window ---------------------------------------------------------------

/// Percolator's central claim, exercised end to end: a client that dies between prewrite and
/// commit leaves a state that a *later* reader classifies correctly, whichever side of the
/// primary's commit the crash fell on.
#[test]
fn a_crash_at_every_step_resolves_the_way_the_primary_says() {
    // Crashed before the primary committed: everything rolls back.
    {
        let mut store = MemoryStore::new();
        let lock = locked_secondary(&mut store, ts(1_000, 0));
        let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
        // Long past the lease, so the owner is presumed dead.
        assert_eq!(
            resolve(&lock, &state, ts(1_000_000, 0)),
            Resolution::RollBack
        );

        // A resolver rolls the primary back first, then the secondary.
        store.apply(&rollback(&store, b"a", lock.start_ts).unwrap());
        store.apply(&rollback(&store, b"b", lock.start_ts).unwrap());
        assert_eq!(read(&store, b"a", u64::MAX).unwrap(), ReadOutcome::NotFound);
        assert_eq!(read(&store, b"b", u64::MAX).unwrap(), ReadOutcome::NotFound);
    }

    // Crashed after the primary committed but before the secondary did: everything rolls
    // forward, and the TTL never gets a say — the record outranks the lease.
    {
        let mut store = MemoryStore::new();
        let start = ts(1_000, 0);
        let lock = locked_secondary(&mut store, start);
        let CommitDecision::Commit(plan) =
            commit_primary(&store, b"a", start, ts(1_001, 0)).unwrap()
        else {
            panic!("refused");
        };
        store.apply(plan.mutations());
        let token = plan.applied();

        let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
        assert_eq!(
            resolve(&lock, &state, ts(1_000_000, 0)),
            Resolution::RollForward {
                commit_ts: ts(1_001, 0)
            },
            "a committed primary outranks an expired lease"
        );

        store.apply(&commit_secondary(&store, b"b", &token).unwrap());
        assert_eq!(
            read(&store, b"b", u64::MAX).unwrap(),
            ReadOutcome::Value(key(b"vb"))
        );
    }
}

/// Lost update, the anomaly snapshot isolation must prevent: two transactions read the same
/// key at the same snapshot and both write it. One commits; the other cannot.
#[test]
fn snapshot_isolation_prevents_a_lost_update() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"balance", b"100", 5, 8);

    let (t1, t2) = (10, 11);
    assert_eq!(
        read(&store, b"balance", t1).unwrap(),
        ReadOutcome::Value(key(b"100"))
    );
    assert_eq!(
        read(&store, b"balance", t2).unwrap(),
        ReadOutcome::Value(key(b"100"))
    );

    lock_key(
        &mut store,
        &prewrite(b"balance", b"balance", t1, put(b"110")),
    );
    let CommitDecision::Commit(plan) = commit_primary(&store, b"balance", t1, 20).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());

    assert_eq!(
        check_prewrite(&store, &prewrite(b"balance", b"balance", t2, put(b"120"))).unwrap(),
        PrewriteDecision::Conflict { commit_ts: 20 },
        "the second writer must not overwrite a commit it never read"
    );
}

/// Write skew, the anomaly snapshot isolation **allows** — pinned as a test so that the
/// guarantee in `docs/txn-spec.md` §6 is a stated property rather than a hope. Two
/// transactions read the same pair and write disjoint keys; both commit, and the invariant
/// "at least one is on call" is broken.
#[test]
fn snapshot_isolation_allows_write_skew() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"on_call/a", b"yes", 1, 2);
    commit_one(&mut store, b"on_call/b", b"yes", 1, 3);

    let (t1, t2) = (10, 11);
    // Both read both rows and each sees the other still on call.
    for snapshot in [t1, t2] {
        assert_eq!(
            read(&store, b"on_call/a", snapshot).unwrap(),
            ReadOutcome::Value(key(b"yes"))
        );
        assert_eq!(
            read(&store, b"on_call/b", snapshot).unwrap(),
            ReadOutcome::Value(key(b"yes"))
        );
    }

    // Each writes only its own row, so neither prewrite sees a conflict.
    lock_key(
        &mut store,
        &prewrite(b"on_call/a", b"on_call/a", t1, put(b"no")),
    );
    let CommitDecision::Commit(plan) = commit_primary(&store, b"on_call/a", t1, 20).unwrap() else {
        panic!("refused");
    };
    store.apply(plan.mutations());

    lock_key(
        &mut store,
        &prewrite(b"on_call/b", b"on_call/b", t2, put(b"no")),
    );
    let CommitDecision::Commit(plan) = commit_primary(&store, b"on_call/b", t2, 21).unwrap() else {
        panic!("refused: SI is expected to allow this, and §6 says so");
    };
    store.apply(plan.mutations());

    assert_eq!(
        read(&store, b"on_call/a", 30).unwrap(),
        ReadOutcome::Value(key(b"no"))
    );
    assert_eq!(
        read(&store, b"on_call/b", 30).unwrap(),
        ReadOutcome::Value(key(b"no")),
        "write skew is legal under SI; docs/txn-spec.md §6 prices the two ways out"
    );
}

// -- check-then-insert, the shape a unique index is built on -------------------------------

/// Two transactions each read a key, find nothing, and both try to claim it. Exactly one
/// commits — and the loser is refused **before** it writes anything.
///
/// `esker-sql` builds unique-index enforcement on this composition (`docs/txn-spec.md` §6.1):
/// the index entry is the key, a snapshot read proves it absent, and an ordinary `Put` claims
/// it. This is the ordering where the loser arrives *after* the winner has committed, so
/// prewrite's first check catches it directly.
#[test]
fn two_inserts_of_one_new_key_leave_one_winner() {
    let mut store = MemoryStore::new();
    let (winner, loser) = (10, 11);

    // Both read the key at their own snapshot and find nothing. That is the read a unique
    // index does before it claims the entry.
    assert_eq!(
        read(&store, b"index/email/a@b", winner).unwrap(),
        ReadOutcome::NotFound
    );
    assert_eq!(
        read(&store, b"index/email/a@b", loser).unwrap(),
        ReadOutcome::NotFound
    );

    // The winner claims it.
    lock_key(
        &mut store,
        &prewrite(
            b"index/email/a@b",
            b"index/email/a@b",
            winner,
            put(b"row-1"),
        ),
    );
    let CommitDecision::Commit(plan) =
        commit_primary(&store, b"index/email/a@b", winner, 20).unwrap()
    else {
        panic!("refused");
    };
    store.apply(plan.mutations());

    // The loser's claim is refused, and nothing of it is written.
    assert_eq!(
        check_prewrite(
            &store,
            &prewrite(b"index/email/a@b", b"index/email/a@b", loser, put(b"row-2"))
        )
        .unwrap(),
        PrewriteDecision::Conflict { commit_ts: 20 },
        "the second insert of a unique key must lose"
    );
    assert_eq!(store.len(Cf::Lock), 0, "a refused prewrite writes nothing");
    assert_eq!(
        read(&store, b"index/email/a@b", u64::MAX).unwrap(),
        ReadOutcome::Value(key(b"row-1")),
        "the winner's row is the one that stands"
    );
}

/// The same race, in the ordering that takes two round trips: the loser arrives while the
/// winner still holds its lock, so it is told `Locked` rather than refused. It resolves the
/// lock — which rolls the winner forward — and only then does prewrite's *first* check see the
/// commit and refuse it.
///
/// Both orderings matter: a reading of the protocol that only ever tested the second would
/// pass while leaving the first as a lock conflict the caller might retry for ever.
#[test]
fn a_second_insert_that_arrives_before_the_commit_still_loses() {
    let mut store = MemoryStore::new();
    let (winner, loser) = (10, 11);

    lock_key(
        &mut store,
        &prewrite(b"unique/k", b"unique/k", winner, put(b"row-1")),
    );

    // The loser meets the winner's lock. Not a conflict yet — nothing has committed.
    let lock = match check_prewrite(
        &store,
        &prewrite(b"unique/k", b"unique/k", loser, put(b"row-2")),
    )
    .unwrap()
    {
        PrewriteDecision::Locked(lock) => lock,
        other => panic!("expected a lock conflict, got {other:?}"),
    };
    assert_eq!(lock.start_ts, winner);

    // The winner commits; the loser resolves the lock and rolls it forward.
    let CommitDecision::Commit(plan) = commit_primary(&store, b"unique/k", winner, 20).unwrap()
    else {
        panic!("refused");
    };
    store.apply(plan.mutations());
    let state = primary_state(&store, &lock.primary, lock.start_ts).unwrap();
    assert_eq!(
        resolve(&lock, &state, ts(1, 0)),
        Resolution::RollForward { commit_ts: 20 }
    );

    // Now the retry is refused by the write-conflict check, which is the durable answer.
    assert_eq!(
        check_prewrite(
            &store,
            &prewrite(b"unique/k", b"unique/k", loser, put(b"row-2"))
        )
        .unwrap(),
        PrewriteDecision::Conflict { commit_ts: 20 }
    );
    assert_eq!(
        read(&store, b"unique/k", u64::MAX).unwrap(),
        ReadOutcome::Value(key(b"row-1"))
    );
}

/// A `Lock`-kind record — what `SELECT … FOR UPDATE` leaves — is bookkeeping like a rollback
/// marker: **neither a version a read returns nor a commit a later writer is stale against.**
///
/// It read the other way while `Kind::Lock` was reserved and nothing could write one: a record in
/// the `write` column family above a writer's snapshot looked like a commit, and was treated as
/// one. [ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md) made them reachable — one
/// per locked row — and the rule the conflict check exists for is first-committer-wins: *somebody
/// wrote a version of this key after my snapshot, so what I computed is stale*. A lock record says
/// its transaction held the key and wrote **nothing** to it. No value moved, so nothing is stale,
/// and the refusal was a `40001` for a race nobody ran.
///
/// What it cost while it stood, both measured in `tests/cross_node_deadlock.rs`: every completed
/// locking read refused the next write of that row by any transaction older than it, and a
/// deadlock victim was told it had lost a race rather than that it had been killed.
#[test]
fn a_lock_kind_record_is_neither_a_version_nor_a_conflict() {
    let mut store = MemoryStore::new();
    commit_one(&mut store, b"k", b"v", 5, 8);
    store.put_raw(
        Cf::Write,
        esker_txn::key::write(b"k", 20),
        WriteRecord::new(Kind::Lock, 15).encode(),
    );

    assert_eq!(
        read(&store, b"k", 30).unwrap(),
        ReadOutcome::Value(key(b"v")),
        "a lock record is stepped past, like a rollback marker"
    );
    assert!(
        matches!(
            check_prewrite(&store, &prewrite(b"k", b"k", 10, put(b"loser"))).unwrap(),
            PrewriteDecision::Lock(_)
        ),
        "and it is not a commit either: nothing was written, so nothing is stale"
    );
}
