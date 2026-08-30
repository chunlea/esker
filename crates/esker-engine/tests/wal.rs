//! Write-ahead log: the golden bytes, and what survives damage.
//!
//! These are the tests `prompts/01-engine.md` step 1 asks for. Three of them are exhaustive
//! rather than sampled, because the properties they check are the ones a crash exercises at an
//! offset nobody chose:
//!
//! * truncating at **every** byte offset must recover exactly the records that were complete;
//! * damaging **every** byte must be noticed;
//! * a fragment moved to another position must fail its own checksum.
//!
//! They read the log through wrappers that truncate or corrupt on the fly rather than by
//! rewriting the file each time, which is what makes a hundred thousand cases cheap enough to
//! run on every commit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::sync::Arc;

use esker_engine::fs::{FileSystem, RandomAccessFile};
use esker_engine::memfs::MemFileSystem;
use esker_engine::wal::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, decode_header};
use esker_engine::wal::{LogReader, LogWriter, ReadOutcome};

/// Builds a log from `records`, returning its bytes and the file length after each record.
fn build(records: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
    let fs = MemFileSystem::new();
    let mut writer = LogWriter::new(fs.create("/log".as_ref()).unwrap(), "/log");
    let mut ends = Vec::with_capacity(records.len());
    for record in records {
        writer.add_record(record);
        ends.push(writer.len());
    }
    writer.sync().unwrap();
    (fs.contents("/log").unwrap(), ends)
}

/// A view of `bytes` that pretends the file is `len` bytes long and that byte `damage.0` is
/// exclusive-ored with `damage.1`. Truncation and corruption without copying anything.
#[derive(Debug)]
struct View {
    bytes: Arc<Vec<u8>>,
    len: usize,
    damage: Option<(usize, u8)>,
}

impl View {
    fn truncated(bytes: &Arc<Vec<u8>>, len: usize) -> Box<dyn RandomAccessFile> {
        Box::new(Self {
            bytes: Arc::clone(bytes),
            len,
            damage: None,
        })
    }

    fn damaged(bytes: &Arc<Vec<u8>>, index: usize, mask: u8) -> Box<dyn RandomAccessFile> {
        Box::new(Self {
            bytes: Arc::clone(bytes),
            len: bytes.len(),
            damage: Some((index, mask)),
        })
    }
}

impl RandomAccessFile for View {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= self.len {
            return Ok(0);
        }
        let n = buf.len().min(self.len - start);
        buf[..n].copy_from_slice(&self.bytes[start..start + n]);
        if let Some((index, mask)) = self.damage
            && index >= start
            && index < start + n
        {
            buf[index - start] ^= mask;
        }
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.len as u64)
    }
}

fn read(file: Box<dyn RandomAccessFile>) -> (Vec<Vec<u8>>, ReadOutcome) {
    LogReader::new(file, "/log").read_all().unwrap()
}

/// Lowercase hex, the form the golden files are written in.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// `crc32c` of the whole file, so a golden file can pin 70 KiB without checking it in.
fn file_crc(bytes: &[u8]) -> u32 {
    esker_engine::crc32c::checksum(bytes)
}

/// Walks the physical fragments the way the reader does, returning their types and lengths,
/// plus how many zero-padding bytes the log contains.
fn fragments(bytes: &[u8]) -> (Vec<RecordType>, Vec<usize>, usize) {
    let (mut types, mut lens, mut padding) = (Vec::new(), Vec::new(), 0);
    let mut offset = 0;
    while offset < bytes.len() {
        let in_block = offset % BLOCK_SIZE;
        if BLOCK_SIZE - in_block < HEADER_SIZE {
            padding += BLOCK_SIZE - in_block;
            offset += BLOCK_SIZE - in_block;
            continue;
        }
        let header: [u8; HEADER_SIZE] = bytes[offset..offset + HEADER_SIZE].try_into().unwrap();
        let (_, len, kind) = decode_header(&header);
        types.push(RecordType::from_u8(kind).unwrap());
        lens.push(len);
        offset += HEADER_SIZE + len;
    }
    (types, lens, padding)
}

