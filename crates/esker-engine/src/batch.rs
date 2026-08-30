//! The unit of atomicity: an ordered list of edits, applied all or not at all.
//!
//! ```text
//! batch = seqno:u64 ++ count:u32 ++ entry[count]        (header is 12 bytes, little-endian)
//! entry = cf:varint ++ kind:u8 ++ key:varint-prefixed [++ value:varint-prefixed]
//! ```
//!
//! A batch is its own serialisation. It is built as bytes, written to the log as bytes, and
//! replayed from those same bytes at recovery — there is no second encoder to disagree with
//! the first, which is how a format drifts (`docs/DESIGN.md` §4.3).
//!
//! `Delete` carries no value. `DeleteRange` carries the exclusive end of the range as its
//! value, so `[key, value)` is the range, which keeps the entry shape uniform.
//!
//! # One sequence number per entry
//!
//! The header holds the batch's **base** sequence number; entry `i` is stored at `base + i`,
//! and a batch of `n` entries consumes `n` of them. That is `LevelDB`'s rule and it exists for
//! one case: a batch that writes the same key twice. Given a single sequence number for the
//! whole batch, those two edits would produce the same internal key and the winner would be
//! whichever the memtable happened to keep. Increasing them makes the later edit win, which is
//! what "an ordered list of edits" has to mean. Atomicity is unaffected, because a snapshot is
//! only ever taken at a batch boundary.

use esker_base::varint;

use crate::dbformat::{EntryKind, SeqNo};
use crate::error::{Error, Result};

/// `seqno:u64 ++ count:u32`, both little-endian.
pub const HEADER_SIZE: usize = 12;

/// One edit inside a batch, with the sequence number it will be stored at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Which column family the edit applies to.
    pub cf: u32,
    /// What the edit does.
    pub kind: EntryKind,
    /// The key, or the inclusive start of the range for [`EntryKind::DeleteRange`].
    pub key: &'a [u8],
    /// The value, empty for [`EntryKind::Delete`] and the exclusive end of the range for
    /// [`EntryKind::DeleteRange`].
    pub value: &'a [u8],
    /// `base + index`: the sequence number this entry is stored at.
    pub seqno: SeqNo,
}

/// An ordered list of edits across any number of column families, applied atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteBatch {
    data: Vec<u8>,
}

impl Default for WriteBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteBatch {
    /// An empty batch at sequence number zero. The engine overwrites the sequence number when
    /// the batch reaches the front of the write queue.
    pub fn new() -> Self {
        Self {
            data: vec![0u8; HEADER_SIZE],
        }
    }

