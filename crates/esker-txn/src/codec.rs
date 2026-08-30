//! The two record formats: the value in the `lock` column family and the value in `write`
//! (`docs/txn-spec.md` §3–§4).
//!
//! Hand-rolled, little-endian, LEB128 lengths — the same rules as every other format in Esker
//! (ADR 0002). Both records begin with the one-byte **kind**, and that byte is the format's
//! discriminator: a decoder that meets a kind it does not know refuses the record rather than
//! guessing at the rest, so a later version adds a tag instead of a version field.
//!
//! # Decoding is strict
//!
//! These bytes come off disk. Every one of the following is an error and not a tolerated
//! oddity (`CLAUDE.md` invariant 9): an unknown kind, a `Rollback` in the `lock` CF, a short
//! value on a record whose kind has no value, a presence byte that is neither 0 nor 1, a
//! length that runs past the buffer, a `primary` of zero bytes, and any trailing byte. The
//! encoding is canonical — exactly one byte string per record — which is what lets a golden
//! file mean anything.

use bytes::Bytes;
use esker_base::varint;

use crate::error::{Result, TxnError};

/// Values at or below this length are stored inline in the `lock` and `write` records instead
/// of in the `default` column family, which saves a lookup for small rows
/// (`docs/DESIGN.md` §8).
pub const SHORT_VALUE_MAX_LEN: usize = 255;

/// Default time-to-live of a lock, in milliseconds. A live client extends it by heartbeat; a
/// crashed one lets it expire so another transaction can resolve it (`docs/DESIGN.md` §14).
pub const LOCK_TTL_MS: u64 = 3_000;

/// The presence byte before an inline value: absent.
const SHORT_VALUE_ABSENT: u8 = 0x00;
/// The presence byte before an inline value: present, followed by `len:u8 ++ len bytes`.
const SHORT_VALUE_PRESENT: u8 = 0x01;

/// What a record says happened to a key.
///
/// The tag is one byte on disk, so these values are part of the format
/// (`docs/txn-spec.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Kind {
    /// The transaction wrote a value.
    Put = 1,
    /// The transaction deleted the key.
    Delete = 2,
    /// The transaction was rolled back; the record is a tombstone at `commit_ts == start_ts`.
    Rollback = 3,
    /// The key was locked but not written (`SELECT … FOR UPDATE`-shaped reads). Reserved: no
    /// API writes one yet, and the tag exists so that adding one is not a format change.
    Lock = 4,
}

impl Kind {
    /// Every tag, for exhaustiveness tests and for decoders.
    pub const ALL: [Self; 4] = [Self::Put, Self::Delete, Self::Rollback, Self::Lock];

    /// The wire and disk byte.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The kind for a byte, or `None` for one this format does not define.
    #[must_use]
    pub fn from_u8(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            3 => Some(Self::Rollback),
            4 => Some(Self::Lock),
            _ => None,
        }
    }

    /// Whether a record of this kind may carry an inline value. Only a `Put` has a value at
    /// all, so a short value on anything else is a corrupt record rather than a redundant one.
    #[must_use]
    pub fn carries_a_value(self) -> bool {
        matches!(self, Self::Put)
    }

    /// Whether a reader that meets this record at or below its snapshot has found a version.
    ///
    /// `Rollback` and `Lock` are bookkeeping: a read steps past them to the next older record
    /// (`docs/txn-spec.md` §5.1). `Delete` *is* a version — it says the key is not there.
    #[must_use]
    pub fn is_a_version(self) -> bool {
        matches!(self, Self::Put | Self::Delete)
    }

    /// Name, for errors and logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Put => "Put",
            Self::Delete => "Delete",
            Self::Rollback => "Rollback",
            Self::Lock => "Lock",
        }
    }
}

/// The value in the `lock` column family: one live transaction's claim on one key.
///
/// Unversioned — there is at most one at a time — and deleted by both `Commit` and `Rollback`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecord {
    /// What the transaction will write when it commits. Never [`Kind::Rollback`]: a
    /// rolled-back transaction leaves a marker in `write`, never an entry here.
    pub kind: Kind,
    /// The transaction holding the lock.
    pub start_ts: u64,
    /// How long the lock lives without a heartbeat, in milliseconds from `start_ts`'s
    /// physical part.
    pub ttl_ms: u64,
    /// The **user key** of this transaction's primary — the one key whose `write` record
    /// decides whether the whole transaction committed (`docs/txn-spec.md` §5.5).
    pub primary: Bytes,
    /// The value, when it is short enough to inline. `None` on a `Put` means the value is in
    /// the `default` CF at `start_ts`.
    pub short_value: Option<Bytes>,
}

