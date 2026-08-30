//! Versions: the set of files that make up the database at one instant.
//!
//! A [`Version`] says which SSTs live at which level of which column family. It is immutable
//! and shared behind an `Arc`, which is the whole mechanism: a reader pins one and every file
//! it names is guaranteed to still exist, while a flush or compaction builds a new version
//! beside it and installs that. Files are deleted only once no version references them
//! (`docs/DESIGN.md` §4.6).
//!
//! * [`edit`] — `VersionEdit`, the delta the manifest is a log of
//! * [`builder`] — applies a run of edits to a version, producing the next one
//! * [`set`] — the manifest, the file numbers, and which versions are still pinned
//!
//! # Why the levels are sorted differently
//!
//! Files below L0 cover disjoint key ranges, so a level is a sorted array and a lookup is a
//! binary search. **L0 is the exception**: its files come straight from memtable flushes and
//! overlap freely, so a read has to consult all of them, and it must do so newest first or an
//! older value would shadow a newer one. L0 is therefore kept in newest-first order and every
//! other level in key order.

pub mod builder;
pub mod edit;
pub mod set;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::dbformat::{Comparator, extract_user_key};

pub use builder::Builder;
pub use edit::{FileMeta, VersionEdit};
pub use set::VersionSet;

/// The files of one column family, level by level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CfVersion {
    /// `levels[0]` is L0, newest first; every deeper level is sorted by smallest key and its
    /// files do not overlap.
    levels: Vec<Vec<Arc<FileMeta>>>,
}

impl CfVersion {
    /// An empty column family with `num_levels` levels.
    pub fn empty(num_levels: usize) -> Self {
        Self {
            levels: vec![Vec::new(); num_levels],
        }
    }

    /// How many levels this column family has.
    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }

    /// The files at `level`, empty if the level does not exist.
    pub fn files(&self, level: usize) -> &[Arc<FileMeta>] {
        self.levels.get(level).map_or(&[], Vec::as_slice)
    }

    /// Total bytes stored at `level`.
    pub fn level_bytes(&self, level: usize) -> u64 {
        self.files(level).iter().map(|file| file.size).sum()
    }

    /// Every file in the column family, in level order.
    pub fn all_files(&self) -> impl Iterator<Item = &Arc<FileMeta>> {
        self.levels.iter().flatten()
    }

    /// Files at `level` whose user-key range intersects `[begin, end]`.
    ///
    /// `None` for either bound means unbounded on that side. Below L0 the files are disjoint
    /// and sorted, so this could binary-search; it scans instead, because a level holds tens
    /// of files and the picker runs once per compaction rather than once per read.
    pub fn overlapping(
        &self,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
        user: &dyn Comparator,
    ) -> Vec<Arc<FileMeta>> {
        self.files(level)
            .iter()
            .filter(|file| {
                let smallest = extract_user_key(&file.smallest);
                let largest = extract_user_key(&file.largest);
                let after_begin =
                    begin.is_none_or(|begin| user.cmp(largest, begin) != Ordering::Less);
                let before_end = end.is_none_or(|end| user.cmp(smallest, end) != Ordering::Greater);
                after_begin && before_end
            })
            .cloned()
            .collect()
    }

    /// The smallest and largest user key across `files`, or `None` if there are none.
    pub fn range_of(files: &[Arc<FileMeta>], user: &dyn Comparator) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut bounds: Option<(Vec<u8>, Vec<u8>)> = None;
        for file in files {
            let smallest = extract_user_key(&file.smallest).to_vec();
            let largest = extract_user_key(&file.largest).to_vec();
            bounds = Some(match bounds {
                None => (smallest, largest),
                Some((low, high)) => (
                    if user.cmp(&smallest, &low) == Ordering::Less {
                        smallest
                    } else {
                        low
                    },
                    if user.cmp(&largest, &high) == Ordering::Greater {
                        largest
                    } else {
                        high
                    },
                ),
            });
        }
        bounds
    }
}

