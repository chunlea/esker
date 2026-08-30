//! The log writer: splits records into block-bounded fragments and buffers them.
//!
//! The buffer is the point. A group commit hands the writer a dozen batches and then calls
//! [`LogWriter::sync`] once; the writer issues **one** `append` and **one** `sync_data` for the
//! whole group (`docs/DESIGN.md` §4.2). Writing per record would turn one sequential I/O into
//! a dozen, which is the throughput the log-structured design exists to get.
//!
//! # A failed append ends the segment
//!
//! A write that fails may have written *some* of its bytes — a full disk and a partial write
//! look the same from here. The segment's length is then unknown, so appending after it would
//! lay the next group's bytes over the tail of a half-written record, and the record's header
//! would still claim the length it meant to have. The reader would find a valid header,
//! well-formed bytes after it, and a checksum that fails: corruption in the middle of a log
//! rather than a torn record at its end, which is the difference between a database that
//! reopens and one that does not.
//!
//! So a writer that has failed an append is finished. Every later call returns the same
//! error, the caller cannot acknowledge anything else, and the segment keeps the torn tail a
//! crash would have left — which recovery already knows how to handle.

use std::fmt;

use crate::error::{Error, IoResultExt, Result};
use crate::fs::WritableFile;

use super::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, encode_header};

/// Appends records to one log segment.
pub struct LogWriter {
    file: Box<dyn WritableFile>,
    /// Named only so errors and panics can say which segment they mean.
    path: String,
    /// How far into the current 32 KiB block the next fragment starts.
    block_offset: usize,
    /// Bytes staged for the next `append`.
    buf: Vec<u8>,
    /// Bytes handed to the writer so far, buffered or not: the segment's logical length.
    len: u64,
    /// Set once an append has failed. See the module docs: the segment is finished.
    failed: Option<String>,
}

