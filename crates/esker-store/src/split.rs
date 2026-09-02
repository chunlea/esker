//! Where to cut a region, and when.
//!
//! A split has to choose a boundary that divides the region's *data* roughly in half. That is a
//! question about what is on disk, and `docs/adr/0012-split-key-selection.md` records why the
//! answer here is a bounded sampling scan rather than the SST-property midpoint
//! `prompts/04-multiraft-pd.md` assumes: the engine exposes no per-range size or key sample
//! (`docs/plans/phase-4.md` §12.3), so the data itself is the only source there is.
//!
//! # Two properties the chosen key must have
//!
//! * **Strictly inside `(start, end)`.** A key equal to `start` splits off an empty left half, and
//!   a key at or past `end` is not this region's to give away. Either produces a region owning
//!   nothing, which is a hole in the key space wearing a region's clothes.
//! * **A key that exists.** The boundary lands on real data, so the halves are as even as the
//!   sample is. Nothing requires it — any key strictly inside would be a legal boundary — but a
//!   synthesised midpoint of two byte strings is even distribution's enemy on prefixed keys, where
//!   the arithmetic middle of `table/1/...` and `table/2/...` is a key nothing sorts near.
//!
//! When no such key exists — a region holding nothing, one key, or only its own `start_key` — this
//! answers `None`. A region that cannot be split is not an error; it is a region that is large
//! because of one big value, and 4b splits by key count rather than by breaking that value up.

use bytes::Bytes;
use esker_engine::{Db, ReadOptions};
use esker_keys::prefix;
use esker_proto::{ProtoError, Region};

use crate::error::engine_to_proto;

/// A region is split once its approximate size passes this (`docs/DESIGN.md` §14).
pub const REGION_SPLIT_SIZE: u64 = crate::REGION_SPLIT_SIZE;

/// Most keys [`choose_split_key`] keeps a sample of.
///
/// The scan itself is not bounded by this — it reads every key in the region, because there is no
/// index to read instead — but its *memory* is. Sixteen thousand samples put the chosen boundary
/// within about 0.006% of the true midpoint by key count, which is far finer than the halves
/// themselves will stay once writing continues.
pub const MAX_SAMPLED_KEYS: usize = 16 * 1024;

/// Fewest samples the scan will work with, whatever it is asked for.
///
/// The sample halves whenever it fills, so a cap of two collapses to *the first key of the region*
/// and stays there — the boundary would be the region's own minimum, which is the one place it must
/// not be. Eight is the smallest cap at which the midpoint is still near the middle, and nothing
/// real configures a cap this low; the floor exists so that a bad number is a poor sample rather
/// than a broken split.
pub const MIN_SAMPLED_KEYS: usize = 8;

/// How this store decides to split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOptions {
    /// Approximate bytes past which a leader looks for a split key.
    pub region_split_size: u64,
    /// Most keys the split-key scan keeps in memory.
    pub max_sampled_keys: usize,
}

