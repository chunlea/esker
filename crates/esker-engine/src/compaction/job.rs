//! Doing the compaction: merge the inputs, drop what nothing can see, write the rest.
//!
//! A pure function of its inputs, as `prompts/01-engine.md` asks: it takes a cursor and
//! somewhere to write, and knows nothing about files, threads or the database. Every rule
//! below is therefore testable on a list in memory, which matters because these rules are the
//! ones that lose data when they are wrong.
//!
//! # What may be dropped
//!
//! Three rules, in this order, and each one is a way to lose data if it is loosened.
//!
//! 1. **A version nothing can reach.** If the *previous* entry for this same user key was at
//!    or below the floor, every reader that can still ask sees that one instead of this. Note
//!    it is the previous entry that matters, not this one: entries arrive newest first, so the
//!    one before is the newer.
//! 2. **A tombstone with nothing beneath it.** A deletion may be forgotten only once no level
//!    below the output can still be holding an older value — that is what `is_bottom` answers.
//!    Dropping one anywhere else resurrects a deleted key, which is the classic
//!    log-structured bug.
//! 3. **What a [`CompactionFilter`] refuses**, and only at or below the floor, because a
//!    snapshot above it may still be entitled to the value. Where older versions could still
//!    exist the entry becomes a tombstone rather than vanishing, so what it was hiding stays
//!    hidden.
//!
//! Everything else is written out unchanged. A compaction never invents an entry and never
//! reorders one.

use std::cmp::Ordering;
use std::fmt;

use crate::dbformat::{EntryKind, InternalKeyComparator, SeqNo, internal_key, split_internal_key};
use crate::error::{Error, Result};
use crate::iterator::Cursor;
use crate::range_del::RangeTombstones;

/// What a filter decides about one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
    /// Write it out unchanged.
    Keep,
    /// Drop it. Where an older version could still exist it becomes a tombstone instead, so
    /// that what it was hiding stays hidden.
    Remove,
}

/// Lets the layer above decide that an entry has outlived its usefulness.
///
/// `esker-txn` is the reason this exists: PD publishes a garbage-collection safepoint, and
/// every MVCC version below it except the newest visible one can go (`docs/DESIGN.md` §8).
/// The engine stays byte-opaque — it hands over the user key and the value and does what it is
/// told (invariant 7).
pub trait CompactionFilter: Send + Sync + fmt::Debug {
    /// Decides about one entry. `level` is the level the entry came from.
    ///
    /// `nothing_below` answers, for a **key range**, whether any level below this compaction's
    /// output still holds something in it. A filter that groups several engine keys into one
    /// logical key — which `esker-txn` does, one per MVCC version — needs that question about the
    /// whole group, and only the caller knows which keys are a group
    /// ([ADR 0111](../../../../docs/adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md)).
    /// A filter with no such grouping ignores it.
    ///
    /// **A parameter and not a defaulted method**, so that a filter added later cannot silently
    /// opt out of a question it should have asked.
    fn filter(
        &self,
        level: usize,
        user_key: &[u8],
        value: &[u8],
        nothing_below: &dyn Fn(&[u8], &[u8]) -> bool,
    ) -> FilterDecision;

    /// A name for logs and for properties.
    fn name(&self) -> &str;
}

/// Where a compaction writes.
///
/// A trait so that the rules above can be tested against a `Vec`, and so that the file-writing
/// half — numbers, the manifest, the filesystem — stays out of them.
pub trait CompactionOutput {
    /// Appends one entry. Keys arrive in internal-key order.
    fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Bytes in the file currently being written; zero when none is open.
    ///
    /// A size, not a state: an SST builder reports bytes it has actually written, and a small
    /// file's entries are all still in an unflushed block, so this reads zero while a file is
    /// very much open. Only [`finish_file`](CompactionOutput::finish_file) knows.
    fn current_file_size(&self) -> u64;

    /// Finishes the open file if there is one, reporting whether that produced a file.
    ///
    /// Returns `false` when nothing was open or nothing had been added, which is why the
    /// caller can end a compaction by calling it unconditionally.
    fn finish_file(&mut self) -> Result<bool>;
}

