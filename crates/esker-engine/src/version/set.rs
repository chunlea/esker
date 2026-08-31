//! The manifest and the versions it produces.
//!
//! `VersionSet` owns three things that have to stay consistent with each other: the file
//! numbers, the sequence numbers, and which manifest is live. It is the only writer of
//! `CURRENT`, and `CURRENT` is the only mutable pointer in the whole engine (invariant 3).
//!
//! # Installing a new version
//!
//! [`VersionSet::log_and_apply`] does four things in an order that cannot be rearranged:
//!
//! 1. build the new version, so an edit that cannot be applied is never logged;
//! 2. append the edit to the manifest and **sync** it;
//! 3. if the manifest is a new one, replace `CURRENT` by temp → sync → rename → fsync the
//!    directory;
//! 4. only then install the version in memory.
//!
//! A crash between any two of them leaves the database readable. Before (2) the edit never
//! happened, and the files it would have added are unreferenced and get deleted at startup.
//! Between (2) and (3) the old manifest is still the live one, so the new one is an orphan and
//! is deleted too. After (3) the new manifest is live and its records are all durable. There
//! is no window in which `CURRENT` names something incomplete, because a rename either happens
//! or does not.
//!
//! # Reclaiming files
//!
//! A version is immutable and shared, so a reader that holds one keeps every file it names
//! alive. [`VersionSet::obsolete_files`] therefore asks the *set of live versions*, not the
//! current one, and a version stops being live when the last `Arc` to it outside this set is
//! dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::dbformat::{InternalKeyComparator, SeqNo};
use crate::error::{Error, IoResultExt, Result};
use crate::filename::{self, FileKind};
use crate::fs::FileSystem;
use crate::options::defaults;
use crate::wal::{LogReader, LogWriter, ReadOutcome};

use super::{Builder, Version, VersionEdit};

/// The manifest, the file numbers, and the live versions.
pub struct VersionSet {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    comparator: Arc<InternalKeyComparator>,
    num_levels: usize,

    manifest: Option<LogWriter>,
    manifest_number: u64,
    next_file_number: u64,
    last_seqno: SeqNo,
    log_number: u64,

    max_manifest_bytes: u64,
    /// Set when an update failed after it may have reached the disk. See [`Error::Poisoned`].
    poisoned: Option<String>,

    cf_names: BTreeMap<u32, String>,
    current: Arc<Version>,
    /// Every version installed and not yet released. Pruned by [`VersionSet::obsolete_files`].
    live: Vec<Arc<Version>>,
}

impl std::fmt::Debug for VersionSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionSet")
            .field("dir", &self.dir)
            .field("manifest_number", &self.manifest_number)
            .field("next_file_number", &self.next_file_number)
            .field("last_seqno", &self.last_seqno)
            .field("log_number", &self.log_number)
            .field("column_families", &self.cf_names)
            .field("live_versions", &self.live.len())
            .finish_non_exhaustive()
    }
}

impl VersionSet {
    /// Creates a database directory with `cfs` as its column families.
    ///
    /// Fails if one already exists: replacing a database is a decision for the caller, never a
    /// side effect of opening one.
    pub fn create(
        fs: Arc<dyn FileSystem>,
        dir: &Path,
        comparator: Arc<InternalKeyComparator>,
        num_levels: usize,
        cfs: &[&str],
    ) -> Result<Self> {
        fs.create_dir_all(dir).at(dir)?;
        let current = filename::current(dir);
        if fs.exists(&current).at(&current)? {
            return Err(Error::InvalidArgument(format!(
                "{} already contains a database",
                dir.display()
            )));
        }

        let mut set = Self {
            fs,
            dir: dir.to_path_buf(),
            comparator,
            num_levels,
            manifest: None,
            manifest_number: 0,
            next_file_number: 2, // 1 is the first manifest
            last_seqno: 0,
            log_number: 0,
            max_manifest_bytes: defaults::MANIFEST_MAX_BYTES,
            poisoned: None,
            cf_names: BTreeMap::new(),
            current: Arc::new(Version::empty()),
            live: Vec::new(),
        };

        let mut edit = VersionEdit::new();
        edit.comparator = Some(set.comparator.user_comparator().name().to_string());
        for (id, name) in cfs.iter().enumerate() {
            let id = u32::try_from(id)
                .map_err(|_| Error::InvalidArgument("too many column families".to_string()))?;
            edit.cf_added.push((id, (*name).to_string()));
        }
        set.log_and_apply(&mut edit)?;
        Ok(set)
    }

