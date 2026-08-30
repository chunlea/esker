//! Internal keys, entry kinds and the comparator seam.
//!
//! Everything the engine stores is keyed by an **internal key**: the caller's key with an
//! 8-byte tag appended.
//!
//! ```text
//! internal key = user_key ++ tag:u64 little-endian
//! tag          = (seqno << 8) | kind          seqno is 56 bits, kind is 8
//! order        = user key ascending (by the user comparator), then tag DESCENDING
//! ```
//!
//! This is `LevelDB`'s layout, byte for byte, and both halves of it matter.
//!
//! * The tag is stored **little-endian**, so the kind byte physically precedes the sequence
//!   number. Nothing compares tags as bytes — they are decoded and compared as integers — so
//!   the byte order is a format detail, but it is a *frozen* one.
//! * The tag orders **descending** so that the newest version of a key sorts first. A read at
//!   snapshot `s` seeks to `(user_key, pack_tag(s, KIND_FOR_SEEK))` and the first entry it
//!   lands on is already the answer: the newest version with `seqno <= s`. That single
//!   property is why a point read is one seek rather than a scan.
//!
//! Nothing here interprets a user key. Tenants, tables and MVCC suffixes live in `esker-keys`
//! and above (invariant 7); to this crate a key is bytes with an order.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use crate::options::PrefixExtractor;

/// The engine's sequence number: one per write batch, ordering everything.
pub type SeqNo = u64;

/// The largest representable sequence number. The tag spends its low 8 bits on the kind, so
/// 56 bits are left — 72 quadrillion writes, which no instance will reach.
pub const MAX_SEQNO: SeqNo = (1 << 56) - 1;

/// Bytes a tag occupies at the end of every internal key.
pub const TAG_LEN: usize = 8;

/// What a memtable or SST entry does to its key.
///
/// The discriminants are on disk in every WAL record, memtable entry and SST. Changing one is
/// a format change (ADR + format version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EntryKind {
    /// The key has no value at and above this sequence number.
    Delete = 0,
    /// The key takes the entry's value.
    Put = 1,
    /// Every key in `[key, value)` is deleted. Subject to the v1 limitation of
    /// `docs/DESIGN.md` §4.7 — range tombstones proper arrive in phase 5.
    DeleteRange = 2,
}

impl EntryKind {
    /// The numerically largest kind. [`KIND_FOR_SEEK`] is derived from it.
    pub const MAX: Self = Self::DeleteRange;

    /// The on-disk byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes a kind byte. An unknown byte is `None` — corruption, or a file written by a
    /// newer format — and never a panic (invariant 9).
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Delete),
            1 => Some(Self::Put),
            2 => Some(Self::DeleteRange),
            _ => None,
        }
    }
}

/// The kind used when building a key to *seek* to rather than to store.
///
/// It must be the largest kind: with tags ordered descending, a seek target carrying the
/// largest kind sorts at or before every real entry with the same sequence number, so the
/// seek cannot skip one. The `kinds_never_exceed_the_seek_kind` test pins that.
pub const KIND_FOR_SEEK: u8 = EntryKind::MAX as u8;

/// Builds a tag from a sequence number and a kind.
///
/// `seqno` is truncated to 56 bits. A caller above [`MAX_SEQNO`] is a bug, caught by a debug
/// assertion; in release the result stays well-formed rather than corrupting the kind.
pub fn pack_tag(seqno: SeqNo, kind: EntryKind) -> u64 {
    debug_assert!(seqno <= MAX_SEQNO, "sequence number overflowed 56 bits");
    ((seqno & MAX_SEQNO) << 8) | u64::from(kind.as_u8())
}

/// The sequence number carried by a tag.
pub fn tag_seqno(tag: u64) -> SeqNo {
    tag >> 8
}

/// The kind carried by a tag, or `None` if the byte is not a kind we know.
pub fn tag_kind(tag: u64) -> Option<EntryKind> {
    // The kind is the low byte, which is also the first byte of the little-endian tag.
    EntryKind::from_u8(tag.to_le_bytes()[0])
}

/// Appends `user_key ++ tag` to `out`.
pub fn append_internal_key(user_key: &[u8], seqno: SeqNo, kind: EntryKind, out: &mut Vec<u8>) {
    out.reserve(user_key.len() + TAG_LEN);
    out.extend_from_slice(user_key);
    out.extend_from_slice(&pack_tag(seqno, kind).to_le_bytes());
}

