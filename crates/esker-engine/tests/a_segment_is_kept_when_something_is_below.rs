//! **#82.** A lower level still holding an older version keeps the segment above it.
//!
//! [ADR 0111](../../../docs/adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md) drops
//! every version of a key whose newest version is a delete, and condition (2) is what stops that
//! **resurrecting** the key: if a level below the compaction's output still holds an older value,
//! the record above it has to stay, because dropping it makes a deleted key answer with a value
//! again.
//!
//! `compaction::picker::tests::a_range_sees_what_a_point_query_under_it_cannot` pins the predicate
//! — that the point form answers "nothing below" where the range form does not. **This pins the
//! consequence**: that the answer reaches a filter through a real compaction, measured from the
//! output level, and that acting on it leaves the older value reachable.
//!
//! # Why the compaction is the background pool's and not `compact_range`'s
//!
//! `Db::compact_range` walks every level for its range, so it always ends at the bottom and there
//! is never anything below its output — the condition this is about cannot arise under it. The
//! level-at-a-time compaction the picker schedules is the one that has levels beneath it, and with
//! an L0 trigger of one a single flush is enough to ask for it.
//!
//! # The keys
//!
//! The engine is byte-opaque (invariant 7) and so is this test. An MVCC key is
//! `'x' ++ enc(user_key) ++ enc_ts(commit_ts)` with the timestamp **complemented**, so the older
//! version of one logical key sorts **after** the newer one. `k0` stands for the newer and `k9`
//! for the older, which is all the ordering this needs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use esker_engine::compaction::{CompactionFilter, FilterDecision};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::{CfOptions, Db, Options, ReadOptions, WriteOptions, cf};

/// The newer version of the logical key — the one a delete would sit at.
const NEWER: &[u8] = b"k0";
/// The older version of the same logical key, which sorts after it.
const OLDER: &[u8] = b"k9";

/// A filter with `MvccCollector`'s segment rule and nothing else: drop the newer version when
/// nothing below holds any version of the key.
///
/// It records whether it was ever asked, because a test whose filter is never called would pass by
/// not looking — the same denominator every other measurement in this repository carries.
#[derive(Debug, Default)]
struct SegmentRule {
    asked: AtomicBool,
    dropped: AtomicBool,
}

impl CompactionFilter for SegmentRule {
    fn filter(
        &self,
        _level: usize,
        user_key: &[u8],
        _value: &[u8],
        nothing_below: &dyn Fn(&[u8], &[u8]) -> bool,
    ) -> FilterDecision {
        if user_key != NEWER {
            return FilterDecision::Keep;
        }
        self.asked.store(true, Ordering::Relaxed);
        if nothing_below(NEWER, OLDER) {
            self.dropped.store(true, Ordering::Relaxed);
            return FilterDecision::Remove;
        }
        FilterDecision::Keep
    }

    fn name(&self) -> &'static str {
        "test.SegmentRule"
    }
}

#[test]
fn a_segment_is_kept_when_a_lower_level_still_holds_an_older_version() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let rule = Arc::new(SegmentRule::default());
    let mut cf_options = CfOptions {
        // One file is enough to ask for a compaction, which is what makes the level-at-a-time
        // compaction happen without a `compact_range` that would walk to the bottom.
        level0_file_num_compaction_trigger: 1,
        ..CfOptions::default()
    };
    cf_options.compaction_filter = Some(Arc::clone(&rule) as Arc<dyn CompactionFilter>);
    let db = Db::open_with(
        "/db",
        Options {
            create_if_missing: true,
            cf_options,
            ..Options::default()
        },
        Arc::clone(&fs),
        &[cf::DEFAULT],
    )
    .unwrap();

    // The older version, pushed to the bottom where it will sit below everything that follows.
    db.put(cf::DEFAULT, OLDER, b"the value that must not come back")
        .unwrap();
    db.flush(cf::DEFAULT).unwrap();
    db.compact_range(cf::DEFAULT, None, None).unwrap();

    // The newer version, flushed into L0. The trigger is one, so the pool compacts L0 into L1 —
    // an output with five levels beneath it, one of which holds the older version.
    db.put(cf::DEFAULT, NEWER, b"the newest version").unwrap();
    db.write(esker_engine::WriteBatch::new(), &WriteOptions::unsynced())
        .unwrap();
    db.flush(cf::DEFAULT).unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !rule.asked.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // **The denominator.** A filter that was never consulted proves nothing about what it would
    // have decided.
    assert!(
        rule.asked.load(Ordering::Relaxed),
        "the filter was never asked about the newer version, so nothing below was tested"
    );
    assert!(
        !rule.dropped.load(Ordering::Relaxed),
        "the compaction was told nothing was below its output while a lower level held the older \
         version: acting on that drops the segment and the deleted key reads as present again \
         (#82, ADR 0111 condition 2)"
    );
    assert_eq!(
        db.get(cf::DEFAULT, OLDER, &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"the value that must not come back"[..]),
        "the older version is gone, which is the resurrection this condition exists to prevent"
    );
}
