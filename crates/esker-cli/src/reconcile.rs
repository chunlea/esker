//! `esker sst-store reconcile` — which objects in a prefix nothing references any more.
//!
//! A `DeleteObject` that fails **leaks the object deliberately**
//! ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md)): a leaked object costs
//! storage and a wrongly deleted one costs data, so the sweep gives up rather than insists. That
//! is the right call on the write path and it leaves somebody to clean up, which is this.
//!
//! # Why it is a tool and not a background task
//!
//! List-and-compare needs both halves at rest. The bucket's listing and the database's manifest
//! are two different things read at two different instants, and a database that is *running* moves
//! the manifest between them — so an object that looks unreferenced may be one a compaction
//! uploaded a moment ago and is about to name. On the write path that is a race with a data-loss
//! ending. Offline it is not a race at all, which is the whole reason this is a command an
//! operator runs rather than a thread that runs itself.
//!
//! # The three gates in front of a delete
//!
//! 1. **The prefix must be this database's.** The claim marker names its owner
//!    ([ADR 0029](../../../docs/adr/0029-the-sst-store-claim.md)), and this refuses a prefix whose
//!    marker names somebody else — which is exactly the mistake that would otherwise delete a live
//!    database's SSTs from under it.
//! 2. **Dry run by default.** `--delete` is the only thing that removes anything, and what it
//!    removes is what the dry run printed.
//! 3. **Nothing newer than the manifest is ever deleted**, `--delete` or not. An object whose file
//!    number is at or above the manifest's next number is one the manifest has not caught up with,
//!    and "the manifest is behind" is not a reason to delete data. It is reported and kept.
//!
//! Anything that is not an SST and not the marker is reported as unknown and never deleted. This
//! tool knows two kinds of object; a third is somebody else's business.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_engine::dbformat::{BytewiseComparator, InternalKeyComparator};
use esker_engine::filename::{self, FileKind};
use esker_engine::fs::claim::{CLAIM_OBJECT, Identity, id_for_directory};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::options::defaults;
use esker_engine::version::builder::Builder;
use esker_engine::version::{Version, VersionEdit};
use esker_engine::wal::{LogReader, ReadOutcome};
use esker_s3::ObjectStore;

use crate::bytes::plural;

/// What to reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconcileOptions {
    /// The database directory, the one holding `CURRENT`.
    pub(crate) data_dir: PathBuf,
    /// `s3://bucket/prefix`, the same URL the database was opened with.
    pub(crate) store_url: String,
    /// Delete what is unreferenced. Without it nothing is removed.
    pub(crate) delete: bool,
}

impl Default for ReconcileOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("."),
            store_url: String::new(),
            delete: false,
        }
    }
}

/// What one object in the prefix is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An SST the manifest names. Never touched.
    Referenced,
    /// An SST no live version names, and below the manifest's next file number.
    Unreferenced,
    /// An SST at or above the manifest's next number: the manifest is behind the bucket, so this
    /// is not something to delete on the strength of it.
    Newer,
    /// The claim marker.
    Marker,
    /// Not an SST and not the marker.
    Unknown,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Referenced => "referenced",
            Self::Unreferenced => "unreferenced",
            Self::Newer => "newer than the manifest",
            Self::Marker => "claim marker",
            Self::Unknown => "not ours",
        }
    }
}

/// Why a reconcile could not finish.
#[derive(Debug)]
pub(crate) enum ReconcileError {
    /// A file could not be read.
    Io {
        /// What was being read.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The manifest could not be understood.
    Manifest(String),
    /// The object store failed, or said no.
    Store(String),
    /// The prefix belongs to another database, or cannot be shown to belong to this one.
    Claim(String),
    /// The store URL will not parse.
    Config(String),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Manifest(detail) | Self::Store(detail) | Self::Config(detail) => {
                write!(f, "{detail}")
            }
            Self::Claim(detail) => write!(f, "{detail}"),
        }
    }
}