/// Builds `user_key ++ tag` as a fresh allocation.
pub fn internal_key(user_key: &[u8], seqno: SeqNo, kind: EntryKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(user_key.len() + TAG_LEN);
    append_internal_key(user_key, seqno, kind, &mut out);
    out
}

/// The key to seek with to find the newest version of `user_key` visible at `snapshot`.
pub fn lookup_key(user_key: &[u8], snapshot: SeqNo) -> Vec<u8> {
    let mut out = Vec::with_capacity(user_key.len() + TAG_LEN);
    out.extend_from_slice(user_key);
    let tag = ((snapshot & MAX_SEQNO) << 8) | u64::from(KIND_FOR_SEEK);
    out.extend_from_slice(&tag.to_le_bytes());
    out
}

/// The user key inside an internal key.
///
/// A slice too short to hold a tag cannot come from this engine, but it can come from a
/// corrupt file, so it is returned whole rather than panicking. Callers that need to *trust*
/// the key use [`split_internal_key`], which reports the malformed case.
pub fn extract_user_key(internal: &[u8]) -> &[u8] {
    match internal.len().checked_sub(TAG_LEN) {
        Some(n) => &internal[..n],
        None => internal,
    }
}

/// The tag inside an internal key, or `None` if it is too short to have one.
pub fn extract_tag(internal: &[u8]) -> Option<u64> {
    let n = internal.len().checked_sub(TAG_LEN)?;
    let bytes: [u8; TAG_LEN] = internal[n..].try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Splits an internal key into its three parts, or `None` if it is malformed — too short, or
/// carrying a kind byte this build does not know.
pub fn split_internal_key(internal: &[u8]) -> Option<(&[u8], SeqNo, EntryKind)> {
    let tag = extract_tag(internal)?;
    let kind = tag_kind(tag)?;
    Some((extract_user_key(internal), tag_seqno(tag), kind))
}

/// The total order the engine sorts by.
///
/// One instance is chosen per column family at creation and recorded in the manifest by
/// [`name`](Comparator::name); reopening with a different comparator is an error rather than
/// a silently unsorted database.
pub trait Comparator: Send + Sync + fmt::Debug {
    /// Orders two keys. Must be a total order, and must agree with `name`.
    fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering;

    /// A stable identifier written into the manifest. Changing the order a name refers to
    /// silently corrupts every database that used it.
    fn name(&self) -> &'static str;

    /// Shortens `start` in place to any key in `[start, limit)`, for SST index separators.
    ///
    /// A shorter separator is a smaller index block; a *wrong* one is a lost key. The default
    /// leaves `start` alone, which is always correct, so an implementation may ignore this.
    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]) {
        let _ = (start, limit);
    }

    /// Shortens `key` in place to any key `>= key`, for the last index entry. The default
    /// leaves it alone.
    fn find_short_successor(&self, key: &mut Vec<u8>) {
        let _ = key;
    }
}

/// Plain `memcmp` order: the default, and what every reserved key layout in
/// `docs/DESIGN.md` §3 is designed for.
#[derive(Debug, Clone, Copy, Default)]
pub struct BytewiseComparator;

impl Comparator for BytewiseComparator {
    fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn name(&self) -> &'static str {
        "esker.BytewiseComparator"
    }

    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]) {
        let common = start
            .iter()
            .zip(limit.iter())
            .take_while(|(a, b)| a == b)
            .count();
        // If one key is a prefix of the other there is nothing shorter in between.
        if common >= start.len() || common >= limit.len() {
            return;
        }
        let byte = start[common];
        if byte < u8::MAX && byte + 1 < limit[common] {
            start[common] = byte + 1;
            start.truncate(common + 1);
        }
    }

    fn find_short_successor(&self, key: &mut Vec<u8>) {
        for i in 0..key.len() {
            if key[i] != u8::MAX {
                key[i] += 1;
                key.truncate(i + 1);
                return;
            }
        }
        // All 0xff: no shorter successor exists, so keep the key.
    }
}

/// Orders internal keys: user key ascending by the wrapped comparator, then tag descending.
#[derive(Debug, Clone)]
pub struct InternalKeyComparator {
    user: Arc<dyn Comparator>,
}

