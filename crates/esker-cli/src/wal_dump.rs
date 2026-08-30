//! `esker wal-dump` — what is actually inside a write-ahead log segment.
//!
//! Two views of the same file, because a log has two structures and a failure usually lives in
//! exactly one of them:
//!
//! * **Fragments** — the physical layer. 32 KiB blocks, each holding
//!   `crc32c:u32 ++ len:u16 ++ type:u8` headers followed by payload, with records split
//!   `FULL` or `FIRST/MIDDLE/LAST` across block boundaries (`docs/DESIGN.md` §4.3). This is
//!   where a bad checksum, a wrong type byte or a length that runs off the end shows up.
//! * **Records** — the logical layer. Each reassembled record is one serialised
//!   [`WriteBatch`], and is printed decoded: sequence number, count, and one line per entry.
//!
//! Both are read through the engine's own code — [`LogReader`] for the record walk and
//! [`wal::format`]'s public decoder for the fragment walk — so the tool cannot disagree with
//! the engine about the format. Nothing here re-derives a layout.
//!
//! # A torn tail is not an error
//!
//! A record that stops part-way through the end of a segment is what a crash looks like
//! (`docs/DESIGN.md` §4.3): the process died between the write and the sync. `wal-dump` says
//! so and **exits 0**. Anything else — a bad checksum, an unknown record type, a fragment
//! order that cannot happen — is corruption and exits non-zero, because at any position other
//! than the tail it means a lost write.
//!
//! [`WriteBatch`]: esker_engine::batch::WriteBatch
//! [`wal::format`]: esker_engine::wal::format

use std::io::{self, Write};
use std::path::PathBuf;

use esker_engine::batch::WriteBatch;
use esker_engine::fs::{FileSystem, LocalFileSystem, read_exact_at};
use esker_engine::wal::format::{HEADER_SIZE, decode_header, fragment_crc};
use esker_engine::wal::{BLOCK_SIZE, LogReader, ReadOutcome, RecordType};

use crate::bytes::{escape, escape_capped, plural};

/// What to dump, and how much of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DumpOptions {
    /// The segment to read.
    pub(crate) path: PathBuf,
    /// Print entry values as well as keys.
    pub(crate) verbose: bool,
}

/// Why a dump could not finish, or finished on damaged bytes.
#[derive(Debug)]
pub(crate) enum DumpError {
    /// The file could not be opened or read.
    Io {
        /// What was being read.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The bytes are not a log this build can read.
    Log(esker_engine::Error),
    /// The log stops on bytes that cannot be part of one.
    Corrupt(String),
    /// Writing the report failed — a closed pipe, usually.
    Output(io::Error),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Log(error) => write!(f, "{error}"),
            Self::Corrupt(why) => write!(f, "corrupt log: {why}"),
            Self::Output(error) => write!(f, "writing the report: {error}"),
        }
    }
}

impl std::error::Error for DumpError {}

impl From<esker_engine::Error> for DumpError {
    fn from(error: esker_engine::Error) -> Self {
        Self::Log(error)
    }
}

macro_rules! line {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).map_err(DumpError::Output)?
    };
}

/// One physical fragment as the header describes it.
struct Fragment {
    offset: u64,
    kind: Option<RecordType>,
    raw_kind: u8,
    payload_len: usize,
    stored_crc: u32,
    /// `None` when the payload does not fit in what is left of the file.
    checksum_ok: Option<bool>,
}

