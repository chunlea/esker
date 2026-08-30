//! The log reader, and the distinction the whole recovery path rests on.
//!
//! A crashed process leaves a log that stops in the middle of a record. That is **torn**, and
//! it is normal: the write was never acknowledged, so nothing was lost. Bytes that are present
//! but wrong — a bad checksum, an impossible length, a fragment out of sequence — are
//! **corrupt**, and something *was* lost.
//!
//! Reporting them as one thing is how a storage engine silently drops acknowledged writes, so
//! [`ReadOutcome`] keeps them apart and refuses to decide which is acceptable. Only the caller
//! knows where it is reading: a torn tail is legal in the last segment of a log and nowhere
//! else (`docs/DESIGN.md` §4.3).
//!
//! The reader does not resynchronise after damage. Once it reports `Torn` or `Corrupt` it will
//! report the same thing again, because recovery must stop at the first hole rather than
//! silently skip past it into records that no longer mean what they say.

use std::fmt;

use crate::error::{IoResultExt, Result};
use crate::fs::RandomAccessFile;

use super::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, decode_header, fragment_crc};

/// What one call to [`LogReader::read_record`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// A complete record whose checksum verified.
    Record(Vec<u8>),
    /// The log ended cleanly, on a record boundary.
    Eof,
    /// The log stops part-way through a record. Expected at the tail of the segment that was
    /// open when the process died; a bug or a lost write anywhere else.
    Torn(String),
    /// Bytes that cannot be a log. Never acceptable, at any position.
    Corrupt(String),
}

impl ReadOutcome {
    /// Whether this outcome ends the segment: everything except a record does.
    pub fn is_end(&self) -> bool {
        !matches!(self, Self::Record(_))
    }
}

/// One physical fragment, or the reason there is not one.
enum Fragment {
    Data(RecordType, Vec<u8>),
    Eof,
    Torn(String),
    Corrupt(String),
}

/// Reads records from one log segment.
pub struct LogReader {
    file: Box<dyn RandomAccessFile>,
    path: String,
    block: Vec<u8>,
    /// File offset of `block[0]`.
    block_start: u64,
    /// Bytes present in `block`. Below [`BLOCK_SIZE`] only in the final block.
    block_len: usize,
    /// Next byte to examine inside `block`.
    offset: usize,
    started: bool,
}

// See `LogWriter`: the file traits stay at the two methods the contract pins, so `Debug` is
// written out here rather than required of them.
impl fmt::Debug for LogReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogReader")
            .field("path", &self.path)
            .field("position", &self.position())
            .field("block_len", &self.block_len)
            .finish_non_exhaustive()
    }
}

impl LogReader {
    /// Reads the segment at `path` from the beginning.
    pub fn new(file: Box<dyn RandomAccessFile>, path: impl Into<String>) -> Self {
        Self {
            file,
            path: path.into(),
            block: Vec::new(),
            block_start: 0,
            block_len: 0,
            offset: 0,
            started: false,
        }
    }

    /// The file offset the next fragment would be read from. Error messages quote it.
    pub fn position(&self) -> u64 {
        self.block_start + self.offset as u64
    }

    /// Reads the next record, reassembling it from fragments if it spans blocks.
    pub fn read_record(&mut self) -> Result<ReadOutcome> {
        let mut payload = Vec::new();
        let mut fragmented = false;
        loop {
            match self.next_fragment()? {
                Fragment::Data(RecordType::Full, data) => {
                    if fragmented {
                        return Ok(self.corrupt("a FULL record began inside a fragmented one"));
                    }
                    return Ok(ReadOutcome::Record(data));
                }
                Fragment::Data(RecordType::First, data) => {
                    if fragmented {
                        return Ok(
                            self.corrupt("a FIRST fragment began inside a fragmented record")
                        );
                    }
                    payload = data;
                    fragmented = true;
                }
                Fragment::Data(RecordType::Middle, data) => {
                    if !fragmented {
                        return Ok(self.corrupt("a MIDDLE fragment with no FIRST before it"));
                    }
                    payload.extend_from_slice(&data);
                }
                Fragment::Data(RecordType::Last, data) => {
                    if !fragmented {
                        return Ok(self.corrupt("a LAST fragment with no FIRST before it"));
                    }
                    payload.extend_from_slice(&data);
                    return Ok(ReadOutcome::Record(payload));
                }
                Fragment::Eof => {
                    return Ok(if fragmented {
                        ReadOutcome::Torn(format!(
                            "{} ends inside a fragmented record at offset {}",
                            self.path,
                            self.position()
                        ))
                    } else {
                        ReadOutcome::Eof
                    });
                }
                Fragment::Torn(why) => return Ok(ReadOutcome::Torn(why)),
                Fragment::Corrupt(why) => return Ok(ReadOutcome::Corrupt(why)),
            }
        }
    }

