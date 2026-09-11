//! `esker sst-dump` — what is actually inside a sorted string table.
//!
//! `docs/DESIGN.md` §12 lists this among the tools the engine owes its operator. It exists for
//! the moment something has gone wrong: a file that will not open, a compaction that produced
//! the wrong key range, a suspicion that a bloom filter is being built over the wrong bytes.
//! So it reads **hostile bytes by definition**, and `CLAUDE.md` invariant 9 applies with full
//! force — every malformed thing it meets is a printed message and a non-zero exit, never a
//! panic and never a silent skip.
//!
//! # It prints what it can, then fails
//!
//! A forensic tool that refuses to say anything about a damaged file is useless. This one
//! prints each section as it reads it, so a table whose index block is corrupt still shows its
//! footer and its properties before the error. The exit code is what tells a script the file
//! is bad; the output is what tells a person why.
//!
//! # Every block is verified
//!
//! The layout walk reads and checksums *every* block — each data block, the filter, the index
//! and the properties — rather than only the ones a scan would touch. That is the difference
//! between "this table can be read" and "this table is intact".
//!
//! # The one thing it has to be told
//!
//! A table records the name of the prefix extractor its filter was built over, but not the
//! extractor itself, and `TableReader` refuses to use a filter it cannot match (which is what
//! stops a prefix-built filter from being probed with whole keys). `--prefix-len N` supplies a
//! matching `StripSuffix`; without it the dump still reads everything, and says the filter is
//! unusable rather than pretending otherwise.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::bytes::{escape, escape_capped};
use esker_engine::dbformat::{BytewiseComparator, Comparator, InternalKeyComparator};
use esker_engine::fs::{FileSystem, LocalFileSystem, RandomAccessFile, read_exact_at};
use esker_engine::options::{Compression, StripSuffix};
use esker_engine::sst::footer::decode_block;
use esker_engine::sst::{Block, BlockHandle, Footer, TableOptions, TableProperties, TableReader};

/// What to dump, and how much of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DumpOptions {
    /// The table to read.
    pub(crate) path: PathBuf,
    /// Print every key and value, not just the summary.
    pub(crate) verbose: bool,
    /// Rebuild a `StripSuffix` extractor of this length, so the filter can be used.
    pub(crate) prefix_len: Option<usize>,
}

/// Why a dump could not finish.
#[derive(Debug)]
pub(crate) enum DumpError {
    /// The file could not be opened or read.
    Io {
        /// What was being read.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The bytes are not a table this build can read.
    Table(esker_engine::Error),
    /// Writing the report failed — a closed pipe, usually.
    Output(io::Error),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Table(error) => write!(f, "{error}"),
            Self::Output(error) => write!(f, "writing the report: {error}"),
        }
    }
}

impl std::error::Error for DumpError {}

impl From<esker_engine::Error> for DumpError {
    fn from(error: esker_engine::Error) -> Self {
        Self::Table(error)
    }
}

/// `write!` into the report, mapping a broken pipe to [`DumpError::Output`].
macro_rules! line {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).map_err(DumpError::Output)?
    };
}

/// Reads a block's stored bytes, refusing a handle that does not lie inside the file.
///
/// The bound is checked *before* the allocation, so a corrupt handle claiming gigabytes is an
/// error rather than an out-of-memory abort.
fn read_raw(
    file: &dyn RandomAccessFile,
    file_size: u64,
    handle: BlockHandle,
    what: &str,
    path: &std::path::Path,
) -> Result<Vec<u8>, DumpError> {
    let end = handle
        .offset
        .checked_add(handle.total_len())
        .filter(|end| *end <= file_size)
        .ok_or_else(|| {
            DumpError::Table(esker_engine::Error::corruption(
                path.display().to_string(),
                format!(
                    "the {what} handle {{offset: {}, size: {}}} does not lie inside {file_size} bytes",
                    handle.offset, handle.size
                ),
            ))
        })?;
    let len = usize::try_from(end - handle.offset).unwrap_or(usize::MAX);
    let mut raw = vec![0u8; len];
    read_exact_at(file, handle.offset, &mut raw).map_err(|source| DumpError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(raw)
}

/// The codec byte a stored block ends with, read from its trailer.
fn codec_of(raw: &[u8], handle: BlockHandle) -> &'static str {
    let at = usize::try_from(handle.size).unwrap_or(usize::MAX);
    match raw.get(at).copied().and_then(Compression::from_u8) {
        Some(Compression::None) => "none",
        Some(Compression::Lz4) => "lz4",
        None => "?",
    }
}

