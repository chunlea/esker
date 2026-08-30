//! Choosing what to compact.
//!
//! Leveled compaction, `LevelDB`'s shape: each level has a target size, the level furthest over
//! it is compacted into the one below, and the picker's whole job is to answer *which files*
//! (`docs/DESIGN.md` §4.7).
//!
//! # Two kinds of score
//!
//! L0 is scored by **file count** and every level below it by **bytes**, and the difference is
//! not arbitrary. L0's files overlap, so a read has to consult all of them: its cost is the
//! number of files, whatever they weigh. Below L0 the files are disjoint and a read touches
//! one per level, so the cost that matters is how much data sits at the level relative to what
//! belongs there.
//!
//! # Why the inputs grow, and where the growth stops
//!
//! Compacting `[L, L+1]` rewrites every L+1 file the L files overlap. Once those are being
//! rewritten anyway, any *other* file at L that fits inside their range is free to include —
//! it costs no extra reading at L+1. The expansion is taken only when it does not pull in
//! another L+1 file, because that would be a different, larger compaction than the one that
//! was scored.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::dbformat::{Comparator, InternalKeyComparator, extract_user_key};
use crate::options::CfOptions;
use crate::version::{CfVersion, FileMeta};

/// One compaction: which files, from which level, into which.
#[derive(Debug, Clone)]
pub struct Compaction {
    /// The column family being compacted.
    pub cf: u32,
    /// The level the inputs come from. Outputs go to `level + 1`.
    pub level: usize,
    /// Files from `level`.
    pub inputs: Vec<Arc<FileMeta>>,
    /// Files from `level + 1` that overlap them.
    pub outputs_overlapped: Vec<Arc<FileMeta>>,
    /// Bytes an output file may reach before the next key starts a new one.
    pub target_file_size: u64,
}

impl Compaction {
    /// The level outputs are written to.
    pub fn output_level(&self) -> usize {
        self.level + 1
    }

    /// Every file this compaction reads.
    pub fn all_inputs(&self) -> impl Iterator<Item = &Arc<FileMeta>> {
        self.inputs.iter().chain(self.outputs_overlapped.iter())
    }

    /// Whether the files can simply be re-labelled as belonging to the next level.
    ///
    /// One input, nothing to merge it with: rewriting it would produce the same bytes under a
    /// different number. The manifest edit alone does the whole job.
    pub fn is_trivial_move(&self) -> bool {
        self.inputs.len() == 1 && self.outputs_overlapped.is_empty()
    }

    /// Total bytes read.
    pub fn input_bytes(&self) -> u64 {
        self.all_inputs().map(|file| file.size).sum()
    }
}

/// Scores levels and chooses the files to compact.
#[derive(Debug)]
pub struct Picker {
    options: CfOptions,
    comparator: Arc<InternalKeyComparator>,
}

impl Picker {
    /// A picker for a column family with these options.
    pub fn new(options: CfOptions, comparator: Arc<InternalKeyComparator>) -> Self {
        Self {
            options,
            comparator,
        }
    }

    /// Target bytes for `level`. Level 0 has none: it is scored by file count.
    pub fn max_bytes_for_level(&self, level: usize) -> u64 {
        let mut bytes = self.options.max_bytes_for_level_base;
        for _ in 1..level {
            bytes = bytes.saturating_mul(self.options.max_bytes_for_level_multiplier);
        }
        bytes
    }

    /// How far past its target `level` is. At or above 1.0 it wants compacting.
    #[allow(clippy::cast_precision_loss)] // Sizes and counts are far below f64's exact range.
    pub fn score(&self, version: &CfVersion, level: usize) -> f64 {
        if level == 0 {
            // File count, not bytes: a read consults every L0 file, whatever they weigh.
            let trigger = self.options.level0_file_num_compaction_trigger.max(1);
            return version.files(0).len() as f64 / trigger as f64;
        }
        let target = self.max_bytes_for_level(level).max(1);
        version.level_bytes(level) as f64 / target as f64
    }

    /// The level most in need of compaction, if any is.
    pub fn worst_level(&self, version: &CfVersion) -> Option<(usize, f64)> {
        // The last level has nowhere to compact into, so it is never a source.
        (0..version.num_levels().saturating_sub(1))
            .map(|level| (level, self.score(version, level)))
            .filter(|(_, score)| *score >= 1.0)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
    }

