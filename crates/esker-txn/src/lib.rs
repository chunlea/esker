//! Percolator-style distributed transactions: optimistic, two-phase, snapshot isolation, over
//! the `lock`, `write` and `default` column families. Timestamps come from the placement
//! driver's oracle and are encoded into keys, so ordering never depends on a node's clock
//! (`docs/DESIGN.md` §8).
//!
//! # Invariants
//!
//! * **The primary key decides.** A transaction is committed exactly when its primary's
//!   `write` record exists. Every reader that meets a lock resolves it by inspecting the
//!   primary — rolling forward if it committed, back if its TTL expired — and never by
//!   guessing.
//! * **Prewrite is atomic per key.** The `lock` entry and the `default` value are written in
//!   one `WriteBatch`, and so are the `write` entry and the lock's removal at commit.
//! * **Timestamps come only from the oracle** (`CLAUDE.md` invariant 6).
//! * **Garbage collection never removes a visible version.** Below the safepoint the newest
//!   visible version of each key survives; everything older may go.
//!
//! Phase 0 contains only the value-kind tags and the lock defaults; transactions are phase 5
//! (`prompts/05-txn.md`).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Kinds of record in the `write` column family (`docs/DESIGN.md` §8). The tag is one byte on
/// disk, so these values are part of the format.
pub mod write_kind {
    /// The transaction wrote a value.
    pub const PUT: u8 = 1;
    /// The transaction deleted the key.
    pub const DELETE: u8 = 2;
    /// The transaction was rolled back; the record is a tombstone for its `start_ts`.
    pub const ROLLBACK: u8 = 3;
    /// The key was only locked, not written (`SELECT … FOR UPDATE`-shaped reads).
    pub const LOCK: u8 = 4;

    /// Every tag, for exhaustiveness tests and decoders.
    pub const ALL: [u8; 4] = [PUT, DELETE, ROLLBACK, LOCK];
}

/// Default time-to-live of a lock, in milliseconds. A live client extends it by heartbeat; a
/// crashed one lets it expire so another transaction can resolve it (`docs/DESIGN.md` §14).
pub const LOCK_TTL_MS: u64 = 3_000;

/// Values at or below this length are stored inline in the `lock` and `write` records instead
/// of in the `default` column family, which saves a lookup for small rows
/// (`docs/DESIGN.md` §8).
pub const SHORT_VALUE_MAX_LEN: usize = 255;

#[cfg(test)]
mod tests {
    use super::{LOCK_TTL_MS, SHORT_VALUE_MAX_LEN, write_kind};

    /// These tags are one byte on disk. A duplicate would make two different records decode
    /// to the same thing, and a zero would be indistinguishable from a zeroed page.
    #[test]
    fn write_kind_tags_are_distinct_and_nonzero() {
        let unique: std::collections::BTreeSet<u8> = write_kind::ALL.into_iter().collect();
        assert_eq!(
            unique.len(),
            write_kind::ALL.len(),
            "two write kinds share a tag"
        );
        assert!(
            !unique.contains(&0),
            "a zero tag cannot be told apart from empty space"
        );
    }

    /// The inline-value cutoff must fit in the single length byte the record format uses.
    #[test]
    fn short_values_fit_in_one_length_byte() {
        assert_eq!(SHORT_VALUE_MAX_LEN, usize::from(u8::MAX));
    }

    /// The TTL has to outlast a heartbeat interval by enough that an ordinary pause does not
    /// look like a crashed client.
    #[test]
    fn lock_ttl_leaves_room_for_a_heartbeat() {
        assert!(LOCK_TTL_MS >= 1_000);
    }
}