/// The byte layout of `docs/DESIGN.md` §4.3, pinned against a golden file whose checksums were
/// produced by a separate CRC32C implementation. Agreeing with ourselves would prove nothing.
#[test]
fn golden_small_log() {
    let golden = include_str!("golden/wal-small.hex");
    let expected: Vec<&str> = golden
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect();

    let records: Vec<Vec<u8>> = vec![b"".to_vec(), b"a".to_vec(), b"hello world".to_vec()];
    let (bytes, ends) = build(&records);
    assert_eq!(bytes.len(), 33, "three headers plus twelve payload bytes");

    let mut start = 0usize;
    for (index, end) in ends.iter().enumerate() {
        let end = usize::try_from(*end).unwrap();
        assert_eq!(
            hex(&bytes[start..end]),
            expected[index],
            "record {index} of the golden log"
        );
        start = end;
    }
}

/// A record too large to check in as hex, pinned by its shape and checksum instead.
#[test]
fn golden_spanning_record() {
    let golden = include_str!("golden/wal-spanning.txt");
    let field = |name: &str| -> String {
        golden
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = ")))
            .unwrap_or_else(|| panic!("{name} missing from the golden file"))
            .to_string()
    };

    let payload: Vec<u8> = (0..70_000u32)
        .map(|i| ((i * 31 + 7) & 0xFF) as u8)
        .collect();
    let (bytes, _) = build(&[payload]);
    let (types, lens, padding) = fragments(&bytes);

    assert_eq!(bytes.len().to_string(), field("file_len"));
    assert_eq!(format!("{:#010x}", file_crc(&bytes)), field("file_crc32c"));
    assert_eq!(
        types,
        [RecordType::First, RecordType::Middle, RecordType::Last]
    );
    assert_eq!(
        lens.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        field("fragment_lens")
    );
    assert_eq!(padding.to_string(), field("padding_bytes"));
}

/// Three records, arranged so the log contains a zero trailer, a record that spans two blocks
/// and a record after it. Truncating anywhere must recover exactly the complete records — no
/// partial record, no corruption report, nothing silently dropped.
#[test]
fn truncating_at_every_offset_recovers_the_complete_records() {
    // 32758 leaves three bytes at the end of block 0: too few for a header, so the writer
    // pads them. 33000 then spans blocks 1 and 2.
    let records: Vec<Vec<u8>> = vec![
        vec![0xA1; BLOCK_SIZE - HEADER_SIZE - 3],
        vec![0xB2; 33_000],
        b"third".to_vec(),
    ];
    let (bytes, ends) = build(&records);
    let (_, _, padding) = fragments(&bytes);
    assert_eq!(padding, 3, "the log should contain a zero trailer");

    let bytes = Arc::new(bytes);
    for cut in 0..=bytes.len() {
        let complete = ends.iter().filter(|end| **end <= cut as u64).count();
        let (read_records, end) = read(View::truncated(&bytes, cut));
        assert_eq!(
            read_records,
            records[..complete],
            "truncated to {cut} bytes: wrong records"
        );
        assert!(
            !matches!(end, ReadOutcome::Corrupt(_)),
            "truncated to {cut} bytes: a torn tail was reported as corruption: {end:?}"
        );
        if complete == records.len() {
            assert_eq!(end, ReadOutcome::Eof, "the whole log should read cleanly");
        }
    }
}

/// Every byte of the log is covered by a checksum, so damaging any one of them must change
/// what the reader reports. A byte nobody checks is a byte a crash can rewrite unnoticed.
#[test]
fn damaging_any_byte_is_detected() {
    // Sized so the log ends on an exact block boundary: with no zero padding, every byte in
    // it is inside a fragment and therefore inside a checksum. Padding bytes are the one part
    // of the format nothing covers, and this test would rightly fail if the log contained any.
    let records: Vec<Vec<u8>> = vec![vec![0xC3; 3_000], b"small".to_vec(), vec![0xD4; BLOCK_SIZE]];
    let (bytes, _) = build(&records);
    let (_, _, padding) = fragments(&bytes);
    assert_eq!(
        padding, 0,
        "every byte of this log must be inside a fragment"
    );
    let bytes = Arc::new(bytes);

    let clean = read(View::truncated(&bytes, bytes.len()));
    assert_eq!(clean.1, ReadOutcome::Eof);
    assert_eq!(clean.0, records);

    for index in 0..bytes.len() {
        let damaged = read(View::damaged(&bytes, index, 0xFF));
        assert_ne!(
            damaged,
            clean,
            "flipping byte {index} of {} changed nothing the reader could see",
            bytes.len()
        );
    }
}