    /// Picks a compaction, or `None` if no level is over its target.
    ///
    /// `pointers[level]` is the largest key the last compaction of that level produced;
    /// picking the first file past it spreads compactions across the key space instead of
    /// grinding the same prefix. Losing the pointers on restart costs nothing but that
    /// spreading, which is why they live in memory and not in the manifest.
    pub fn pick(
        &self,
        cf: u32,
        version: &CfVersion,
        pointers: &[Option<Vec<u8>>],
    ) -> Option<Compaction> {
        let (level, _) = self.worst_level(version)?;
        let files = version.files(level);
        if files.is_empty() {
            return None;
        }

        let user = self.comparator.user_comparator().as_ref();
        let start = pointers
            .get(level)
            .and_then(Option::as_ref)
            .and_then(|pointer| {
                files
                    .iter()
                    .find(|file| {
                        user.cmp(extract_user_key(&file.largest), pointer) == Ordering::Greater
                    })
                    .cloned()
            })
            .unwrap_or_else(|| Arc::clone(&files[0]));

        Some(self.assemble(cf, version, level, vec![start]))
    }

    /// Picks a compaction covering `[begin, end]` at `level`, for an explicit `compact_range`.
    pub fn pick_range(
        &self,
        cf: u32,
        version: &CfVersion,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Option<Compaction> {
        if level + 1 >= version.num_levels() {
            return None;
        }
        let user = self.comparator.user_comparator().as_ref();
        let inputs = version.overlapping(level, begin, end, user);
        if inputs.is_empty() {
            return None;
        }
        Some(self.assemble(cf, version, level, inputs))
    }

    /// Grows a seed selection into the compaction it implies.
    fn assemble(
        &self,
        cf: u32,
        version: &CfVersion,
        level: usize,
        seed: Vec<Arc<FileMeta>>,
    ) -> Compaction {
        let user = self.comparator.user_comparator().as_ref();
        let mut inputs = seed;

        // L0 files overlap each other, so a compaction of one is a compaction of everything
        // that shares its range — otherwise the output would sit below entries that are newer
        // than it.
        if level == 0 {
            inputs = l0_closure(version, inputs, user);
        }

        let mut overlapped = match CfVersion::range_of(&inputs, user) {
            Some((low, high)) => version.overlapping(level + 1, Some(&low), Some(&high), user),
            None => Vec::new(),
        };

        // Those L+1 files are being rewritten anyway, so any other file at L inside their
        // range comes along free. Only if it does not drag in another L+1 file: that would be
        // a bigger compaction than the one that was scored.
        if !overlapped.is_empty()
            && let Some((low, high)) = CfVersion::range_of(&overlapped, user)
        {
            let mut expanded = version.overlapping(level, Some(&low), Some(&high), user);
            if level == 0 {
                // Same rule as above: a set of L0 inputs that is not closed under overlap
                // inverts the pair it splits.
                expanded = l0_closure(version, expanded, user);
            }
            if expanded.len() > inputs.len()
                && let Some((low, high)) = CfVersion::range_of(&expanded, user)
            {
                let still = version.overlapping(level + 1, Some(&low), Some(&high), user);
                if still.len() == overlapped.len() {
                    inputs = expanded;
                    overlapped = still;
                }
            }
        }

        Compaction {
            cf,
            level,
            inputs,
            outputs_overlapped: overlapped,
            target_file_size: self.options.target_file_size,
        }
    }

    /// Whether no level below `level + 1` holds `user_key`.
    ///
    /// A tombstone may only be dropped where nothing older can be hiding beneath it. This is
    /// what makes that decision, and getting it wrong resurrects a deleted key.
    pub fn is_bottom_level_for_key(
        &self,
        version: &CfVersion,
        level: usize,
        user_key: &[u8],
    ) -> bool {
        let user = self.comparator.user_comparator().as_ref();
        for below in (level + 2)..version.num_levels() {
            if !version
                .overlapping(below, Some(user_key), Some(user_key), user)
                .is_empty()
            {
                return false;
            }
        }
        true
    }
}

/// Grows `files` until it holds every L0 file that overlaps its own key range.
///
/// One pass is not enough, and that is the whole point of this function. Each file pulled in
/// can widen the range, and a file that overlaps only the *widened* range has to come too:
/// leave it behind and it stays at L0 still holding an older version of a key whose newer
/// version has just moved to L1 — and the read path consults all of L0 before L1, so the older
/// version wins and an acknowledged write is lost. `LevelDB` says this by restarting its scan
/// whenever an added file widens the range; a fixed point is the same rule without the index
/// arithmetic.
///
/// Terminates because every round is a superset of the last and L0 holds finitely many files.
fn l0_closure(
    version: &CfVersion,
    mut files: Vec<Arc<FileMeta>>,
    user: &dyn Comparator,
) -> Vec<Arc<FileMeta>> {
    loop {
        let Some((low, high)) = CfVersion::range_of(&files, user) else {
            return files;
        };
        let grown = version.overlapping(0, Some(&low), Some(&high), user);
        // Every file of `files` lies inside `[low, high]`, so `grown` contains all of them and
        // equal lengths mean the set has stopped growing.
        if grown.len() == files.len() {
            return grown;
        }
        files = grown;
    }
}

#[cfg(test)]
mod tests {
    use super::Picker;
    use crate::dbformat::{BytewiseComparator, EntryKind, InternalKeyComparator, internal_key};
    use crate::options::CfOptions;
    use crate::version::{Builder, CfVersion, FileMeta, Version, VersionEdit};
    use std::sync::Arc;