/// Walks the physical layer with the engine's own header decoder.
///
/// Stops at the first thing it cannot read, which is what a reader would do; the record walk
/// is the authority on whether that was a torn tail or corruption.
fn fragments(bytes: &[u8]) -> Vec<Fragment> {
    let mut found = Vec::new();
    let mut block_start = 0usize;

    while block_start < bytes.len() {
        let block_end = (block_start + BLOCK_SIZE).min(bytes.len());
        let mut at = block_start;

        while at + HEADER_SIZE <= block_end {
            let mut header = [0u8; HEADER_SIZE];
            header.copy_from_slice(&bytes[at..at + HEADER_SIZE]);
            let (stored_crc, payload_len, raw_kind) = decode_header(&header);

            // A zero-filled block *tail* is padding, not a fragment: §4.3 says a tail too
            // short for a header is zero-filled, and the writer zeroes the remainder. Only a
            // tail, though — zeros at the very start of a block mean the block was never
            // written, which `LogReader` calls corruption, and a fragment table that quietly
            // showed nothing there would disagree with the summary below it.
            if stored_crc == 0 && payload_len == 0 && raw_kind == 0 && at > block_start {
                break;
            }

            let kind = RecordType::from_u8(raw_kind);
            let payload_start = at + HEADER_SIZE;
            let checksum_ok = match (kind, payload_start.checked_add(payload_len)) {
                (Some(kind), Some(end)) if end <= block_end => {
                    Some(fragment_crc(kind, &bytes[payload_start..end]) == stored_crc)
                }
                // The length runs off the block or the type is unknown: nothing to checksum.
                _ => None,
            };

            found.push(Fragment {
                offset: at as u64,
                kind,
                raw_kind,
                payload_len,
                stored_crc,
                checksum_ok,
            });

            match payload_start.checked_add(payload_len) {
                Some(next) if next <= block_end => at = next,
                _ => break,
            }
        }
        block_start += BLOCK_SIZE;
    }
    found
}

fn print_fragments(out: &mut dyn Write, found: &[Fragment]) -> Result<(), DumpError> {
    line!(out, "fragments");
    line!(
        out,
        "  {:>10}  {:<7} {:>8}  {:<10}  {}",
        "offset",
        "type",
        "payload",
        "crc",
        "status"
    );
    for fragment in found {
        let kind = match fragment.kind {
            Some(RecordType::Full) => "FULL".to_owned(),
            Some(RecordType::First) => "FIRST".to_owned(),
            Some(RecordType::Middle) => "MIDDLE".to_owned(),
            Some(RecordType::Last) => "LAST".to_owned(),
            None => format!("?{}", fragment.raw_kind),
        };
        let status = match fragment.checksum_ok {
            Some(true) => "ok",
            Some(false) => "CHECKSUM MISMATCH",
            None => "truncated or unknown type",
        };
        line!(
            out,
            "  {:>10}  {kind:<7} {:>8}  {:#010x}  {status}",
            fragment.offset,
            fragment.payload_len,
            fragment.stored_crc
        );
    }
    line!(out, "");
    Ok(())
}

/// Prints one reassembled record as the batch it is.
fn print_record(
    out: &mut dyn Write,
    index: usize,
    offset: u64,
    payload: &[u8],
    verbose: bool,
) -> Result<(), DumpError> {
    let batch = match WriteBatch::from_bytes(payload) {
        Ok(batch) => batch,
        Err(error) => {
            // The record's checksum verified, so the bytes are what was written — they are
            // just not a batch. That is a format problem worth naming precisely.
            line!(
                out,
                "  {index:<4} {offset:>10} {:>8}  NOT A WRITE BATCH: {error}",
                payload.len()
            );
            return Err(DumpError::Corrupt(format!(
                "record {index} at offset {offset} is not a write batch: {error}"
            )));
        }
    };

    line!(
        out,
        "  {index:<4} {offset:>10} {:>8}  seqno {:<8} {} {}",
        payload.len(),
        batch.seqno(),
        batch.count(),
        if batch.count() == 1 {
            "entry"
        } else {
            "entries"
        }
    );

    for entry in &batch {
        let entry = entry?;
        let kind = format!("{:?}", entry.kind);
        if verbose {
            line!(
                out,
                "         cf={:<3} {kind:<11} seqno {:<8} \"{}\" -> \"{}\"",
                entry.cf,
                entry.seqno,
                escape(entry.key),
                escape(entry.value)
            );
        } else {
            line!(
                out,
                "         cf={:<3} {kind:<11} seqno {:<8} \"{}\" ({} value bytes)",
                entry.cf,
                entry.seqno,
                escape_capped(entry.key, 48),
                entry.value.len()
            );
        }
    }
    Ok(())
}

