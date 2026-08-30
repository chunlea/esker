//! `esker manifest-dump` — the history of a database's file set, and where it ended up.
//!
//! The manifest is a log of [`VersionEdit`]s (`docs/DESIGN.md` §4.6): each one an atomic change
//! to the set of live SSTs and to the numbers that describe the database. `CURRENT` names the
//! manifest that is in force, replaced by write-temp, fsync, rename — so the walk starts there,
//! never by guessing at a file name.
//!
//! Two things are printed, because two different questions get asked of a manifest:
//!
//! * **Every edit, decoded.** What changed, in order. This is what you read when a database
//!   will not open, or when a file exists that nothing seems to reference.
//! * **The reconstructed version.** The same fold the engine performs at open, through
//!   [`version::Builder`], printed per column family and per level. This is what you read when
//!   you want to know what the database currently believes it is made of.
//!
//! # Exit codes
//!
//! Any edit that will not decode is a non-zero exit: the manifest is the one file whose loss
//! makes every SST unreadable, so a byte out of place in it is never a shrug. A **torn tail**
//! is not a decode error — it is a crash between an append and its sync, which
//! [`VersionSet::recover`] tolerates for exactly the same reason the WAL reader does — and is
//! reported with a notice and exit 0.
//!
//! [`VersionEdit`]: esker_engine::version::VersionEdit
//! [`version::Builder`]: esker_engine::version::builder::Builder
//! [`VersionSet::recover`]: esker_engine::version::VersionSet::recover

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_engine::dbformat::{
    BytewiseComparator, Comparator, InternalKeyComparator, split_internal_key,
};
use esker_engine::filename;
use esker_engine::fs::{FileSystem, LocalFileSystem, read_exact_at};
use esker_engine::options::defaults;
use esker_engine::version::builder::Builder;
use esker_engine::version::{FileMeta, Version, VersionEdit};
use esker_engine::wal::{LogReader, ReadOutcome};

use crate::bytes::{escape_capped, plural};

/// What to dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DumpOptions {
    /// The database directory — the one holding `CURRENT`.
    pub(crate) path: PathBuf,
}

/// Why a dump could not finish.
#[derive(Debug)]
pub(crate) enum DumpError {
    /// A file could not be opened or read.
    Io {
        /// What was being read.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The directory does not hold a database, or the manifest will not decode.
    Manifest(String),
    /// An engine error while decoding or folding.
    Engine(esker_engine::Error),
    /// Writing the report failed.
    Output(io::Error),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Manifest(why) => write!(f, "{why}"),
            Self::Engine(error) => write!(f, "{error}"),
            Self::Output(error) => write!(f, "writing the report: {error}"),
        }
    }
}

impl std::error::Error for DumpError {}

impl From<esker_engine::Error> for DumpError {
    fn from(error: esker_engine::Error) -> Self {
        Self::Engine(error)
    }
}

macro_rules! line {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).map_err(DumpError::Output)?
    };
}

/// Reads a whole file through the filesystem seam.
fn read_all(fs: LocalFileSystem, path: &Path) -> Result<Vec<u8>, DumpError> {
    let io_error = |source: io::Error| DumpError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = fs.open(path).map_err(io_error)?;
    let size = file.size().map_err(io_error)?;
    let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
    read_exact_at(file.as_ref(), 0, &mut bytes).map_err(io_error)?;
    Ok(bytes)
}

/// Renders an internal key as the user key, sequence number and kind it is made of.
///
/// A `FileMeta`'s bounds are *internal* keys — `user_key ++ tag` — so printing them raw would
/// show eight bytes of little-endian tag glued to every key and invite someone to compare them
/// with a user key.
fn internal(key: &[u8]) -> String {
    match split_internal_key(key) {
        Some((user, seqno, kind)) => {
            format!("\"{}\" @{seqno} {kind:?}", escape_capped(user, 40))
        }
        None => format!("\"{}\" (not an internal key)", escape_capped(key, 40)),
    }
}