/// What one compaction did. Reported as metrics, and asserted in tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionStats {
    /// Entries read from the inputs.
    pub entries_read: u64,
    /// Entries written to the outputs.
    pub entries_written: u64,
    /// Versions dropped because a newer one is visible to every reader.
    pub dropped_shadowed: u64,
    /// Tombstones dropped because nothing older can be beneath them.
    pub dropped_tombstones: u64,
    /// Entries the filter refused.
    pub dropped_by_filter: u64,
    /// Entries dropped because a range tombstone this compaction is discharging covers them.
    pub dropped_by_range: u64,
    /// Output files produced.
    pub files_written: u64,
}

/// One compaction's rules, ready to run.
pub struct CompactionJob<'a> {
    /// The order the inputs are in.
    pub comparator: &'a InternalKeyComparator,
    /// The sequence number below which a shadowed version may be dropped: the oldest live
    /// snapshot, or the newest visible sequence number when there are none.
    pub floor: SeqNo,
    /// The level the inputs came from. Passed to the filter, which may care.
    pub level: usize,
    /// Bytes an output file may reach before the next entry starts a new one.
    pub target_file_size: u64,
    /// Optional; without one nothing is filtered.
    pub filter: Option<&'a dyn CompactionFilter>,
    /// Whether no level below the output level holds this user key.
    pub is_bottom: &'a dyn Fn(&[u8]) -> bool,
    /// The same question over a **range**, for a filter whose logical key spans several engine
    /// keys. See [`CompactionFilter::filter`].
    pub nothing_below: &'a dyn Fn(&[u8], &[u8]) -> bool,
    /// The range tombstones this compaction is **discharging**.
    ///
    /// Empty for every ordinary compaction. When it is not, this compaction has taken every
    /// file at every level that the tombstones cover
    /// ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md) decision 6), so an entry a
    /// tombstone hides can be dropped outright rather than propagated: there is no older
    /// version left anywhere for it to resurrect.
    ///
    /// The tombstones themselves are **never written to the output**. That is the whole of the
    /// invariant "no SST below L0 holds a range tombstone", and the caller asserts it.
    pub tombstones: &'a RangeTombstones,
}

impl fmt::Debug for CompactionJob<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompactionJob")
            .field("level", &self.level)
            .field("floor", &self.floor)
            .field("target_file_size", &self.target_file_size)
            .field("filter", &self.filter.map(CompactionFilter::name))
            .finish_non_exhaustive()
    }
}

