//! Checkpoints: a consistent, self-contained copy of the database, made almost for free.
//!
//! SSTs are immutable and named by a number that is never reused, so a copy of a database is a
//! directory of **hard links** to the files a version names, plus a manifest describing them.
//! No bytes move. That is what makes `esker-store` able to ship a region to another node by
//! checkpointing it (`docs/DESIGN.md` §6) rather than by reading it out key by key.
//!
//! # It flushes first
//!
//! A memtable is not a file and cannot be linked, so a checkpoint would either have to copy
//! the log as well or leave the newest writes behind. It flushes instead: simpler, and it
//! means the copy needs no recovery at all — its manifest names every file and there is no log
//! to replay. The cost is a flush on the source, which the caller should know about, so it is
//! said here and in the method's own documentation.
//!
//! # The manifest is written, not copied
//!
//! The source's manifest is a *history* — every edit since the database was created. The
//! checkpoint gets one record instead: a single edit that reconstructs exactly the version
//! that was pinned. Copying the history would carry over references to files the checkpoint
//! does not contain.

use std::path::Path;

use crate::error::{Error, IoResultExt, Result};
use crate::filename;
use crate::fs::FileSystem;
use crate::version::VersionEdit;
use crate::wal::LogWriter;

use super::{Db, lock, read_lock};

/// Restricts a checkpoint to one column family, optionally to part of its key space.
///
/// Phase 4 uses this to ship one region: the region is a key range, and the files that overlap
/// it are the ones the receiving store needs.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointRange<'a> {
    /// The only column family the checkpoint will contain.
    pub cf: &'a str,
    /// Inclusive lower bound on the user key, or `None` for unbounded.
    pub begin: Option<&'a [u8]>,
    /// Inclusive upper bound, or `None` for unbounded.
    pub end: Option<&'a [u8]>,
}

impl Db {
    /// Writes a consistent copy of the database into `dir`.
    ///
    /// `only` restricts it to one column family and optionally part of its key space; `None`
    /// copies everything. **Flushes first**, so the copy contains every write acknowledged
    /// before the call and needs no log replay to open.
    ///
    /// Files are hard-linked where the filesystem allows it and copied where it does not, so a
    /// checkpoint of a large database is normally instant and always self-contained. Nothing
    /// in the source is modified beyond the flush.
    ///
    /// Fails if `dir` already contains a database: replacing one is the caller's decision.
    pub fn checkpoint(
        &self,
        dir: impl AsRef<Path>,
        only: Option<CheckpointRange<'_>>,
    ) -> Result<()> {
        let dir = dir.as_ref();
        let inner = &self.inner;

        // A memtable cannot be linked, so its contents have to reach a file first.
        match only {
            Some(range) => self.flush(range.cf)?,
            None => self.flush_all()?,
        }

        inner.fs.create_dir_all(dir).at(dir)?;
        let current = filename::current(dir);
        if inner.fs.exists(&current).at(&current)? {
            return Err(Error::InvalidArgument(format!(
                "{} already contains a database",
                dir.display()
            )));
        }

        // Pinned for the whole call: every file named below stays on disk until this is
        // dropped, however many compactions run meanwhile.
        let version = lock(&inner.versions)?.current();
        let families: Vec<(u32, String)> = read_lock(&inner.cfs)?
            .values()
            .map(|cf| (cf.id(), cf.name().to_string()))
            .collect();
        let wanted = match only {
            Some(range) => Some(inner.cf_by_name(range.cf)?.id()),
            None => None,
        };
        let user = inner.comparator.user_comparator().as_ref();

        let mut edit = VersionEdit::new();
        edit.comparator = Some(user.name().to_string());
        let mut highest = 0u64;

        for (id, name) in families {
            if wanted.is_some_and(|only| only != id) {
                continue;
            }
            edit.cf_added.push((id, name));
            let Some(cf_version) = version.cf(id) else {
                continue;
            };
            for level in 0..cf_version.num_levels() {
                let files = match only {
                    Some(range) if range.begin.is_some() || range.end.is_some() => {
                        cf_version.overlapping(level, range.begin, range.end, user)
                    }
                    _ => cf_version.files(level).to_vec(),
                };
                for file in files {
                    link_or_copy(inner.fs.as_ref(), &inner.dir, dir, file.number)?;
                    highest = highest.max(file.number);
                    edit.add_file(
                        id,
                        u32::try_from(level).unwrap_or(u32::MAX),
                        (*file).clone(),
                    );
                }
            }
        }

        // One past the highest linked file, and the same value for the log number: the
        // checkpoint has no log segments, so recovery finds nothing to replay.
        let next_file_number = highest + 2;
        edit.next_file_number = Some(next_file_number);
        edit.log_number = Some(next_file_number);
        edit.last_seqno = Some(self.last_seqno());

        write_manifest(inner.fs.as_ref(), dir, &edit)?;
        tracing::info!(
            source = %inner.dir.display(),
            checkpoint = %dir.display(),
            files = edit.added_files.len(),
            "checkpoint written"
        );
        Ok(())
    }
}

/// Links `number` from `source` into `target`, copying if the filesystem will not link it.
fn link_or_copy(fs: &dyn FileSystem, source: &Path, target: &Path, number: u64) -> Result<()> {
    let from = filename::sst(source, number);
    let to = filename::sst(target, number);
    if fs.hard_link(&from, &to).is_ok() {
        return Ok(());
    }
    // A different filesystem, or one without links. The bytes are immutable either way.
    let reader = fs.open(&from).at(&from)?;
    let size = reader.size().at(&from)?;
    let mut writer = fs.create(&to).at(&to)?;
    let mut offset = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    while offset < size {
        let read = reader.read_at(offset, &mut buffer).at(&from)?;
        if read == 0 {
            break;
        }
        writer.append(&buffer[..read]).at(&to)?;
        offset += read as u64;
    }
    writer.sync_data().at(&to)?;
    Ok(())
}

/// Writes `MANIFEST-000001` and the `CURRENT` that names it.
fn write_manifest(fs: &dyn FileSystem, dir: &Path, edit: &VersionEdit) -> Result<()> {
    let number = 1;
    let path = filename::manifest(dir, number);
    let mut manifest = LogWriter::new(fs.create(&path).at(&path)?, path.display().to_string());
    manifest.add_record(&edit.encode());
    manifest.sync()?;

    let temp = filename::temp(dir, number);
    {
        let mut file = fs.create(&temp).at(&temp)?;
        file.append(filename::current_contents(number).as_bytes())
            .at(&temp)?;
        file.sync_data().at(&temp)?;
    }
    fs.rename(&temp, &filename::current(dir))
        .map_err(|err| Error::io(&temp, err))?;
    fs.fsync_dir(dir).at(dir)
}