impl SplitOptions {
    /// The defaults of `docs/DESIGN.md` §14.
    #[must_use]
    pub fn new() -> Self {
        Self {
            region_split_size: REGION_SPLIT_SIZE,
            max_sampled_keys: MAX_SAMPLED_KEYS,
        }
    }
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Picks the key `region` should be split at, or `None` if it has no legal boundary.
///
/// Scans the region's range in the `default` column family, keeping an approximately uniform
/// sample of at most `max_sampled` keys, and returns the middle one — the first sample strictly
/// above `region.start_key`, since a boundary equal to the start would leave an empty half.
///
/// # Why the sample halves rather than the scan stopping
///
/// Keeping the *first* `max_sampled` keys and taking their middle would put the boundary at the
/// 50th percentile **of the region's first few thousand keys**, which for a region of a million is
/// the 0.2th percentile of the region. The split would be wildly uneven and the next split of the
/// right half would be too, so a region that should become two becomes twenty. Halving the sample
/// and doubling the stride whenever it fills keeps the sample spread over everything the scan has
/// seen, at the cost of one pass and no extra memory.
pub fn choose_split_key(
    db: &Db,
    region: &Region,
    max_sampled: usize,
) -> Result<Option<Bytes>, ProtoError> {
    let max_sampled = max_sampled.max(MIN_SAMPLED_KEYS);
    let low = prefix::raw_key(&region.start_key);
    let high = if region.end_key.is_empty() {
        // The first key past the whole `'r'` namespace, which is where an unbounded region ends.
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(&region.end_key)
    };

    let mut iter = db
        .iter(esker_engine::cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;

    let mut samples: Vec<Bytes> = Vec::new();
    let mut stride: u64 = 1;
    let mut seen: u64 = 0;

    iter.seek(&low);
    while iter.valid() && iter.key() < &high[..] {
        if seen % stride == 0 {
            let Some(user) = iter.key().strip_prefix(&[prefix::RAW]) else {
                // The bounds keep the scan inside the namespace, so this cannot happen without
                // the namespace itself being wrong — which is not something to guess about.
                return Err(ProtoError::internal(format!(
                    "a split scan read a key outside the 'r' namespace: {:?}",
                    iter.key()
                )));
            };
            samples.push(Bytes::copy_from_slice(user));
            if samples.len() == max_sampled {
                // Keep every second sample and look at half as many from here on, so the sample
                // stays spread over the whole of what has been read.
                let mut kept = Vec::with_capacity(max_sampled / 2 + 1);
                for (at, key) in samples.drain(..).enumerate() {
                    if at % 2 == 0 {
                        kept.push(key);
                    }
                }
                samples = kept;
                stride *= 2;
            }
        }
        seen += 1;
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;

    Ok(midpoint(&samples, region))
}

/// The first sample at or after the middle that is a legal boundary for `region`.
///
/// Two rules, and the second is the one a test found. Walking *forward* from the middle rather than
/// backward is deliberate: forward can only make the left half larger, and the left half is the one
/// at risk of being empty. And the search starts at index **1, never 0** — a boundary at the very
/// first sample leaves no sampled key below it, which is a split whose left half holds nothing. A
/// region has to have at least two keys before it has a middle.
fn midpoint(samples: &[Bytes], region: &Region) -> Option<Bytes> {
    if samples.len() < 2 {
        return None;
    }
    samples
        .iter()
        .skip((samples.len() / 2).max(1))
        .find(|key| is_legal_boundary(key, region))
        .cloned()
}

/// Roughly how many bytes `region` holds, from the engine rather than from a counter.
///
/// This replaces 4b's applied-bytes hint, which counted what a peer's own apply had staged and so
/// never shrank on a delete, never counted what was on disk before the process opened, and started
/// again at zero after a restart (`docs/plans/phase-4.md` §12.3 asked for the accessor; §13.3
/// swapped to it). The number is now a property of the data, which means every peer agrees on it
/// and a restarted leader splits as readily as one that has been up for a week.
///
/// It is still a *hint* and nothing deterministic reads it — `Db::approximate_size` names the three
/// directions it over-counts in — but it is now the same hint on every peer, which is what makes a
/// region heartbeat's `approximate_size` a number PD can compare across stores.
pub fn approximate_size(db: &Db, region: &Region) -> Result<u64, ProtoError> {
    let low = prefix::raw_key(&region.start_key);
    let high = if region.end_key.is_empty() {
        // The first key past the whole `'r'` namespace, which is where an unbounded region ends.
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(&region.end_key)
    };
    db.approximate_size(esker_engine::cf::DEFAULT, Some(&low), Some(&high))
        .map_err(|error| engine_to_proto(&error))
}

/// Whether `key` divides `region` into two non-empty ranges.
#[must_use]
pub fn is_legal_boundary(key: &[u8], region: &Region) -> bool {
    // Strictly above the start: a boundary at `start_key` leaves the left half owning nothing.
    // Strictly below the end, where an empty end is the end of the key space and nothing is past
    // it — the same convention every region comparison uses.
    key > &region.start_key[..] && (region.end_key.is_empty() || key < &region.end_key[..])
}

#[cfg(test)]
mod tests {
    use super::{SplitOptions, choose_split_key, is_legal_boundary};
    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, WriteBatch, WriteOptions, cf};
    use esker_keys::prefix;
    use esker_proto::{Epoch, Peer, Region};
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap();
        (dir, Arc::new(db))
    }

    fn region(start: &[u8], end: &[u8]) -> Region {
        Region {
            id: 1,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::INITIAL,
        }
    }

    fn write(db: &Db, keys: impl IntoIterator<Item = Vec<u8>>) {
        let cf_id = db.cf_id(cf::DEFAULT).unwrap();
        let mut batch = WriteBatch::new();
        for key in keys {
            batch.put(cf_id, &prefix::raw_key(&key), b"v");
        }
        db.write(batch, &WriteOptions::unsynced()).unwrap();
    }

    fn keys(count: u32) -> Vec<Vec<u8>> {
        (0..count)
            .map(|n| format!("k{n:08}").into_bytes())
            .collect()
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let options = SplitOptions::new();
        assert_eq!(options.region_split_size, 96 * 1024 * 1024);
        assert!(options.max_sampled_keys >= super::MIN_SAMPLED_KEYS);
        assert!(options.max_sampled_keys >= 1024);
    }

    /// The boundary must leave two non-empty halves. A key equal to the start gives the left half
    /// nothing; a key at or past the end is not this region's to hand over.
    #[test]
    fn a_legal_boundary_leaves_two_non_empty_halves() {
        let bounded = region(b"d", b"m");
        assert!(!is_legal_boundary(b"c", &bounded), "below the region");
        assert!(!is_legal_boundary(b"d", &bounded), "the start itself");
        assert!(is_legal_boundary(b"d\x00", &bounded));
        assert!(is_legal_boundary(b"g", &bounded));
        assert!(!is_legal_boundary(b"m", &bounded), "the end itself");
        assert!(!is_legal_boundary(b"z", &bounded), "past the region");

        // An empty end key is the end of the key space, so nothing is past it.
        let unbounded = region(b"d", b"");
        assert!(is_legal_boundary(b"\xff\xff\xff", &unbounded));
        assert!(!is_legal_boundary(b"d", &unbounded));
    }

    #[test]
    fn the_split_key_is_a_key_that_exists_near_the_middle() {
        let (_dir, db) = open();
        write(&db, keys(1000));
        let whole = region(b"", b"");

        let split = choose_split_key(&db, &whole, 16 * 1024)
            .unwrap()
            .expect("a thousand keys can be split");
        assert!(is_legal_boundary(&split, &whole));
        assert!(
            split.starts_with(b"k"),
            "the boundary was synthesised rather than taken from the data: {split:?}"
        );

        // Within a few percent of the middle by key count, which is all the halves need.
        let position: u32 = String::from_utf8(split[1..].to_vec())
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (400..=600).contains(&position),
            "the boundary landed at {position} of 1000"
        );
    }

