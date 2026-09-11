//! Range tombstones: `[begin, end)` deleted at one sequence number
//! (`docs/DESIGN.md` §4.7, [ADR 0017](../../../docs/adr/0017-range-tombstones.md)).
//!
//! A point delete is an entry in the sorted run, and the read path finds it because it sits
//! exactly where the key it hides sits. A range delete cannot work that way: it hides keys
//! that may not exist yet and keys it has never seen, so it is stored *beside* the run rather
//! than in it — a list in the memtable, and a block in every table a flush or a compaction
//! writes.
//!
//! # The rule, in one line
//!
//! A key found at sequence number `s` is hidden from a read at snapshot `t` when some
//! tombstone covers it with `s < tombstone.seqno <= t`.
//!
//! Both bounds matter. `tombstone.seqno > s` is what makes a write *after* a range delete
//! survive it — otherwise `delete_range(a, z)` would swallow every later write to that range
//! for ever. `tombstone.seqno <= t` is what keeps a snapshot taken before the delete from
//! seeing it, which is the same rule every other entry lives under.
//!
//! # Why a tombstone is never in the run
//!
//! Putting `[begin, end)` at `begin` in the sorted run is the v1 behaviour
//! `docs/DESIGN.md` §4.7 refuses: it deletes one key while telling the caller a range is gone.
//! Every read path here therefore asks two questions — "what is the newest entry for this
//! key?" and "does a tombstone hide it?" — and the second is what this module answers.

use std::cmp::Ordering;

use esker_base::varint;

use crate::dbformat::{Comparator, MAX_SEQNO, SeqNo};
use crate::error::{Error, Result};

/// One deleted range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeTombstone {
    /// Inclusive lower bound.
    pub begin: Vec<u8>,
    /// Exclusive upper bound. Always strictly above `begin`: an empty or inverted range is a
    /// caller error rather than a no-op, because the two readings differ and only one of them
    /// is what the caller meant.
    pub end: Vec<u8>,
    /// The sequence number the delete was assigned. A key written at or above it survives.
    pub seqno: SeqNo,
}

impl RangeTombstone {
    /// A tombstone over `[begin, end)`.
    #[must_use]
    pub fn new(begin: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>, seqno: SeqNo) -> Self {
        Self {
            begin: begin.into(),
            end: end.into(),
            seqno,
        }
    }

    /// Whether `key` is inside `[begin, end)` under `comparator`.
    #[must_use]
    pub fn contains(&self, key: &[u8], comparator: &dyn Comparator) -> bool {
        comparator.cmp(&self.begin, key) != Ordering::Greater
            && comparator.cmp(key, &self.end) == Ordering::Less
    }

    /// Whether this tombstone hides an entry at `entry_seqno` for a reader at `snapshot`.
    ///
    /// The two bounds are the whole rule: strictly above the entry, at or below the snapshot.
    #[must_use]
    pub fn hides(&self, entry_seqno: SeqNo, snapshot: SeqNo) -> bool {
        self.seqno > entry_seqno && self.seqno <= snapshot
    }
}

/// The tombstones one source holds — a memtable, or one table — kept sorted.
///
/// Sorted by `(begin, seqno)` under the comparator, which makes the encoding canonical and so
/// makes a golden file mean something. Nothing depends on the order at read time: a covering
/// check is a scan, because the count per source is small by construction — a range delete is
/// an administrative act (`DROP TABLE`, a GC sweep), not something a write path emits per key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeTombstones {
    entries: Vec<RangeTombstone>,
}

impl RangeTombstones {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are none. The common case, and the one the read path short-circuits on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The tombstones, in sorted order.
    #[must_use]
    pub fn as_slice(&self) -> &[RangeTombstone] {
        &self.entries
    }