/// Dumps the segment named by `options` into `out`.
pub(crate) fn run(options: &DumpOptions, out: &mut dyn Write) -> Result<(), DumpError> {
    let fs = LocalFileSystem::new();
    let path = options.path.as_path();
    let io_error = |source: io::Error| DumpError::Io {
        path: path.to_path_buf(),
        source,
    };

    let file = fs.open(path).map_err(io_error)?;
    let size = file.size().map_err(io_error)?;
    line!(out, "file:   {}", path.display());
    line!(out, "size:   {size} bytes");
    line!(out, "block:  {BLOCK_SIZE} bytes");
    line!(out, "");

    // The whole segment, so the fragment walk can look at it directly. A log segment is one
    // memtable generation, which `docs/DESIGN.md` §14 caps at 64 MiB.
    let len = usize::try_from(size).unwrap_or(usize::MAX);
    let mut bytes = vec![0u8; len];
    read_exact_at(file.as_ref(), 0, &mut bytes).map_err(io_error)?;
    print_fragments(out, &fragments(&bytes))?;

    // The record walk, through the reader the engine itself recovers with.
    let mut reader = LogReader::new(fs.open(path).map_err(io_error)?, path.display().to_string());
    line!(out, "records");
    line!(
        out,
        "  {:<4} {:>10} {:>8}  {}",
        "#",
        "offset",
        "bytes",
        "batch"
    );

    let mut records = 0usize;
    let ending = loop {
        let offset = reader.position();
        match reader.read_record()? {
            ReadOutcome::Record(payload) => {
                print_record(out, records, offset, &payload, options.verbose)?;
                records += 1;
            }
            other => break other,
        }
    };

    line!(out, "");
    match &ending {
        ReadOutcome::Eof => {
            line!(
                out,
                "summary: {}, ends cleanly at {} bytes",
                plural(records, "record"),
                reader.position()
            );
            Ok(())
        }
        ReadOutcome::Torn(why) => {
            // The expected shape of a crash, at the tail of the segment that was open.
            line!(
                out,
                "summary: {}, then a torn record at {}: {why}",
                plural(records, "record"),
                reader.position()
            );
            line!(
                out,
                "notice:  a torn record at the tail is what a crash looks like, not damage \
                 (docs/DESIGN.md §4.3). Everything above it was recovered."
            );
            Ok(())
        }
        ReadOutcome::Corrupt(why) => {
            line!(
                out,
                "summary: {}, then corruption at {}: {why}",
                plural(records, "record"),
                reader.position()
            );
            Err(DumpError::Corrupt(format!(
                "{} at offset {}: {why}",
                path.display(),
                reader.position()
            )))
        }
        ReadOutcome::Record(_) => unreachable!("the loop breaks only on a non-record outcome"),
    }
}