fn print_edit(
    out: &mut dyn Write,
    index: usize,
    offset: u64,
    edit: &VersionEdit,
    names: &BTreeMap<u32, String>,
) -> Result<(), DumpError> {
    let name_of = |cf: u32| {
        names
            .get(&cf)
            .map_or_else(|| format!("cf {cf}"), |name| format!("{name} (cf {cf})"))
    };

    line!(out, "  edit {index} at offset {offset}");
    if let Some(comparator) = &edit.comparator {
        line!(out, "    comparator        {comparator}");
    }
    if let Some(number) = edit.log_number {
        line!(out, "    log_number        {number}");
    }
    if let Some(number) = edit.next_file_number {
        line!(out, "    next_file_number  {number}");
    }
    if let Some(seqno) = edit.last_seqno {
        line!(out, "    last_seqno        {seqno}");
    }
    for (id, name) in &edit.cf_added {
        line!(out, "    + column family   {name} (cf {id})");
    }
    for id in &edit.cf_dropped {
        line!(out, "    - column family   {}", name_of(*id));
    }
    for (cf, level, number) in &edit.deleted_files {
        line!(out, "    - {number:06}.sst    L{level} of {}", name_of(*cf));
    }
    for (cf, level, meta) in &edit.added_files {
        line!(
            out,
            "    + {:06}.sst    L{level} of {}, {} bytes, seqno {}..={}",
            meta.number,
            name_of(*cf),
            meta.size,
            meta.smallest_seqno,
            meta.largest_seqno
        );
        line!(out, "        smallest      {}", internal(&meta.smallest));
        line!(out, "        largest       {}", internal(&meta.largest));
    }
    if edit.is_empty() {
        line!(out, "    (changes nothing)");
    }
    Ok(())
}

fn print_version(
    out: &mut dyn Write,
    version: &Version,
    names: &BTreeMap<u32, String>,
) -> Result<(), DumpError> {
    line!(out, "version");
    let mut families: Vec<u32> = version.column_families().collect();
    families.sort_unstable();
    if families.is_empty() {
        line!(out, "  (no column families)");
    }

    for cf in families {
        let name = names
            .get(&cf)
            .map_or_else(|| format!("cf {cf}"), Clone::clone);
        let Some(family) = version.cf(cf) else {
            continue;
        };
        let total: usize = (0..family.num_levels())
            .map(|level| family.files(level).len())
            .sum();
        line!(out, "  {name} (cf {cf}): {}", plural(total, "file"));

        for level in 0..family.num_levels() {
            let files = family.files(level);
            if files.is_empty() {
                continue;
            }
            line!(
                out,
                "    L{level}: {}, {} bytes",
                plural(files.len(), "file"),
                family.level_bytes(level)
            );
            for meta in files {
                print_file(out, meta)?;
            }
        }
    }
    Ok(())
}

fn print_file(out: &mut dyn Write, meta: &FileMeta) -> Result<(), DumpError> {
    line!(
        out,
        "      {:06}.sst  {:>10} bytes  seqno {}..={}",
        meta.number,
        meta.size,
        meta.smallest_seqno,
        meta.largest_seqno
    );
    line!(out, "        smallest  {}", internal(&meta.smallest));
    line!(out, "        largest   {}", internal(&meta.largest));
    Ok(())
}

/// Reads `CURRENT` and returns the manifest it names, with the text it held.
///
/// Never a guessed file name: `CURRENT` is the only pointer the engine replaces atomically
/// (`CLAUDE.md` invariant 3), so it is the only honest way to know which manifest is in force.
/// A directory can hold several `MANIFEST-*` files; one of them counts and the rest are
/// waiting to be collected.
fn locate_manifest(fs: LocalFileSystem, dir: &Path) -> Result<(PathBuf, String), DumpError> {
    let current_path = filename::current(dir);
    let bytes = read_all(fs, &current_path)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| DumpError::Manifest(format!("{} is not text", current_path.display())))?;
    let number = filename::parse_current(&text).ok_or_else(|| {
        DumpError::Manifest(format!(
            "{} does not name a manifest (it holds {:?})",
            current_path.display(),
            text
        ))
    })?;
    Ok((filename::manifest(dir, number), text))
}