    /// Reads a batch back from its serialised form, validating every entry.
    ///
    /// Validation is not optional here: these bytes come from a log written before a crash, so
    /// a truncated entry, an unknown kind or a trailing byte is corruption to report rather
    /// than something to iterate past (invariant 2).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::corruption(
                "write batch",
                format!("{} bytes is shorter than the 12-byte header", bytes.len()),
            ));
        }
        let batch = Self {
            data: bytes.to_vec(),
        };
        let mut seen = 0u32;
        let mut iter = batch.iter();
        for entry in &mut iter {
            entry?;
            seen = seen.saturating_add(1);
        }
        if seen != batch.count() {
            return Err(Error::corruption(
                "write batch",
                format!("header claims {} entries, found {seen}", batch.count()),
            ));
        }
        if let Some(trailing) = iter.trailing_bytes() {
            return Err(Error::corruption(
                "write batch",
                format!("{trailing} bytes after the last entry"),
            ));
        }
        Ok(batch)
    }

    /// The serialised batch: exactly the bytes that go into one log record.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The base sequence number. Entry `i` is stored at `seqno() + i`.
    pub fn seqno(&self) -> SeqNo {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data[0..8]);
        SeqNo::from_le_bytes(bytes)
    }

    /// Assigns the base sequence number, which the writer does under the queue lock.
    pub fn set_seqno(&mut self, seqno: SeqNo) {
        self.data[0..8].copy_from_slice(&seqno.to_le_bytes());
    }

    /// How many entries the batch holds, and therefore how many sequence numbers it consumes.
    pub fn count(&self) -> u32 {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&self.data[8..12]);
        u32::from_le_bytes(bytes)
    }

    /// Whether the batch has no entries. An empty batch is still a valid record.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// The serialised size, which is what group commit measures its budget in.
    pub fn byte_size(&self) -> usize {
        self.data.len()
    }

    /// Records a put.
    pub fn put(&mut self, cf: u32, key: &[u8], value: &[u8]) {
        self.push(cf, EntryKind::Put, key, Some(value));
    }

    /// Records a deletion. A deletion is a stored entry, not the absence of one: an older
    /// version may still exist in a lower level, and only a tombstone hides it.
    pub fn delete(&mut self, cf: u32, key: &[u8]) {
        self.push(cf, EntryKind::Delete, key, None);
    }

    /// Records the deletion of `[begin, end)`.
    ///
    /// Subject to the v1 limitation of `docs/DESIGN.md` §4.7: proper range tombstones arrive
    /// in phase 5. The entry shape is frozen now so that the format does not change then.
    pub fn delete_range(&mut self, cf: u32, begin: &[u8], end: &[u8]) {
        self.push(cf, EntryKind::DeleteRange, begin, Some(end));
    }

    /// Appends every entry of `other`, keeping this batch's base sequence number.
    ///
    /// This is what group commit does: the leader merges the queue into one batch so the log
    /// takes one record and one `fsync` for the whole group (`docs/DESIGN.md` §4.2).
    pub fn append(&mut self, other: &Self) {
        self.data.extend_from_slice(&other.data[HEADER_SIZE..]);
        self.set_count(self.count().saturating_add(other.count()));
    }

    /// Walks the entries in order, with their sequence numbers.
    pub fn iter(&self) -> WriteBatchIter<'_> {
        WriteBatchIter {
            data: &self.data,
            offset: HEADER_SIZE,
            remaining: self.count(),
            base: self.seqno(),
            index: 0,
        }
    }

    fn push(&mut self, cf: u32, kind: EntryKind, key: &[u8], value: Option<&[u8]>) {
        varint::put_u32(cf, &mut self.data);
        self.data.push(kind.as_u8());
        varint::put_u64(key.len() as u64, &mut self.data);
        self.data.extend_from_slice(key);
        if let Some(value) = value {
            varint::put_u64(value.len() as u64, &mut self.data);
            self.data.extend_from_slice(value);
        }
        // A batch runs out of memory long before it runs out of counter: at three bytes an
        // entry, u32::MAX entries is twelve gigabytes, and group commit caps a batch at one
        // megabyte.
        self.set_count(self.count().saturating_add(1));
    }

    fn set_count(&mut self, count: u32) {
        self.data[8..12].copy_from_slice(&count.to_le_bytes());
    }
}

impl<'a> IntoIterator for &'a WriteBatch {
    type Item = Result<Entry<'a>>;
    type IntoIter = WriteBatchIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterates a batch's entries, reporting malformed bytes rather than panicking on them.
#[derive(Debug)]
pub struct WriteBatchIter<'a> {
    data: &'a [u8],
    offset: usize,
    remaining: u32,
    base: SeqNo,
    index: u32,
}

impl WriteBatchIter<'_> {
    /// Bytes left over after the last entry, if any. Nonzero means the batch is corrupt: the
    /// header's count and the payload disagree.
    pub fn trailing_bytes(&self) -> Option<usize> {
        (self.remaining == 0 && self.offset < self.data.len())
            .then(|| self.data.len() - self.offset)
    }

    /// Reports a malformed entry and ends the iteration.
    ///
    /// A batch with one unreadable entry has no readable entries after it: the length that
    /// would say where the next one starts is exactly what is in doubt.
    fn corrupt(&mut self, detail: String) -> Error {
        self.remaining = 0;
        Error::corruption("write batch", detail)
    }
}

impl<'a> Iterator for WriteBatchIter<'a> {
    type Item = Result<Entry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let start = self.offset;

        let (cf, used) = match varint::get_u32(&self.data[self.offset..]) {
            Ok(pair) => pair,
            Err(err) => {
                return Some(Err(self.corrupt(format!(
                    "entry at byte {start}: bad column family: {err}"
                ))));
            }
        };
        self.offset += used;