    /// Reopens the database in `dir`, replaying its manifest.
    ///
    /// A torn record at the end of the manifest is expected: it is an edit that was being
    /// written when the process died, and because it was never acknowledged the work it
    /// described never happened. Corruption anywhere else is an error.
    pub fn recover(
        fs: Arc<dyn FileSystem>,
        dir: &Path,
        comparator: Arc<InternalKeyComparator>,
        num_levels: usize,
    ) -> Result<Self> {
        let current_path = filename::current(dir);
        if !fs.exists(&current_path).at(&current_path)? {
            return Err(Error::NotFound(format!(
                "{} is not a database: no CURRENT file",
                dir.display()
            )));
        }

        let contents = read_to_string(fs.as_ref(), &current_path)?;
        let manifest_number = filename::parse_current(&contents).ok_or_else(|| {
            Error::corruption("CURRENT", format!("{contents:?} does not name a manifest"))
        })?;

        let manifest_path = filename::manifest(dir, manifest_number);
        let file = fs.open(&manifest_path).at(&manifest_path)?;
        let mut reader = LogReader::new(file, manifest_path.display().to_string());
        let (records, end) = reader.read_all()?;
        match end {
            ReadOutcome::Eof | ReadOutcome::Torn(_) => {}
            ReadOutcome::Corrupt(why) => return Err(Error::corruption("manifest", why)),
            // `read_all` stops at a non-record outcome, so this cannot happen; it is an error
            // rather than a panic because the input came off a disk (invariant 9).
            ReadOutcome::Record(_) => {
                return Err(Error::corruption("manifest", "reader did not terminate"));
            }
        }

        let mut set = Self {
            fs,
            dir: dir.to_path_buf(),
            comparator,
            num_levels,
            manifest: None,
            manifest_number,
            next_file_number: manifest_number + 1,
            last_seqno: 0,
            log_number: 0,
            max_manifest_bytes: defaults::MANIFEST_MAX_BYTES,
            poisoned: None,
            cf_names: BTreeMap::new(),
            current: Arc::new(Version::empty()),
            live: Vec::new(),
        };

        let mut builder = Builder::new(Version::empty(), num_levels);
        let mut saw_comparator = false;
        for record in &records {
            let edit = VersionEdit::decode(record)?;
            if let Some(name) = &edit.comparator {
                let expected = set.comparator.user_comparator().name();
                if name != expected {
                    return Err(Error::InvalidArgument(format!(
                        "database was written with comparator {name:?}, opened with {expected:?}"
                    )));
                }
                saw_comparator = true;
            }
            if let Some(number) = edit.log_number {
                set.log_number = number;
            }
            if let Some(number) = edit.next_file_number {
                set.next_file_number = set.next_file_number.max(number);
            }
            if let Some(seqno) = edit.last_seqno {
                set.last_seqno = set.last_seqno.max(seqno);
            }
            for (id, name) in &edit.cf_added {
                set.cf_names.insert(*id, name.clone());
            }
            for id in &edit.cf_dropped {
                set.cf_names.remove(id);
            }
            builder.apply(&edit)?;
        }
        if !saw_comparator {
            return Err(Error::corruption(
                "manifest",
                "no comparator name; this is not an esker manifest".to_string(),
            ));
        }

        let version = builder.build(&set.comparator)?;
        // A file number below anything already on disk would be handed out twice.
        for file in version.live_files() {
            set.next_file_number = set.next_file_number.max(file + 1);
        }
        set.install(version);
        Ok(set)
    }

    /// Rolls the manifest once it passes this many bytes. Defaults to
    /// [`MANIFEST_MAX_BYTES`](crate::options::defaults::MANIFEST_MAX_BYTES);
    /// tests lower it to exercise the `CURRENT` swap without writing 64 MiB of edits.
    pub fn set_max_manifest_bytes(&mut self, bytes: u64) {
        self.max_manifest_bytes = bytes;
    }

    /// The current version. Cloning the returned `Arc` pins every file it names.
    pub fn current(&self) -> Arc<Version> {
        Arc::clone(&self.current)
    }

    /// The number of the live manifest.
    pub fn manifest_number(&self) -> u64 {
        self.manifest_number
    }

    /// The oldest write-ahead-log segment that still needs replaying.
    pub fn log_number(&self) -> u64 {
        self.log_number
    }

    /// Records that segments below `number` have been flushed.
    pub fn set_log_number(&mut self, number: u64) {
        self.log_number = number;
    }

    /// The highest sequence number that has been made durable.
    pub fn last_seqno(&self) -> SeqNo {
        self.last_seqno
    }

    /// Advances the durable sequence number. Never moves it backwards.
    pub fn set_last_seqno(&mut self, seqno: SeqNo) {
        self.last_seqno = self.last_seqno.max(seqno);
    }

    /// Hands out a file number, which is never reused.
    pub fn new_file_number(&mut self) -> u64 {
        let number = self.next_file_number;
        self.next_file_number += 1;
        number
    }