/// Lists what the prefix holds, says what nothing references, and deletes it if asked.
pub(crate) fn run(options: &ReconcileOptions, out: &mut dyn Write) -> Result<(), ReconcileError> {
    let (store, prefix) =
        crate::sst_store::object_store(&options.store_url).map_err(ReconcileError::Config)?;
    reconcile(options, store.as_ref(), &prefix, out)
}

/// The whole of it, against any [`ObjectStore`] — which is what lets a test drive it with
/// `MemoryStore` and no container.
pub(crate) fn reconcile(
    options: &ReconcileOptions,
    store: &dyn ObjectStore,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<(), ReconcileError> {
    let dir = options.data_dir.as_path();
    // **The claim first, before the manifest is even read.** It is the gate that says this prefix
    // is this database's, and a run against somebody else's should say so rather than complain
    // about whatever it found in `--data-dir` on the way there.
    check_claim(store, prefix, dir)?;
    let (live, next_file_number) = live_files(dir)?;

    let listed = store
        .list(prefix)
        .map_err(|error| ReconcileError::Store(format!("listing {prefix}: {error}")))?;

    writeln!(out, "directory:  {}", dir.display()).map_err(stdout_error)?;
    writeln!(out, "prefix:     {prefix}").map_err(stdout_error)?;
    writeln!(
        out,
        "manifest:   {} live, next file number {next_file_number}",
        plural(live.len(), "SST")
    )
    .map_err(stdout_error)?;
    writeln!(out, "objects:    {}", plural(listed.len(), "object")).map_err(stdout_error)?;
    writeln!(out).map_err(stdout_error)?;

    let mut unreferenced = Vec::new();
    let mut leaked_bytes = 0u64;
    let mut newer = 0usize;
    let mut unknown = 0usize;
    for object in &listed {
        let kind = classify(&object.key, prefix, &live, next_file_number);
        match kind {
            Kind::Unreferenced => {
                unreferenced.push(object.key.clone());
                leaked_bytes += object.size;
            }
            Kind::Newer => newer += 1,
            Kind::Unknown => unknown += 1,
            Kind::Referenced | Kind::Marker => {}
        }
        // Every object is printed, not only the leaked ones: an operator about to delete
        // something wants to see what was *not* selected as much as what was.
        writeln!(
            out,
            "  {:<24} {:>10}  {}",
            kind.label(),
            object.size,
            object.key
        )
        .map_err(stdout_error)?;
    }
    if listed.is_empty() {
        writeln!(out, "  (nothing)").map_err(stdout_error)?;
    }
    writeln!(out).map_err(stdout_error)?;

    if newer > 0 {
        writeln!(
            out,
            "note: {newer} object(s) are at or above the manifest's next file number. The \
             manifest is behind the bucket, which is not a reason to delete anything; they are \
             left alone."
        )
        .map_err(stdout_error)?;
    }
    if unknown > 0 {
        writeln!(
            out,
            "note: {unknown} object(s) are neither an SST nor the claim marker, and this tool \
             does not delete what it cannot name."
        )
        .map_err(stdout_error)?;
    }

    if unreferenced.is_empty() {
        writeln!(out, "summary: nothing is unreferenced").map_err(stdout_error)?;
        return Ok(());
    }

    if !options.delete {
        writeln!(
            out,
            "summary: {} unreferenced, {leaked_bytes} bytes. Re-run with --delete to remove them.",
            plural(unreferenced.len(), "object")
        )
        .map_err(stdout_error)?;
        return Ok(());
    }

    let mut deleted = 0usize;
    for key in &unreferenced {
        match store.delete(key) {
            Ok(()) => {
                deleted += 1;
                writeln!(out, "deleted {key}").map_err(stdout_error)?;
            }
            // One failure does not stop the rest: the object was already a leak, and leaving the
            // others behind because of it would be the same mistake the write path makes.
            Err(error) => {
                writeln!(out, "could not delete {key}: {error}").map_err(stdout_error)?;
            }
        }
    }
    writeln!(
        out,
        "summary: deleted {deleted} of {}, {leaked_bytes} bytes were unreferenced",
        unreferenced.len()
    )
    .map_err(stdout_error)?;
    Ok(())
}

/// What one object key is, relative to the live set.
fn classify(key: &str, prefix: &str, live: &BTreeSet<u64>, next: u64) -> Kind {
    let Some(rest) = key.strip_prefix(prefix) else {
        return Kind::Unknown;
    };
    if rest == CLAIM_OBJECT {
        return Kind::Marker;
    }
    match filename::classify(rest) {
        Some(FileKind::Sst(number)) if live.contains(&number) => Kind::Referenced,
        Some(FileKind::Sst(number)) if number >= next => Kind::Newer,
        Some(FileKind::Sst(_)) => Kind::Unreferenced,
        _ => Kind::Unknown,
    }
}

/// The prefix must carry this database's own claim marker.
///
/// The gate that stops a mistyped prefix from deleting a *live* database's SSTs. A prefix with no
/// marker is refused too: it might be pre-6c and it might be somebody's, and there is no way to
/// tell from outside — which is the same reasoning `--adopt-sst-store` exists for, and this tool
/// deliberately has no such hatch, because adopting in order to delete is not a thing to make easy.
fn check_claim(store: &dyn ObjectStore, prefix: &str, dir: &Path) -> Result<(), ReconcileError> {
    let local = LocalFileSystem::new();
    let id = id_for_directory(&local, dir).map_err(|source| ReconcileError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let ours = Identity::new(id);
    let key = format!("{prefix}{CLAIM_OBJECT}");
    let marker = store.get(&key).map_err(|error| {
        if error.is_not_found() {
            ReconcileError::Claim(format!(
                "the prefix {prefix:?} carries no claim marker, so there is no way to tell that \
                 it is this database's. Reconciling is only safe against a prefix this database \
                 owns."
            ))
        } else {
            ReconcileError::Store(format!("reading {key}: {error}"))
        }
    })?;
    let theirs = Identity::decode(&marker.body).map_err(|error| {
        ReconcileError::Claim(format!("the claim marker of {prefix:?}: {error}"))
    })?;
    if theirs.claim != ours.claim {
        return Err(ReconcileError::Claim(format!(
            "the prefix {prefix:?} is claimed by {theirs}, and the database in {} is {ours}. \
             Reconciling somebody else's prefix would delete a live database's SSTs.",
            dir.display()
        )));
    }
    Ok(())
}

/// The SST numbers a database's manifest names, and the next number it would hand out.
fn live_files(dir: &Path) -> Result<(BTreeSet<u64>, u64), ReconcileError> {
    let fs = LocalFileSystem::new();
    let current = filename::current(dir);
    let text = read_to_string(&fs, &current)?;
    let number = text
        .trim()
        .strip_prefix("MANIFEST-")
        .and_then(|rest| rest.parse::<u64>().ok())
        .ok_or_else(|| {
            ReconcileError::Manifest(format!(
                "{}: {:?} does not name a manifest",
                current.display(),
                text.trim()
            ))
        })?;
    let path = filename::manifest(dir, number);

    let file = fs.open(&path).map_err(|source| ReconcileError::Io {
        path: path.clone(),
        source,
    })?;
    let mut reader = LogReader::new(file, path.display().to_string());
    let mut builder = Builder::new(Version::empty(), defaults::NUM_LEVELS);
    let mut next_file_number = 0u64;
    loop {
        match reader
            .read_record()
            .map_err(|error| ReconcileError::Manifest(format!("{}: {error}", path.display())))?
        {
            ReadOutcome::Record(payload) => {
                let edit = VersionEdit::decode(&payload).map_err(|error| {
                    ReconcileError::Manifest(format!("{}: {error}", path.display()))
                })?;
                if let Some(next) = edit.next_file_number {
                    next_file_number = next_file_number.max(next);
                }
                for (_, _, meta) in &edit.added_files {
                    next_file_number = next_file_number.max(meta.number + 1);
                }
                builder
                    .apply(&edit)
                    .map_err(|error| ReconcileError::Manifest(error.to_string()))?;
            }
            // A torn tail is a crash between an append and its sync, which the engine tolerates
            // for the same reason the WAL reader does. What it means here is that the last edit is
            // not in the live set — so the reconcile sees a *smaller* live set than the database
            // will, which is the direction that deletes something. Refused rather than guessed at.
            ReadOutcome::Torn(why) => {
                return Err(ReconcileError::Manifest(format!(
                    "{}: the manifest ends in a torn record ({why}). Open the database once so it \
                     recovers, then reconcile: a truncated manifest names fewer files than the \
                     database does, and this tool would call the difference garbage.",
                    path.display()
                )));
            }
            // Bytes that cannot be a log at all. Nothing is deletable on the strength of a
            // manifest this build cannot read.
            ReadOutcome::Corrupt(why) => {
                return Err(ReconcileError::Manifest(format!(
                    "{}: the manifest is corrupt ({why})",
                    path.display()
                )));
            }
            ReadOutcome::Eof => break,
        }
    }

    let comparator = Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)));
    let version = builder
        .build(&comparator)
        .map_err(|error| ReconcileError::Manifest(error.to_string()))?;

    let mut live = BTreeSet::new();
    for cf in version.column_families() {
        let Some(family) = version.cf(cf) else {
            continue;
        };
        for level in 0..family.num_levels() {
            for file in family.files(level) {
                live.insert(file.number);
                next_file_number = next_file_number.max(file.number + 1);
            }
        }
    }
    Ok((live, next_file_number))
}