/// Dumps the manifest of the database in `options.path`.
pub(crate) fn run(options: &DumpOptions, out: &mut dyn Write) -> Result<(), DumpError> {
    let fs = LocalFileSystem::new();
    let dir = options.path.as_path();
    let (manifest_path, current_text) = locate_manifest(fs, dir)?;

    line!(out, "directory:  {}", dir.display());
    line!(out, "current:    {}", current_text.trim_end());
    line!(out, "manifest:   {}", manifest_path.display());
    line!(out, "");

    let file = fs.open(&manifest_path).map_err(|source| DumpError::Io {
        path: manifest_path.clone(),
        source,
    })?;
    let mut reader = LogReader::new(file, manifest_path.display().to_string());

    // Column family names accumulate as the edits create them, so an edit that only names an
    // id can still be printed with the name that id was given earlier in the same log.
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    let mut builder = Builder::new(Version::empty(), defaults::NUM_LEVELS);
    let mut edits = 0usize;
    let mut comparator_name: Option<String> = None;

    line!(out, "edits");
    let ending = loop {
        let offset = reader.position();
        match reader.read_record()? {
            ReadOutcome::Record(payload) => {
                let edit = VersionEdit::decode(&payload).map_err(|error| {
                    DumpError::Manifest(format!(
                        "{}: edit {edits} at offset {offset} will not decode: {error}",
                        manifest_path.display()
                    ))
                })?;
                print_edit(out, edits, offset, &edit, &names)?;
                if let Some(name) = &edit.comparator {
                    comparator_name = Some(name.clone());
                }
                for (id, name) in &edit.cf_added {
                    names.insert(*id, name.clone());
                }
                builder.apply(&edit)?;
                edits += 1;
            }
            other => break other,
        }
    };
    if edits == 0 {
        line!(out, "  (none)");
    }
    line!(out, "");

    let comparator = Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)));
    if let Some(name) = &comparator_name
        && name != BytewiseComparator.name()
    {
        return Err(DumpError::Manifest(format!(
            "the manifest was written with comparator {name:?}; this build only has {:?}, so \
             the version it describes cannot be reconstructed in the right order",
            BytewiseComparator.name()
        )));
    }

    let version = builder.build(&comparator)?;
    print_version(out, &version, &names)?;
    line!(out, "");

    match &ending {
        ReadOutcome::Eof => {
            line!(
                out,
                "summary: {} edits, {} live, ends cleanly at {} bytes",
                edits,
                plural(version.file_count(), "file"),
                reader.position()
            );
            Ok(())
        }
        ReadOutcome::Torn(why) => {
            line!(
                out,
                "summary: {} edits, {} live, then a torn record at {}: {why}",
                edits,
                plural(version.file_count(), "file"),
                reader.position()
            );
            line!(
                out,
                "notice:  a torn record at the tail is a crash between an append and its \
                 sync; recovery tolerates it and so does this."
            );
            Ok(())
        }
        ReadOutcome::Corrupt(why) => {
            line!(
                out,
                "summary: {edits} edits, then corruption at {}: {why}",
                reader.position()
            );
            Err(DumpError::Manifest(format!(
                "{} at offset {}: {why}",
                manifest_path.display(),
                reader.position()
            )))
        }
        ReadOutcome::Record(_) => unreachable!("the loop breaks only on a non-record outcome"),
    }
}

#[cfg(test)]
mod tests {
    use super::{DumpOptions, run};
    use esker_engine::batch::WriteBatch;
    use esker_engine::fs::{FileSystem, LocalFileSystem};
    use esker_engine::options::{Options, WriteOptions};
    use esker_engine::{Db, cf, filename};
    use std::path::Path;
    use std::sync::Arc;

    /// A real database, produced by a small scripted workload rather than by hand: writes
    /// across two column families, a flush that adds an L0 file, then more writes.
    fn a_database() -> tempfile::TempDir {
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

        for i in 0..40u32 {
            db.put(
                cf::DEFAULT,
                format!("key-{i:04}").as_bytes(),
                format!("value-{i}").as_bytes(),
            )
            .unwrap();
        }
        let mut batch = WriteBatch::new();
        batch.put(db.cf_id(cf::DEFAULT).unwrap(), b"cross", b"family");
        batch.put(db.cf_id(cf::LOCK).unwrap(), b"lock-key", b"held");
        db.write(batch, &WriteOptions::synced()).unwrap();

        // The flush is what puts a file in the manifest; without it there is only metadata.
        db.flush(cf::DEFAULT).unwrap();
        db.put(cf::DEFAULT, b"after", b"the flush").unwrap();
        drop(db);
        dir
    }