impl LockRecord {
    /// A lock with the default TTL.
    #[must_use]
    pub fn new(kind: Kind, start_ts: u64, primary: Bytes) -> Self {
        Self {
            kind,
            start_ts,
            ttl_ms: LOCK_TTL_MS,
            primary,
            short_value: None,
        }
    }

    /// Whether this lock is the primary's own.
    #[must_use]
    pub fn is_primary(&self, user_key: &[u8]) -> bool {
        self.primary == user_key
    }

    /// Appends the record's bytes.
    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(self.kind.as_u8());
        varint::put_u64(self.start_ts, out);
        varint::put_u64(self.ttl_ms, out);
        varint::put_u64(self.primary.len() as u64, out);
        out.extend_from_slice(&self.primary);
        put_short_value(self.short_value.as_deref(), out);
    }

    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.primary.len());
        self.encode_to(&mut out);
        out
    }

    /// Reads a record, refusing anything this format does not define.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Reader::new("lock", bytes);
        let kind = input.kind()?;
        if kind == Kind::Rollback {
            // A rollback is a marker in `write`, never a lock. Accepting it here would let a
            // damaged byte turn a tombstone into a live lock that no resolver can classify.
            return Err(TxnError::corrupt("lock", "a rollback is not a lock"));
        }
        let start_ts = input.varint("start_ts")?;
        let ttl_ms = input.varint("ttl_ms")?;
        let primary = input.length_prefixed("primary")?;
        if primary.is_empty() {
            return Err(TxnError::corrupt("lock", "a lock with no primary key"));
        }
        let short_value = input.short_value(kind)?;
        input.finish()?;
        Ok(Self {
            kind,
            start_ts,
            ttl_ms,
            primary,
            short_value,
        })
    }
}

/// The value in the `write` column family, at `commit_ts`: what one transaction did to one
/// key, and how to find its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    /// What happened. A [`Kind::Rollback`] sits at `commit_ts == start_ts`.
    pub kind: Kind,
    /// The transaction that wrote it — the link to the `default` entry, which is versioned by
    /// `start_ts` rather than by the commit timestamp this record is filed under.
    pub start_ts: u64,
    /// The value, when it was short enough to inline.
    pub short_value: Option<Bytes>,
}

impl WriteRecord {
    /// A record with no inline value.
    #[must_use]
    pub fn new(kind: Kind, start_ts: u64) -> Self {
        Self {
            kind,
            start_ts,
            short_value: None,
        }
    }

    /// The marker a rollback leaves behind, at `commit_ts == start_ts`.
    #[must_use]
    pub fn rollback(start_ts: u64) -> Self {
        Self::new(Kind::Rollback, start_ts)
    }

    /// Appends the record's bytes.
    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(self.kind.as_u8());
        varint::put_u64(self.start_ts, out);
        put_short_value(self.short_value.as_deref(), out);
    }

    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        self.encode_to(&mut out);
        out
    }

    /// Reads a record, refusing anything this format does not define.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Reader::new("write", bytes);
        let kind = input.kind()?;
        let start_ts = input.varint("start_ts")?;
        let short_value = input.short_value(kind)?;
        input.finish()?;
        Ok(Self {
            kind,
            start_ts,
            short_value,
        })
    }
}

/// `0x00`, or `0x01 ++ len:u8 ++ bytes`.
///
/// The presence byte is separate from the length because an empty value is a legal value: a
/// caller may `put(k, b"")`, and "absent" and "present, zero bytes" mean different things.
/// A value longer than [`SHORT_VALUE_MAX_LEN`] is a caller error the encoder will not produce
/// — the value belongs in the `default` CF — so it is truncated to absent only in the sense
/// that [`crate::percolator`] never offers one.
fn put_short_value(value: Option<&[u8]>, out: &mut Vec<u8>) {
    match value {
        None => out.push(SHORT_VALUE_ABSENT),
        Some(value) => {
            debug_assert!(
                value.len() <= SHORT_VALUE_MAX_LEN,
                "a value over the cutoff belongs in the default CF, not inline"
            );
            out.push(SHORT_VALUE_PRESENT);
            // Saturating rather than casting: an over-long value is a bug caught by the
            // assert above in a debug build, and must not silently write a wrapped length
            // in a release one.
            out.push(u8::try_from(value.len()).unwrap_or(u8::MAX));
            out.extend_from_slice(&value[..value.len().min(SHORT_VALUE_MAX_LEN)]);
        }
    }
}