fn read_to_string(fs: &impl FileSystem, path: &Path) -> Result<String, ReconcileError> {
    let file = fs.open(path).map_err(|source| ReconcileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file.size().map_err(|source| ReconcileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(0)];
    esker_engine::fs::read_exact_at(file.as_ref(), 0, &mut bytes).map_err(|source| {
        ReconcileError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    String::from_utf8(bytes)
        .map_err(|error| ReconcileError::Manifest(format!("{}: {error}", path.display())))
}

fn stdout_error(source: io::Error) -> ReconcileError {
    ReconcileError::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use esker_engine::fs::claim::{CLAIM_OBJECT, Identity, id_for_directory};
    use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
    use esker_engine::fs::{FileSystem, LocalFileSystem};
    use esker_engine::{Db, Options};
    use esker_s3::{MemoryStore, ObjectStore};

    use super::{ReconcileError, ReconcileOptions, reconcile};

    const PREFIX: &str = "reconcile/";

    /// A database on a tier over `store`, with two SSTs uploaded and then closed.
    fn database(store: &Arc<MemoryStore>, dir: &std::path::Path) {
        let local = Arc::new(LocalFileSystem::new());
        let id = id_for_directory(local.as_ref(), dir).unwrap();
        let tier = TieredFileSystem::new(
            Arc::clone(&local) as Arc<dyn FileSystem>,
            Arc::clone(store) as Arc<dyn ObjectStore>,
            dir,
            TierOptions {
                key_prefix: PREFIX.to_string(),
                background: false,
                identity: Some(Identity::new(id).of(1, 1)),
                ..TierOptions::default()
            },
        )
        .unwrap();
        let db = Db::open_with(
            dir,
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            tier as Arc<dyn FileSystem>,
            &["default"],
        )
        .unwrap();
        for batch in 0..2u32 {
            for index in 0..64u32 {
                db.put(
                    "default",
                    format!("b{batch}-{index:04}").as_bytes(),
                    b"value",
                )
                .unwrap();
            }
            db.flush("default").unwrap();
            db.tier_maintenance().unwrap();
        }
        drop(db);
    }

    fn options(dir: &std::path::Path, delete: bool) -> ReconcileOptions {
        ReconcileOptions {
            data_dir: dir.to_path_buf(),
            store_url: String::new(),
            delete,
        }
    }

    fn ssts(store: &MemoryStore) -> Vec<String> {
        store
            .keys()
            .into_iter()
            .filter(|key| {
                std::path::Path::new(key)
                    .extension()
                    .is_some_and(|ext| ext == "sst")
            })
            .collect()
    }

    /// **The unit.** A planted orphan is found, named, and removed only when asked.
    #[test]
    fn a_planted_orphan_is_reported_and_then_deleted() {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        database(&store, dir.path());
        let live = ssts(&store);
        assert!(!live.is_empty(), "the database uploaded nothing");

        // The leak: an object from a compaction whose DeleteObject failed. Number 3, which is
        // below the first number a database hands out, so it is unambiguously stale.
        let orphan = format!("{PREFIX}000003.sst");
        store.put(&orphan, b"a leaked sst").unwrap();

        let mut out = Vec::new();
        reconcile(
            &options(dir.path(), false),
            store.as_ref(),
            PREFIX,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("unreferenced") && text.contains("000003.sst"),
            "the orphan was not reported:\n{text}"
        );
        assert!(
            text.contains("Re-run with --delete"),
            "a dry run did not say how to act on it:\n{text}"
        );
        for key in &live {
            assert!(store.contains(key), "a dry run deleted {key}");
        }
        assert!(store.contains(&orphan), "a dry run deleted the orphan");

        let mut out = Vec::new();
        reconcile(&options(dir.path(), true), store.as_ref(), PREFIX, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("deleted 1 of 1"), "{text}");
        assert!(!store.contains(&orphan), "the orphan survived --delete");
        for key in &live {
            assert!(store.contains(key), "--delete took a live SST: {key}");
        }
        assert!(
            store.contains(&format!("{PREFIX}{CLAIM_OBJECT}")),
            "--delete took the claim marker",
        );
    }

    /// An object the manifest has not caught up with is never deleted, `--delete` or not.
    ///
    /// The manifest being behind the bucket is exactly what an interrupted upload looks like from
    /// outside, and "the manifest does not name it" is then a statement about the manifest.
    #[test]
    fn an_object_newer_than_the_manifest_is_kept() {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        database(&store, dir.path());

        let newer = format!("{PREFIX}009999.sst");
        store.put(&newer, b"uploaded, not yet named").unwrap();

        let mut out = Vec::new();
        reconcile(&options(dir.path(), true), store.as_ref(), PREFIX, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("newer than the manifest"),
            "it was not classified as newer:\n{text}"
        );
        assert!(
            store.contains(&newer),
            "--delete took an object the manifest is behind"
        );
    }

    /// A prefix another database claimed is refused before anything is listed.
    #[test]
    fn another_databases_prefix_is_refused_and_the_error_names_both() {
        let store = Arc::new(MemoryStore::new());
        let theirs = tempfile::tempdir().unwrap();
        database(&store, theirs.path());

        let ours = tempfile::tempdir().unwrap();
        let local = LocalFileSystem::new();
        let our_id = Identity::new(id_for_directory(&local, ours.path()).unwrap());

        let mut out = Vec::new();
        let error = reconcile(
            &options(ours.path(), true),
            store.as_ref(),
            PREFIX,
            &mut out,
        )
        .expect_err("another database's prefix was reconciled");
        let ReconcileError::Claim(detail) = &error else {
            panic!("expected a claim refusal, got {error:?}");
        };
        assert!(detail.contains(&our_id.claim.to_string()), "{detail}");
        assert!(
            ssts(&store).iter().all(|key| store.contains(key)),
            "a refused reconcile deleted something",
        );
    }

    /// A prefix with no marker at all is refused too: it might be somebody's.
    #[test]
    fn an_unclaimed_prefix_is_refused() {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        database(&store, dir.path());
        // The marker goes, which is what a 6b-era prefix looks like.
        store.delete(&format!("{PREFIX}{CLAIM_OBJECT}")).unwrap();

        let mut out = Vec::new();
        let error = reconcile(&options(dir.path(), true), store.as_ref(), PREFIX, &mut out)
            .expect_err("an unclaimed prefix was reconciled");
        assert!(
            matches!(&error, ReconcileError::Claim(detail) if detail.contains("no claim marker")),
            "{error:?}"
        );
    }

    /// Nothing to do is said plainly rather than as an empty report.
    #[test]
    fn a_clean_prefix_says_so() {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        database(&store, dir.path());

        let mut out = Vec::new();
        reconcile(&options(dir.path(), true), store.as_ref(), PREFIX, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nothing is unreferenced"), "{text}");
    }
}