    /// Iterates them.
    pub fn iter(&self) -> std::slice::Iter<'_, RangeTombstone> {
        self.entries.iter()
    }

    /// Adds one, keeping the set sorted.
    pub fn push(&mut self, tombstone: RangeTombstone, comparator: &dyn Comparator) {
        let at = self
            .entries
            .partition_point(|existing| order(existing, &tombstone, comparator) == Ordering::Less);
        self.entries.insert(at, tombstone);
    }

    /// Merges another set in.
    pub fn extend(&mut self, other: &Self, comparator: &dyn Comparator) {
        for tombstone in &other.entries {
            self.push(tombstone.clone(), comparator);
        }
    }

    /// Whether any tombstone here hides an entry for `key` at `entry_seqno`, read at
    /// `snapshot`.
    #[must_use]
    pub fn hides(
        &self,
        key: &[u8],
        entry_seqno: SeqNo,
        snapshot: SeqNo,
        comparator: &dyn Comparator,
    ) -> bool {
        self.entries
            .iter()
            .any(|t| t.hides(entry_seqno, snapshot) && t.contains(key, comparator))
    }

    /// The largest sequence number of any tombstone covering `key` at or below `snapshot`.
    ///
    /// What a compaction wants: an entry survives only if its own sequence number is at or
    /// above this.
    #[must_use]
    pub fn newest_covering(
        &self,
        key: &[u8],
        snapshot: SeqNo,
        comparator: &dyn Comparator,
    ) -> Option<SeqNo> {
        self.entries
            .iter()
            .filter(|t| t.seqno <= snapshot && t.contains(key, comparator))
            .map(|t| t.seqno)
            .max()
    }

    /// The lowest `begin` and the highest `end` across every tombstone.
    ///
    /// A table's key bounds have to be widened to cover its tombstones, or a read for a key
    /// inside a deleted range would never open the file that says so
    /// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)).
    #[must_use]
    pub fn key_bounds(&self, comparator: &dyn Comparator) -> Option<(&[u8], &[u8])> {
        let mut bounds: Option<(&[u8], &[u8])> = None;
        for tombstone in &self.entries {
            bounds = Some(match bounds {
                None => (&tombstone.begin, &tombstone.end),
                Some((low, high)) => (
                    if comparator.cmp(&tombstone.begin, low) == Ordering::Less {
                        &tombstone.begin
                    } else {
                        low
                    },
                    if comparator.cmp(&tombstone.end, high) == Ordering::Greater {
                        &tombstone.end
                    } else {
                        high
                    },
                ),
            });
        }
        bounds
    }

    /// The block payload: `count ++ (begin ++ end ++ seqno)*`, all length-prefixed LEB128.
    ///
    /// Written through the ordinary block framing, so it carries the same trailer, checksum
    /// and optional compression as every other block in a table
    /// (`crates/esker-engine/src/sst/footer.rs`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 * self.entries.len() + 8);
        varint::put_u64(self.entries.len() as u64, &mut out);
        for tombstone in &self.entries {
            put_bytes(&tombstone.begin, &mut out);
            put_bytes(&tombstone.end, &mut out);
            varint::put_u64(tombstone.seqno, &mut out);
        }
        out
    }

    /// The fewest bytes one encoded tombstone can occupy: an empty `begin`, an empty `end` and a
    /// one-byte sequence number, each a single LEB128 byte.
    ///
    /// Only ever used to refuse an impossible count before it becomes an allocation. Such an entry
    /// would itself be refused a moment later — `begin` is not below `end` — which is why this is a
    /// floor on the *encoding* and not a claim about valid tombstones.
    const MIN_ENCODED_ENTRY: usize = 3;

    /// Reads a block payload, refusing anything this format does not define.
    ///
    /// These bytes come off disk, so every malformed shape is an error and never a panic
    /// (`CLAUDE.md` invariant 9): a truncated buffer, a length past the end, a sequence number
    /// above the 56-bit ceiling, an empty or inverted range, entries out of order, and any
    /// trailing byte.
    pub fn decode(payload: &[u8], comparator: &dyn Comparator) -> Result<Self> {
        let mut cursor = Cursor::new(payload);
        let count = cursor.varint("count")?;
        let count = usize::try_from(count)
            .map_err(|_| corrupt(format!("range tombstone count {count} overflows")))?;
        // A count is not an allocation request: each entry costs **at least three bytes** — an
        // empty `begin`, an empty `end` and a one-byte sequence number — so a count the payload
        // could not hold is corruption caught before the `Vec` is sized.
        //
        // Divided by that three, and it is what the sentence above always claimed. Comparing the
        // count against the payload's whole length accepts one three times too large, so a
        // megabyte of damage could ask for three megabytes of `RangeTombstone` before the first
        // short read refused it. The bound is not the difference between working and panicking; it
        // is the difference between a bad block costing its own size and costing a multiple of it.
        if count > payload.len() / Self::MIN_ENCODED_ENTRY {
            return Err(corrupt(format!(
                "range tombstone count {count} cannot fit in {} bytes",
                payload.len()
            )));
        }
        let mut entries: Vec<RangeTombstone> = Vec::with_capacity(count);
        for index in 0..count {
            let begin = cursor.bytes("begin")?.to_vec();
            let end = cursor.bytes("end")?.to_vec();
            let seqno = cursor.varint("seqno")?;
            if seqno > MAX_SEQNO {
                return Err(corrupt(format!(
                    "range tombstone sequence number {seqno} is above the 56-bit ceiling"
                )));
            }
            if comparator.cmp(&begin, &end) != Ordering::Less {
                return Err(corrupt(
                    "a range tombstone whose end is not above its begin covers nothing".to_owned(),
                ));
            }
            let tombstone = RangeTombstone { begin, end, seqno };
            if let Some(previous) = entries.last()
                && order(previous, &tombstone, comparator) != Ordering::Less
            {
                return Err(corrupt(format!(
                    "range tombstone {index} is not above the one before it; the block is \
                     sorted, so this is damage rather than a different writer"
                )));
            }
            entries.push(tombstone);
        }
        cursor.finish()?;
        Ok(Self { entries })
    }
}