impl CompactionJob<'_> {
    /// Runs the compaction, reading `input` to the end and writing what survives to `output`.
    pub fn run(
        &self,
        input: &mut dyn Cursor,
        output: &mut dyn CompactionOutput,
    ) -> Result<CompactionStats> {
        let user = self.comparator.user_comparator().as_ref();
        let mut stats = CompactionStats::default();
        let mut current_user: Option<Vec<u8>> = None;
        // The sequence number of the entry before this one for the same user key. `MAX` means
        // there was none, so nothing shadows this entry yet.
        let mut previous_seqno = SeqNo::MAX;

        input.seek_to_first();
        while input.valid() {
            let key = input.key().to_vec();
            let value = input.value().to_vec();
            stats.entries_read += 1;

            let Some((user_key, seqno, kind)) = split_internal_key(&key) else {
                return Err(Error::corruption(
                    "compaction",
                    format!(
                        "an entry at position {} has no decodable tag",
                        stats.entries_read
                    ),
                ));
            };

            let new_key = current_user
                .as_deref()
                .is_none_or(|current| user.cmp(user_key, current) != Ordering::Equal);
            if new_key {
                current_user = Some(user_key.to_vec());
                previous_seqno = SeqNo::MAX;
            }

            let mut emit = Some(kind);
            // A range tombstone hides an entry strictly older than it, and this compaction
            // holds every file the tombstone covers — so "hidden" and "gone" are the same
            // thing here. The `floor` guard is the ordinary one: a reader whose snapshot
            // predates the delete must still see what it deleted.
            if !self.tombstones.is_empty()
                && self
                    .tombstones
                    .newest_covering(user_key, self.floor, user)
                    .is_some_and(|covering| covering > seqno)
            {
                stats.dropped_by_range += 1;
                // Nothing older survives anywhere, so this needs no tombstone standing in for
                // it — unlike the filter's case below, where lower levels are untouched.
                emit = None;
            } else if previous_seqno <= self.floor {
                // A newer version of this key is visible to every reader that is left.
                emit = None;
                stats.dropped_shadowed += 1;
            } else if matches!(kind, EntryKind::Delete | EntryKind::DeleteRange)
                && seqno <= self.floor
                && (self.is_bottom)(user_key)
            {
                emit = None;
                stats.dropped_tombstones += 1;
            } else if kind == EntryKind::Put
                && seqno <= self.floor
                && let Some(filter) = self.filter
                && filter.filter(self.level, user_key, &value, self.nothing_below)
                    == FilterDecision::Remove
            {
                stats.dropped_by_filter += 1;
                // Vanishing outright is only safe where nothing older is underneath.
                emit = (!(self.is_bottom)(user_key)).then_some(EntryKind::Delete);
            }
            previous_seqno = seqno;

            if let Some(emit) = emit {
                if output.current_file_size() >= self.target_file_size && output.finish_file()? {
                    stats.files_written += 1;
                }
                if emit == kind {
                    output.add(&key, &value)?;
                } else {
                    // Rewritten as a tombstone at the same sequence number: same user key, a
                    // smaller kind, so it still sorts where the original did relative to the
                    // versions around it.
                    output.add(&internal_key(user_key, seqno, emit), &[])?;
                }
                stats.entries_written += 1;
            }
            input.next();
        }
        // An unreadable block ends a cursor exactly as running out does, so the reason has to
        // be asked for rather than assumed.
        input.status()?;

        // Unconditional: `current_file_size` cannot tell an empty output from an unflushed
        // one, and mistaking the second for the first drops every entry the compaction wrote
        // while still deleting its inputs.
        if output.finish_file()? {
            stats.files_written += 1;
        }
        Ok(stats)
    }
}

#[cfg(test)]
// A test filter's name is a literal, which clippy would rather see as `&'static str` — but the
// trait says `&str`, because a real filter may build its name from its configuration.
#[allow(clippy::unnecessary_literal_bound)]
mod tests {
    use super::{
        CompactionFilter, CompactionJob, CompactionOutput, CompactionStats, FilterDecision,
    };
    use crate::dbformat::{
        BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, SeqNo, extract_user_key,
        internal_key, split_internal_key,
    };
    use crate::error::Result;
    use crate::iterator::Cursor;
    use crate::range_del::{RangeTombstone, RangeTombstones};
    use std::cmp::Ordering;
    use std::sync::Arc;

    /// A cursor over a sorted list, so the rules can be tested with nothing else present.
    #[derive(Debug)]
    struct Input {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        position: Option<usize>,
        comparator: Arc<InternalKeyComparator>,
    }

    impl Input {
        fn new(mut entries: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
            let comparator = comparator();
            entries.sort_by(|a, b| comparator.cmp(&a.0, &b.0));
            Self {
                entries,
                position: None,
                comparator,
            }
        }
    }

    impl Cursor for Input {
        fn valid(&self) -> bool {
            self.position.is_some()
        }
        fn key(&self) -> &[u8] {
            &self.entries[self.position.unwrap()].0
        }
        fn value(&self) -> &[u8] {
            &self.entries[self.position.unwrap()].1
        }
        fn seek(&mut self, target: &[u8]) {
            self.position = self
                .entries
                .iter()
                .position(|(key, _)| self.comparator.cmp(key, target) != Ordering::Less);
        }
        fn seek_for_prev(&mut self, target: &[u8]) {
            self.position = self
                .entries
                .iter()
                .rposition(|(key, _)| self.comparator.cmp(key, target) != Ordering::Greater);
        }
        fn seek_to_first(&mut self) {
            self.position = (!self.entries.is_empty()).then_some(0);
        }
        fn seek_to_last(&mut self) {
            self.position = self.entries.len().checked_sub(1);
        }
        fn next(&mut self) {
            self.position = self
                .position
                .and_then(|p| (p + 1 < self.entries.len()).then_some(p + 1));
        }
        fn prev(&mut self) {
            self.position = self.position.and_then(|p| p.checked_sub(1));
        }
        fn status(&self) -> Result<()> {
            Ok(())
        }
    }

