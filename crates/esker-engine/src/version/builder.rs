//! Applying edits to a version.
//!
//! Recovery replays a whole manifest through one builder; a flush or a compaction pushes a
//! single edit through another. Both go through the same code, so a version reconstructed at
//! startup is the same object the running system would have had — which is the only way to
//! know that recovery is correct rather than merely plausible.
//!
//! The builder is strict about one thing: an edit that deletes a file the version does not
//! have is corruption. `LevelDB` shrugs at this; we do not, because in a database with column
//! families it is also how a mis-attributed file quietly disappears from one and reappears in
//! another.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::dbformat::{Comparator, InternalKeyComparator};
use crate::error::{Error, Result};

use super::{CfVersion, FileLocation, FileMeta, Version, VersionEdit};

/// Accumulates edits and produces the version they describe.
#[derive(Debug)]
pub struct Builder {
    base: Version,
    num_levels: usize,
    deleted: BTreeMap<(u32, usize), BTreeSet<u64>>,
    added: BTreeMap<(u32, usize), Vec<Arc<FileMeta>>>,
    /// Locations an edit moved, applied in `build` once the level's file list is assembled.
    relocated: BTreeMap<(u32, usize), BTreeMap<u64, FileLocation>>,
    cf_added: BTreeSet<u32>,
    cf_dropped: BTreeSet<u32>,
}

impl Builder {
    /// Starts from `base`, which is [`Version::empty`] during recovery.
    pub fn new(base: Version, num_levels: usize) -> Self {
        Self {
            base,
            num_levels,
            deleted: BTreeMap::new(),
            added: BTreeMap::new(),
            relocated: BTreeMap::new(),
            cf_added: BTreeSet::new(),
            cf_dropped: BTreeSet::new(),
        }
    }

    /// Folds one edit in. Edits must be applied in the order the manifest holds them.
    pub fn apply(&mut self, edit: &VersionEdit) -> Result<()> {
        for (cf, _) in &edit.cf_added {
            self.cf_dropped.remove(cf);
            self.cf_added.insert(*cf);
        }
        for cf in &edit.cf_dropped {
            self.cf_added.remove(cf);
            self.cf_dropped.insert(*cf);
            // A dropped column family takes its pending files with it, so a later re-create
            // does not inherit them.
            self.added.retain(|(id, _), _| id != cf);
            self.deleted.retain(|(id, _), _| id != cf);
        }
        for (cf, level, number) in &edit.deleted_files {
            let level = self.level(*level)?;
            self.deleted
                .entry((*cf, level))
                .or_default()
                .insert(*number);
        }
        for (cf, level, meta) in &edit.added_files {
            let level = self.level(*level)?;
            // An add cancels a pending delete of the same number at the same level: a
            // compaction that rewrites a file in place would otherwise erase it.
            if let Some(deleted) = self.deleted.get_mut(&(*cf, level)) {
                deleted.remove(&meta.number);
            }
            self.added
                .entry((*cf, level))
                .or_default()
                .push(Arc::new(meta.clone()));
        }
        for (cf, level, number, location) in &edit.file_locations {
            let level = self.level(*level)?;
            self.relocated
                .entry((*cf, level))
                .or_default()
                .insert(*number, *location);
        }
        Ok(())
    }

    /// Produces the version the accumulated edits describe.
    ///
    /// Fails if an edit deleted a file that was never there, or if the result would put
    /// overlapping files in a level below L0 — both of which mean the manifest and the version
    /// disagree, and reads would start missing keys.
    pub fn build(mut self, comparator: &InternalKeyComparator) -> Result<Version> {
        let mut version = std::mem::take(&mut self.base);

        for cf in &self.cf_added {
            if version.cf(*cf).is_none() {
                version.insert_cf(*cf, CfVersion::empty(self.num_levels));
            }
        }
        for cf in &self.cf_dropped {
            version.remove_cf(*cf);
        }

        let touched: BTreeSet<(u32, usize)> = self
            .deleted
            .keys()
            .chain(self.added.keys())
            .chain(self.relocated.keys())
            .copied()
            .collect();
        for (cf, level) in touched {
            if version.cf(cf).is_none() {
                if self.cf_dropped.contains(&cf) {
                    continue; // The column family is gone; its files go with it.
                }
                return Err(Error::corruption(
                    "manifest",
                    format!("files at level {level} for column family {cf}, which does not exist"),
                ));
            }
            let deleted = self.deleted.remove(&(cf, level)).unwrap_or_default();
            let added = self.added.remove(&(cf, level)).unwrap_or_default();

            let mut files: Vec<Arc<FileMeta>> = version
                .files(cf, level)
                .iter()
                .filter(|file| !deleted.contains(&file.number))
                .cloned()
                .collect();
            let kept = files.len();
            let carried_over = version.files(cf, level).len() - kept;
            files.extend(
                added
                    .iter()
                    .filter(|file| !deleted.contains(&file.number))
                    .cloned(),
            );

            let accounted =
                carried_over + added.iter().filter(|f| deleted.contains(&f.number)).count();
            if accounted < deleted.len() {
                return Err(Error::corruption(
                    "manifest",
                    format!(
                        "column family {cf} level {level}: an edit deleted {} files that were not there",
                        deleted.len() - accounted
                    ),
                ));
            }

            // A location change is not a file change: it rewrites one field of a meta the
            // level already holds. A promotion naming a file that is not here is *ignored*
            // rather than rejected — the upload that wrote it raced a compaction that removed
            // the file, and refusing to open a database over a stale hint would be absurd
            // (ADR 0024 decision 4: the location is a record, never a decision).
            if let Some(moved) = self.relocated.remove(&(cf, level)) {
                for file in &mut files {
                    if let Some(location) = moved.get(&file.number)
                        && file.location != *location
                    {
                        let mut updated = FileMeta::clone(file);
                        updated.location = *location;
                        *file = Arc::new(updated);
                    }
                }
            }

            sort_level(&mut files, level, comparator);
            if level > 0 {
                check_disjoint(&files, cf, level, comparator)?;
            }
            version.set_level(cf, level, files);
        }
        Ok(version)
    }

