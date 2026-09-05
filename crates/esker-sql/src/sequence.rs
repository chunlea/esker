//! The node's reserved sequence blocks — one allocator for every session it serves.
//!
//! # Why the node and not the connection
//!
//! `nextval` reserves [`crate::catalog::SEQUENCE_BATCH`] values in a transaction of its own and
//! hands them out from memory, so a durable counter bump costs one write per batch instead of one
//! per row. The counter is a single key, and one bump per row would make every concurrent insert
//! into a table contend on it across the cluster — which in this system means a Raft round trip
//! per row to whichever store holds that key.
//!
//! The block used to live on the `Executor`, and an `Executor` is **one per connection**. That is
//! invisible to a client with one connection and glaring to one that pools: `ActiveRecord`'s
//! default pool is five, so five inserts land on five connections and five blocks —
//! `1, 33, 65, 97, 129` where PostgreSQL gives `1, 2, 3, 4, 5`. Measured by r1 against a live
//! node, and `tests/real_backend.rs` pins the other half: on *one* connection the ids already were
//! consecutive. Nothing about the allocator was wrong; the scope it was held at was
//! ([ADR 0072](../../../docs/adr/0072-a-sequence-block-belongs-to-the-node-not-to-the-connection.md)).
//!
//! One [`Blocks`] per node, joined the way [`crate::advisory::Locks`] is: `Sessions` in
//! `bin/esker-sql.rs` holds it and hands each executor an `Arc`, and an executor built without one
//! gets a private allocator — which is right for a single-session test and is what keeps the
//! corpus replay honest.
//!
//! **The gap that is left is a cross-node one**, and that is the whole of the declared divergence
//! now: a client whose pool spans two SQL processes sees two blocks, exactly as `CACHE 32` on a
//! real server would across two backends that each cached.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// One sequence's reserved run: the next value to hand out, and the first value past the block.
type Block = (i64, i64);

/// Every block this node holds, by tenant and sequence.
///
/// **Keyed by the tenant too**, because a node serves every database in the cluster and two
/// tenants' sequences are two counters that may share an id.
#[derive(Debug, Default)]
pub struct Blocks {
    held: Mutex<BTreeMap<(u64, u64), Block>>,
}

impl Blocks {
    /// The next value of one sequence, taking a fresh block through `allocate` if none is left.
    ///
    /// **The allocation happens under the lock**, which is the point rather than an oversight: two
    /// sessions racing on one sequence take *one* batch between them, where releasing the lock to
    /// allocate would let both take one and burn 32 values. `allocate` is one small transaction
    /// against one key, and nothing inside it takes another lock of this node's.
    pub fn next(
        &self,
        tenant: u64,
        sequence_id: u64,
        batch: u64,
        allocate: impl FnOnce() -> crate::Result<i64>,
    ) -> crate::Result<i64> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((next, end)) = held.get_mut(&(tenant, sequence_id))
            && *next < *end
        {
            let value = *next;
            *next += 1;
            return Ok(value);
        }
        let first = allocate()?;
        let batch = i64::try_from(batch).unwrap_or(i64::MAX);
        held.insert(
            (tenant, sequence_id),
            (first + 1, first.saturating_add(batch)),
        );
        Ok(first)
    }

    /// Drops one sequence's block, so the next `nextval` re-reads the stored counter.
    ///
    /// `TRUNCATE … RESTART IDENTITY` needs it — resetting the key while a block is in memory would
    /// hand out values from inside a run that no longer means anything — and so does `DROP TABLE`,
    /// which is a leak rather than a wrong value: a relation id is never reused, so a re-created
    /// sequence is a different sequence. At node scope the leak would be shared by every session.
    pub fn forget(&self, tenant: u64, sequence_id: u64) {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(tenant, sequence_id));
    }

    /// How many blocks this node holds for one tenant — for the tests that a dropped sequence's
    /// block goes with it.
    #[must_use]
    pub fn held_for(&self, tenant: u64) -> usize {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|(held, _)| *held == tenant)
            .count()
    }

    /// The value this sequence would hand out next, if a block is held — what `currval` reads back
    /// through the session that took it.
    #[must_use]
    pub fn last_taken(&self, tenant: u64, sequence_id: u64) -> Option<i64> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(tenant, sequence_id))
            .map(|(next, _)| next - 1)
    }

    /// Puts one sequence's block back to a single known value — `setval`'s half of the pair.
    pub fn set(&self, tenant: u64, sequence_id: u64, value: i64) {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((tenant, sequence_id), (value + 1, value + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::Blocks;

    /// **Consecutive across sessions, which is the whole change**: two callers on one `Blocks`
    /// share the run, where two `Executor`s used to take a block each.
    #[test]
    fn two_sessions_on_one_node_share_a_block() {
        let blocks = Blocks::default();
        // One `allocate` answer for all five: the second call onwards must serve from the block
        // rather than ask again, which is what a per-connection allocator stopped doing the
        // moment the connection changed.
        let taken: Vec<i64> = (0..5)
            .map(|_| blocks.next(1, 7, 32, || Ok(1)).unwrap())
            .collect();
        assert_eq!(taken, [1, 2, 3, 4, 5]);
    }

    /// A block is per **tenant** and per sequence, so neither shares with the other.
    #[test]
    fn a_block_is_per_tenant_and_per_sequence() {
        let blocks = Blocks::default();
        assert_eq!(blocks.next(1, 7, 32, || Ok(1)).unwrap(), 1);
        assert_eq!(blocks.next(1, 7, 32, || Ok(1)).unwrap(), 2);
        // A different tenant with the same sequence id allocates its own.
        assert_eq!(blocks.next(2, 7, 32, || Ok(100)).unwrap(), 100);
        assert_eq!(blocks.next(2, 7, 32, || Ok(100)).unwrap(), 101);
        // And the first tenant's run is untouched.
        assert_eq!(blocks.next(1, 7, 32, || Ok(1)).unwrap(), 3);
        assert_eq!(blocks.held_for(1), 1);
        assert_eq!(blocks.held_for(2), 1);
    }

    /// The batch runs out and the next call allocates again, starting where the store says.
    #[test]
    fn an_exhausted_block_asks_for_another() {
        let blocks = Blocks::default();
        for want in 1..=4 {
            assert_eq!(blocks.next(1, 7, 4, || Ok(1)).unwrap(), want);
        }
        assert_eq!(blocks.next(1, 7, 4, || Ok(5)).unwrap(), 5);
    }

    /// Forgetting one drops it, and the count is what the leak test reads.
    #[test]
    fn a_forgotten_block_is_gone() {
        let blocks = Blocks::default();
        assert_eq!(blocks.next(1, 7, 32, || Ok(1)).unwrap(), 1);
        assert_eq!(blocks.held_for(1), 1);
        blocks.forget(1, 7);
        assert_eq!(blocks.held_for(1), 0);
        // And the next call starts from whatever the store now says.
        assert_eq!(blocks.next(1, 7, 32, || Ok(1)).unwrap(), 1);
    }
}
