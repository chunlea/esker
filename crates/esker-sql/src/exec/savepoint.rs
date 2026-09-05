//! `SAVEPOINT`, `ROLLBACK TO` and `RELEASE`, as a compensating undo log over a write buffer this
//! crate cannot reach into.
//!
//! # Why an undo log and not a truncation
//!
//! A savepoint is usually described as a mark in the transaction's write buffer, and a
//! `ROLLBACK TO` as truncating the buffer back to it. That is not available here.
//! `esker_client::Transaction` does not expose its buffer, and [`crate::backend::Txn`] — the trait
//! this crate actually holds — has exactly four operations on it: `get`, `scan`, `put`, `delete`.
//! There is nothing to truncate.
//!
//! So the undo is **compensating**. While a savepoint is open, every write first reads the key's
//! pre-image *through the same transaction* and records it; a `ROLLBACK TO` replays those
//! backwards, putting each old value back and deleting the keys that had none. The pre-image is
//! what this transaction sees, which is exactly what restoring it has to put back — a value from
//! anywhere else would be a different transaction's idea of the row.
//!
//! It costs one read per write, and only while a savepoint is open. That is the shape a Rails test
//! has (a savepoint per test, a handful of rows) and not the shape a bulk load has, which takes no
//! savepoint and pays nothing.
//!
//! **It cannot leave a write behind**, which is the failure this feature must not have: a
//! `ROLLBACK TO` that missed one would be a wrong answer with no error attached. Every write that
//! happens while the stack is non-empty is recorded by construction, because the recording is the
//! `Txn` the executor is handed rather than something a call site has to remember to do.
//!
//! # Names stack
//!
//! Two `SAVEPOINT dup` are **two marks**, not one rebound. `ROLLBACK TO dup` finds the most recent
//! and `RELEASE dup` releases the most recent, after which `ROLLBACK TO dup` finds the older one.
//! Measured (`tests/corpus/pg19_savepoint.txt`), and a map from name to mark gets it wrong in a way
//! no single-savepoint test can see — which is why this is a `Vec` searched from the back.

use bytes::Bytes;

use crate::backend::Txn;
use crate::error::{Result, SqlError};

/// The most pre-images one block will hold before `53400`, rather than an unbounded allocation on
/// a client's behalf. The same bound, for the same reason, as the sort's and the group table's.
pub(super) const UNDO_LIMIT: usize = 1_000_000;

/// One mark: a name, how far the undo log had got when it was taken, and the session parameters
/// as they stood.
///
/// The parameters are here because a `SET` is transactional on a real server and a `ROLLBACK TO`
/// undoes one — measured, `tests/corpus/pg19_set.txt`. They are a whole copy rather than a delta:
/// there are six of them, a savepoint is rare, and a delta would be a second thing to get right.
#[derive(Debug)]
struct Mark {
    name: String,
    undo_at: usize,
    /// How far `locks` had got when the mark was taken. Separate from `undo_at` because a
    /// `SELECT … FOR UPDATE` locks a row without writing it, so the two logs do not move together.
    locks_at: usize,
    /// The recorded read set as it stood, for the same reason the parameters are here.
    reads: crate::backend::ReadSet,
    /// The session authorization as it stood, so a `SET LOCAL SESSION AUTHORIZATION` inside the
    /// mark is undone by a `ROLLBACK TO` as well as by the transaction ending.
    authorization: Option<String>,
    parameters: Parameters,
}

/// The session's parameters, as [`crate::exec::Executor`] holds them.
pub(super) type Parameters = std::collections::BTreeMap<&'static str, String>;

/// What a block has to remember to undo part of itself.
#[derive(Debug, Default)]
pub(super) struct Savepoints {
    marks: Vec<Mark>,
    /// Every key written since the oldest open mark, with **what the write buffer held for it**
    /// before, oldest first.
    ///
    /// The buffer entry rather than the visible value, and that is the whole of ADR 0057's savepoint
    /// bug: restoring a *value* leaves the key in the write set, so the commit still prewrites a row
    /// the transaction has rolled back and a concurrent commit on it refuses everything.
    ///
    /// A key written twice appears twice, and that is required rather than wasteful: replaying
    /// backwards restores the *earliest* entry last, which is the one that was true at the mark.
    undo: Vec<(Vec<u8>, crate::backend::Buffered)>,
    /// Every key **first** locked since the oldest open mark, oldest first.
    ///
    /// First only: a key the transaction already held before the mark is not recorded, because
    /// rolling back must not give away a lock the outer transaction still needs.
    locks: Vec<Vec<u8>>,
}

impl Savepoints {
    /// Whether anything needs recording. False is the ordinary case and costs a write nothing.
    pub(super) fn recording(&self) -> bool {
        !self.marks.is_empty()
    }

    /// `SAVEPOINT <name>`, with the session parameters it can be rolled back to.
    pub(super) fn savepoint(
        &mut self,
        name: &str,
        reads: crate::backend::ReadSet,
        authorization: Option<String>,
        parameters: Parameters,
    ) {
        self.marks.push(Mark {
            name: name.to_owned(),
            undo_at: self.undo.len(),
            locks_at: self.locks.len(),
            reads,
            authorization,
            parameters,
        });
    }