/// The same property at bit granularity, over a log small enough to do exhaustively.
#[test]
fn flipping_any_single_bit_is_detected() {
    let records: Vec<Vec<u8>> = vec![b"alpha".to_vec(), b"".to_vec(), b"gamma delta".to_vec()];
    let (bytes, _) = build(&records);
    let bytes = Arc::new(bytes);
    let clean = read(View::truncated(&bytes, bytes.len()));

    for index in 0..bytes.len() {
        for bit in 0..8 {
            let damaged = read(View::damaged(&bytes, index, 1 << bit));
            assert_ne!(damaged, clean, "bit {bit} of byte {index} went unnoticed");
        }
    }
}

/// The reason `docs/DESIGN.md` §4.3 seeds the checksum with the record type: a header and
/// payload lifted from one position to another must not verify at the new one.
#[test]
fn a_record_replayed_at_another_position_does_not_verify() {
    let (one_record, _) = build(&[b"the same twelve bytes".to_vec()]);

    // Splice the record in as if a misdirected write had duplicated it mid-log, relabelled
    // as a continuation fragment. Only the type byte differs, and only the type seed catches it.
    let mut spliced = one_record.clone();
    let mut moved = one_record.clone();
    moved[6] = RecordType::Middle.as_u8();
    spliced.extend_from_slice(&moved);

    let fs = MemFileSystem::new();
    fs.install("/log", spliced).unwrap();
    let (records, end) = read(fs.open("/log".as_ref()).unwrap());
    assert_eq!(
        records.len(),
        1,
        "only the record at its own position is valid"
    );
    assert!(matches!(end, ReadOutcome::Corrupt(_)), "{end:?}");
}

mod proptests {
    use super::{View, build, read};
    use esker_engine::wal::ReadOutcome;
    use esker_engine::wal::format::{BLOCK_SIZE, HEADER_SIZE};
    use proptest::prelude::*;
    use std::sync::Arc;

    /// Sizes that land on the boundaries the fragmenting code branches on, not just small
    /// random ones: a payload that exactly fills a block, one that leaves a sub-header tail,
    /// one that needs a MIDDLE fragment.
    fn interesting_size() -> impl Strategy<Value = usize> {
        prop_oneof![
            0..64usize,
            (BLOCK_SIZE - HEADER_SIZE - 10)..(BLOCK_SIZE + 10),
            (2 * BLOCK_SIZE - HEADER_SIZE - 10)..(2 * BLOCK_SIZE + 10),
        ]
    }

    fn payload(size: usize) -> Vec<u8> {
        (0..size)
            .map(|i| u8::try_from((i * 7 + 3) % 256).unwrap_or(0))
            .collect()
    }

    proptest! {
        #[test]
        fn records_of_any_size_round_trip(sizes in prop::collection::vec(interesting_size(), 1..5)) {
            let records: Vec<Vec<u8>> = sizes.iter().map(|size| payload(*size)).collect();
            let (bytes, _) = build(&records);
            let bytes = Arc::new(bytes);
            let (read_records, end) = read(View::truncated(&bytes, bytes.len()));
            prop_assert_eq!(end, ReadOutcome::Eof);
            prop_assert_eq!(read_records, records);
        }

        /// Whatever the sizes, a truncated log never yields a record that was not complete and
        /// never reports corruption.
        #[test]
        fn truncation_never_invents_a_record(
            sizes in prop::collection::vec(interesting_size(), 1..4),
            cut in 0.0f64..1.0,
        ) {
            let records: Vec<Vec<u8>> = sizes.iter().map(|size| payload(*size)).collect();
            let (bytes, ends) = build(&records);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let cut = (cut * bytes.len() as f64) as usize;
            let complete = ends.iter().filter(|end| **end <= cut as u64).count();
            let bytes = Arc::new(bytes);
            let (read_records, end) = read(View::truncated(&bytes, cut));
            prop_assert_eq!(&read_records[..], &records[..complete]);
            prop_assert!(!matches!(end, ReadOutcome::Corrupt(_)), "{:?}", end);
        }
    }
}