/// One row of the layout table.
struct BlockRow {
    kind: String,
    handle: BlockHandle,
    codec: &'static str,
    entries: usize,
    /// The index entry's separator key, for a data block; nothing for the rest.
    separator: Option<Vec<u8>>,
}

/// Reads and checksums one block, returning its row.
fn verify(
    file: &dyn RandomAccessFile,
    file_size: u64,
    handle: BlockHandle,
    kind: String,
    path: &std::path::Path,
    count_entries: bool,
) -> Result<(BlockRow, Vec<u8>), DumpError> {
    let raw = read_raw(file, file_size, handle, &kind, path)?;
    let codec = codec_of(&raw, handle);
    // `decode_block` is what verifies the CRC over the payload and the codec byte.
    let payload = decode_block(&raw, &path.display().to_string())?;

    let entries = if count_entries {
        let block = Block::new(Arc::from(payload.clone().into_boxed_slice()))?;
        let mut iter = block.iter(Arc::new(BytewiseComparator));
        let mut n = 0usize;
        iter.seek_to_first();
        while iter.valid() {
            n += 1;
            iter.next();
        }
        iter.status()?;
        n
    } else {
        0
    };

    Ok((
        BlockRow {
            kind,
            handle,
            codec,
            entries,
            separator: None,
        },
        payload,
    ))
}