    /// `RELEASE <name>`: drops the most recent mark of that name and every mark above it, and
    /// touches no data — the writes they cover are kept, which is the whole difference between
    /// this and a `ROLLBACK TO`.
    ///
    /// The undo log is **not** truncated with them: an outer savepoint may still be open, and its
    /// rollback needs the pre-images these marks were sitting on top of.
    pub(super) fn release(&mut self, name: &str) -> Result<()> {
        let at = self.find(name)?;
        self.marks.truncate(at);
        if self.marks.is_empty() {
            // Nothing left to roll back to, so nothing left to remember.
            self.undo.clear();
            self.locks.clear();
        }
        Ok(())
    }

    /// `ROLLBACK TO <name>`: replays the undo log backwards to the most recent mark of that name
    /// and **leaves the mark in place**, so the same savepoint can be rolled back to again.
    ///
    /// Marks above it go, because their writes have just been undone: PostgreSQL does the same,
    /// and a mark pointing into a log that has been truncated past it would be a mark that could
    /// never be reached.
    /// Answers with the session parameters as they stood at the mark, which the caller puts back:
    /// a `SET` inside the savepoint is undone with the writes, exactly as a real server does it.
    pub(super) fn rollback_to(
        &mut self,
        name: &str,
        txn: &mut dyn Txn,
    ) -> Result<(Parameters, Option<String>)> {
        let at = self.find(name)?;
        let undo_at = self.marks[at].undo_at;
        // Backwards, so that a key written more than once lands on the value it had at the mark
        // rather than on the one it had in between.
        while self.undo.len() > undo_at {
            // The loop condition is the bound, so there is always one to take; a `while let` says
            // that to the compiler rather than to a reader.
            let Some((key, before)) = self.undo.pop() else {
                break;
            };
            txn.restore(&key, before);
        }
        // **And the locks the rolled-back statements took.** PostgreSQL gives back a
        // subtransaction's row locks when it aborts; keeping them blocks every other session on
        // those rows for the life of the outer transaction, and leaves this one in a wait-for graph
        // it has already left — which is the `40P01` Rails sees on the statement *after* its
        // `rescue` (`transaction_nested_test.rb:187`).
        let locks_at = self.marks[at].locks_at;
        while self.locks.len() > locks_at {
            let Some(key) = self.locks.pop() else { break };
            txn.unlock(&key);
        }
        // And the reads, which are the other half of what a commit is validated against.
        txn.restore_read_set(self.marks[at].reads.clone());
        self.marks.truncate(at + 1);
        Ok((
            self.marks[at].parameters.clone(),
            self.marks[at].authorization.clone(),
        ))
    }

    /// The block is over: every mark and every pre-image with it.
    pub(super) fn clear(&mut self) {
        self.marks.clear();
        self.undo.clear();
        self.locks.clear();
    }

    /// The most recent mark of that name, or `3B001`.
    fn find(&self, name: &str) -> Result<usize> {
        self.marks
            .iter()
            .rposition(|mark| mark.name == name)
            .ok_or_else(|| SqlError::NoSuchSavepoint(name.to_owned()))
    }

    /// Records one key's pre-image, before it is written over.
    fn record(&mut self, key: &[u8], before: crate::backend::Buffered) -> Result<()> {
        if self.undo.len() == UNDO_LIMIT {
            return Err(SqlError::ConfigurationLimitExceeded(format!(
                "a transaction that has written more than {UNDO_LIMIT} rows since a SAVEPOINT \
                 needs more memory than this node will use; release the savepoint or commit"
            )));
        }
        self.undo.push((key.to_vec(), before));
        Ok(())
    }
}

/// A [`Txn`] that records each key's pre-image before writing over it.
///
/// The executor is handed this instead of the transaction whenever a savepoint is open, which is
/// what makes "every write is recorded" a fact about the type rather than a rule every call site
/// has to follow. `put` and `delete` cannot fail — the trait says so, and the buffer is why — so a
/// failed recording is remembered and reported by [`Recording::finish`] at the end of the
/// statement, which is the first place that can say anything.
#[derive(Debug)]
pub(super) struct Recording<'a> {
    inner: &'a mut dyn Txn,
    savepoints: &'a mut Savepoints,
    /// The first recording failure, if there was one. Only the bound can produce one.
    failed: Option<SqlError>,
}

impl<'a> Recording<'a> {
    pub(super) fn new(inner: &'a mut dyn Txn, savepoints: &'a mut Savepoints) -> Self {
        Recording {
            inner,
            savepoints,
            failed: None,
        }
    }

    /// The recording failure this statement hit, if any.
    pub(super) fn finish(self) -> Result<()> {
        self.failed.map_or(Ok(()), Err)
    }