    const LEVELS: usize = 7;

    fn comparator() -> Arc<InternalKeyComparator> {
        Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
    }

    fn options() -> CfOptions {
        CfOptions {
            level0_file_num_compaction_trigger: 4,
            max_bytes_for_level_base: 1000,
            max_bytes_for_level_multiplier: 10,
            ..CfOptions::default()
        }
    }

    fn picker() -> Picker {
        Picker::new(options(), comparator())
    }

    fn meta(number: u64, smallest: &str, largest: &str, size: u64) -> FileMeta {
        FileMeta {
            number,
            size,
            smallest: internal_key(smallest.as_bytes(), number, EntryKind::Put),
            largest: internal_key(largest.as_bytes(), number, EntryKind::Put),
            smallest_seqno: number,
            largest_seqno: number,
        }
    }

    /// Builds a version from `(level, number, smallest, largest, size)` tuples.
    fn version(files: &[(u32, u64, &str, &str, u64)]) -> Version {
        let mut create = VersionEdit::new();
        create.cf_added.push((0, "default".to_string()));
        let mut builder = Builder::new(Version::empty(), LEVELS);
        builder.apply(&create).unwrap();
        let base = builder.build(&comparator()).unwrap();

        let mut edit = VersionEdit::new();
        for (level, number, smallest, largest, size) in files {
            edit.add_file(0, *level, meta(*number, smallest, largest, *size));
        }
        let mut builder = Builder::new(base, LEVELS);
        builder.apply(&edit).unwrap();
        builder.build(&comparator()).unwrap()
    }

    fn cf(version: &Version) -> &CfVersion {
        version.cf(0).unwrap()
    }

    fn numbers(files: &[Arc<FileMeta>]) -> Vec<u64> {
        files.iter().map(|file| file.number).collect()
    }

    /// L0 is scored by file count and every level below by bytes, because a read consults
    /// every L0 file and one file per level below.
    #[test]
    fn l0_is_scored_by_count_and_the_rest_by_size() {
        let version = version(&[
            (0, 1, "a", "z", 1),
            (0, 2, "a", "z", 1),
            (1, 3, "a", "m", 400),
            (2, 4, "a", "m", 20_000),
        ]);
        let picker = picker();
        let cf = cf(&version);

        assert!(
            (picker.score(cf, 0) - 0.5).abs() < 1e-9,
            "two files of a four-file trigger"
        );
        assert!(
            (picker.score(cf, 1) - 0.4).abs() < 1e-9,
            "400 bytes of a 1000-byte target"
        );
        assert!(
            (picker.score(cf, 2) - 2.0).abs() < 1e-9,
            "20000 bytes of a 10000-byte target"
        );
        assert_eq!(picker.worst_level(cf).map(|(level, _)| level), Some(2));
    }

    #[test]
    fn nothing_is_picked_while_every_level_is_under_its_target() {
        let version = version(&[(0, 1, "a", "z", 1), (1, 2, "a", "m", 100)]);
        assert!(picker().pick(0, cf(&version), &[]).is_none());
    }