impl<'a> IntoIterator for &'a RangeTombstones {
    type Item = &'a RangeTombstone;
    type IntoIter = std::slice::Iter<'a, RangeTombstone>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl FromIterator<RangeTombstone> for RangeTombstones {
    /// Collects tombstones **already in order**. Used by decoders and by tests; the ordinary
    /// way in is [`RangeTombstones::push`], which sorts.
    fn from_iter<I: IntoIterator<Item = RangeTombstone>>(iter: I) -> Self {
        Self {
            entries: iter.into_iter().collect(),
        }
    }
}

/// The sort order of the block: by `begin`, then by sequence number.
fn order(a: &RangeTombstone, b: &RangeTombstone, comparator: &dyn Comparator) -> Ordering {
    comparator
        .cmp(&a.begin, &b.begin)
        .then_with(|| a.seqno.cmp(&b.seqno))
        .then_with(|| comparator.cmp(&a.end, &b.end))
}

fn put_bytes(value: &[u8], out: &mut Vec<u8>) {
    varint::put_u64(value.len() as u64, out);
    out.extend_from_slice(value);
}

fn corrupt(detail: String) -> Error {
    Error::corruption("range tombstone block", detail)
}

/// A cursor that turns every malformed shape into a typed error.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..]
    }

    fn varint(&mut self, field: &'static str) -> Result<u64> {
        let (value, read) =
            varint::get_u64(self.rest()).map_err(|error| corrupt(format!("{field}: {error}")))?;
        self.at += read;
        Ok(value)
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8]> {
        let len = self.varint(field)?;
        let len = usize::try_from(len)
            .map_err(|_| corrupt(format!("{field}: length {len} overflows")))?;
        let rest = self.rest();
        if rest.len() < len {
            return Err(corrupt(format!(
                "{field}: {len} bytes wanted, {} left",
                rest.len()
            )));
        }
        self.at += len;
        Ok(&rest[..len])
    }

    fn finish(self) -> Result<()> {
        let left = self.bytes.len() - self.at;
        if left == 0 {
            Ok(())
        } else {
            Err(corrupt(format!("{left} trailing bytes")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RangeTombstone, RangeTombstones};
    use crate::dbformat::BytewiseComparator;

    fn cmp() -> BytewiseComparator {
        BytewiseComparator
    }

    fn set(entries: &[(&[u8], &[u8], u64)]) -> RangeTombstones {
        let mut set = RangeTombstones::new();
        for (begin, end, seqno) in entries {
            set.push(RangeTombstone::new(*begin, *end, *seqno), &cmp());
        }
        set
    }

    /// The two bounds of the rule, one assertion each. A tombstone hides what was written
    /// before it and not what was written after — otherwise one `delete_range` would swallow
    /// every later write to that range for ever.
    #[test]
    fn a_tombstone_hides_only_what_was_written_before_it() {
        let tombstone = RangeTombstone::new(b"a".to_vec(), b"m".to_vec(), 50);
        assert!(tombstone.hides(49, 100), "written before the delete");
        assert!(!tombstone.hides(50, 100), "written at the same seqno");
        assert!(!tombstone.hides(51, 100), "written after the delete");
    }

    /// And a snapshot older than the delete does not see it, exactly as for a point delete.
    #[test]
    fn a_snapshot_older_than_the_delete_does_not_see_it() {
        let tombstone = RangeTombstone::new(b"a".to_vec(), b"m".to_vec(), 50);
        assert!(tombstone.hides(10, 50), "the snapshot is at the delete");
        assert!(!tombstone.hides(10, 49), "the snapshot predates it");
    }

    /// `[begin, end)`: the lower bound is in, the upper bound is out.
    #[test]
    fn the_range_is_half_open() {
        let tombstone = RangeTombstone::new(b"b".to_vec(), b"d".to_vec(), 1);
        assert!(!tombstone.contains(b"a", &cmp()));
        assert!(tombstone.contains(b"b", &cmp()), "begin is inside");
        assert!(tombstone.contains(b"c", &cmp()));
        assert!(!tombstone.contains(b"d", &cmp()), "end is outside");
        assert!(!tombstone.contains(b"e", &cmp()));
    }

    #[test]
    fn the_newest_covering_tombstone_wins() {
        let tombstones = set(&[(b"a", b"z", 10), (b"b", b"c", 30), (b"a", b"z", 20)]);
        assert_eq!(tombstones.newest_covering(b"b", 100, &cmp()), Some(30));
        assert_eq!(tombstones.newest_covering(b"b", 25, &cmp()), Some(20));
        assert_eq!(tombstones.newest_covering(b"y", 100, &cmp()), Some(20));
        assert_eq!(tombstones.newest_covering(b"zz", 100, &cmp()), None);
    }

    /// A table's bounds must cover its tombstones, or a read for a key inside a deleted range
    /// would never open the file that says so.
    #[test]
    fn the_key_bounds_span_every_tombstone() {
        let tombstones = set(&[(b"m", b"p", 1), (b"a", b"c", 2)]);
        assert_eq!(tombstones.key_bounds(&cmp()), Some((&b"a"[..], &b"p"[..])));
        assert_eq!(RangeTombstones::new().key_bounds(&cmp()), None);
    }

    #[test]
    fn tombstones_round_trip() {
        let tombstones = set(&[(b"a", b"m", 5), (b"", b"\xff", 7), (b"a", b"z", 9)]);
        let bytes = tombstones.encode();
        let decoded = RangeTombstones::decode(&bytes, &cmp()).unwrap();
        assert_eq!(decoded, tombstones);
        assert_eq!(decoded.encode(), bytes, "the encoding is canonical");
    }

    #[test]
    fn an_empty_set_encodes_to_one_byte() {
        let bytes = RangeTombstones::new().encode();
        assert_eq!(bytes, vec![0]);
        assert!(RangeTombstones::decode(&bytes, &cmp()).unwrap().is_empty());
    }

    /// Every malformed shape is an error, never a panic and never a plausible-looking set
    /// built out of noise.
    #[test]
    fn a_malformed_block_is_an_error() {
        assert!(RangeTombstones::decode(b"", &cmp()).is_err(), "empty");
        // A count far past what the bytes could hold is not an allocation request.
        assert!(
            RangeTombstones::decode(&[0xff, 0xff, 0xff, 0x7f], &cmp()).is_err(),
            "absurd count"
        );

        let good = set(&[(b"a", b"m", 5), (b"b", b"c", 6)]).encode();
        for cut in 0..good.len() {
            assert!(
                RangeTombstones::decode(&good[..cut], &cmp()).is_err(),
                "a block cut to {cut} bytes decoded"
            );
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(
            RangeTombstones::decode(&trailing, &cmp()).is_err(),
            "trailing"
        );

        // An inverted range covers nothing, so it is damage rather than a range.
        let inverted =
            RangeTombstones::from_iter([RangeTombstone::new(b"m".to_vec(), b"a".to_vec(), 1)]);
        assert!(RangeTombstones::decode(&inverted.encode(), &cmp()).is_err());
        let empty_range =
            RangeTombstones::from_iter([RangeTombstone::new(b"a".to_vec(), b"a".to_vec(), 1)]);
        assert!(RangeTombstones::decode(&empty_range.encode(), &cmp()).is_err());

        // Out of order is damage too: the block is written sorted.
        let unsorted = RangeTombstones::from_iter([
            RangeTombstone::new(b"m".to_vec(), b"z".to_vec(), 1),
            RangeTombstone::new(b"a".to_vec(), b"c".to_vec(), 1),
        ]);
        assert!(RangeTombstones::decode(&unsorted.encode(), &cmp()).is_err());
    }

    /// `push` sorts, so a set built in any order encodes to the same bytes.
    #[test]
    fn the_order_a_set_is_built_in_does_not_change_its_bytes() {
        let forwards = set(&[(b"a", b"c", 1), (b"a", b"c", 2), (b"m", b"z", 1)]);
        let backwards = set(&[(b"m", b"z", 1), (b"a", b"c", 2), (b"a", b"c", 1)]);
        assert_eq!(forwards.encode(), backwards.encode());
    }
}