    /// What the write buffer holds for a key right now, which is what a rollback has to put back.
    ///
    /// **Not the visible value.** `get` merges the buffer over the store and cannot tell "this
    /// transaction had already written this key" from "this key's value comes from the store", and
    /// a rollback that cannot tell those apart re-buffers a write the transaction has abandoned.
    /// This asks the buffer itself, so an absent entry stays absent. It cannot fail, which also
    /// retires the read error this used to have to carry.
    fn before(&mut self, key: &[u8]) -> crate::backend::Buffered {
        self.inner.buffered(key)
    }
}

impl Txn for Recording<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.inner.get(key)
    }

    /// The transaction underneath's answer. A `Recording` is a lens over one, not a second buffer:
    /// what it adds is the pre-image, and a write reaches the inner transaction either way.
    fn has_written(&self) -> bool {
        self.inner.has_written()
    }

    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>> {
        self.inner.scan(start, end, limit)
    }

    // **Forwarded, and the default would have been silent.** A `Txn` method this wrapper does not
    // pass through is a method the statement inside a savepoint does not really call: with the
    // default `lock` the recording took every lock without telling the store, so the transaction
    // *holding* a row never held it and the one waiting never waited. Found by the red test in
    // `tests/read_committed.rs`, which then failed on the wrong side (ADR 0057).
    fn lock(&mut self, key: &[u8]) -> Result<crate::backend::Lock> {
        // Recorded **before** the call, while the answer to "did we already hold this?" is still
        // the honest one: afterwards `Lock::Taken` means "holds it now, or held it already" and
        // cannot tell a fresh lock from one the outer transaction took.
        let fresh = !self.inner.holds(key);
        let taken = self.inner.lock(key)?;
        if fresh && matches!(taken, crate::backend::Lock::Taken) {
            self.savepoints.locks.push(key.to_vec());
        }
        Ok(taken)
    }

    /// The transaction underneath's view, and **not** the default.
    ///
    /// `Txn::locks` has a default, so leaving it out here is not a compile error — it is this
    /// wrapper quietly answering "nothing is held on this node" for every `pg_locks` read taken
    /// while a savepoint is open, which is every statement of a Rails nested `transaction do`. The
    /// one question this view exists to answer is what a stuck session is holding, and that is the
    /// state it is most likely to be stuck in. `tests/pg_locks.rs` asks it either side of one
    /// `SAVEPOINT`.
    fn locks(&self) -> crate::backend::LockView {
        self.inner.locks()
    }

    fn holds(&self, key: &[u8]) -> bool {
        self.inner.holds(key)
    }

    fn unlock(&mut self, key: &[u8]) {
        self.inner.unlock(key);
    }

    fn read_set(&self) -> crate::backend::ReadSet {
        self.inner.read_set()
    }

    fn restore_read_set(&mut self, set: crate::backend::ReadSet) {
        self.inner.restore_read_set(set);
    }

    fn changed_since_statement(&self, key: &[u8]) -> Result<bool> {
        self.inner.changed_since_statement(key)
    }

    fn restart_statement(&mut self) -> Result<()> {
        self.inner.restart_statement()
    }

    fn abandon_locks(&mut self) {
        self.inner.abandon_locks();
    }

    fn begin_statement(&mut self) -> Result<()> {
        self.inner.begin_statement()
    }

    // The two that were missed the *second* time, and they were missed the same way: a `Txn`
    // method with a default is a method this wrapper silently answers for. With `validate_reads`
    // defaulted, a SERIALIZABLE transaction with a savepoint open recorded nothing and validated
    // nothing — `test_SerializationFailure_inside_nested_SavepointTransaction_is_recoverable`, and
    // Rails opens a savepoint for every nested `transaction do` block, so this is not a corner. With
    // `changed_since_statement` defaulted, the statement re-check that replaced a hundred spurious
    // `40001`s did not run there either.
    fn validate_reads(&mut self, on: bool) {
        self.inner.validate_reads(on);
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        if self.failed.is_none() {
            let before = self.before(key);
            if let Err(error) = self.savepoints.record(key, before) {
                self.failed.get_or_insert(error);
            }
        }
        self.inner.put(key, value);
    }

    fn delete(&mut self, key: &[u8]) {
        if self.failed.is_none() {
            let before = self.before(key);
            if let Err(error) = self.savepoints.record(key, before) {
                self.failed.get_or_insert(error);
            }
        }
        self.inner.delete(key);
    }

    fn buffered(&self, key: &[u8]) -> crate::backend::Buffered {
        self.inner.buffered(key)
    }

    /// Straight through, and **deliberately not recorded**: this is the undo being applied, so
    /// writing it into the undo log would be a rollback that has to be rolled back.
    fn restore(&mut self, key: &[u8], prior: crate::backend::Buffered) {
        self.inner.restore(key, prior);
    }

    fn start_ts(&self) -> u64 {
        self.inner.start_ts()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn commit(self: Box<Self>) -> Result<Option<u64>> {
        // Never: the executor holds the real transaction and commits that. A `Recording` is a
        // borrow of one for the length of a statement.
        Err(SqlError::Internal(
            "a recording transaction was committed".to_owned(),
        ))
    }

    fn rollback(self: Box<Self>) -> Result<()> {
        Err(SqlError::Internal(
            "a recording transaction was rolled back".to_owned(),
        ))
    }
}