    /// The last level has nowhere to compact into, so however large it is it is never a source.
    #[test]
    fn the_bottom_level_is_never_picked() {
        let bottom = u32::try_from(LEVELS).unwrap() - 1;
        let mut files = vec![(bottom, 1u64, "a", "z", 100_000_000u64)];
        files.push((0, 2, "a", "b", 1));
        let version = version(&files);
        assert_eq!(picker().worst_level(cf(&version)), None);
    }

    /// L0 files overlap, so compacting one means compacting everything that shares its range —
    /// otherwise the output would sit below entries newer than itself.
    #[test]
    fn an_l0_compaction_takes_every_overlapping_l0_file() {
        let version = version(&[
            (0, 1, "c", "e", 1),
            (0, 2, "d", "f", 1),
            (0, 3, "x", "z", 1),
            (0, 4, "a", "b", 1),
        ]);
        let compaction = picker()
            .pick(0, cf(&version), &[])
            .expect("four files trigger L0");
        assert_eq!(compaction.level, 0);
        assert_eq!(compaction.output_level(), 1);
        // L0 is stored newest first, so the seed is file 4 and nothing else touches [a, b].
        assert_eq!(numbers(&compaction.inputs), vec![4]);
        assert!(compaction.outputs_overlapped.is_empty());
        assert!(compaction.is_trivial_move(), "one file, nothing under it");
    }

    #[test]
    fn overlapping_l0_files_are_compacted_together() {
        let version = version(&[
            (0, 4, "c", "e", 1),
            (0, 3, "d", "f", 1),
            (0, 2, "e", "g", 1),
            (0, 1, "x", "z", 1),
        ]);
        let compaction = picker().pick(0, cf(&version), &[]).unwrap();
        let mut picked = numbers(&compaction.inputs);
        picked.sort_unstable();
        assert_eq!(
            picked,
            vec![2, 3, 4],
            "the chain of overlapping files, not the disjoint one"
        );
    }

    /// **Regression.** The overlap has to be taken to a fixed point, not swept once.
    ///
    /// The seed `[k09, k09]` pulls in `[k08, k19]`, and *that* widens the range onto
    /// `[k08, k08]`, which overlaps nothing the seed touched. One sweep leaves it at L0 while
    /// the newer version of `k08` moves to L1 — and a point read consults every L0 file before
    /// it reaches L1, so the older value wins and an acknowledged write is lost. The model
    /// test found this shape; [`an_l0_compaction_takes_every_overlapping_l0_file`] and
    /// [`overlapping_l0_files_are_compacted_together`] both miss it because their chains are
    /// reachable in a single sweep.
    #[test]
    fn an_l0_file_reached_only_through_another_is_still_taken() {
        let version = version(&[
            (0, 10, "k09", "k09", 1),
            (0, 8, "k08", "k19", 1),
            (0, 6, "k08", "k08", 1),
            (0, 4, "k00", "k00", 1),
        ]);
        let compaction = picker()
            .pick(0, cf(&version), &[])
            .expect("four files trigger L0");
        let mut picked = numbers(&compaction.inputs);
        picked.sort_unstable();
        assert_eq!(
            picked,
            vec![6, 8, 10],
            "file 6 overlaps only the range file 8 widened the seed to"
        );
        assert!(
            !picked.contains(&4),
            "the closure stops at files that really do not overlap"
        );
    }

    #[test]
    fn a_level_compaction_takes_the_overlapping_files_below() {
        let version = version(&[
            (1, 1, "a", "c", 600),
            (1, 2, "m", "p", 600),
            (2, 3, "b", "d", 10),
            (2, 4, "x", "z", 10),
        ]);
        let compaction = picker()
            .pick(0, cf(&version), &[])
            .expect("L1 is over target");
        assert_eq!(compaction.level, 1);
        assert_eq!(numbers(&compaction.inputs), vec![1]);
        assert_eq!(
            numbers(&compaction.outputs_overlapped),
            vec![3],
            "only what it overlaps"
        );
        assert!(!compaction.is_trivial_move());
        assert_eq!(compaction.input_bytes(), 610);
    }