    fn dump(path: &Path) -> (String, bool) {
        let mut out = Vec::new();
        let ok = run(
            &DumpOptions {
                path: path.to_path_buf(),
            },
            &mut out,
        )
        .is_ok();
        (String::from_utf8_lossy(&out).into_owned(), ok)
    }

    #[test]
    fn dumps_a_real_database() {
        let dir = a_database();
        let (report, ok) = dump(dir.path());
        assert!(ok, "{report}");

        // It starts from CURRENT, and says which manifest that named.
        assert!(report.contains("current:    MANIFEST-"), "{report}");
        assert!(report.contains("MANIFEST-000001"), "{report}");

        // Every edit, decoded, including the one that created the families.
        assert!(
            report.contains("comparator        esker.BytewiseComparator"),
            "{report}"
        );
        assert!(
            report.contains("+ column family   default (cf 0)"),
            "{report}"
        );
        assert!(report.contains("+ column family   lock (cf 1)"), "{report}");
        assert!(report.contains("last_seqno"), "{report}");

        // The flush's file, in the edit and again in the reconstructed version.
        assert!(report.contains(".sst    L0 of default (cf 0)"), "{report}");
        assert!(report.contains("version"), "{report}");
        assert!(report.contains("L0: 1 file,"), "{report}");
        assert!(report.contains("1 file live"), "{report}");

        // File bounds are internal keys, and are shown as what they are.
        assert!(report.contains("smallest  "), "{report}");
        assert!(
            report.contains(" Put"),
            "the tag was not decoded:\n{report}"
        );

        // A family with nothing in it is still listed, so "where did my data go" has an answer.
        assert!(report.contains("raft (cf 3): 0 files"), "{report}");
        assert!(report.contains("ends cleanly"), "{report}");
    }

    /// A directory that is not a database says so, rather than guessing at a manifest name.
    #[test]
    fn a_directory_without_current_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ok) = dump(dir.path());
        assert!(!ok, "an empty directory dumped cleanly");

        std::fs::write(dir.path().join("CURRENT"), b"not a manifest name\n").unwrap();
        let mut out = Vec::new();
        let error = run(
            &DumpOptions {
                path: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("does not name a manifest"),
            "{error}"
        );
    }

    /// `CURRENT` pointing at a manifest that is not there is a broken database, not a panic.
    #[test]
    fn a_current_naming_a_missing_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CURRENT"),
            filename::current_contents(4242).as_bytes(),
        )
        .unwrap();
        let (_, ok) = dump(dir.path());
        assert!(!ok, "a missing manifest dumped cleanly");
    }

    /// A manifest with a flipped bit exits non-zero, having printed what it could read first.
    #[test]
    fn a_damaged_manifest_is_an_error() {
        let dir = a_database();
        let manifest = LocalFileSystem::new()
            .list(dir.path())
            .unwrap()
            .into_iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("MANIFEST-"))
            })
            .expect("the database has a manifest");

        let good = std::fs::read(&manifest).unwrap();
        // Well past the first record's header, inside a payload.
        let mut damaged = good.clone();
        damaged[40] ^= 0xff;
        std::fs::write(&manifest, &damaged).unwrap();

        let (report, ok) = dump(dir.path());
        assert!(!ok, "a damaged manifest dumped cleanly:\n{report}");
        // It still said where it started before it gave up.
        assert!(report.contains("directory:"), "{report}");

        // And a truncated one: a torn tail is a crash, not damage, so it is tolerated.
        std::fs::write(&manifest, &good[..good.len() - 3]).unwrap();
        let (report, ok) = dump(dir.path());
        assert!(
            ok,
            "a torn manifest tail was reported as an error:\n{report}"
        );
        assert!(report.contains("torn record"), "{report}");
    }
}