    /// The next number [`new_file_number`](Self::new_file_number) would return.
    pub fn next_file_number(&self) -> u64 {
        self.next_file_number
    }

    /// The id of the column family called `name`.
    pub fn cf_id(&self, name: &str) -> Option<u32> {
        self.cf_names
            .iter()
            .find(|(_, existing)| existing.as_str() == name)
            .map(|(id, _)| *id)
    }

    /// The name of column family `id`.
    pub fn cf_name(&self, id: u32) -> Option<&str> {
        self.cf_names.get(&id).map(String::as_str)
    }

    /// Every live column family, by id.
    pub fn column_families(&self) -> &BTreeMap<u32, String> {
        &self.cf_names
    }

    /// Creates a column family, returning its id.
    pub fn create_cf(&mut self, name: &str) -> Result<u32> {
        if self.cf_id(name).is_some() {
            return Err(Error::InvalidArgument(format!(
                "column family {name:?} already exists"
            )));
        }
        let id = self.cf_names.keys().last().map_or(0, |last| last + 1);
        let mut edit = VersionEdit::new();
        edit.cf_added.push((id, name.to_string()));
        self.log_and_apply(&mut edit)?;
        Ok(id)
    }

    /// Drops a column family. Its files become obsolete at the next sweep.
    pub fn drop_cf(&mut self, name: &str) -> Result<()> {
        let id = self
            .cf_id(name)
            .ok_or_else(|| Error::InvalidArgument(format!("no column family {name:?}")))?;
        let mut edit = VersionEdit::new();
        edit.cf_dropped.push(id);
        self.log_and_apply(&mut edit)
    }

    /// Logs `edit` durably and installs the version it produces. See the module docs for why
    /// the order of operations here is not negotiable.
    pub fn log_and_apply(&mut self, edit: &mut VersionEdit) -> Result<()> {
        if let Some(why) = &self.poisoned {
            return Err(Error::Poisoned(why.clone()));
        }
        edit.next_file_number = Some(self.next_file_number);
        edit.last_seqno = Some(self.last_seqno);
        if edit.log_number.is_none() {
            edit.log_number = Some(self.log_number);
        }

        // 1. Build first: an edit that cannot be applied must never reach the manifest.
        let mut builder = Builder::new((*self.current).clone(), self.num_levels);
        builder.apply(edit)?;
        let version = builder.build(&self.comparator)?;

        // 2. A fresh manifest starts with a snapshot of the version this edit applies to, so
        //    it is self-contained and the old one can be deleted.
        let rolling = match &self.manifest {
            None => true,
            Some(writer) => writer.len() >= self.max_manifest_bytes,
        };
        let new_manifest_number = if rolling {
            let number = if self.manifest_number == 0 {
                1
            } else {
                self.new_file_number()
            };
            let path = filename::manifest(&self.dir, number);
            let file = self.fs.create(&path).at(&path)?;
            let mut writer = LogWriter::new(file, path.display().to_string());
            writer.add_record(&self.snapshot_edit().encode());
            self.manifest = Some(writer);
            edit.next_file_number = Some(self.next_file_number);
            Some(number)
        } else {
            None
        };

        // 3. Append and sync. Until this returns, the edit did not happen.
        {
            let Some(writer) = self.manifest.as_mut() else {
                return Err(Error::ShuttingDown);
            };
            writer.add_record(&edit.encode());
            if let Err(err) = writer.sync() {
                // The record may or may not have reached the disk, so memory and disk may
                // already disagree. Stop rather than guess.
                self.manifest = None;
                self.poison(format!("the manifest could not be synced: {err}"));
                return Err(err);
            }
        }

        // 4. Point CURRENT at the new manifest, atomically.
        if let Some(number) = new_manifest_number {
            if let Err(err) = self.write_current(number) {
                // The rename either happened or did not, and after an error we cannot tell
                // which. Recovery will see one manifest or the other and be correct either
                // way; this process must not carry on with a version the disk may not share.
                self.manifest = None;
                self.poison(format!("CURRENT could not be replaced: {err}"));
                return Err(err);
            }
            self.manifest_number = number;
        }

        // 5. Only now is the new version the truth.
        for (id, name) in &edit.cf_added {
            self.cf_names.insert(*id, name.clone());
        }
        for id in &edit.cf_dropped {
            self.cf_names.remove(id);
        }
        self.install(version);
        Ok(())
    }