    /// The L+1 files are being rewritten anyway, so another L file inside their range is free
    /// to include — but only while it does not drag in a further L+1 file.
    #[test]
    fn inputs_expand_only_while_the_level_below_does_not() {
        let narrow = version(&[
            (1, 1, "a", "c", 600),
            (1, 2, "d", "e", 600),
            (2, 3, "a", "f", 10),
        ]);
        let compaction = picker().pick(0, cf(&narrow), &[]).unwrap();
        let mut picked = numbers(&compaction.inputs);
        picked.sort_unstable();
        assert_eq!(
            picked,
            vec![1, 2],
            "both L1 files sit inside file 3's range"
        );
        assert_eq!(numbers(&compaction.outputs_overlapped), vec![3]);

        // Now the second L1 file reaches past file 3 into file 4, so taking it would make this
        // a bigger compaction than the one that was scored.
        let wider = version(&[
            (1, 1, "a", "c", 600),
            (1, 2, "d", "z", 600),
            (2, 3, "a", "f", 10),
            (2, 4, "g", "z", 10),
        ]);
        let compaction = picker().pick(0, cf(&wider), &[]).unwrap();
        assert_eq!(numbers(&compaction.inputs), vec![1]);
        assert_eq!(numbers(&compaction.outputs_overlapped), vec![3]);
    }

    /// The pointer spreads compactions across the key space instead of grinding one prefix.
    #[test]
    fn the_compact_pointer_moves_the_starting_file() {
        let version = version(&[
            (1, 1, "a", "c", 400),
            (1, 2, "m", "p", 400),
            (1, 3, "x", "z", 400),
        ]);
        let picker = picker();
        let mut pointers: Vec<Option<Vec<u8>>> = vec![None; LEVELS];
        assert_eq!(
            numbers(&picker.pick(0, cf(&version), &pointers).unwrap().inputs),
            vec![1]
        );

        pointers[1] = Some(b"c".to_vec());
        assert_eq!(
            numbers(&picker.pick(0, cf(&version), &pointers).unwrap().inputs),
            vec![2]
        );
        pointers[1] = Some(b"p".to_vec());
        assert_eq!(
            numbers(&picker.pick(0, cf(&version), &pointers).unwrap().inputs),
            vec![3]
        );
        // Past the last file it wraps to the first, rather than declining to compact.
        pointers[1] = Some(b"zz".to_vec());
        assert_eq!(
            numbers(&picker.pick(0, cf(&version), &pointers).unwrap().inputs),
            vec![1]
        );
    }

    #[test]
    fn an_explicit_range_picks_only_what_it_covers() {
        let version = version(&[
            (1, 1, "a", "c", 10),
            (1, 2, "m", "p", 10),
            (2, 3, "b", "d", 10),
        ]);
        let picker = picker();
        let compaction = picker
            .pick_range(0, cf(&version), 1, Some(b"a"), Some(b"c"))
            .expect("file 1 covers [a, c]");
        assert_eq!(numbers(&compaction.inputs), vec![1]);
        assert_eq!(numbers(&compaction.outputs_overlapped), vec![3]);

        assert!(
            picker
                .pick_range(0, cf(&version), 1, Some(b"q"), Some(b"w"))
                .is_none()
        );
        // The whole key space, and the whole level.
        let all = picker.pick_range(0, cf(&version), 1, None, None).unwrap();
        assert_eq!(numbers(&all.inputs), vec![1, 2]);
    }

    /// A tombstone may only be dropped where nothing older can hide beneath it. Getting this
    /// wrong resurrects a deleted key.
    #[test]
    fn a_key_is_at_the_bottom_only_when_no_deeper_level_holds_it() {
        let version = version(&[
            (1, 1, "a", "z", 10),
            (2, 2, "a", "z", 10),
            (4, 3, "m", "p", 10),
        ]);
        let picker = picker();
        let cf = cf(&version);

        // Compacting L1 into L2: anything at L3 or below still hides under the output.
        assert!(
            picker.is_bottom_level_for_key(cf, 1, b"a"),
            "nothing below L2 holds a"
        );
        assert!(!picker.is_bottom_level_for_key(cf, 1, b"n"), "L4 holds n");
        // Compacting L3 into L4 puts the output beside file 3, so nothing is below it.
        assert!(picker.is_bottom_level_for_key(cf, 3, b"n"));
    }

    #[test]
    fn level_targets_grow_by_the_multiplier() {
        let picker = picker();
        assert_eq!(picker.max_bytes_for_level(1), 1000);
        assert_eq!(picker.max_bytes_for_level(2), 10_000);
        assert_eq!(picker.max_bytes_for_level(3), 100_000);
    }
}
