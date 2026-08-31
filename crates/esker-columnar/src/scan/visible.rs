//! MVCC visibility: the newest version of each key that a read at `ts` may see.
//!
//! ADR 0022 Decision 4 evaluates visibility **at read time**, so a columnar learner's runs hold
//! every version exactly as it was committed. Applying it means "the newest version of each key
//! with `commit_ts <= ts`, unless that version is a tombstone".
//!
//! # Why this is a mode and not an `Expr`
//!
//! `commit_ts <= ts` *is* expressible as a filter. **Newest-per-key is not**: it is an argmax over
//! a group, and the fragment format has no node for a windowing operation. Adding one would be a
//! new node tag and a format version bump on a format that has been fuzzed a million and a half
//! times; a mode changes no bytes at all (`docs/plans/phase-8-learner.md`, RULED-2).
//!
//! # One forward pass, because ingestion pays for it
//!
//! Runs are sorted `(key, commit_ts DESC)`, so for any key the rows appear newest-first and the
//! **first** row at or below `ts` is the answer. Everything after it for that key is older and can
//! be skipped without looking. No hashing, no buffering, no second sort — the ordering does the
//! work, which is why the apply target sorts at all.
//!
//! # The trap: pruning must be off
//!
//! Stripe pruning skips a stripe whose statistics say no row in it can match the **filter**. That
//! is sound when the filter is the only thing deciding, and **unsound** here, because visibility
//! decides *which* row is the candidate before the filter ever sees it.
//!
//! Concretely: key `K` has `v2 (name='x', ts=20)` and `v1 (name='y', ts=10)`, the filter is
//! `name = 'y'`, and the read is at `ts=30`. The right answer is that `K` resolves to `v2`, which
//! fails the filter, so `K` contributes nothing. But `v2`'s stripe contains no `'y'`, so pruning
//! drops it — and the scan then sees `v1` first, resolves `K` to it, and **emits a row that was
//! overwritten ten timestamps ago**. A wrong answer with a plausible shape, which is the only kind
//! that matters.
//!
//! So [`Visibility`] turns pruning off for the whole scan. That costs the pruning win on a
//! visible read, and it is the honest price until pruning can be made to reason about the key and
//! timestamp columns rather than the filter's.

use crate::column::Column;
use crate::error::Result;
use crate::reader::Reader;
use crate::value::{Value, ValueRef};

/// How a scan resolves versions, in **file column** indexes rather than projection slots — the
/// columns it needs are the run's own and need not appear in the fragment's projection at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visibility {
    /// The columns forming the key, in key order.
    pub key_columns: Vec<u32>,
    /// The column holding each version's commit timestamp.
    pub ts_column: u32,
    /// The column holding each version's tombstone mark.
    pub deleted_column: u32,
    /// Read as of this instant: a version is a candidate when its timestamp is at or below it.
    pub ts: i64,
}

impl Visibility {
    /// Reads the columns this needs out of one stripe.
    pub(crate) fn columns(&self, reader: &Reader, stripe: usize) -> Result<Vec<Column>> {
        let mut out = Vec::with_capacity(self.key_columns.len() + 2);
        for column in &self.key_columns {
            out.push(reader.read_column(stripe, *column as usize)?);
        }
        out.push(reader.read_column(stripe, self.ts_column as usize)?);
        out.push(reader.read_column(stripe, self.deleted_column as usize)?);
        Ok(out)
    }
}

/// Carries "which key was last resolved" across stripes, because a key's versions may span them.
#[derive(Debug, Default)]
pub(crate) struct Resolver {
    /// The key whose visible version has already been chosen, if any.
    settled: Option<Vec<Value>>,
}