// `WritableFile` is deliberately not `Debug`: it is a two-method trait the SST lane also
// implements, and a supertrait there would be a contract change. The interesting state is
// the position in the log anyway.
impl fmt::Debug for LogWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogWriter")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("block_offset", &self.block_offset)
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl LogWriter {
    /// Starts a new, empty segment.
    pub fn new(file: Box<dyn WritableFile>, path: impl Into<String>) -> Self {
        Self {
            file,
            path: path.into(),
            block_offset: 0,
            buf: Vec::with_capacity(BLOCK_SIZE),
            len: 0,
            failed: None,
        }
    }

    /// Appends one record, fragmenting it across blocks as needed.
    ///
    /// Infallible: it only touches the buffer. Errors happen at [`flush`](Self::flush) and
    /// [`sync`](Self::sync), which is also where the caller decides to care about them.
    ///
    /// A zero-length record is a record: it is written as one empty `FULL` fragment and read
    /// back as an empty payload, not skipped.
    pub fn add_record(&mut self, payload: &[u8]) {
        let mut rest = payload;
        let mut first = true;
        loop {
            let leftover = BLOCK_SIZE - self.block_offset;
            if leftover < HEADER_SIZE {
                // Not even a header fits: zero-fill the tail so the reader can tell padding
                // from a record, and start the next block.
                if leftover > 0 {
                    self.push(&[0u8; HEADER_SIZE][..leftover]);
                }
                self.block_offset = 0;
            }

            let available = BLOCK_SIZE - self.block_offset - HEADER_SIZE;
            let take = rest.len().min(available);
            let last = take == rest.len();
            let kind = match (first, last) {
                (true, true) => RecordType::Full,
                (true, false) => RecordType::First,
                (false, true) => RecordType::Last,
                (false, false) => RecordType::Middle,
            };

            let fragment = &rest[..take];
            self.push(&encode_header(kind, fragment));
            self.push(fragment);
            self.block_offset += HEADER_SIZE + take;

            rest = &rest[take..];
            first = false;
            if last {
                return;
            }
        }
    }

    /// Hands every buffered byte to the file in one `append`.
    ///
    /// After this fails once, it fails for good: see the module docs for why writing more
    /// after a partial write turns a torn tail into corruption.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(reason) = &self.failed {
            return Err(Error::Poisoned(format!(
                "{} stopped accepting records: {reason}",
                self.path
            )));
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        match self.file.append(&self.buf).at(&self.path) {
            Ok(()) => {
                self.buf.clear();
                Ok(())
            }
            Err(err) => {
                self.failed = Some(err.to_string());
                // These bytes will never be written, so there is no reason to keep them.
                self.buf.clear();
                Err(err)
            }
        }
    }

    /// Flushes, then makes the segment durable. Invariant 1's "fsync before ack" is this call.
    pub fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.file.sync_data().at(&self.path)
    }

    /// The segment's length in bytes, buffered writes included.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes waiting for the next [`flush`](Self::flush).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Whether an append has failed and the segment is therefore finished.
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.len += bytes.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::LogWriter;
    use crate::fs::{FileSystem, WritableFile};
    use crate::memfs::MemFileSystem;
    use crate::wal::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, decode_header};
    use std::io;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn writer(fs: &MemFileSystem) -> LogWriter {
        LogWriter::new(fs.create(Path::new("/log")).unwrap(), "/log")
    }

    fn kinds(bytes: &[u8]) -> Vec<RecordType> {
        // Walk the physical fragments the way a reader does, block by block.
        let mut kinds = Vec::new();
        let mut offset = 0;
        while offset + HEADER_SIZE <= bytes.len() {
            let in_block = offset % BLOCK_SIZE;
            if BLOCK_SIZE - in_block < HEADER_SIZE {
                offset += BLOCK_SIZE - in_block;
                continue;
            }
            let header: [u8; HEADER_SIZE] = bytes[offset..offset + HEADER_SIZE].try_into().unwrap();
            let (_, len, kind) = decode_header(&header);
            kinds.push(RecordType::from_u8(kind).unwrap());
            offset += HEADER_SIZE + len;
        }
        kinds
    }

    #[test]
    fn a_small_record_is_one_full_fragment() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        w.add_record(b"hello");
        w.sync().unwrap();
        let bytes = fs.contents("/log").unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE + 5);
        assert_eq!(kinds(&bytes), [RecordType::Full]);
    }

    /// An empty record must survive the round trip as an empty record. Skipping it would
    /// silently drop a write batch that happened to serialise to nothing.
    #[test]
    fn a_zero_length_record_is_still_a_record() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        w.add_record(b"");
        w.sync().unwrap();
        assert_eq!(fs.contents("/log").unwrap().len(), HEADER_SIZE);
        assert_eq!(kinds(&fs.contents("/log").unwrap()), [RecordType::Full]);
    }

    #[test]
    fn a_record_spanning_three_blocks_is_first_middle_last() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        w.add_record(&vec![7u8; 2 * BLOCK_SIZE]);
        w.sync().unwrap();
        assert_eq!(
            kinds(&fs.contents("/log").unwrap()),
            [RecordType::First, RecordType::Middle, RecordType::Last]
        );
    }

    /// The case the format's trailer exists for: a block with 1..7 bytes left cannot hold a
    /// header, so those bytes are zeroed and the record starts in the next block.
    #[test]
    fn a_block_tail_too_small_for_a_header_is_zero_filled() {
        for leftover in 1..HEADER_SIZE {
            let fs = MemFileSystem::new();
            let mut w = writer(&fs);
            // Fill the block to exactly `leftover` bytes short of its end.
            let first_payload = BLOCK_SIZE - HEADER_SIZE - leftover;
            w.add_record(&vec![1u8; first_payload]);
            assert_eq!(w.len(), (BLOCK_SIZE - leftover) as u64);
            w.add_record(b"next");
            w.sync().unwrap();

            let bytes = fs.contents("/log").unwrap();
            assert_eq!(
                &bytes[BLOCK_SIZE - leftover..BLOCK_SIZE],
                vec![0u8; leftover],
                "leftover {leftover} should be zero padding"
            );
            assert_eq!(bytes.len(), BLOCK_SIZE + HEADER_SIZE + 4);
            assert_eq!(kinds(&bytes), [RecordType::Full, RecordType::Full]);
        }
    }

    /// A record that exactly fills the rest of a block leaves no trailer at all.
    #[test]
    fn a_record_that_exactly_fills_a_block_leaves_no_padding() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        w.add_record(&vec![3u8; BLOCK_SIZE - HEADER_SIZE]);
        w.sync().unwrap();
        let bytes = fs.contents("/log").unwrap();
        assert_eq!(bytes.len(), BLOCK_SIZE);
        assert_eq!(kinds(&bytes), [RecordType::Full]);
    }

    /// One `append` per group, not one per record: the buffer is what makes group commit
    /// worth having.
    #[test]
    fn records_are_buffered_until_flush() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        w.add_record(b"one");
        w.add_record(b"two");
        assert_eq!(fs.contents("/log").unwrap().len(), 0, "nothing written yet");
        assert_eq!(w.buffered(), 2 * (HEADER_SIZE + 3));
        w.flush().unwrap();
        assert_eq!(fs.contents("/log").unwrap().len(), 2 * (HEADER_SIZE + 3));
        assert_eq!(w.buffered(), 0);
        // Flushed is not synced.
        assert_eq!(fs.sync_len("/log").unwrap(), 0);
        w.sync().unwrap();
        assert_eq!(fs.sync_len("/log").unwrap(), 2 * (HEADER_SIZE + 3));
    }

    /// A file that writes only part of what it is given and then fails, which is what a full
    /// disk looks like from here.
    #[derive(Debug)]
    struct ShortWriter {
        written: Arc<Mutex<Vec<u8>>>,
        keep: usize,
    }

    impl WritableFile for ShortWriter {
        fn append(&mut self, data: &[u8]) -> io::Result<()> {
            let take = self.keep.min(data.len());
            self.written
                .lock()
                .unwrap()
                .extend_from_slice(&data[..take]);
            self.keep -= take;
            if take < data.len() {
                return Err(io::Error::other("no space left on device"));
            }
            Ok(())
        }

        fn sync_data(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The bug this guards against: after a partial write the segment's length is unknown, so
    /// writing more lays the next group over the tail of a half-written record. The reader
    /// then finds a valid header, plausible bytes and a failing checksum — corruption in the
    /// middle of a log rather than a torn record at its end.
    #[test]
    fn a_writer_that_failed_an_append_never_writes_again() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut writer = LogWriter::new(
            Box::new(ShortWriter {
                written: Arc::clone(&written),
                keep: 20,
            }),
            "/log",
        );

        writer.add_record(&[7u8; 100]);
        let err = writer.flush().unwrap_err();
        assert!(err.to_string().contains("no space left"), "{err}");
        assert!(writer.is_failed());
        assert_eq!(
            written.lock().unwrap().len(),
            20,
            "a partial write happened"
        );

        // Everything after it is refused, and nothing more reaches the file.
        writer.add_record(b"this must not be written");
        assert!(writer.flush().is_err());
        assert!(writer.sync().is_err());
        assert_eq!(
            written.lock().unwrap().len(),
            20,
            "the segment must not grow after a failed append"
        );
    }

    #[test]
    fn length_tracks_every_byte_including_padding() {
        let fs = MemFileSystem::new();
        let mut w = writer(&fs);
        assert!(w.is_empty());
        w.add_record(&vec![9u8; BLOCK_SIZE]);
        w.sync().unwrap();
        assert_eq!(w.len(), fs.contents("/log").unwrap().len() as u64);
        assert!(!w.is_empty());
    }
}