    /// Paths in the directory that no live version needs.
    ///
    /// Obsolete are: SSTs no live version references, log segments below the log number, every
    /// manifest but the live one, and any leftover temporary file.
    pub fn obsolete_files(&mut self) -> Result<Vec<PathBuf>> {
        let live_files = self.live_file_numbers();

        let mut obsolete = Vec::new();
        for path in self.fs.list(&self.dir).at(&self.dir)? {
            let keep = match filename::classify_path(&path) {
                Some(FileKind::Sst(number)) => live_files.contains(&number),
                Some(FileKind::Wal(number)) => number >= self.log_number,
                Some(FileKind::Manifest(number)) => number == self.manifest_number,
                Some(FileKind::Temp(_)) => false,
                // `CURRENT` is the pointer itself, and anything the engine did not create is
                // not the engine's to delete.
                Some(FileKind::Current) | None => true,
            };
            if !keep {
                obsolete.push(path);
            }
        }
        Ok(obsolete)
    }

    /// Every file number any pinned version still needs.
    ///
    /// The same set [`obsolete_files`](Self::obsolete_files) computes, exposed because object
    /// deletion cannot be driven from a directory listing: a file the tier has evicted is
    /// absent from the listing, so its object would never be reclaimed
    /// ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 5).
    pub fn live_file_numbers(&mut self) -> BTreeSet<u64> {
        self.live.retain(|version| Arc::strong_count(version) > 1);
        let mut live_files: BTreeSet<u64> = self.current.live_files();
        for version in &self.live {
            live_files.extend(version.live_files());
        }
        live_files
    }

    /// Deletes what [`obsolete_files`](Self::obsolete_files) found, returning what went.
    ///
    /// A file that vanished between the listing and the delete is not an error: another sweep
    /// may have taken it, and the goal is that it be gone.
    pub fn purge_obsolete_files(&mut self) -> Result<Vec<PathBuf>> {
        let obsolete = self.obsolete_files()?;
        let mut deleted = Vec::new();
        for path in obsolete {
            match self.fs.delete(&path) {
                Ok(()) => deleted.push(path),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io(&path, err)),
            }
        }
        Ok(deleted)
    }

    /// How many versions are still pinned, the current one included. Tests read it; so does
    /// the `pinned_versions` property.
    pub fn live_version_count(&mut self) -> usize {
        self.live.retain(|version| Arc::strong_count(version) > 1);
        self.live.len()
    }

    fn poison(&mut self, why: String) {
        tracing::error!(reason = %why, "version set poisoned; the database must be reopened");
        self.poisoned.get_or_insert(why);
    }

    fn install(&mut self, version: Version) {
        let version = Arc::new(version);
        self.live.push(Arc::clone(&version));
        self.current = version;
    }

    /// An edit that reconstructs the current version from nothing: what every new manifest
    /// starts with.
    fn snapshot_edit(&self) -> VersionEdit {
        let mut edit = VersionEdit::new();
        edit.comparator = Some(self.comparator.user_comparator().name().to_string());
        for (id, name) in &self.cf_names {
            edit.cf_added.push((*id, name.clone()));
        }
        for cf in self.current.column_families() {
            let Some(files) = self.current.cf(cf) else {
                continue;
            };
            for level in 0..files.num_levels() {
                for file in files.files(level) {
                    let level = u32::try_from(level).unwrap_or(u32::MAX);
                    edit.add_file(cf, level, (**file).clone());
                }
            }
        }
        edit
    }

    /// Replaces `CURRENT` atomically: temp → sync → rename → fsync the directory.
    ///
    /// The rename is what makes it atomic; the directory fsync is what makes the rename
    /// durable. Skipping the second is the classic way to lose a file that was definitely
    /// written.
    fn write_current(&self, manifest_number: u64) -> Result<()> {
        let temp = filename::temp(&self.dir, manifest_number);
        {
            let mut file = self.fs.create(&temp).at(&temp)?;
            file.append(filename::current_contents(manifest_number).as_bytes())
                .at(&temp)?;
            file.sync_data().at(&temp)?;
        }
        let current = filename::current(&self.dir);
        if let Err(err) = self.fs.rename(&temp, &current) {
            let _ = self.fs.delete(&temp);
            return Err(Error::io(&temp, err));
        }
        self.fs.fsync_dir(&self.dir).at(&self.dir)
    }
}

/// Reads a small file whole. Used for `CURRENT`, which is one short line.
fn read_to_string(fs: &dyn FileSystem, path: &Path) -> Result<String> {
    let file = fs.open(path).at(path)?;
    let size = usize::try_from(file.size().at(path)?).unwrap_or(usize::MAX);
    let mut bytes = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let read = file.read_at(done as u64, &mut bytes[done..]).at(path)?;
        if read == 0 {
            break;
        }
        done += read;
    }
    bytes.truncate(done);
    String::from_utf8(bytes)
        .map_err(|_| Error::corruption(path.display().to_string(), "not UTF-8".to_string()))
}