    /// Reads until the segment ends, returning the complete records and why it stopped.
    ///
    /// The outcome is never [`ReadOutcome::Record`]: it is the [`Eof`](ReadOutcome::Eof),
    /// [`Torn`](ReadOutcome::Torn) or [`Corrupt`](ReadOutcome::Corrupt) that ended the read,
    /// which is exactly what recovery has to make a decision about.
    pub fn read_all(&mut self) -> Result<(Vec<Vec<u8>>, ReadOutcome)> {
        let mut records = Vec::new();
        loop {
            match self.read_record()? {
                ReadOutcome::Record(record) => records.push(record),
                end => return Ok((records, end)),
            }
        }
    }

    fn corrupt(&self, why: &str) -> ReadOutcome {
        ReadOutcome::Corrupt(format!(
            "{} at offset {}: {why}",
            self.path,
            self.position()
        ))
    }

    fn next_fragment(&mut self) -> Result<Fragment> {
        loop {
            if !self.started && !self.load_next_block()? {
                return Ok(Fragment::Eof);
            }

            let leftover = self.block_len - self.offset;
            if leftover < HEADER_SIZE {
                if self.block_len == BLOCK_SIZE {
                    // A full block with a tail too small for a header: that tail is the
                    // zero padding the writer left. The record continues in the next block.
                    if !self.load_next_block()? {
                        return Ok(Fragment::Eof);
                    }
                    continue;
                }
                // The final block, with fewer bytes left than a header needs.
                if leftover == 0 {
                    return Ok(Fragment::Eof);
                }
                if self.block[self.offset..self.block_len]
                    .iter()
                    .all(|&b| b == 0)
                {
                    // Padding the writer had begun to lay down. Nothing was lost: no record
                    // ever starts in the last six bytes of a block.
                    return Ok(Fragment::Eof);
                }
                return Ok(Fragment::Torn(format!(
                    "{} ends with {leftover} bytes at offset {}, too few for a record header",
                    self.path,
                    self.position()
                )));
            }

            let position = self.position();
            let header: [u8; HEADER_SIZE] = self.block[self.offset..self.offset + HEADER_SIZE]
                .try_into()
                .unwrap_or([0u8; HEADER_SIZE]);
            let (crc, len, kind_byte) = decode_header(&header);

            let Some(kind) = RecordType::from_u8(kind_byte) else {
                return Ok(Fragment::Corrupt(format!(
                    "{} at offset {position}: {kind_byte} is not a record type",
                    self.path
                )));
            };
            if HEADER_SIZE + len > BLOCK_SIZE - self.offset {
                return Ok(Fragment::Corrupt(format!(
                    "{} at offset {position}: a {len}-byte record overruns its block",
                    self.path
                )));
            }
            if HEADER_SIZE + len > leftover {
                return Ok(Fragment::Torn(format!(
                    "{} at offset {position}: a {len}-byte record runs past the end of the log",
                    self.path
                )));
            }

            let start = self.offset + HEADER_SIZE;
            let payload = &self.block[start..start + len];
            if fragment_crc(kind, payload) != crc {
                return Ok(Fragment::Corrupt(format!(
                    "{} at offset {position}: checksum mismatch over {len} bytes",
                    self.path
                )));
            }

            let data = payload.to_vec();
            self.offset = start + len;
            return Ok(Fragment::Data(kind, data));
        }
    }