    /// The reason the sample halves rather than the scan stopping. With a sample cap far below the
    /// key count, keeping the *first* N keys would put the boundary at the 50th percentile of the
    /// first N — the 1st percentile of the region — and the next split of the right half would be
    /// just as lopsided, so a region that should become two becomes twenty.
    #[test]
    fn a_sample_cap_far_below_the_key_count_still_finds_the_middle() {
        let (_dir, db) = open();
        write(&db, keys(5000));
        let whole = region(b"", b"");

        for cap in [2, 3, 8, 64, 1000] {
            let split = choose_split_key(&db, &whole, cap)
                .unwrap()
                .expect("splittable");
            let position: u32 = String::from_utf8(split[1..].to_vec())
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                (1000..=4000).contains(&position),
                "a cap of {cap} put the boundary at {position} of 5000"
            );
        }
    }

    /// A region with nothing to divide is not an error. It is a region that is large because of
    /// one big value, and this phase splits by key rather than by breaking a value up.
    #[test]
    fn a_region_with_no_legal_boundary_is_not_split() {
        let (_dir, db) = open();
        let whole = region(b"", b"");
        assert_eq!(choose_split_key(&db, &whole, 64).unwrap(), None, "empty");

        write(&db, [b"only".to_vec()]);
        assert_eq!(
            choose_split_key(&db, &whole, 64).unwrap(),
            None,
            "one key has nothing on one side of it"
        );

        // Two keys can be split: the boundary is the second, leaving one key each side.
        write(&db, [b"other".to_vec()]);
        assert_eq!(
            choose_split_key(&db, &whole, 64).unwrap(),
            Some(Bytes::from_static(b"other")),
            "`only` sorts after `other`, so the middle sample is `other` — and it is legal"
        );
    }

    /// A region whose only key *is* its start key has no boundary above it, and a boundary at the
    /// start would leave the left half owning nothing.
    #[test]
    fn a_region_holding_only_its_own_start_key_is_not_split() {
        let (_dir, db) = open();
        write(&db, [b"d".to_vec()]);
        assert_eq!(
            choose_split_key(&db, &region(b"d", b"m"), 64).unwrap(),
            None
        );

        // Add one above it and the boundary is that one.
        write(&db, [b"g".to_vec()]);
        assert_eq!(
            choose_split_key(&db, &region(b"d", b"m"), 64).unwrap(),
            Some(Bytes::from_static(b"g"))
        );
    }

    /// Only the region's own range is looked at. Keys outside it belong to another region — on
    /// this store or another — and sampling them would put the boundary outside the range.
    #[test]
    fn the_scan_stays_inside_the_regions_range() {
        let (_dir, db) = open();
        write(&db, keys(200));
        write(&db, [b"a-below".to_vec(), b"z-above".to_vec()]);

        let bounded = region(b"k", b"l");
        let split = choose_split_key(&db, &bounded, 1024)
            .unwrap()
            .expect("splittable");
        assert!(is_legal_boundary(&split, &bounded));
        assert!(split.starts_with(b"k"), "{split:?}");

        // A region covering only what is outside has nothing of its own to split.
        assert_eq!(
            choose_split_key(&db, &region(b"m", b"y"), 1024).unwrap(),
            None
        );
    }

    /// A skewed distribution still yields a legal boundary — the halves are uneven by *bytes*,
    /// which is what the next size check is for, but neither is empty.
    #[test]
    fn a_skewed_region_still_yields_a_legal_boundary() {
        let (_dir, db) = open();
        // Nine hundred keys under one prefix and ten under another.
        write(&db, (0..900).map(|n| format!("aaa{n:05}").into_bytes()));
        write(&db, (0..10).map(|n| format!("zzz{n:05}").into_bytes()));

        let whole = region(b"", b"");
        let split = choose_split_key(&db, &whole, 1024)
            .unwrap()
            .expect("splittable");
        assert!(is_legal_boundary(&split, &whole));
        assert!(
            split.starts_with(b"aaa"),
            "the middle by key count is inside the dense prefix: {split:?}"
        );
    }
}