impl Resolver {
    /// Whether this row is the one visible version of its key.
    ///
    /// Called in run order, once per row, before the fragment's filter.
    pub(crate) fn visible(
        &mut self,
        key: &[ValueRef<'_>],
        commit_ts: i64,
        deleted: bool,
        at: i64,
    ) -> bool {
        let same_key = self
            .settled
            .as_ref()
            .is_some_and(|settled| matches_key(key, settled));
        if same_key {
            // This key's visible version was already chosen; everything after it is older.
            return false;
        }
        if commit_ts > at {
            // Written after the read. Not yet a candidate — and not a decision about the key,
            // because an older version of it may still be visible further down.
            return false;
        }
        // The first row at or below `at` for a key it has not settled: this is the one.
        self.settled = Some(key.iter().map(owned).collect());
        !deleted
    }
}

fn matches_key(key: &[ValueRef<'_>], settled: &[Value]) -> bool {
    key.len() == settled.len()
        && key
            .iter()
            .zip(settled)
            .all(|(left, right)| same(left, right))
}

/// Whether two key values are the same key.
///
/// **This must be the ordering the runs are sorted by, and nothing else.** The apply target's seal
/// and the merge both place rows with [`ValueRef::pg_cmp`], so "adjacent" means "`pg_cmp` says
/// equal" — and a resolver using a different notion of sameness would split a key the file
/// considers whole and emit two visible versions of it.
///
/// The case that catches it is `-0.0` against `0.0`: bitwise they differ, `pg_cmp` calls them
/// equal, and an earlier draft of this function compared bits. It would have shown up only for a
/// floating-point key holding both signs of zero — rare, silent, and wrong. `Datum`'s `PartialEq`
/// in `esker-keys` is deliberately bitwise for the opposite reason (storage asks whether bytes
/// survived); this is the SQL question, so it asks the SQL comparison.
fn same(left: &ValueRef<'_>, right: &Value) -> bool {
    left.pg_cmp(&right.as_ref()) == std::cmp::Ordering::Equal
}

fn owned(value: &ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Int(int) => Value::Int8(*int),
        ValueRef::Bool(flag) => Value::Bool(*flag),
        ValueRef::Double(double) => Value::Double(*double),
        ValueRef::Bytes(bytes) => Value::Bytea((*bytes).to_vec()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Resolver, Visibility};
    use crate::value::{Value, ValueRef};

    /// Rows arrive `(key, commit_ts DESC)`, so this is what the scan actually feeds the resolver.
    fn key(id: i64) -> Vec<ValueRef<'static>> {
        vec![ValueRef::Int(id)]
    }

    /// The newest version at or below the read, and nothing else for that key.
    #[test]
    fn the_first_version_at_or_below_the_read_wins_and_the_rest_are_skipped() {
        let mut resolver = Resolver::default();
        // key 1: versions 30, 20, 10 — reading at 25.
        assert!(!resolver.visible(&key(1), 30, false, 25), "30 > 25");
        assert!(resolver.visible(&key(1), 20, false, 25), "20 is the newest");
        assert!(!resolver.visible(&key(1), 10, false, 25), "10 is shadowed");
    }

    /// A tombstone settles its key and reports nothing. The key is *decided*; it just has no row.
    #[test]
    fn a_tombstone_settles_the_key_and_hides_what_is_under_it() {
        let mut resolver = Resolver::default();
        assert!(
            !resolver.visible(&key(1), 20, true, 25),
            "a delete shows nothing"
        );
        assert!(
            !resolver.visible(&key(1), 10, false, 25),
            "the row under a delete must stay hidden"
        );
    }

    /// And a read *before* the delete still finds the row, which is the whole reason a delete is
    /// stored as a version rather than as an absence.
    #[test]
    fn a_read_before_the_delete_still_sees_the_row() {
        let mut resolver = Resolver::default();
        assert!(
            !resolver.visible(&key(1), 20, true, 15),
            "the delete is in the future"
        );
        assert!(
            resolver.visible(&key(1), 10, false, 15),
            "the row is still there"
        );
    }

    /// A version newer than the read does not settle the key — an older one may still be visible,
    /// and treating "seen this key" as "decided this key" would hide it.
    #[test]
    fn a_version_after_the_read_does_not_settle_the_key() {
        let mut resolver = Resolver::default();
        assert!(!resolver.visible(&key(1), 99, false, 50));
        assert!(
            resolver.visible(&key(1), 10, false, 50),
            "a future version swallowed the visible one"
        );
    }

    /// Keys are independent, and the resolver carries its decision across the boundary between
    /// them exactly once.
    #[test]
    fn each_key_settles_on_its_own() {
        let mut resolver = Resolver::default();
        assert!(resolver.visible(&key(1), 20, false, 25));
        assert!(!resolver.visible(&key(1), 10, false, 25));
        assert!(resolver.visible(&key(2), 20, false, 25));
        assert!(!resolver.visible(&key(2), 10, false, 25));
    }

    /// A key of several columns compares as a whole, and without owning anything on the way.
    #[test]
    fn a_composite_key_compares_on_every_column() {
        let mut resolver = Resolver::default();
        let left = vec![ValueRef::Int(1), ValueRef::Bytes(b"a")];
        let right = vec![ValueRef::Int(1), ValueRef::Bytes(b"b")];
        assert!(resolver.visible(&left, 20, false, 25));
        assert!(!resolver.visible(&left, 10, false, 25));
        assert!(
            resolver.visible(&right, 10, false, 25),
            "a different second column is a different key"
        );
    }

    /// A `Text` key round-trips through the resolver's owned copy as bytes, so it must still
    /// compare equal on the next row — the owned form is `Bytea` and the borrowed one is `Bytes`.
    #[test]
    fn a_text_key_still_matches_its_owned_copy() {
        let mut resolver = Resolver::default();
        let k = vec![ValueRef::Bytes(b"abc")];
        assert!(resolver.visible(&k, 20, false, 25));
        assert!(
            !resolver.visible(&k, 10, false, 25),
            "a text key did not match itself across rows"
        );
    }

    /// Key sameness follows `pg_cmp`, because that is what the runs are sorted by. `-0.0` and
    /// `0.0` are one key to the sort, so they must be one key here — a bitwise comparison would
    /// split them and report two visible versions of one row.
    #[test]
    fn a_key_is_the_same_key_the_sort_thinks_it_is() {
        let mut resolver = Resolver::default();
        assert!(resolver.visible(&[ValueRef::Double(0.0)], 20, false, 25));
        assert!(
            !resolver.visible(&[ValueRef::Double(-0.0)], 10, false, 25),
            "-0.0 and 0.0 are one key to pg_cmp, so they must be one key here"
        );
        // And NaN, which `pg_cmp` also calls equal to itself.
        let mut resolver = Resolver::default();
        assert!(resolver.visible(&[ValueRef::Double(f64::NAN)], 20, false, 25));
        assert!(!resolver.visible(&[ValueRef::Double(f64::NAN)], 10, false, 25));
    }

    #[test]
    fn visibility_names_the_columns_it_needs() {
        let visibility = Visibility {
            key_columns: vec![0],
            ts_column: 2,
            deleted_column: 3,
            ts: 5,
        };
        assert_eq!(visibility.key_columns.len(), 1);
        assert_eq!(
            visibility,
            Visibility {
                key_columns: vec![0],
                ts_column: 2,
                deleted_column: 3,
                ts: 5,
            }
        );
    }

    /// Nothing here allocates a key until a key is settled, which is the property that keeps the
    /// resolver off the hot path's allocator. Asserted by construction rather than by measurement:
    /// `same` compares borrowed against owned.
    #[test]
    fn comparing_a_key_does_not_require_owning_it() {
        assert!(super::same(&ValueRef::Int(7), &Value::Int8(7)));
        assert!(super::same(
            &ValueRef::Bytes(b"x"),
            &Value::Text("x".into())
        ));
        assert!(super::same(
            &ValueRef::Bytes(b"x"),
            &Value::Bytea(b"x".to_vec())
        ));
        assert!(!super::same(&ValueRef::Int(7), &Value::Int8(8)));
        assert!(!super::same(&ValueRef::Null, &Value::Int8(7)));
    }
}