/// Opens the table's file, twice: once for the raw block walk and once for the reader.
fn open_file(
    fs: LocalFileSystem,
    path: &std::path::Path,
) -> Result<Box<dyn RandomAccessFile>, DumpError> {
    fs.open(path).map_err(|source| DumpError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads and decodes the last 48 bytes.
/// The comparator this table was built with, read from the table's own properties.
///
/// **Read and not assumed, and that is the whole of the fix.** Every SST a store writes is built
/// with `esker.InternalKeyComparator` — a key's MVCC suffix is part of its order — while
/// `TableOptions::default()` names the bytewise one. `TableReader::open` refuses the mismatch,
/// correctly and loudly, so `esker sst-dump` could not open a single file this system had written:
///
/// ```text
/// table was built with comparator "esker.InternalKeyComparator" but is being read with
/// "esker.BytewiseComparator"; its keys would be searched in the wrong order
/// ```
///
/// Hardcoding the internal one would only move the refusal: the golden tables this format is
/// pinned by are bytewise, and dumping *those* is what the tool was first written for. The file
/// says which it is, and the properties block is readable before the reader that needs the answer
/// — which is why this reads it here rather than asking the `TableReader` that cannot be built yet.
///
/// `reconcile.rs` and `manifest_dump.rs` already wrap the internal comparator. This was the third
/// reader of the same fact and the one that did not.
fn comparator_of(
    file: &dyn RandomAccessFile,
    file_size: u64,
    footer: &Footer,
    path: &std::path::Path,
) -> Result<Arc<dyn Comparator>, DumpError> {
    let raw = read_raw(file, file_size, footer.properties, "properties", path)?;
    let payload = decode_block(&raw, &path.display().to_string())?;
    let props =
        TableProperties::decode(Arc::from(payload.into_boxed_slice())).map_err(DumpError::Table)?;
    let bytewise = Arc::new(BytewiseComparator);
    if props.comparator_name == BytewiseComparator.name() {
        return Ok(bytewise);
    }
    let internal = Arc::new(InternalKeyComparator::new(bytewise));
    if props.comparator_name == internal.name() {
        return Ok(internal);
    }
    // Named rather than guessed at: a table built by something else is a fact worth printing,
    // and a tool that silently picked an order would search it wrongly.
    Err(DumpError::Table(esker_engine::Error::InvalidArgument(
        format!(
            "{}: built with comparator {:?}, which this build does not have",
            path.display(),
            props.comparator_name
        ),
    )))
}

fn read_footer(
    file: &dyn RandomAccessFile,
    file_size: u64,
    path: &std::path::Path,
) -> Result<Footer, DumpError> {
    let footer_size = esker_engine::format::SST_FOOTER_SIZE;
    if file_size < footer_size as u64 {
        return Err(DumpError::Table(esker_engine::Error::corruption(
            path.display().to_string(),
            format!("{file_size} bytes is smaller than a {footer_size}-byte footer"),
        )));
    }
    let mut bytes = vec![0u8; footer_size];
    read_exact_at(file, file_size - footer_size as u64, &mut bytes).map_err(|source| {
        DumpError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(Footer::decode(&bytes)?)
}

fn print_footer(out: &mut dyn Write, footer: &Footer) -> Result<(), DumpError> {
    line!(
        out,
        "footer (last {} bytes)",
        esker_engine::format::SST_FOOTER_SIZE
    );
    line!(out, "  format version  {}", footer.format_version);
    for (name, handle) in [
        ("index", footer.index),
        ("filter", footer.filter),
        ("properties", footer.properties),
    ] {
        if handle.is_none() {
            line!(out, "  {name:<14}  absent");
        } else {
            line!(
                out,
                "  {name:<14}  offset {:<10} size {}",
                handle.offset,
                handle.size
            );
        }
    }
    line!(out, "  magic           ESKERSST1");
    line!(out, "");
    Ok(())
}

/// Prints the ranges this table declares deleted, if any.
///
/// The standing invariant is that **no SST below L0 holds one**: a compaction discharges a
/// range tombstone rather than propagating it, which is what lets the read path keep the binary
/// search that assumes a level partitions the key space
/// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md) decision 6). A file is not
/// self-describing about its level, so this prints what it holds and leaves the judgement to
/// whoever knows where the file sits — which is the operator, and `Db::files_by_level` in a
/// test.
fn print_range_deletions(out: &mut dyn Write, table: &TableReader) -> Result<(), DumpError> {
    let tombstones = table.range_tombstones();
    if tombstones.is_empty() {
        return Ok(());
    }
    line!(out, "range deletions ({})", tombstones.len());
    line!(
        out,
        "  a file below L0 holding any of these breaks ADR 0017 decision 6"
    );
    for tombstone in tombstones {
        line!(
            out,
            "  [\"{}\", \"{}\") @ {}",
            escape_capped(&tombstone.begin, 48),
            escape_capped(&tombstone.end, 48),
            tombstone.seqno
        );
    }
    Ok(())
}

fn print_properties(
    out: &mut dyn Write,
    table: &TableReader,
    options: &DumpOptions,
) -> Result<(), DumpError> {
    let props = table.properties();
    line!(out, "properties");
    line!(out, "  entry_count           {}", props.entry_count);
    line!(out, "  data_block_count      {}", props.data_block_count);
    line!(out, "  raw_key_bytes         {}", props.raw_key_bytes);
    line!(out, "  raw_value_bytes       {}", props.raw_value_bytes);
    line!(out, "  data_size             {}", props.data_size);
    line!(out, "  index_size            {}", props.index_size);
    line!(out, "  filter_size           {}", props.filter_size);
    line!(out, "  bloom_bits_per_key    {}", props.bloom_bits_per_key);
    line!(out, "  range_deletions       {}", props.range_del_count);
    line!(out, "  compression           {:?}", props.compression);
    line!(out, "  comparator            {}", props.comparator_name);
    line!(
        out,
        "  prefix_extractor      {}",
        props.prefix_extractor_name.as_deref().unwrap_or("(none)")
    );
    line!(
        out,
        "  seqno range           {}..={}",
        props.smallest_seqno,
        props.largest_seqno
    );
    line!(
        out,
        "  smallest_key          \"{}\"",
        escape_capped(&props.smallest_key, 48)
    );
    line!(
        out,
        "  largest_key           \"{}\"",
        escape_capped(&props.largest_key, 48)
    );

    // Saying "no filter" without saying why would send an operator hunting a bug that is not
    // there: a filter this reader cannot match is the documented behaviour, not damage.
    let usable = if table.has_filter() {
        "yes".to_owned()
    } else if props.prefix_extractor_name.is_some() && options.prefix_len.is_none() {
        format!(
            "no — built over {}; pass --prefix-len to match it",
            props.prefix_extractor_name.as_deref().unwrap_or("?")
        )
    } else {
        "no".to_owned()
    };
    line!(out, "  filter usable         {usable}");
    line!(out, "");
    Ok(())
}

/// Reads and checksums every block in the file, in the order ADR 0005 fixes.
fn verify_all_blocks(
    file: &dyn RandomAccessFile,
    file_size: u64,
    footer: &Footer,
    path: &std::path::Path,
) -> Result<Vec<BlockRow>, DumpError> {
    let (index_row, index_payload) = verify(
        file,
        file_size,
        footer.index,
        "index".to_owned(),
        path,
        true,
    )?;
    let index_block = Block::new(Arc::from(index_payload.into_boxed_slice()))?;

    let mut rows = Vec::new();
    let mut index_iter = index_block.iter(Arc::new(BytewiseComparator));
    index_iter.seek_to_first();
    let mut data_index = 0usize;
    while index_iter.valid() {
        let (handle, _) = BlockHandle::decode_from(index_iter.value())?;
        let (mut row, _) = verify(
            file,
            file_size,
            handle,
            format!("data[{data_index}]"),
            path,
            true,
        )?;
        row.separator = Some(index_iter.key().to_vec());
        rows.push(row);
        data_index += 1;
        index_iter.next();
    }
    index_iter.status()?;

    if !footer.filter.is_none() {
        let (row, _) = verify(
            file,
            file_size,
            footer.filter,
            "filter".to_owned(),
            path,
            false,
        )?;
        rows.push(row);
    }
    rows.push(index_row);
    let (props_row, _) = verify(
        file,
        file_size,
        footer.properties,
        "properties".to_owned(),
        path,
        true,
    )?;
    rows.push(props_row);
    Ok(rows)
}

fn print_blocks(out: &mut dyn Write, rows: &[BlockRow]) -> Result<(), DumpError> {
    line!(out, "blocks");
    line!(
        out,
        "  {:<12} {:>10} {:>9}  {:<5} {:>8}  {}",
        "kind",
        "offset",
        "size",
        "codec",
        "entries",
        "separator key"
    );
    for row in rows {
        // A filter block is a bit array, not entries; printing 0 would read as "empty".
        let entries = if row.kind == "filter" {
            "-".to_owned()
        } else {
            row.entries.to_string()
        };
        let separator = match &row.separator {
            Some(key) => format!("\"{}\"", escape_capped(key, 32)),
            None => String::new(),
        };
        line!(
            out,
            "  {:<12} {:>10} {:>9}  {:<5} {:>8}  {}",
            row.kind,
            row.handle.offset,
            row.handle.size,
            row.codec,
            entries,
            separator
        );
    }
    line!(out, "");
    line!(out, "checksums: {} blocks verified, all ok", rows.len());
    Ok(())
}

fn print_entries(out: &mut dyn Write, table: &TableReader) -> Result<(), DumpError> {
    line!(out, "");
    line!(out, "entries");
    let mut iter = table.iter();
    iter.seek_to_first();
    while iter.valid() {
        line!(
            out,
            "  \"{}\" -> \"{}\"",
            escape(iter.key()),
            escape(iter.value())
        );
        iter.next();
    }
    iter.status()?;
    Ok(())
}

/// Dumps the table named by `options` into `out`.
///
/// Sections are printed as they are read, so a file that fails halfway still explains as much
/// of itself as it could — see the module docs.
pub(crate) fn run(options: &DumpOptions, out: &mut dyn Write) -> Result<(), DumpError> {
    let fs = LocalFileSystem::new();
    let path = options.path.as_path();

    let file = open_file(fs, path)?;
    let file_size = file.size().map_err(|source| DumpError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    line!(out, "file:  {}", path.display());
    line!(out, "size:  {file_size} bytes");
    line!(out, "");

    let footer = read_footer(file.as_ref(), file_size, path)?;
    print_footer(out, &footer)?;

    let comparator = comparator_of(file.as_ref(), file_size, &footer, path)?;
    let table_options = TableOptions {
        prefix_extractor: options
            .prefix_len
            .map(|len| Arc::new(StripSuffix::new(len)) as Arc<_>),
        comparator: Arc::clone(&comparator),
        ..TableOptions::default()
    };
    let table = TableReader::open(open_file(fs, path)?, 0, table_options, None)?;
    print_properties(out, &table, options)?;
    print_range_deletions(out, &table)?;

    let rows = verify_all_blocks(file.as_ref(), file_size, &footer, path)?;
    print_blocks(out, &rows)?;

    if options.verbose {
        print_entries(out, &table)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DumpOptions, run};
    use std::path::{Path, PathBuf};

    /// The golden tables the `cl-p1-sst` lane froze. Reading the tool's output against the
    /// same files the format is pinned by is what stops the two drifting apart.
    fn golden(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../esker-engine/tests/golden/sst")
            .join(name)
    }

    fn options(path: PathBuf) -> DumpOptions {
        DumpOptions {
            path,
            verbose: false,
            prefix_len: None,
        }
    }

    /// Runs a dump and returns its report plus whether it succeeded.
    fn dump(options: &DumpOptions) -> (String, bool) {
        let mut out = Vec::new();
        let ok = run(options, &mut out).is_ok();
        (String::from_utf8_lossy(&out).into_owned(), ok)
    }

    /// Writes `bytes` to a temporary file and dumps it.
    fn dump_bytes(bytes: &[u8]) -> (String, bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        std::fs::write(&path, bytes).unwrap();
        dump(&options(path))
    }

    /// The whole report, against a table whose bytes are frozen.
    #[test]
    fn dumps_a_golden_table() {
        let (report, ok) = dump(&options(golden("default.sst")));
        assert!(ok, "{report}");

        for expected in [
            "footer (last 48 bytes)",
            "format version  1",
            "magic           ESKERSST1",
            "properties",
            "comparator            esker.BytewiseComparator",
            "blocks",
            "data[0]",
            "filter",
            "index",
            "checksums:",
            "blocks verified, all ok",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }

        // The layout it prints is the order ADR 0005 fixes. Scoped to the blocks section,
        // because `filter` and `index` also name properties above it.
        let blocks = report
            .split("blocks\n")
            .nth(1)
            .and_then(|rest| rest.split("\nchecksums:").next())
            .expect("a blocks section");
        let at = |needle: &str| {
            blocks
                .find(needle)
                .unwrap_or_else(|| panic!("no {needle} in:\n{blocks}"))
        };
        assert!(at("data[0]") < at("filter"), "{blocks}");
        assert!(at("filter") < at("index"), "{blocks}");
        assert!(at("index") < at("properties"), "{blocks}");
    }

    /// A table whose filter was built over prefixes says so, and says what to do about it.
    #[test]
    fn a_prefix_built_filter_is_reported_and_can_be_matched() {
        let (report, ok) = dump(&options(golden("plain-prefix.sst")));
        assert!(ok, "{report}");
        assert!(
            report.contains("prefix_extractor      esker.StripSuffix.8"),
            "{report}"
        );
        assert!(
            report.contains("filter usable         no — built over esker.StripSuffix.8"),
            "{report}"
        );

        let (report, ok) = dump(&DumpOptions {
            prefix_len: Some(8),
            ..options(golden("plain-prefix.sst"))
        });
        assert!(ok, "{report}");
        assert!(report.contains("filter usable         yes"), "{report}");
    }

    /// `--verbose` prints every entry the properties claim, and no more.
    #[test]
    fn verbose_prints_every_entry() {
        let (report, ok) = dump(&DumpOptions {
            verbose: true,
            ..options(golden("default.sst"))
        });
        assert!(ok, "{report}");

        let claimed: usize = report
            .lines()
            .find_map(|line| line.trim().strip_prefix("entry_count"))
            .and_then(|rest| rest.trim().parse().ok())
            .expect("the report states an entry count");
        let printed = report
            .lines()
            .skip_while(|line| *line != "entries")
            .filter(|line| line.contains("\" -> \""))
            .count();
        assert_eq!(
            printed, claimed,
            "{printed} entries printed, {claimed} claimed"
        );
        assert!(claimed > 100, "the golden table lost its entries");

        // Without --verbose the entries are not printed at all.
        let (quiet, _) = dump(&options(golden("default.sst")));
        assert!(!quiet.contains("\" -> \""), "{quiet}");
    }

    /// Corruption anywhere is a non-zero exit — and the report still says everything it read
    /// before it hit the bad bytes, which is the point of the tool.
    #[test]
    fn a_corrupt_block_fails_after_printing_what_it_could() {
        let good = std::fs::read(golden("default.sst")).unwrap();

        // A byte inside the first data block.
        let mut damaged = good.clone();
        damaged[100] ^= 0xff;
        let (report, ok) = dump_bytes(&damaged);
        assert!(!ok, "a corrupt data block dumped cleanly:\n{report}");
        assert!(report.contains("footer (last 48 bytes)"), "{report}");
        assert!(report.contains("properties"), "{report}");

        // A byte inside the properties block: caught before the report gets that far.
        let mut damaged = good.clone();
        let at = good.len() - 60;
        damaged[at] ^= 0xff;
        let (report, ok) = dump_bytes(&damaged);
        assert!(!ok, "corrupt properties dumped cleanly:\n{report}");

        // The magic.
        let mut damaged = good.clone();
        let last = damaged.len() - 1;
        damaged[last] = b'9';
        let (_, ok) = dump_bytes(&damaged);
        assert!(!ok, "a bad magic dumped cleanly");
    }

    /// The tool reads whatever it is pointed at, so every hostile shape has to come back as an
    /// error rather than a panic (`CLAUDE.md` invariant 9).
    #[test]
    fn hostile_files_are_errors_not_panics() {
        assert!(!dump_bytes(&[]).1, "an empty file dumped cleanly");
        assert!(!dump_bytes(&[0u8; 47]).1, "47 bytes dumped cleanly");
        assert!(!dump_bytes(&[0u8; 48]).1, "48 zero bytes dumped cleanly");
        assert!(!dump_bytes(&vec![0xab; 8192]).1, "garbage dumped cleanly");

        // A real table truncated at every 64th byte: never a panic, and never a clean dump,
        // because the footer is the last thing written.
        let good = std::fs::read(golden("default.sst")).unwrap();
        for cut in (0..good.len()).step_by(64) {
            let (_, ok) = dump_bytes(&good[..cut]);
            assert!(!ok, "truncating to {cut} bytes dumped cleanly");
        }

        // A footer whose handles point outside the file: refused before anything is allocated
        // for them.
        let mut forged = good.clone();
        let footer_at = forged.len() - 48;
        forged[footer_at..footer_at + 8].copy_from_slice(&[0xff; 8]);
        let (_, ok) = dump_bytes(&forged);
        assert!(!ok, "an out-of-range handle dumped cleanly");
    }

    /// A path that is not there names itself in the error, rather than panicking on an unwrap.
    #[test]
    fn a_missing_file_is_an_error() {
        let mut out = Vec::new();
        let error = run(&options(PathBuf::from("/no/such/table.sst")), &mut out).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("/no/such/table.sst"), "{text}");
        assert!(
            out.is_empty(),
            "it printed a report for a file it never opened"
        );
    }
}