    /// Collects what a compaction writes, one `Vec` per output file.
    #[derive(Debug, Default)]
    struct Collected {
        files: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
        open: Option<Vec<(Vec<u8>, Vec<u8>)>>,
        bytes: u64,
    }

    impl Collected {
        fn entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
            self.files.iter().flatten().cloned().collect()
        }

        /// `(user key, seqno, kind)` of everything written, in order.
        fn decoded(&self) -> Vec<(String, SeqNo, EntryKind)> {
            self.entries()
                .iter()
                .map(|(key, _)| {
                    let (user, seqno, kind) = split_internal_key(key).unwrap();
                    (String::from_utf8_lossy(user).into_owned(), seqno, kind)
                })
                .collect()
        }
    }

    impl CompactionOutput for Collected {
        fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
            self.bytes += u64::try_from(key.len() + value.len()).unwrap_or(u64::MAX);
            self.open
                .get_or_insert_with(Vec::new)
                .push((key.to_vec(), value.to_vec()));
            Ok(())
        }
        fn current_file_size(&self) -> u64 {
            if self.open.is_some() { self.bytes } else { 0 }
        }

        fn finish_file(&mut self) -> Result<bool> {
            self.bytes = 0;
            match self.open.take() {
                Some(file) if !file.is_empty() => {
                    self.files.push(file);
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
    }

    #[derive(Debug)]
    struct RemoveValuesStartingWith(u8);

    impl CompactionFilter for RemoveValuesStartingWith {
        fn filter(
            &self,
            _level: usize,
            _user_key: &[u8],
            value: &[u8],
            _nothing_below: &dyn Fn(&[u8], &[u8]) -> bool,
        ) -> FilterDecision {
            if value.first() == Some(&self.0) {
                FilterDecision::Remove
            } else {
                FilterDecision::Keep
            }
        }
        fn name(&self) -> &str {
            "test.RemoveValuesStartingWith"
        }
    }

    fn comparator() -> Arc<InternalKeyComparator> {
        Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
    }

    fn entry(key: &str, seqno: SeqNo, kind: EntryKind, value: &str) -> (Vec<u8>, Vec<u8>) {
        (
            internal_key(key.as_bytes(), seqno, kind),
            value.as_bytes().to_vec(),
        )
    }

    /// Runs a job discharging `tombstones`, with every key at the bottom — which is what a
    /// discharge arranges by taking every file the tombstone covers.
    fn run_discharging(
        entries: Vec<(&str, SeqNo, EntryKind)>,
        floor: SeqNo,
        tombstones: &RangeTombstones,
    ) -> (Collected, CompactionStats) {
        let comparator = comparator();
        let mut input = Input::new(
            entries
                .into_iter()
                .map(|(key, seqno, kind)| entry(key, seqno, kind, "v"))
                .collect(),
        );
        let mut output = Collected::default();
        let is_bottom = |_: &[u8]| true;
        let job = CompactionJob {
            comparator: &comparator,
            floor,
            level: 0,
            target_file_size: u64::MAX,
            filter: None,
            nothing_below: &|_, _| true,
            is_bottom: &is_bottom,
            tombstones,
        };
        let stats = job.run(&mut input, &mut output).unwrap();
        (output, stats)
    }

    /// The discharge: an entry a tombstone covers is dropped outright, because this compaction
    /// holds every file the tombstone reaches and there is no older version left to resurrect
    /// ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md) decision 6).
    #[test]
    fn a_discharge_drops_the_entries_its_tombstone_covers() {
        let mut tombstones = RangeTombstones::new();
        tombstones.push(
            RangeTombstone::new(b"b".to_vec(), b"d".to_vec(), 50),
            &BytewiseComparator,
        );
        let (output, stats) = run_discharging(
            vec![
                ("a", 10, EntryKind::Put),
                ("b", 10, EntryKind::Put),
                ("c", 10, EntryKind::Put),
                ("d", 10, EntryKind::Put),
            ],
            100,
            &tombstones,
        );
        assert_eq!(
            output.decoded(),
            vec![
                ("a".to_string(), 10, EntryKind::Put),
                ("d".to_string(), 10, EntryKind::Put),
            ],
            "`[b, d)` went, and the half-open bounds held"
        );
        assert_eq!(stats.dropped_by_range, 2);
    }

    /// A write *after* the delete survives it: the tombstone hides only what is strictly older.
    #[test]
    fn a_discharge_keeps_what_was_written_after_the_delete() {
        let mut tombstones = RangeTombstones::new();
        tombstones.push(
            RangeTombstone::new(b"a".to_vec(), b"z".to_vec(), 50),
            &BytewiseComparator,
        );
        let (output, stats) = run_discharging(
            vec![("k", 60, EntryKind::Put), ("m", 40, EntryKind::Put)],
            100,
            &tombstones,
        );
        assert_eq!(
            output.decoded(),
            vec![("k".to_string(), 60, EntryKind::Put)],
            "60 is above the tombstone, 40 is below it"
        );
        assert_eq!(stats.dropped_by_range, 1);
    }

    /// And a reader whose snapshot predates the delete still has to see what it deleted, so a
    /// tombstone above the floor drops nothing.
    #[test]
    fn a_tombstone_above_the_floor_drops_nothing() {
        let mut tombstones = RangeTombstones::new();
        tombstones.push(
            RangeTombstone::new(b"a".to_vec(), b"z".to_vec(), 50),
            &BytewiseComparator,
        );
        let (output, stats) = run_discharging(vec![("k", 10, EntryKind::Put)], 20, &tombstones);
        assert_eq!(
            output.decoded(),
            vec![("k".to_string(), 10, EntryKind::Put)]
        );
        assert_eq!(stats.dropped_by_range, 0);
    }

    /// Runs a job with `floor`, treating every key as being at the bottom unless told
    /// otherwise.
    fn run(
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        floor: SeqNo,
        bottom: bool,
        filter: Option<&dyn CompactionFilter>,
    ) -> (Collected, CompactionStats) {
        let comparator = comparator();
        let is_bottom = |_: &[u8]| bottom;
        let job = CompactionJob {
            comparator: &comparator,
            floor,
            level: 1,
            target_file_size: u64::MAX,
            filter,
            nothing_below: &|_, _| true,
            is_bottom: &is_bottom,
            tombstones: &RangeTombstones::new(),
        };
        let mut input = Input::new(entries);
        let mut output = Collected::default();
        let stats = job.run(&mut input, &mut output).unwrap();
        (output, stats)
    }

    /// With no snapshots the floor is the present, so only the newest version of each key
    /// survives — and the ones behind it are what a compaction exists to remove.
    #[test]
    fn older_versions_are_dropped_below_the_floor() {
        let (output, stats) = run(
            vec![
                entry("a", 3, EntryKind::Put, "a3"),
                entry("a", 2, EntryKind::Put, "a2"),
                entry("a", 1, EntryKind::Put, "a1"),
                entry("b", 5, EntryKind::Put, "b5"),
            ],
            10,
            true,
            None,
        );
        assert_eq!(
            output.decoded(),
            vec![
                ("a".to_string(), 3, EntryKind::Put),
                ("b".to_string(), 5, EntryKind::Put),
            ]
        );
        assert_eq!(stats.entries_read, 4);
        assert_eq!(stats.entries_written, 2);
        assert_eq!(stats.dropped_shadowed, 2);
        assert_eq!(stats.files_written, 1);
    }

    /// A snapshot below the newest version keeps everything down to it: the floor is the whole
    /// mechanism by which a reader's history survives.
    #[test]
    fn versions_a_snapshot_can_still_see_are_kept() {
        let (output, stats) = run(
            vec![
                entry("a", 5, EntryKind::Put, "a5"),
                entry("a", 4, EntryKind::Put, "a4"),
                entry("a", 3, EntryKind::Put, "a3"),
                entry("a", 2, EntryKind::Put, "a2"),
            ],
            3,
            true,
            None,
        );
        // 5 and 4 are above the floor, so nothing shadows them yet; 3 is the newest at or
        // below it and survives; 2 is behind 3 and goes.
        assert_eq!(
            output
                .decoded()
                .iter()
                .map(|(_, seqno, _)| *seqno)
                .collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
        assert_eq!(stats.dropped_shadowed, 1);
    }

    /// The classic log-structured bug: forget a tombstone while an older value is still
    /// underneath it, and the deleted key comes back.
    #[test]
    fn a_tombstone_is_kept_unless_nothing_is_beneath_it() {
        let deleted = vec![entry("a", 4, EntryKind::Delete, "")];

        let (bottom, stats) = run(deleted.clone(), 10, true, None);
        assert!(
            bottom.entries().is_empty(),
            "nothing below it, so it can go"
        );
        assert_eq!(stats.dropped_tombstones, 1);

        let (not_bottom, stats) = run(deleted, 10, false, None);
        assert_eq!(
            not_bottom.decoded(),
            vec![("a".to_string(), 4, EntryKind::Delete)],
            "a level below may still hold an older value"
        );
        assert_eq!(stats.dropped_tombstones, 0);
    }

    /// A tombstone above the floor is visible to a snapshot, so it stays wherever it is.
    #[test]
    fn a_tombstone_above_the_floor_is_kept_even_at_the_bottom() {
        let (output, _) = run(vec![entry("a", 9, EntryKind::Delete, "")], 3, true, None);
        assert_eq!(
            output.decoded(),
            vec![("a".to_string(), 9, EntryKind::Delete)]
        );
    }

    /// The tombstone hides the older value, and both go together at the bottom.
    #[test]
    fn a_tombstone_takes_the_value_under_it_with_it() {
        let (output, stats) = run(
            vec![
                entry("a", 4, EntryKind::Delete, ""),
                entry("a", 1, EntryKind::Put, "gone"),
                entry("b", 2, EntryKind::Put, "kept"),
            ],
            10,
            true,
            None,
        );
        assert_eq!(output.decoded(), vec![("b".to_string(), 2, EntryKind::Put)]);
        assert_eq!(stats.dropped_tombstones, 1);
        assert_eq!(stats.dropped_shadowed, 1);
    }

    /// What `esker-txn` will use it for: dropping versions the safepoint has passed.
    #[test]
    fn the_filter_removes_entries_at_the_bottom_and_hides_them_elsewhere() {
        let filter = RemoveValuesStartingWith(b'x');
        let entries = vec![
            entry("a", 5, EntryKind::Put, "xdead"),
            entry("b", 5, EntryKind::Put, "keep"),
        ];

        let (bottom, stats) = run(entries.clone(), 10, true, Some(&filter));
        assert_eq!(bottom.decoded(), vec![("b".to_string(), 5, EntryKind::Put)]);
        assert_eq!(stats.dropped_by_filter, 1);

        // Above the bottom it becomes a tombstone rather than vanishing, so whatever it was
        // hiding stays hidden.
        let (middle, stats) = run(entries, 10, false, Some(&filter));
        assert_eq!(
            middle.decoded(),
            vec![
                ("a".to_string(), 5, EntryKind::Delete),
                ("b".to_string(), 5, EntryKind::Put),
            ]
        );
        assert_eq!(stats.dropped_by_filter, 1);
        assert!(
            middle.entries()[0].1.is_empty(),
            "a tombstone carries no value"
        );
    }

    /// A snapshot above the floor may still be entitled to the value, so the filter does not
    /// get to see it.
    #[test]
    fn the_filter_leaves_entries_above_the_floor_alone() {
        let filter = RemoveValuesStartingWith(b'x');
        let (output, stats) = run(
            vec![entry("a", 9, EntryKind::Put, "xdead")],
            3,
            true,
            Some(&filter),
        );
        assert_eq!(output.decoded(), vec![("a".to_string(), 9, EntryKind::Put)]);
        assert_eq!(stats.dropped_by_filter, 0);
    }

    /// A builder that reports zero until it has enough to flush, as a real one does. The job
    /// must still write everything: this is the shape that lost a whole compaction the first
    /// time round.
    #[test]
    fn a_small_compaction_is_written_even_though_it_reports_no_size() {
        #[derive(Debug, Default)]
        struct NeverFlushes {
            files: Vec<Vec<Vec<u8>>>,
            open: Option<Vec<Vec<u8>>>,
        }
        impl CompactionOutput for NeverFlushes {
            fn add(&mut self, key: &[u8], _value: &[u8]) -> Result<()> {
                self.open.get_or_insert_with(Vec::new).push(key.to_vec());
                Ok(())
            }
            fn current_file_size(&self) -> u64 {
                0 // Everything is still in an unflushed block.
            }
            fn finish_file(&mut self) -> Result<bool> {
                match self.open.take() {
                    Some(file) if !file.is_empty() => {
                        self.files.push(file);
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }

        let comparator = comparator();
        let is_bottom = |_: &[u8]| true;
        let job = CompactionJob {
            comparator: &comparator,
            floor: 1000,
            level: 1,
            target_file_size: 8 << 20,
            filter: None,
            nothing_below: &|_, _| true,
            is_bottom: &is_bottom,
            tombstones: &RangeTombstones::new(),
        };
        let mut input = Input::new(vec![
            entry("a", 5, EntryKind::Put, "a"),
            entry("b", 5, EntryKind::Put, "b"),
        ]);
        let mut output = NeverFlushes::default();
        let stats = job.run(&mut input, &mut output).unwrap();
        assert_eq!(stats.entries_written, 2);
        assert_eq!(
            stats.files_written, 1,
            "the file must be finished even at size zero"
        );
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0].len(), 2);
    }

    /// Output is cut into files at the target size, and the entries are the same either way.
    #[test]
    fn output_is_split_into_files_at_the_target_size() {
        let comparator = comparator();
        let is_bottom = |_: &[u8]| true;
        let entries: Vec<_> = (0..20u32)
            .map(|i| entry(&format!("key-{i:03}"), 100, EntryKind::Put, "value"))
            .collect();

        let job = CompactionJob {
            comparator: &comparator,
            floor: 1000,
            level: 1,
            target_file_size: 40,
            filter: None,
            nothing_below: &|_, _| true,
            is_bottom: &is_bottom,
            tombstones: &RangeTombstones::new(),
        };
        let mut input = Input::new(entries.clone());
        let mut output = Collected::default();
        let stats = job.run(&mut input, &mut output).unwrap();

        assert!(
            output.files.len() > 1,
            "twenty entries should not fit in one 40-byte file"
        );
        assert_eq!(
            usize::try_from(stats.files_written).unwrap(),
            output.files.len()
        );
        assert_eq!(stats.entries_written, 20);

        let mut written: Vec<Vec<u8>> = output.entries().into_iter().map(|(key, _)| key).collect();
        let mut expected: Vec<Vec<u8>> = entries.into_iter().map(|(key, _)| key).collect();
        written.sort_by(|a, b| comparator.cmp(a, b));
        expected.sort_by(|a, b| comparator.cmp(a, b));
        assert_eq!(
            written, expected,
            "splitting must not change what was written"
        );
    }

    #[test]
    fn an_empty_compaction_writes_nothing() {
        let (output, stats) = run(Vec::new(), 10, true, None);
        assert!(output.files.is_empty());
        assert_eq!(stats, CompactionStats::default());
    }

    /// Keys are byte-opaque to the engine; the user key handed to a filter is exactly what was
    /// stored, tag stripped and nothing else (invariant 7).
    #[test]
    fn the_filter_sees_the_user_key_and_nothing_else() {
        #[derive(Debug)]
        struct Recorder(std::sync::Mutex<Vec<Vec<u8>>>);
        impl CompactionFilter for Recorder {
            fn filter(
                &self,
                _level: usize,
                user_key: &[u8],
                _value: &[u8],
                _nothing_below: &dyn Fn(&[u8], &[u8]) -> bool,
            ) -> FilterDecision {
                self.0.lock().unwrap().push(user_key.to_vec());
                FilterDecision::Keep
            }
            fn name(&self) -> &str {
                "test.Recorder"
            }
        }
        let filter = Recorder(std::sync::Mutex::new(Vec::new()));
        let key = internal_key(b"user\xffkey", 4, EntryKind::Put);
        run(vec![(key.clone(), b"v".to_vec())], 10, true, Some(&filter));
        assert_eq!(
            filter.0.lock().unwrap().as_slice(),
            &[extract_user_key(&key).to_vec()]
        );
    }
}