impl InternalKeyComparator {
    /// Wraps a user comparator.
    pub fn new(user: Arc<dyn Comparator>) -> Self {
        Self { user }
    }

    /// The user comparator underneath. The manifest records *its* name, since that is the
    /// order the caller chose.
    pub fn user_comparator(&self) -> &Arc<dyn Comparator> {
        &self.user
    }
}

impl Comparator for InternalKeyComparator {
    fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        match (extract_tag(a), extract_tag(b)) {
            (Some(tag_a), Some(tag_b)) => {
                match self.user.cmp(extract_user_key(a), extract_user_key(b)) {
                    Ordering::Equal => tag_b.cmp(&tag_a), // descending: newest first
                    other => other,
                }
            }
            // A key too short to hold a tag is corrupt. Fall back to a byte comparison so the
            // order stays total and nothing panics; the decode path reports the corruption.
            _ => a.cmp(b),
        }
    }

    fn name(&self) -> &'static str {
        "esker.InternalKeyComparator"
    }

    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]) {
        let mut shortened = extract_user_key(start).to_vec();
        let user_start_len = shortened.len();
        self.user
            .find_shortest_separator(&mut shortened, extract_user_key(limit));
        // Only take it if the user key got physically shorter *and* logically larger; then the
        // largest possible tag keeps it below every real entry with that user key.
        if shortened.len() < user_start_len
            && self.user.cmp(extract_user_key(start), &shortened) == Ordering::Less
        {
            let tag = (MAX_SEQNO << 8) | u64::from(KIND_FOR_SEEK);
            shortened.extend_from_slice(&tag.to_le_bytes());
            *start = shortened;
        }
    }

    fn find_short_successor(&self, key: &mut Vec<u8>) {
        let mut shortened = extract_user_key(key).to_vec();
        let user_len = shortened.len();
        self.user.find_short_successor(&mut shortened);
        if shortened.len() < user_len
            && self.user.cmp(extract_user_key(key), &shortened) == Ordering::Less
        {
            let tag = (MAX_SEQNO << 8) | u64::from(KIND_FOR_SEEK);
            shortened.extend_from_slice(&tag.to_le_bytes());
            *key = shortened;
        }
    }
}

/// Applies a user-level prefix extractor to the user key inside an internal key.
///
/// SSTs store internal keys, so a bloom filter built over them straight would be built over
/// `user_key ++ tag` — and every version of a key has a different tag, so a point read could
/// never probe it. `LevelDB` solves this with an `InternalFilterPolicy` that strips the tag
/// first; this is the same idea, expressed as a prefix extractor so that a column family's own
/// extractor composes with it.
///
/// With no user extractor the filter is over whole user keys. With one — "strip the 8-byte
/// MVCC timestamp", for the versioned column families of `docs/DESIGN.md` §3 — the filter
/// answers "does this user key exist at any version", which is the question a seek asks.
#[derive(Debug, Clone)]
pub struct InternalPrefixExtractor {
    user: Option<Arc<dyn PrefixExtractor>>,
    name: String,
}

impl InternalPrefixExtractor {
    /// Wraps `user`, or extracts the whole user key when there is none.
    pub fn new(user: Option<Arc<dyn PrefixExtractor>>) -> Self {
        let name = match &user {
            Some(user) => format!("esker.Internal({})", user.name()),
            None => "esker.Internal".to_string(),
        };
        Self { user, name }
    }
}