    /// Reads the next 32 KiB block. Returns `false` when there is nothing left to read.
    fn load_next_block(&mut self) -> Result<bool> {
        let start = if self.started {
            self.block_start + BLOCK_SIZE as u64
        } else {
            0
        };
        self.block.clear();
        self.block.resize(BLOCK_SIZE, 0);
        let mut got = 0;
        while got < BLOCK_SIZE {
            let n = self
                .file
                .read_at(start + got as u64, &mut self.block[got..])
                .at(&self.path)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        self.block_start = start;
        self.block_len = got;
        self.offset = 0;
        self.started = true;
        Ok(got > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::{LogReader, ReadOutcome};
    use crate::fs::FileSystem;
    use crate::memfs::MemFileSystem;
    use crate::wal::format::{BLOCK_SIZE, HEADER_SIZE};
    use crate::wal::writer::LogWriter;
    use std::path::Path;

    /// Writes `records` to a fresh in-memory log and returns the filesystem and its bytes.
    fn write_log(records: &[&[u8]]) -> (MemFileSystem, Vec<u8>) {
        let fs = MemFileSystem::new();
        let mut w = LogWriter::new(fs.create(Path::new("/log")).unwrap(), "/log");
        for record in records {
            w.add_record(record);
        }
        w.sync().unwrap();
        let bytes = fs.contents("/log").unwrap();
        (fs, bytes)
    }

    fn read_back(fs: &MemFileSystem) -> (Vec<Vec<u8>>, ReadOutcome) {
        let mut reader = LogReader::new(fs.open(Path::new("/log")).unwrap(), "/log");
        reader.read_all().unwrap()
    }

    fn read_bytes(bytes: &[u8]) -> (Vec<Vec<u8>>, ReadOutcome) {
        let fs = MemFileSystem::new();
        fs.install("/log", bytes.to_vec()).unwrap();
        read_back(&fs)
    }

    #[test]
    fn records_round_trip_including_empty_ones() {
        let payloads: Vec<&[u8]> = vec![b"", b"a", b"hello world", b""];
        let (fs, _) = write_log(&payloads);
        let (records, end) = read_back(&fs);
        assert_eq!(end, ReadOutcome::Eof);
        assert_eq!(
            records,
            payloads.iter().map(|p| p.to_vec()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_record_spanning_blocks_is_reassembled() {
        let big = vec![0xABu8; 3 * BLOCK_SIZE + 11];
        let (fs, bytes) = write_log(&[b"before", &big, b"after"]);
        assert!(bytes.len() > 3 * BLOCK_SIZE);
        let (records, end) = read_back(&fs);
        assert_eq!(end, ReadOutcome::Eof);
        assert_eq!(records, vec![b"before".to_vec(), big, b"after".to_vec()]);
    }

    /// An empty file is a clean end, not a torn one: a segment created and never written to
    /// is what a crash between `create` and the first record leaves behind.
    #[test]
    fn an_empty_log_reads_as_eof() {
        let (records, end) = read_bytes(b"");
        assert!(records.is_empty());
        assert_eq!(end, ReadOutcome::Eof);
    }

    #[test]
    fn a_bad_checksum_is_corruption_not_a_torn_tail() {
        let (_, mut bytes) = write_log(&[b"first", b"second"]);
        let payload_start = HEADER_SIZE;
        bytes[payload_start] ^= 0xFF;
        let (records, end) = read_bytes(&bytes);
        assert!(
            records.is_empty(),
            "the damaged record must not be returned"
        );
        assert!(matches!(end, ReadOutcome::Corrupt(_)), "{end:?}");
    }

    #[test]
    fn an_unknown_record_type_is_corruption() {
        let (_, mut bytes) = write_log(&[b"first"]);
        bytes[6] = 9;
        assert!(matches!(read_bytes(&bytes).1, ReadOutcome::Corrupt(_)));
        bytes[6] = 0;
        assert!(matches!(read_bytes(&bytes).1, ReadOutcome::Corrupt(_)));
    }

    /// The trap the type-seeded CRC exists for: a whole valid record moved to another offset
    /// must not verify. Here a FULL record's header is rewritten as a MIDDLE one.
    #[test]
    fn a_fragment_relabelled_as_another_type_fails_its_checksum() {
        let (_, mut bytes) = write_log(&[b"payload"]);
        bytes[6] = 3; // FULL -> MIDDLE, checksum untouched
        assert!(matches!(read_bytes(&bytes).1, ReadOutcome::Corrupt(_)));
    }

    #[test]
    fn a_middle_fragment_without_a_first_is_corruption() {
        // Two blocks: a complete record, then a LAST fragment with nothing before it.
        let (_, bytes) = write_log(&[&vec![1u8; 2 * BLOCK_SIZE]]);
        // Skip the FIRST fragment by starting the log at the second block.
        let (records, end) = read_bytes(&bytes[BLOCK_SIZE..]);
        assert!(records.is_empty());
        assert!(matches!(end, ReadOutcome::Corrupt(_)), "{end:?}");
    }

    #[test]
    fn a_log_that_stops_inside_a_fragmented_record_is_torn() {
        let (_, bytes) = write_log(&[&vec![2u8; 2 * BLOCK_SIZE]]);
        let (records, end) = read_bytes(&bytes[..BLOCK_SIZE + 100]);
        assert!(records.is_empty());
        assert!(matches!(end, ReadOutcome::Torn(_)), "{end:?}");
    }

    #[test]
    fn a_truncated_header_is_torn() {
        let (_, bytes) = write_log(&[b"complete", b"cut short"]);
        let cut = HEADER_SIZE + 8 + 3;
        let (records, end) = read_bytes(&bytes[..cut]);
        assert_eq!(records, vec![b"complete".to_vec()]);
        assert!(matches!(end, ReadOutcome::Torn(_)), "{end:?}");
    }

    /// A crash while laying down a block's zero trailer loses nothing — no record can start
    /// in those bytes — so it reads as a clean end rather than a torn one.
    #[test]
    fn a_partially_written_trailer_reads_as_a_clean_end() {
        let leftover = 5;
        let fs = MemFileSystem::new();
        let mut w = LogWriter::new(fs.create(Path::new("/log")).unwrap(), "/log");
        w.add_record(&vec![1u8; BLOCK_SIZE - HEADER_SIZE - leftover]);
        w.add_record(b"next block");
        w.sync().unwrap();
        let bytes = fs.contents("/log").unwrap();

        // Cut inside the zero padding, as a crash mid-flush would.
        let (records, end) = read_bytes(&bytes[..BLOCK_SIZE - 2]);
        assert_eq!(records.len(), 1);
        assert_eq!(end, ReadOutcome::Eof);
    }

    #[test]
    fn the_reader_does_not_resynchronise_past_damage() {
        let (_, mut bytes) = write_log(&[b"first", b"second", b"third"]);
        bytes[HEADER_SIZE] ^= 0x01;
        let fs = MemFileSystem::new();
        fs.install("/log", bytes).unwrap();
        let mut reader = LogReader::new(fs.open(Path::new("/log")).unwrap(), "/log");
        let first = reader.read_record().unwrap();
        assert!(matches!(first, ReadOutcome::Corrupt(_)));
        // Asking again gives the same answer rather than skipping to "second".
        assert_eq!(reader.read_record().unwrap(), first);
    }

    #[test]
    fn error_messages_name_the_segment_and_the_offset() {
        let (_, mut bytes) = write_log(&[b"payload"]);
        bytes[HEADER_SIZE] ^= 0xFF;
        match read_bytes(&bytes).1 {
            ReadOutcome::Corrupt(why) => {
                assert!(why.contains("/log"), "{why}");
                assert!(why.contains("offset 0"), "{why}");
            }
            other => panic!("expected corruption, got {other:?}"),
        }
    }
}