/// Which files make up the database, at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Version {
    cfs: BTreeMap<u32, CfVersion>,
}

impl Version {
    /// A version with no column families and no files.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The files of `cf`, or `None` if there is no such column family.
    pub fn cf(&self, cf: u32) -> Option<&CfVersion> {
        self.cfs.get(&cf)
    }

    /// The ids of every live column family, ascending.
    pub fn column_families(&self) -> impl Iterator<Item = u32> {
        self.cfs.keys().copied()
    }

    /// The files at `(cf, level)`, empty if either does not exist.
    pub fn files(&self, cf: u32, level: usize) -> &[Arc<FileMeta>] {
        self.cf(cf).map_or(&[], |cf| cf.files(level))
    }

    /// Every file number any column family references.
    ///
    /// This is what makes deletion safe: a file may be removed from disk only when it appears
    /// in no live version's live set.
    pub fn live_files(&self) -> BTreeSet<u64> {
        self.cfs
            .values()
            .flat_map(CfVersion::all_files)
            .map(|file| file.number)
            .collect()
    }

    /// How many files the version references in total.
    pub fn file_count(&self) -> usize {
        self.cfs.values().flat_map(CfVersion::all_files).count()
    }

    pub(crate) fn insert_cf(&mut self, cf: u32, version: CfVersion) {
        self.cfs.insert(cf, version);
    }

    pub(crate) fn remove_cf(&mut self, cf: u32) -> Option<CfVersion> {
        self.cfs.remove(&cf)
    }

    pub(crate) fn set_level(&mut self, cf: u32, level: usize, files: Vec<Arc<FileMeta>>) {
        if let Some(entry) = self.cfs.get_mut(&cf)
            && let Some(slot) = entry.levels.get_mut(level)
        {
            *slot = files;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CfVersion, FileMeta, Version};
    use std::sync::Arc;

    fn file(number: u64) -> Arc<FileMeta> {
        Arc::new(FileMeta {
            number,
            size: 1024 * number,
            smallest: vec![0u8; 9],
            largest: vec![9u8; 9],
            smallest_seqno: number,
            largest_seqno: number,
        })
    }

    #[test]
    fn an_empty_version_references_nothing() {
        let version = Version::empty();
        assert!(version.live_files().is_empty());
        assert_eq!(version.file_count(), 0);
        assert!(
            version.files(0, 0).is_empty(),
            "a missing cf is not an error"
        );
        assert!(version.cf(3).is_none());
    }

    #[test]
    fn live_files_spans_every_column_family_and_level() {
        let mut version = Version::empty();
        version.insert_cf(0, CfVersion::empty(7));
        version.insert_cf(1, CfVersion::empty(7));
        version.set_level(0, 0, vec![file(1), file(2)]);
        version.set_level(0, 3, vec![file(3)]);
        version.set_level(1, 0, vec![file(4)]);

        assert_eq!(version.live_files(), [1, 2, 3, 4].into_iter().collect());
        assert_eq!(version.file_count(), 4);
        assert_eq!(version.column_families().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(version.cf(0).unwrap().level_bytes(0), 1024 * 3);
    }

    /// Dropping a column family takes its files out of the live set, which is what lets the
    /// deleter reclaim them.
    #[test]
    fn dropping_a_column_family_releases_its_files() {
        let mut version = Version::empty();
        version.insert_cf(0, CfVersion::empty(7));
        version.set_level(0, 0, vec![file(1)]);
        assert_eq!(version.live_files().len(), 1);
        version.remove_cf(0);
        assert!(version.live_files().is_empty());
    }

    #[test]
    fn a_level_out_of_range_is_empty_rather_than_a_panic() {
        let cf = CfVersion::empty(2);
        assert!(cf.files(99).is_empty());
        assert_eq!(cf.level_bytes(99), 0);
        assert_eq!(cf.num_levels(), 2);
    }
}
