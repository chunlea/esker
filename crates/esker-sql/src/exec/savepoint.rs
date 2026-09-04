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
    parameters: Parameters,
}

/// The session's parameters, as [`crate::exec::Executor`] holds them.
pub(super) type Parameters = std::collections::BTreeMap<&'static str, String>;

/// What a block has to remember to undo part of itself.
#[derive(Debug, Default)]
pub(super) struct Savepoints {
    marks: Vec<Mark>,
    /// Every key written since the oldest open mark, with what it held before, oldest first.
    ///
    /// A key written twice appears twice, and that is required rather than wasteful: replaying
    /// backwards restores the *earliest* pre-image last, which is the one that was true at the
    /// mark.
    undo: Vec<(Vec<u8>, Option<Bytes>)>,
}

impl Savepoints {
    /// Whether anything needs recording. False is the ordinary case and costs a write nothing.
    pub(super) fn recording(&self) -> bool {
        !self.marks.is_empty()
    }

    /// `SAVEPOINT <name>`, with the session parameters it can be rolled back to.
    pub(super) fn savepoint(&mut self, name: &str, parameters: Parameters) {
        self.marks.push(Mark {
            name: name.to_owned(),
            undo_at: self.undo.len(),
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
    pub(super) fn rollback_to(&mut self, name: &str, txn: &mut dyn Txn) -> Result<Parameters> {
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
            match before {
                Some(value) => txn.put(&key, &value),
                None => txn.delete(&key),
            }
        }
        self.marks.truncate(at + 1);
        Ok(self.marks[at].parameters.clone())
    }

    /// The block is over: every mark and every pre-image with it.
    pub(super) fn clear(&mut self) {
        self.marks.clear();
        self.undo.clear();
    }

    /// The most recent mark of that name, or `3B001`.
    fn find(&self, name: &str) -> Result<usize> {
        self.marks
            .iter()
            .rposition(|mark| mark.name == name)
            .ok_or_else(|| SqlError::NoSuchSavepoint(name.to_owned()))
    }

    /// Records one key's pre-image, before it is written over.
    fn record(&mut self, key: &[u8], before: Option<Bytes>) -> Result<()> {
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

    /// The pre-image of a key, as **this transaction** sees it — its own buffered writes merged in,
    /// which is what makes restoring it correct rather than approximately correct.
    fn before(&mut self, key: &[u8]) -> Option<Bytes> {
        match self.inner.get(key) {
            Ok(before) => before,
            // A read that failed here is a read the statement is about to fail on anyway, and
            // recording `None` would make a rollback *delete* a row that is there. Remember the
            // error and record nothing; `finish` reports it.
            Err(error) => {
                self.failed.get_or_insert(error);
                None
            }
        }
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
        self.inner.lock(key)
    }

    fn restart_statement(&mut self) -> Result<()> {
        self.inner.restart_statement()
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