#[cfg(test)]
mod tests {
    use super::{DumpOptions, run};
    use esker_engine::fs::{FileSystem, LocalFileSystem};
    use esker_engine::options::Options;
    use esker_engine::{Db, cf};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Builds a real database in a temporary directory and returns it with its log segment.
    ///
    /// The log is written by the engine rather than assembled here on purpose: a dump tested
    /// against bytes the test itself laid out would only prove the two agree with each other.
    fn a_log() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            fs,
            &cf::BUILTIN,
        )
        .unwrap();

        for i in 0..30u32 {
            db.put(
                cf::DEFAULT,
                format!("key-{i:04}").as_bytes(),
                format!("value-{i}").as_bytes(),
            )
            .unwrap();
        }
        db.delete(cf::DEFAULT, b"key-0007").unwrap();
        // A record long enough to be split across blocks, so FIRST/LAST appear.
        db.put(cf::DEFAULT, b"big", &vec![b'z'; 40_000]).unwrap();
        drop(db);

        let log = LocalFileSystem::new()
            .list(dir.path())
            .unwrap()
            .into_iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "wal"))
            .expect("the database has a log segment");
        (dir, log)
    }

    fn dump(path: &Path, verbose: bool) -> (String, bool) {
        let mut out = Vec::new();
        let ok = run(
            &DumpOptions {
                path: path.to_path_buf(),
                verbose,
            },
            &mut out,
        )
        .is_ok();
        (String::from_utf8_lossy(&out).into_owned(), ok)
    }

    /// Writes `bytes` to a fresh temporary file and dumps it.
    fn dump_bytes(bytes: &[u8]) -> (String, bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("000001.wal");
        std::fs::write(&path, bytes).unwrap();
        dump(&path, false)
    }

    #[test]
    fn dumps_a_log_the_engine_wrote() {
        let (_dir, log) = a_log();
        let (report, ok) = dump(&log, false);
        assert!(ok, "{report}");

        for expected in ["fragments", "records", "FULL", "seqno", "ends cleanly"] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
        // A 40 KiB value cannot fit one 32 KiB block, so the log must show a split record.
        assert!(report.contains("FIRST"), "no split record in:\n{report}");
        assert!(report.contains("LAST"), "no split record in:\n{report}");
        assert!(!report.contains("CHECKSUM MISMATCH"), "{report}");

        // Keys are shown, values only counted, until --verbose.
        assert!(report.contains("\"key-0000\""), "{report}");
        assert!(report.contains("value bytes"), "{report}");
        assert!(!report.contains("-> \"value-0\""), "{report}");

        let (verbose, ok) = dump(&log, true);
        assert!(ok);
        assert!(verbose.contains("-> \"value-0\""), "{verbose}");
        // A delete has no value, and says so rather than inventing one.
        assert!(verbose.contains("Delete"), "{verbose}");
    }

    /// A record that stops part-way through the end of the file is what a crash looks like.
    /// It is reported, and it is not an error.
    #[test]
    fn a_truncated_log_is_a_torn_tail_not_a_failure() {
        let (_dir, log) = a_log();
        let full = std::fs::read(&log).unwrap();

        let mut torn = 0;
        // Every truncation inside the last record, and a few well before it.
        for cut in [
            full.len() - 1,
            full.len() - 5,
            full.len() - 60,
            full.len() / 2,
        ] {
            let (report, ok) = dump_bytes(&full[..cut]);
            assert!(
                ok,
                "truncating to {cut} was reported as an error:\n{report}"
            );
            if report.contains("torn record at the tail") {
                torn += 1;
                assert!(report.contains("notice:"), "{report}");
            }
        }
        assert!(torn > 0, "no truncation produced a torn tail");
    }

    /// A flipped bit inside a record is corruption, wherever it is: exit non-zero.
    #[test]
    fn a_bit_flipped_log_is_corruption() {
        let (_dir, log) = a_log();
        let full = std::fs::read(&log).unwrap();

        // Byte 20 is inside the first record's payload, well past its header.
        let mut damaged = full.clone();
        damaged[20] ^= 0xff;
        let (report, ok) = dump_bytes(&damaged);
        assert!(!ok, "a flipped payload byte dumped cleanly:\n{report}");
        assert!(
            report.contains("CHECKSUM MISMATCH"),
            "the fragment table did not flag it:\n{report}"
        );
        assert!(report.contains("corruption at"), "{report}");

        // And a flipped byte in a header, which changes the length or the type.
        let mut damaged = full.clone();
        damaged[6] ^= 0xff;
        let (_, ok) = dump_bytes(&damaged);
        assert!(!ok, "a flipped type byte dumped cleanly");
    }

    /// The tool reads whatever it is pointed at, so hostile shapes must be errors or empty
    /// reports, never panics (`CLAUDE.md` invariant 9).
    #[test]
    fn hostile_files_do_not_panic() {
        // An empty log is a legal, empty log.
        let (report, ok) = dump_bytes(&[]);
        assert!(ok, "{report}");
        assert!(report.contains("0 records"), "{report}");

        // Random bytes are not.
        let (_, ok) = dump_bytes(&vec![0xab; 4096]);
        assert!(!ok, "garbage dumped cleanly");

        // A header alone, promising a payload that is not there.
        let (_, ok) = dump_bytes(&[0u8, 0, 0, 0, 0xff, 0xff, 1]);
        assert!(!ok, "a header with no payload dumped cleanly");

        // A file of zeros is not an empty log, it is a block that was never written, and
        // `LogReader` says so. The fragment table has to agree rather than showing nothing.
        let (report, ok) = dump_bytes(&vec![0u8; 512]);
        assert!(!ok, "a file of zeros dumped cleanly:\n{report}");
        assert!(
            report.contains("?0"),
            "the fragment table hid it:\n{report}"
        );
        assert!(report.contains("not a record type"), "{report}");
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let mut out = Vec::new();
        let error = run(
            &DumpOptions {
                path: PathBuf::from("/no/such/000001.wal"),
                verbose: false,
            },
            &mut out,
        )
        .unwrap_err();
        assert!(error.to_string().contains("/no/such/000001.wal"), "{error}");
    }
}