/// A cursor over a record's bytes that turns every malformed shape into a typed error.
struct Reader<'a> {
    what: &'static str,
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(what: &'static str, bytes: &'a [u8]) -> Self {
        Self { what, bytes, at: 0 }
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..]
    }

    fn kind(&mut self) -> Result<Kind> {
        let tag = *self
            .rest()
            .first()
            .ok_or_else(|| TxnError::corrupt(self.what, "empty record"))?;
        self.at += 1;
        Kind::from_u8(tag)
            .ok_or_else(|| TxnError::corrupt(self.what, format!("unknown kind {tag}")))
    }

    fn varint(&mut self, field: &'static str) -> Result<u64> {
        let (value, read) = varint::get_u64(self.rest())
            .map_err(|error| TxnError::corrupt(self.what, format!("{field}: {error}")))?;
        self.at += read;
        Ok(value)
    }

    fn length_prefixed(&mut self, field: &'static str) -> Result<Bytes> {
        let len = self.varint(field)?;
        let len = usize::try_from(len).map_err(|_| {
            TxnError::corrupt(self.what, format!("{field}: length {len} overflows"))
        })?;
        let rest = self.rest();
        if rest.len() < len {
            return Err(TxnError::corrupt(
                self.what,
                format!("{field}: {len} bytes wanted, {} left", rest.len()),
            ));
        }
        self.at += len;
        Ok(Bytes::copy_from_slice(&rest[..len]))
    }

    fn short_value(&mut self, kind: Kind) -> Result<Option<Bytes>> {
        let marker = *self
            .rest()
            .first()
            .ok_or_else(|| TxnError::corrupt(self.what, "no short-value marker"))?;
        self.at += 1;
        match marker {
            SHORT_VALUE_ABSENT => Ok(None),
            SHORT_VALUE_PRESENT => {
                if !kind.carries_a_value() {
                    return Err(TxnError::corrupt(
                        self.what,
                        format!("a {} record carries no value", kind.name()),
                    ));
                }
                let len = usize::from(
                    *self
                        .rest()
                        .first()
                        .ok_or_else(|| TxnError::corrupt(self.what, "no short-value length"))?,
                );
                self.at += 1;
                let rest = self.rest();
                if rest.len() < len {
                    return Err(TxnError::corrupt(
                        self.what,
                        format!("short value: {len} bytes wanted, {} left", rest.len()),
                    ));
                }
                self.at += len;
                Ok(Some(Bytes::copy_from_slice(&rest[..len])))
            }
            other => Err(TxnError::corrupt(
                self.what,
                format!("short-value marker {other} is neither absent nor present"),
            )),
        }
    }

    fn finish(self) -> Result<()> {
        let left = self.bytes.len() - self.at;
        if left == 0 {
            Ok(())
        } else {
            Err(TxnError::corrupt(
                self.what,
                format!("{left} trailing bytes"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{Kind, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};

    fn lock() -> LockRecord {
        LockRecord::new(Kind::Put, 42, Bytes::from_static(b"primary"))
    }

    /// These tags are one byte on disk. A duplicate would make two different records decode to
    /// the same thing, and a zero would be indistinguishable from a zeroed page.
    #[test]
    fn kind_tags_are_distinct_and_nonzero() {
        let unique: std::collections::BTreeSet<u8> =
            Kind::ALL.into_iter().map(Kind::as_u8).collect();
        assert_eq!(unique.len(), Kind::ALL.len(), "two kinds share a tag");
        assert!(
            !unique.contains(&0),
            "a zero tag is not tellable from empty space"
        );
        for kind in Kind::ALL {
            assert_eq!(Kind::from_u8(kind.as_u8()), Some(kind));
        }
        assert_eq!(Kind::from_u8(0), None);
        assert_eq!(Kind::from_u8(5), None);
    }

    /// The inline-value cutoff must fit in the single length byte the record format uses.
    #[test]
    fn short_values_fit_in_one_length_byte() {
        assert_eq!(SHORT_VALUE_MAX_LEN, usize::from(u8::MAX));
    }

    #[test]
    fn records_round_trip() {
        for kind in [Kind::Put, Kind::Delete, Kind::Lock] {
            let mut record = LockRecord::new(kind, 7, Bytes::from_static(b"p"));
            record.ttl_ms = 1234;
            assert_eq!(LockRecord::decode(&record.encode()).unwrap(), record);
        }
        for kind in Kind::ALL {
            let record = WriteRecord::new(kind, 9);
            assert_eq!(WriteRecord::decode(&record.encode()).unwrap(), record);
        }
    }

    /// "Absent" and "present, zero bytes" are different records, because `put(k, b"")` is a
    /// legal write and must not read back as a missing value.
    #[test]
    fn an_empty_inline_value_is_not_an_absent_one() {
        let mut with = lock();
        with.short_value = Some(Bytes::new());
        let without = lock();
        assert_ne!(with.encode(), without.encode());
        assert_eq!(LockRecord::decode(&with.encode()).unwrap(), with);
        assert_eq!(LockRecord::decode(&without.encode()).unwrap(), without);
    }

    #[test]
    fn an_inline_value_of_the_maximum_length_round_trips() {
        let mut record = lock();
        record.short_value = Some(Bytes::from(vec![0xab; SHORT_VALUE_MAX_LEN]));
        assert_eq!(LockRecord::decode(&record.encode()).unwrap(), record);
    }

    /// Every malformed shape named in `docs/txn-spec.md` §3, one assertion each.
    #[test]
    fn malformed_records_are_errors() {
        assert!(LockRecord::decode(b"").is_err(), "empty");
        assert!(LockRecord::decode(&[0]).is_err(), "kind 0");
        assert!(LockRecord::decode(&[5]).is_err(), "unknown kind");
        assert!(WriteRecord::decode(&[9]).is_err(), "unknown kind");

        // A rollback is a marker in `write`, never a lock.
        let mut rollback_lock = lock().encode();
        rollback_lock[0] = Kind::Rollback.as_u8();
        assert!(LockRecord::decode(&rollback_lock).is_err(), "rollback lock");
        // ... but it is a perfectly good write record.
        assert!(WriteRecord::decode(&WriteRecord::rollback(7).encode()).is_ok());

        // A value on a kind that has none.
        let mut delete = WriteRecord::new(Kind::Delete, 7);
        delete.short_value = Some(Bytes::from_static(b"v"));
        assert!(
            WriteRecord::decode(&delete.encode()).is_err(),
            "a delete carries no value"
        );

        // A presence byte that is neither.
        let mut bad_marker = WriteRecord::new(Kind::Put, 7).encode();
        *bad_marker.last_mut().unwrap() = 2;
        assert!(WriteRecord::decode(&bad_marker).is_err(), "marker 2");

        // A primary of no bytes.
        let mut empty_primary = lock();
        empty_primary.primary = Bytes::new();
        assert!(
            LockRecord::decode(&empty_primary.encode()).is_err(),
            "a lock with no primary"
        );

        // Truncation at every length, and trailing bytes.
        let good = lock().encode();
        for cut in 0..good.len() {
            assert!(
                LockRecord::decode(&good[..cut]).is_err(),
                "a record cut to {cut} bytes decoded"
            );
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(LockRecord::decode(&trailing).is_err(), "trailing byte");
    }

    /// A `Rollback` and a `Lock` are bookkeeping; a read steps past them. A `Delete` is a
    /// version — it says the key is not there — and skipping it would resurrect an old value.
    #[test]
    fn only_put_and_delete_are_versions() {
        assert!(Kind::Put.is_a_version());
        assert!(Kind::Delete.is_a_version());
        assert!(!Kind::Rollback.is_a_version());
        assert!(!Kind::Lock.is_a_version());
        assert!(Kind::Put.carries_a_value());
        for kind in [Kind::Delete, Kind::Rollback, Kind::Lock] {
            assert!(!kind.carries_a_value(), "{}", kind.name());
        }
    }
}
