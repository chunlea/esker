//! What a decision in [`crate::percolator`] asks the store to write: a `WriteBatch` in the
//! shape this crate can build without linking the engine.
//!
//! Every list here is meant to be applied **atomically** — one `WriteBatch`, one Raft
//! proposal. That is not a hint. A prewrite that landed its `default` value but not its `lock`
//! is a value no reader can see and no resolver can clean up; a commit that wrote its `write`
//! record but left the lock is a key that reads as locked for ever. `docs/DESIGN.md` §4.8
//! makes a cross-CF batch atomic, and `CLAUDE.md` invariant 1 makes it durable before the
//! acknowledgement; between them the list below is safe to hand to a caller as one unit.
//!
//! The keys are **engine keys** — namespaced and versioned by [`crate::key`] — so the store's
//! handler is a loop over this list and not a second implementation of the layout.

use crate::codec::{LockRecord, WriteRecord};

/// Which column family a mutation lands in.
///
/// The names match `esker_engine::cf`, which is where they are defined; the enum exists so
/// that `esker-txn` can name a column family without linking the engine — it is a library of
/// decisions and has no database of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cf {
    /// Percolator locks, one live entry per locked key. Unversioned.
    Lock,
    /// Commit records, keyed by user key and commit timestamp.
    Write,
    /// User values, keyed by user key and the writing transaction's `start_ts`.
    Default,
}

impl Cf {
    /// The engine's name for this column family.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Write => "write",
            Self::Default => "default",
        }
    }
}

/// One entry of a write batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Write `value` at `key`.
    Put {
        /// Where it goes.
        cf: Cf,
        /// The engine key — namespaced, and versioned where the CF is.
        key: Vec<u8>,
        /// The encoded record, or the raw value in the `default` CF.
        value: Vec<u8>,
    },
    /// Remove `key`.
    Delete {
        /// Where it goes.
        cf: Cf,
        /// The engine key.
        key: Vec<u8>,
    },
}

impl Mutation {
    /// Which column family this touches.
    #[must_use]
    pub fn cf(&self) -> Cf {
        match self {
            Self::Put { cf, .. } | Self::Delete { cf, .. } => *cf,
        }
    }

    /// The engine key.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key, .. } => key,
        }
    }
}

/// An ordered list of mutations to apply as one batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mutations {
    entries: Vec<Mutation>,
}

impl Mutations {
    /// An empty list — a decision that asks for nothing to be written.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there is nothing to apply. An empty list is a legitimate answer: it is what an
    /// idempotent retry of an operation that already happened returns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many mutations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The mutations, in the order they must be applied.
    #[must_use]
    pub fn as_slice(&self) -> &[Mutation] {
        &self.entries
    }

    /// Iterates the mutations.
    pub fn iter(&self) -> std::slice::Iter<'_, Mutation> {
        self.entries.iter()
    }

    /// Appends every mutation of `other`, in order.
    ///
    /// One request may decide several keys — a `Prewrite` batch, a `Commit` of many — and the
    /// whole lot has to reach the engine as one atomic batch, so the per-key lists are merged
    /// rather than applied one at a time.
    pub fn extend(&mut self, other: Self) {
        self.entries.extend(other.entries);
    }

    /// Stages a lock record at `user_key`. Public because a `Heartbeat` rewrites a lock in
    /// place rather than deciding anything, so it has no decision function to come from.
    pub fn put_lock_record(&mut self, user_key: &[u8], record: &LockRecord) {
        self.put_lock(user_key, record);
    }

    pub(crate) fn put(&mut self, cf: Cf, key: Vec<u8>, value: Vec<u8>) {
        self.entries.push(Mutation::Put { cf, key, value });
    }

    pub(crate) fn delete(&mut self, cf: Cf, key: Vec<u8>) {
        self.entries.push(Mutation::Delete { cf, key });
    }

    pub(crate) fn put_lock(&mut self, user_key: &[u8], record: &LockRecord) {
        self.put(Cf::Lock, crate::key::lock(user_key), record.encode());
    }

    pub(crate) fn delete_lock(&mut self, user_key: &[u8]) {
        self.delete(Cf::Lock, crate::key::lock(user_key));
    }

    pub(crate) fn put_write(&mut self, user_key: &[u8], commit_ts: u64, record: &WriteRecord) {
        self.put(
            Cf::Write,
            crate::key::write(user_key, commit_ts),
            record.encode(),
        );
    }

    pub(crate) fn put_value(&mut self, user_key: &[u8], start_ts: u64, value: &[u8]) {
        self.put(
            Cf::Default,
            crate::key::value(user_key, start_ts),
            value.to_vec(),
        );
    }

    pub(crate) fn delete_value(&mut self, user_key: &[u8], start_ts: u64) {
        self.delete(Cf::Default, crate::key::value(user_key, start_ts));
    }
}

impl IntoIterator for Mutations {
    type Item = Mutation;
    type IntoIter = std::vec::IntoIter<Mutation>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Mutations {
    type Item = &'a Mutation;
    type IntoIter = std::slice::Iter<'a, Mutation>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Cf;

    /// The names are the engine's, and a mismatch would write a lock into the wrong column
    /// family — where nothing looks for it.
    #[test]
    fn the_column_family_names_are_the_engine_s() {
        assert_eq!(Cf::Lock.name(), "lock");
        assert_eq!(Cf::Write.name(), "write");
        assert_eq!(Cf::Default.name(), "default");
    }
}
