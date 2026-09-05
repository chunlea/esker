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
    use super::{SplitOptions, approximate_size, choose_split_key, is_legal_boundary};
    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, WriteBatch, WriteOptions, cf};
    use esker_keys::prefix;
    use esker_proto::{Epoch, Peer, Region};
    use esker_txn::key as txn_key;
    use esker_txn::{Kind, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};
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

    // -- what a region holds when it holds a table ----------------------------------------

    /// The user key of row `n` of one SQL table — what `esker-sql` hands the transaction layer,
    /// built by `esker-keys` rather than spelled out here (`CLAUDE.md` invariant 7).
    fn row_key(n: u32) -> Vec<u8> {
        let mut key = prefix::table_row_prefix(1, 7);
        key.extend_from_slice(&n.to_be_bytes());
        key
    }

    /// Writes `keys` as a **committed** transaction leaves them: a `write` record per key, with
    /// the value inline when it is short enough and its own `default` entry when it is not.
    ///
    /// This is the shape the mpp lane found in a WAL dump of a real SQL table
    /// (`docs/bench/columnar-learner.md`, `cf=2 Put "xt…"`), reproduced without going through
    /// `esker-sql`: the defect is in this module's measurement and belongs to a test of it.
    fn commit_rows(
        db: &Db,
        keys: impl IntoIterator<Item = Vec<u8>>,
        value_len: usize,
        commit_ts: u64,
    ) {
        let write_cf = db.cf_id(cf::WRITE).unwrap();
        let default_cf = db.cf_id(cf::DEFAULT).unwrap();
        let start_ts = commit_ts - 1;
        let value = vec![b'v'; value_len];
        let mut batch = WriteBatch::new();
        for key in keys {
            let mut record = WriteRecord::new(Kind::Put, start_ts);
            if value_len <= SHORT_VALUE_MAX_LEN {
                record.short_value = Some(Bytes::from(value.clone()));
            } else {
                batch.put(default_cf, &txn_key::value(&key, start_ts), &value);
            }
            batch.put(write_cf, &txn_key::write(&key, commit_ts), &record.encode());
        }
        db.write(batch, &WriteOptions::unsynced()).unwrap();
    }

    /// **The finding of `docs/plans/phase-16-mpp.md` §10.** A region holding a SQL table reported
    /// `~0 bytes` to PD and so never crossed a split threshold at any setting — 4 MiB over ~20 MB
    /// of table produced exactly the one region a 512 MiB threshold did.
    ///
    /// The rows here are short, which is the case that decides the fix: a value of
    /// `SHORT_VALUE_MAX_LEN` or under is inlined into the `write` record and costs no `default`
    /// entry at all, so a measurement that reads only `default` reports zero however wide its
    /// range is.
    #[test]
    fn a_region_of_committed_rows_is_not_zero_bytes() {
        let (_dir, db) = open();
        const ROWS: u32 = 5_000;
        const VALUE_LEN: usize = 200;
        commit_rows(&db, (0..ROWS).map(row_key), VALUE_LEN, 20);

        let whole = region(b"", b"");
        let written = u64::from(ROWS) * VALUE_LEN as u64;
        let size = approximate_size(&db, &whole).unwrap();
        assert!(
            size >= written / 2,
            "{ROWS} committed rows of {VALUE_LEN} bytes report {size} bytes, \
             which is not a table PD can see"
        );
    }

    /// A region of committed rows has a boundary, and it is one of the rows.
    ///
    /// The size saying "split" and the scan saying "there is nothing here" is worse than either
    /// alone: `Store::spawn_split_checker` records the region as refused and does not look at it
    /// again until it has grown by another whole threshold.
    #[test]
    fn a_boundary_is_found_in_committed_rows() {
        let (_dir, db) = open();
        const ROWS: u32 = 5_000;
        commit_rows(&db, (0..ROWS).map(row_key), 200, 20);

        let whole = region(b"", b"");
        let split = choose_split_key(&db, &whole, 1024)
            .unwrap()
            .expect("five thousand rows can be split");
        assert!(is_legal_boundary(&split, &whole));

        let at = (0..ROWS)
            .position(|n| row_key(n) == split)
            .expect("the boundary is a row that exists, not a synthesised key");
        assert!(
            (1_000..=4_000).contains(&(at as u32)),
            "the boundary landed at row {at} of {ROWS}"
        );
    }

    /// A key's versions are one key. Without that the sample is weighted by how often a row was
    /// updated rather than by how many rows there are, and a table where one row is rewritten a
    /// thousand times splits inside that row's versions — a boundary that divides no rows at all.
    #[test]
    fn the_versions_of_one_row_count_once() {
        let (_dir, db) = open();
        const ROWS: u32 = 10;
        for version in 0..100 {
            commit_rows(&db, (0..ROWS).map(row_key), 32, 20 + version);
        }

        let whole = region(b"", b"");
        let split = choose_split_key(&db, &whole, 1024)
            .unwrap()
            .expect("ten rows can be split");
        let at = (0..ROWS)
            .position(|n| row_key(n) == split)
            .expect("the boundary is one of the ten rows");
        assert!(
            (2..=8).contains(&(at as u32)),
            "the boundary landed at row {at} of {ROWS}, so versions were sampled as keys"
        );
    }

    /// A value too long to inline lives in `default` under `'x'`, keyed by `start_ts`. Both its
    /// bytes and its key have to be visible, or a table of large rows is the same bug again.
    #[test]
    fn a_value_too_long_to_inline_is_counted_and_can_be_split_at() {
        let (_dir, db) = open();
        const ROWS: u32 = 500;
        const VALUE_LEN: usize = SHORT_VALUE_MAX_LEN + 1_000;
        commit_rows(&db, (0..ROWS).map(row_key), VALUE_LEN, 20);

        let whole = region(b"", b"");
        let size = approximate_size(&db, &whole).unwrap();
        let written = u64::from(ROWS) * VALUE_LEN as u64;
        assert!(
            size >= written / 2,
            "{ROWS} rows of {VALUE_LEN} bytes in the `default` CF report {size} bytes"
        );
        let split = choose_split_key(&db, &whole, 1024)
            .unwrap()
            .expect("five hundred long rows can be split");
        assert!(
            (0..ROWS).any(|n| row_key(n) == split),
            "the boundary is not one of the rows: {split:?}"
        );
    }

    /// **A lock is not a region's size.** It is one in-flight transaction's state, cleared at
    /// commit or rollback, so counting it would make a region's size a function of concurrency
    /// and let a burst of prewrites split data that has not been written
    /// ([ADR 0073](../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
    ///
    /// It is the one test in this group that passes **before** the fix, and it passes for the
    /// wrong reason: everything reported zero. What makes it mean something is the four tests
    /// beside it — once `write` and the `'x'` half of `default` are counted, a zero here is a
    /// decision rather than a blind spot.
    #[test]
    fn a_lock_is_not_a_regions_size() {
        let (_dir, db) = open();
        let lock_cf = db.cf_id(cf::LOCK).unwrap();
        let mut batch = WriteBatch::new();
        let mut record = LockRecord::new(Kind::Put, 19, Bytes::from_static(b"primary"));
        record.short_value = Some(Bytes::from(vec![b'v'; 200]));
        let encoded = record.encode();
        for n in 0..5_000 {
            batch.put(lock_cf, &txn_key::lock(&row_key(n)), &encoded);
        }
        db.write(batch, &WriteOptions::unsynced()).unwrap();

        let whole = region(b"", b"");
        assert_eq!(
            approximate_size(&db, &whole).unwrap(),
            0,
            "five thousand held locks are not five thousand rows of data"
        );
    }

    /// Only the region's own rows are measured and sampled. The `'x'` bound is
    /// `esker_txn::key::prefix`, and the group encoding is order-preserving and prefix-free, so a
    /// user key is inside the region exactly when its engine key is inside the bounds.
    #[test]
    fn the_transactional_scan_stays_inside_the_regions_range() {
        let (_dir, db) = open();
        commit_rows(&db, (0..2_000).map(row_key), 200, 20);

        // A region covering the second half of the table by row key.
        let half = region(&row_key(1_000), &row_key(2_000));
        let whole = region(b"", b"");
        let part = approximate_size(&db, &half).unwrap();
        let all = approximate_size(&db, &whole).unwrap();
        assert!(part > 0, "the half of the table it owns reports nothing");
        assert!(
            part < all,
            "half the table ({part}) is not less than all of it ({all})"
        );

        let split = choose_split_key(&db, &half, 1024)
            .unwrap()
            .expect("a thousand rows can be split");
        assert!(is_legal_boundary(&split, &half), "{split:?}");

        // A region covering rows that were never written has nothing of its own to split.
        let empty = region(&row_key(5_000), &row_key(6_000));
        assert_eq!(approximate_size(&db, &empty).unwrap(), 0);
        assert_eq!(choose_split_key(&db, &empty, 1024).unwrap(), None);
    }
}