impl PrefixExtractor for InternalPrefixExtractor {
    fn prefix<'a>(&self, key: &'a [u8]) -> &'a [u8] {
        let user_key = extract_user_key(key);
        match &self.user {
            Some(user) => user.prefix(user_key),
            None => user_key,
        }
    }

    fn in_domain(&self, key: &[u8]) -> bool {
        if key.len() < TAG_LEN {
            return false;
        }
        let user_key = extract_user_key(key);
        self.user
            .as_ref()
            .is_none_or(|user| user.in_domain(user_key))
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, InternalPrefixExtractor,
        KIND_FOR_SEEK, MAX_SEQNO, PrefixExtractor, TAG_LEN, extract_tag, extract_user_key,
        internal_key, lookup_key, pack_tag, split_internal_key, tag_kind, tag_seqno,
    };
    use std::cmp::Ordering;
    use std::sync::Arc;

    fn ikc() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    /// The physical layout is frozen: the kind is the first byte after the user key, because
    /// the tag is little-endian. A golden test in `tests/` pins the same bytes from outside.
    #[test]
    fn internal_key_layout_is_little_endian_tag() {
        let key = internal_key(b"abc", 0x0102_0304_0506, EntryKind::Put);
        assert_eq!(
            key, b"abc\x01\x06\x05\x04\x03\x02\x01\x00",
            "user_key ++ (kind, seqno little-endian)"
        );
        assert_eq!(key.len(), 3 + TAG_LEN);
    }

    #[test]
    fn tag_round_trips() {
        for seqno in [0, 1, 42, MAX_SEQNO] {
            for kind in [EntryKind::Delete, EntryKind::Put, EntryKind::DeleteRange] {
                let tag = pack_tag(seqno, kind);
                assert_eq!(tag_seqno(tag), seqno);
                assert_eq!(tag_kind(tag), Some(kind));
            }
        }
    }

    #[test]
    fn split_reports_malformed_keys_instead_of_panicking() {
        assert!(split_internal_key(b"").is_none());
        assert!(split_internal_key(b"short").is_none());
        // A tag whose kind byte is not a kind we know: a newer format, or corruption.
        let mut key = internal_key(b"k", 7, EntryKind::Put);
        let last = key.len() - TAG_LEN;
        key[last] = 0x7f;
        assert!(split_internal_key(&key).is_none());
        // The lenient accessors still answer, and still do not panic.
        assert_eq!(extract_user_key(b"short"), b"short");
        assert!(extract_tag(b"short").is_none());
    }

    /// The property the whole read path rests on: newest version first within a user key.
    #[test]
    fn internal_order_is_user_ascending_then_seqno_descending() {
        let c = ikc();
        let a1 = internal_key(b"a", 1, EntryKind::Put);
        let a2 = internal_key(b"a", 2, EntryKind::Put);
        let b1 = internal_key(b"b", 1, EntryKind::Put);
        assert_eq!(c.cmp(&a2, &a1), Ordering::Less, "seqno 2 sorts before 1");
        assert_eq!(c.cmp(&a1, &b1), Ordering::Less, "user key a before b");
        assert_eq!(c.cmp(&a1, &a1), Ordering::Equal);
        // Same seqno: the larger kind sorts first, which is what makes KIND_FOR_SEEK work.
        let put = internal_key(b"a", 3, EntryKind::Put);
        let del = internal_key(b"a", 3, EntryKind::Delete);
        assert_eq!(c.cmp(&put, &del), Ordering::Less);
    }

    /// A seek key must sort at or before every stored entry with `seqno <= snapshot`, and
    /// after every entry that is newer than the snapshot.
    #[test]
    fn lookup_key_lands_on_the_newest_visible_version() {
        let c = ikc();
        let seek = lookup_key(b"a", 5);
        for (seqno, kind, expected) in [
            (6, EntryKind::Put, Ordering::Less), // newer than the snapshot: before the seek
            (5, EntryKind::Put, Ordering::Greater), // visible: at or after the seek
            (5, EntryKind::DeleteRange, Ordering::Equal),
            (1, EntryKind::Delete, Ordering::Greater),
        ] {
            let stored = internal_key(b"a", seqno, kind);
            assert_eq!(c.cmp(&stored, &seek), expected, "seqno {seqno} {kind:?}");
        }
    }

    /// If a kind is ever added above `KIND_FOR_SEEK`, seeks silently start skipping entries.
    #[test]
    fn kinds_never_exceed_the_seek_kind() {
        for kind in [EntryKind::Delete, EntryKind::Put, EntryKind::DeleteRange] {
            assert!(
                kind.as_u8() <= KIND_FOR_SEEK,
                "{kind:?} outranks the seek kind"
            );
        }
        assert_eq!(KIND_FOR_SEEK, EntryKind::MAX.as_u8());
    }

    #[test]
    fn unknown_kind_bytes_decode_to_none() {
        assert_eq!(EntryKind::from_u8(0), Some(EntryKind::Delete));
        assert_eq!(EntryKind::from_u8(2), Some(EntryKind::DeleteRange));
        for byte in 3..=u8::MAX {
            assert_eq!(EntryKind::from_u8(byte), None, "byte {byte}");
        }
    }

    #[test]
    fn bytewise_separator_shortens_only_when_it_is_safe() {
        let c = BytewiseComparator;
        let mut start = b"abcdefg".to_vec();
        c.find_shortest_separator(&mut start, b"abzz");
        assert_eq!(start, b"abd", "one byte past the common prefix");

        // `abc` -> `abd` would equal the limit, so there is no room: leave it alone.
        let mut start = b"abcdefg".to_vec();
        c.find_shortest_separator(&mut start, b"abd");
        assert_eq!(start, b"abcdefg");

        // A prefix relationship leaves no room, so the key must not move.
        let mut start = b"ab".to_vec();
        c.find_shortest_separator(&mut start, b"abcd");
        assert_eq!(start, b"ab");

        // Adjacent bytes leave no room either.
        let mut start = b"a".to_vec();
        c.find_shortest_separator(&mut start, b"b");
        assert_eq!(start, b"a");
    }

    #[test]
    fn bytewise_successor_handles_all_ones() {
        let c = BytewiseComparator;
        let mut key = b"abc".to_vec();
        c.find_short_successor(&mut key);
        assert_eq!(key, b"b");

        let mut key = vec![0xff, 0xff];
        c.find_short_successor(&mut key);
        assert_eq!(key, vec![0xff, 0xff], "no shorter successor exists");
    }

    /// A shortened separator must still sit strictly between the two internal keys, or the
    /// SST index would point past a key that exists.
    #[test]
    fn internal_separator_stays_between_its_bounds() {
        let c = ikc();
        let start = internal_key(b"abcdefg", 100, EntryKind::Put);
        let limit = internal_key(b"abzz", 200, EntryKind::Put);
        let mut sep = start.clone();
        c.find_shortest_separator(&mut sep, &limit);
        assert!(sep.len() < start.len(), "it should have shortened");
        assert_eq!(c.cmp(&start, &sep), Ordering::Less);
        assert_eq!(c.cmp(&sep, &limit), Ordering::Less);

        // Nothing to shorten: the separator must be left exactly alone.
        let start = internal_key(b"a", 1, EntryKind::Put);
        let limit = internal_key(b"b", 1, EntryKind::Put);
        let mut sep = start.clone();
        c.find_shortest_separator(&mut sep, &limit);
        assert_eq!(sep, start);
    }

    #[test]
    fn internal_successor_is_greater_than_its_key() {
        let c = ikc();
        let key = internal_key(b"abc", 9, EntryKind::Put);
        let mut succ = key.clone();
        c.find_short_successor(&mut succ);
        assert_eq!(c.cmp(&key, &succ), Ordering::Less);
        assert_eq!(extract_user_key(&succ), b"b");
    }

    /// A filter over internal keys would be a filter over `user_key ++ tag`, which a point
    /// read could never probe: every version has a different tag.
    #[test]
    fn the_internal_extractor_strips_the_tag() {
        let extractor = InternalPrefixExtractor::new(None);
        let key = internal_key(b"user", 42, EntryKind::Put);
        assert!(extractor.in_domain(&key));
        assert_eq!(extractor.prefix(&key), b"user");
        assert_eq!(
            extractor.prefix(&internal_key(b"user", 7, EntryKind::Delete)),
            extractor.prefix(&key),
            "every version of a key must probe the same filter entry"
        );
        assert!(!extractor.in_domain(b"short"), "not an internal key");
    }

    /// A column family's own extractor composes: the tag comes off, then its suffix.
    #[test]
    fn the_internal_extractor_composes_with_a_user_one() {
        let user = Arc::new(crate::options::StripSuffix::new(8));
        let extractor = InternalPrefixExtractor::new(Some(user));
        let versioned = [b"user".as_slice(), &[0u8; 8]].concat();
        let key = internal_key(&versioned, 42, EntryKind::Put);
        assert!(extractor.in_domain(&key));
        assert_eq!(extractor.prefix(&key), b"user");
        assert!(
            extractor.name().contains("StripSuffix.8"),
            "{}",
            extractor.name()
        );
    }

    #[test]
    fn comparator_names_are_stable() {
        assert_eq!(BytewiseComparator.name(), "esker.BytewiseComparator");
        assert_eq!(ikc().name(), "esker.InternalKeyComparator");
        assert_eq!(ikc().user_comparator().name(), "esker.BytewiseComparator");
    }
}