        let Some(&kind_byte) = self.data.get(self.offset) else {
            return Some(Err(self.corrupt(format!(
                "entry at byte {start}: truncated before its kind"
            ))));
        };
        self.offset += 1;
        let Some(kind) = EntryKind::from_u8(kind_byte) else {
            return Some(Err(self.corrupt(format!(
                "entry at byte {start}: {kind_byte} is not an entry kind"
            ))));
        };

        let key = match self.take_slice() {
            Ok(key) => key,
            Err(why) => {
                return Some(Err(
                    self.corrupt(format!("entry at byte {start}: key: {why}"))
                ));
            }
        };
        let value = if kind == EntryKind::Delete {
            &self.data[self.offset..self.offset]
        } else {
            match self.take_slice() {
                Ok(value) => value,
                Err(why) => {
                    return Some(Err(
                        self.corrupt(format!("entry at byte {start}: value: {why}"))
                    ));
                }
            }
        };

        let seqno = self.base.wrapping_add(SeqNo::from(self.index));
        self.index = self.index.saturating_add(1);
        self.remaining -= 1;
        Some(Ok(Entry {
            cf,
            kind,
            key,
            value,
            seqno,
        }))
    }
}

impl<'a> WriteBatchIter<'a> {
    /// Reads a varint length followed by that many bytes.
    fn take_slice(&mut self) -> std::result::Result<&'a [u8], String> {
        let (len, used) = varint::get_u64(&self.data[self.offset..])
            .map_err(|err| format!("bad length: {err}"))?;
        self.offset += used;
        let len =
            usize::try_from(len).map_err(|_| format!("length {len} does not fit in memory"))?;
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| format!("length {len} overflows the batch"))?;
        if end > self.data.len() {
            return Err(format!(
                "length {len} runs past the end of the batch at byte {}",
                self.offset
            ));
        }
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::{HEADER_SIZE, WriteBatch};
    use crate::dbformat::EntryKind;

    /// `(cf, kind, key, value, seqno)`, flattened so a whole batch can be asserted at once.
    type Decoded = (u32, EntryKind, Vec<u8>, Vec<u8>, u64);

    fn entries(batch: &WriteBatch) -> Vec<Decoded> {
        batch
            .iter()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.cf,
                    entry.kind,
                    entry.key.to_vec(),
                    entry.value.to_vec(),
                    entry.seqno,
                )
            })
            .collect()
    }

    #[test]
    fn an_empty_batch_is_just_a_header() {
        let batch = WriteBatch::new();
        assert_eq!(batch.byte_size(), HEADER_SIZE);
        assert_eq!(batch.count(), 0);
        assert!(batch.is_empty());
        assert_eq!(batch.iter().count(), 0);
        // And it survives a round trip, because a group commit can produce one.
        assert_eq!(WriteBatch::from_bytes(batch.as_bytes()).unwrap(), batch);
    }

    #[test]
    fn entries_round_trip_with_their_kinds() {
        let mut batch = WriteBatch::new();
        batch.set_seqno(100);
        batch.put(0, b"alpha", b"one");
        batch.delete(1, b"beta");
        batch.delete_range(2, b"g", b"m");
        batch.put(0, b"", b"");

        assert_eq!(batch.count(), 4);
        let decoded = WriteBatch::from_bytes(batch.as_bytes()).unwrap();
        assert_eq!(
            entries(&decoded),
            vec![
                (0, EntryKind::Put, b"alpha".to_vec(), b"one".to_vec(), 100),
                (1, EntryKind::Delete, b"beta".to_vec(), Vec::new(), 101),
                (2, EntryKind::DeleteRange, b"g".to_vec(), b"m".to_vec(), 102),
                (0, EntryKind::Put, Vec::new(), Vec::new(), 103),
            ]
        );
    }

    /// The rule that makes "an ordered list of edits" mean something: the second put to a key
    /// must land at a higher sequence number than the first, or the memtable cannot tell them
    /// apart.
    #[test]
    fn each_entry_gets_its_own_sequence_number() {
        let mut batch = WriteBatch::new();
        batch.set_seqno(7);
        batch.put(0, b"k", b"first");
        batch.put(0, b"k", b"second");
        let seqnos: Vec<u64> = batch.iter().map(|e| e.unwrap().seqno).collect();
        assert_eq!(seqnos, vec![7, 8]);
    }

    #[test]
    fn the_sequence_number_can_be_assigned_after_the_fact() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"k", b"v");
        assert_eq!(batch.seqno(), 0);
        batch.set_seqno(0x0102_0304_0506);
        assert_eq!(batch.seqno(), 0x0102_0304_0506);
        assert_eq!(
            batch.iter().next().unwrap().unwrap().seqno,
            0x0102_0304_0506
        );
    }

    /// Group commit merges the queue into one batch; the merged one must read as the
    /// concatenation, numbered from the leader's base.
    #[test]
    fn appending_merges_two_batches() {
        let mut leader = WriteBatch::new();
        leader.set_seqno(50);
        leader.put(0, b"a", b"1");

        let mut follower = WriteBatch::new();
        follower.set_seqno(999); // discarded: the group takes the leader's numbering
        follower.delete(1, b"b");
        follower.put(1, b"c", b"3");

        leader.append(&follower);
        assert_eq!(leader.count(), 3);
        assert_eq!(leader.seqno(), 50);
        let seqnos: Vec<u64> = leader.iter().map(|e| e.unwrap().seqno).collect();
        assert_eq!(seqnos, vec![50, 51, 52]);
        assert_eq!(leader.iter().last().unwrap().unwrap().key, b"c");
    }

    #[test]
    fn a_truncated_batch_is_corruption_not_a_panic() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"key", b"value");
        let bytes = batch.as_bytes();
        for cut in HEADER_SIZE..bytes.len() {
            let err = WriteBatch::from_bytes(&bytes[..cut]).unwrap_err();
            assert!(err.is_corruption(), "cut at {cut}: {err}");
        }
        // Shorter than the header is corruption too, at every length.
        for cut in 0..HEADER_SIZE {
            assert!(
                WriteBatch::from_bytes(&bytes[..cut])
                    .unwrap_err()
                    .is_corruption()
            );
        }
    }

    #[test]
    fn a_bad_kind_byte_is_corruption() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"key", b"value");
        let mut bytes = batch.as_bytes().to_vec();
        bytes[HEADER_SIZE + 1] = 0x7F; // the kind byte, after a one-byte cf varint
        let err = WriteBatch::from_bytes(&bytes).unwrap_err();
        assert!(err.is_corruption(), "{err}");
    }

    #[test]
    fn a_count_that_disagrees_with_the_payload_is_corruption() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"key", b"value");
        let mut bytes = batch.as_bytes().to_vec();

        bytes[8] = 2; // claims two entries, carries one
        assert!(WriteBatch::from_bytes(&bytes).unwrap_err().is_corruption());

        bytes[8] = 0; // claims none, carries one: the trailing bytes give it away
        let err = WriteBatch::from_bytes(&bytes).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("after the last entry"), "{err}");
    }

    /// A length field is the one part of the format that can point outside it.
    #[test]
    fn a_length_that_runs_past_the_end_is_corruption() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"key", b"value");
        let mut bytes = batch.as_bytes().to_vec();
        bytes[HEADER_SIZE + 2] = 0x7F; // the key length varint
        assert!(WriteBatch::from_bytes(&bytes).unwrap_err().is_corruption());
    }

    #[test]
    fn iteration_stops_at_the_first_unreadable_entry() {
        let mut batch = WriteBatch::new();
        batch.put(0, b"good", b"value");
        batch.put(0, b"also good", b"value");
        let mut bytes = batch.as_bytes().to_vec();
        bytes[HEADER_SIZE + 1] = 0x7F; // break the first entry's kind

        let batch = WriteBatch { data: bytes };
        let results: Vec<_> = batch.iter().collect();
        assert_eq!(
            results.len(),
            1,
            "no entry is readable after an unreadable one"
        );
        assert!(results[0].is_err());
    }
}