    fn level(&self, level: u32) -> Result<usize> {
        let level = level as usize;
        if level >= self.num_levels {
            return Err(Error::corruption(
                "manifest",
                format!("level {level} but the database has {}", self.num_levels),
            ));
        }
        Ok(level)
    }
}

/// L0 newest first, everything below it in key order. See the module docs of [`super`].
fn sort_level(files: &mut [Arc<FileMeta>], level: usize, comparator: &InternalKeyComparator) {
    if level == 0 {
        // Newest first. The sequence number rather than the file number, because an ingested
        // file gets a fresh number while carrying older data (`Db::ingest`, step 8).
        files.sort_by(|a, b| {
            b.largest_seqno
                .cmp(&a.largest_seqno)
                .then_with(|| b.number.cmp(&a.number))
        });
    } else {
        files.sort_by(|a, b| {
            comparator
                .cmp(&a.smallest, &b.smallest)
                .then_with(|| a.number.cmp(&b.number))
        });
    }
}

/// Below L0 the files partition the key space; two that overlap would make a binary search
/// return the wrong file and a read miss a key that is on disk.
fn check_disjoint(
    files: &[Arc<FileMeta>],
    cf: u32,
    level: usize,
    comparator: &InternalKeyComparator,
) -> Result<()> {
    for pair in files.windows(2) {
        if comparator.cmp(&pair[0].largest, &pair[1].smallest) != Ordering::Less {
            return Err(Error::corruption(
                "manifest",
                format!(
                    "column family {cf} level {level}: files {} and {} overlap",
                    pair[0].number, pair[1].number
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Builder;
    use crate::dbformat::{BytewiseComparator, EntryKind, InternalKeyComparator, internal_key};
    use crate::version::FileLocation;
    use crate::version::{FileMeta, Version, VersionEdit};
    use std::sync::Arc;

    const LEVELS: usize = 7;

    fn comparator() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    fn meta(number: u64, smallest: &[u8], largest: &[u8]) -> FileMeta {
        FileMeta {
            number,
            size: 1024,
            smallest: internal_key(smallest, number, EntryKind::Put),
            largest: internal_key(largest, number, EntryKind::Put),
            smallest_seqno: number,
            largest_seqno: number,
            location: FileLocation::Local,
        }
    }

    fn with_cf(cfs: &[u32]) -> Version {
        let mut edit = VersionEdit::new();
        for cf in cfs {
            edit.cf_added.push((*cf, format!("cf{cf}")));
        }
        let mut builder = Builder::new(Version::empty(), LEVELS);
        builder.apply(&edit).unwrap();
        builder.build(&comparator()).unwrap()
    }

    fn apply(base: Version, edit: &VersionEdit) -> crate::error::Result<Version> {
        let mut builder = Builder::new(base, LEVELS);
        builder.apply(edit)?;
        builder.build(&comparator())
    }

    #[test]
    fn creating_and_dropping_column_families() {
        let version = with_cf(&[0, 1]);
        assert_eq!(version.column_families().collect::<Vec<_>>(), vec![0, 1]);

        let mut edit = VersionEdit::new();
        edit.cf_dropped.push(1);
        let version = apply(version, &edit).unwrap();
        assert_eq!(version.column_families().collect::<Vec<_>>(), vec![0]);
    }

    /// Dropping a column family releases its files even though no edit deleted them one by one.
    #[test]
    fn dropping_a_column_family_releases_its_files() {
        let mut edit = VersionEdit::new();
        edit.add_file(1, 0, meta(5, b"a", b"z"));
        let version = apply(with_cf(&[0, 1]), &edit).unwrap();
        assert_eq!(version.live_files(), [5].into_iter().collect());

        let mut edit = VersionEdit::new();
        edit.cf_dropped.push(1);
        let version = apply(version, &edit).unwrap();
        assert!(version.live_files().is_empty());
    }

    #[test]
    fn l0_is_newest_first_and_deeper_levels_are_in_key_order() {
        let mut edit = VersionEdit::new();
        edit.add_file(0, 0, meta(1, b"m", b"z"));
        edit.add_file(0, 0, meta(3, b"a", b"c"));
        edit.add_file(0, 0, meta(2, b"d", b"f"));
        edit.add_file(0, 2, meta(7, b"m", b"n"));
        edit.add_file(0, 2, meta(6, b"a", b"c"));
        let version = apply(with_cf(&[0]), &edit).unwrap();

        let l0: Vec<u64> = version.files(0, 0).iter().map(|f| f.number).collect();
        assert_eq!(l0, vec![3, 2, 1], "L0 is consulted newest first");
        let l2: Vec<u64> = version.files(0, 2).iter().map(|f| f.number).collect();
        assert_eq!(l2, vec![6, 7], "deeper levels are sorted by key");
    }

    /// A flush adds to L0; a compaction deletes from L0 and adds to L1. Replaying both is the
    /// same as having done them one at a time.
    #[test]
    fn a_compaction_moves_files_between_levels() {
        let mut flush = VersionEdit::new();
        flush.add_file(0, 0, meta(1, b"a", b"m"));
        flush.add_file(0, 0, meta(2, b"n", b"z"));
        let version = apply(with_cf(&[0]), &flush).unwrap();

        let mut compact = VersionEdit::new();
        compact.delete_file(0, 0, 1);
        compact.delete_file(0, 0, 2);
        compact.add_file(0, 1, meta(3, b"a", b"z"));
        let version = apply(version, &compact).unwrap();

        assert!(version.files(0, 0).is_empty());
        assert_eq!(version.files(0, 1).len(), 1);
        assert_eq!(version.live_files(), [3].into_iter().collect());
    }

    /// Both edits go through one builder, as manifest replay does. The intermediate state is
    /// never materialised, and the answer must still be the same.
    #[test]
    fn replaying_a_run_of_edits_equals_applying_them_one_at_a_time() {
        let mut flush = VersionEdit::new();
        flush.add_file(0, 0, meta(1, b"a", b"m"));
        let mut compact = VersionEdit::new();
        compact.delete_file(0, 0, 1);
        compact.add_file(0, 1, meta(2, b"a", b"m"));

        let one_at_a_time = apply(apply(with_cf(&[0]), &flush).unwrap(), &compact).unwrap();

        let mut builder = Builder::new(with_cf(&[0]), LEVELS);
        builder.apply(&flush).unwrap();
        builder.apply(&compact).unwrap();
        let replayed = builder.build(&comparator()).unwrap();

        assert_eq!(replayed, one_at_a_time);
    }

    #[test]
    fn deleting_a_file_that_was_never_there_is_corruption() {
        let mut edit = VersionEdit::new();
        edit.delete_file(0, 0, 42);
        let err = apply(with_cf(&[0]), &edit).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("were not there"), "{err}");
    }

    /// Overlapping files below L0 would make a binary search return the wrong file, and a read
    /// would miss a key that is sitting on disk.
    #[test]
    fn overlapping_files_below_l0_are_corruption() {
        let mut edit = VersionEdit::new();
        edit.add_file(0, 1, meta(1, b"a", b"m"));
        edit.add_file(0, 1, meta(2, b"f", b"z"));
        let err = apply(with_cf(&[0]), &edit).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("overlap"), "{err}");

        // The same files at L0 are fine: L0 overlaps by design.
        let mut edit = VersionEdit::new();
        edit.add_file(0, 0, meta(1, b"a", b"m"));
        edit.add_file(0, 0, meta(2, b"f", b"z"));
        assert!(apply(with_cf(&[0]), &edit).is_ok());
    }

    #[test]
    fn files_for_a_column_family_that_does_not_exist_are_corruption() {
        let mut edit = VersionEdit::new();
        edit.add_file(9, 0, meta(1, b"a", b"z"));
        let err = apply(with_cf(&[0]), &edit).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_level_beyond_the_configured_depth_is_corruption() {
        let mut edit = VersionEdit::new();
        edit.add_file(0, 99, meta(1, b"a", b"z"));
        let err = apply(with_cf(&[0]), &edit).unwrap_err();
        assert!(err.is_corruption(), "{err}");
    }
}
