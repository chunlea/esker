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
//!
//! # What "the region's data" is
//!
//! A region owns a range of the **user** key space, and a store writes each user key into one of
//! two physical shapes, in more than one column family: `'r' ++ key` for `RawKV`, and
//! `'x' ++ enc(key) ++ !ts` in `default` and `write` for anything transactional — which is every
//! SQL row, since a row is the user key a transaction writes. Both functions here read every one
//! of those ranges, through this crate's `keyspace` module (`crates/esker-store/src/keyspace.rs`,
//! private, so it is named rather than linked) — the same mapping a snapshot ships a region by.
//!
//! Until [ADR 0073](../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md) they read
//! `['r' ++ start, 's')` in `default` alone, so a region holding a SQL table reported `~0` bytes
//! and offered no boundary: a SQL table occupied exactly one region whatever its size, at any
//! threshold (`docs/plans/phase-16-mpp.md` §10). The `lock` family is still not read, and that is
//! a decision rather than an omission — the ADR says why.

use bytes::Bytes;
use esker_engine::{Db, DbIterator, ReadOptions, cf};
use esker_keys::prefix;
use esker_proto::{ProtoError, Region};

use crate::error::engine_to_proto;
use crate::keyspace::{PHYSICAL_NAMESPACES, physical_ranges};

/// The column families whose bytes are a region's **data**, and so its size and the source of its
/// split boundary ([ADR 0073](../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
///
/// `lock` is absent on purpose: a lock is one in-flight transaction's claim on one key, deleted by
/// both commit and rollback, so counting it would make a region's size a function of how many
/// transactions happen to be open and let a burst of prewrites split data that has not been
/// written. `raft` is absent for the reason `snapshot::SNAPSHOT_CFS` excludes it — it is this
/// store's log and metadata, keyed by region id rather than by user key.
const DATA_CFS: [&str; 2] = [cf::DEFAULT, cf::WRITE];

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
/// Walks every range the region owns — each data column family, each physical namespace — as one
/// stream of **user keys**, keeps an approximately uniform sample of at most `max_sampled` of
/// them, and returns the middle one: the first sample strictly above `region.start_key`, since a
/// boundary equal to the start would leave an empty half.
///
/// User keys, because that is what a boundary is. It is checked against `start_key`/`end_key`, it
/// travels in the `Split` command, and every peer applies it to its own copy of the range —
/// none of which knows about a `'x'` prefix or a timestamp suffix.
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
    let mut keys = RegionKeys::open(db, region)?;

    let mut samples: Vec<Bytes> = Vec::new();
    let mut stride: u64 = 1;
    let mut seen: u64 = 0;

    while let Some(user) = keys.next_key()? {
        if seen.is_multiple_of(stride) {
            samples.push(Bytes::from(user));
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
    }

    Ok(midpoint(&samples, region))
}

/// Every user key a region holds, in order, once each.
///
/// One [`Source`] per (data column family × physical namespace) — the same set of ranges
/// [`approximate_size`] adds up, which is the property worth keeping: a region whose size says
/// "split" and whose scan says "there is nothing here" is recorded as unsplittable and not looked
/// at again until it has grown by another whole threshold
/// (`Store::spawn_split_checker`).
struct RegionKeys {
    sources: Vec<Source>,
}

impl RegionKeys {
    /// Opens a cursor on each range, all reading **one snapshot**, so the merged stream is one
    /// view of the region rather than four taken a moment apart.
    fn open(db: &Db, region: &Region) -> Result<Self, ProtoError> {
        let options = ReadOptions {
            snapshot: Some(db.snapshot()),
            ..ReadOptions::default()
        };
        let mut sources = Vec::with_capacity(DATA_CFS.len() * PHYSICAL_NAMESPACES.len());
        for name in DATA_CFS {
            for (namespace, (low, high)) in
                PHYSICAL_NAMESPACES.into_iter().zip(physical_ranges(region))
            {
                sources.push(Source::open(db, name, &options, namespace, &low, high)?);
            }
        }
        Ok(Self { sources })
    }

    /// The next user key any source holds, and `None` once every source is spent.
    ///
    /// A key held by more than one source — a row whose value was long enough to need a `default`
    /// entry beside its `write` record — is **one** key: every source sitting on it advances
    /// together.
    fn next_key(&mut self) -> Result<Option<Vec<u8>>, ProtoError> {
        let Some(smallest) = self
            .sources
            .iter()
            .filter_map(|source| source.current.as_ref())
            .min()
            .cloned()
        else {
            return Ok(None);
        };
        for source in &mut self.sources {
            if source.current.as_deref() == Some(&smallest[..]) {
                source.advance()?;
            }
        }
        Ok(Some(smallest))
    }
}

/// One column family's slice of one physical namespace, yielding the user keys in it.
struct Source {
    iter: DbIterator,
    /// Exclusive upper bound, an engine key.
    high: Vec<u8>,
    /// Which shape the engine keys in this range have, so they can be read back as user keys.
    namespace: u8,
    /// The user key the cursor is on, or `None` past the end of the range.
    current: Option<Vec<u8>>,
}

impl Source {
    fn open(
        db: &Db,
        name: &str,
        options: &ReadOptions,
        namespace: u8,
        low: &[u8],
        high: Vec<u8>,
    ) -> Result<Self, ProtoError> {
        let mut iter = db
            .iter(name, options)
            .map_err(|error| engine_to_proto(&error))?;
        iter.seek(low);
        let mut source = Self {
            iter,
            high,
            namespace,
            current: None,
        };
        source.load()?;
        Ok(source)
    }

    /// Reads the key the cursor is on into `current`, or clears it past the range.
    ///
    /// The status is checked on every step rather than once at the end, so a read error stops the
    /// scan where it happened instead of being discovered after a boundary was chosen from a
    /// partial one.
    fn load(&mut self) -> Result<(), ProtoError> {
        self.iter
            .status()
            .map_err(|error| engine_to_proto(&error))?;
        self.current = if self.iter.valid() && self.iter.key() < &self.high[..] {
            Some(user_key(self.namespace, self.iter.key())?)
        } else {
            None
        };
        Ok(())
    }

    /// Steps to the next **distinct** user key, passing over the rest of the current key's
    /// versions.
    ///
    /// Without that, the sample is weighted by how often a row was updated rather than by how many
    /// rows there are: a table where one row was rewritten a thousand times would have a thousand
    /// samples inside one row, and its boundary would divide no rows at all.
    fn advance(&mut self) -> Result<(), ProtoError> {
        let Some(passed) = self.current.take() else {
            return Ok(());
        };
        loop {
            self.iter.next();
            self.load()?;
            if self.current.as_deref() != Some(&passed[..]) {
                return Ok(());
            }
        }
    }
}

/// The user key an engine key of `namespace` carries.
///
/// The bounds keep each scan inside its own namespace, so a key that does not fit its shape means
/// the namespace itself is wrong — which is not something to guess about, and not something to
/// panic on either (`CLAUDE.md` invariant 9). A third physical namespace has to be given a decoder
/// here as well as a range in the `keyspace` module.
fn user_key(namespace: u8, engine_key: &[u8]) -> Result<Vec<u8>, ProtoError> {
    match namespace {
        prefix::RAW => engine_key
            .strip_prefix(&[prefix::RAW])
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                ProtoError::internal(format!(
                    "a split scan read a key outside the 'r' namespace: {engine_key:?}"
                ))
            }),
        // `'x' ++ enc(user_key) ++ !ts`, in both the `default` and `write` families. The decode is
        // `esker-txn`'s, because the encoding is (`CLAUDE.md` invariant 7).
        prefix::TXN => esker_txn::key::split(engine_key)
            .map(|(user, _)| user)
            .map_err(|error| {
                ProtoError::internal(format!(
                    "a split scan could not read a key in the 'x' namespace: {error}"
                ))
            }),
        other => Err(ProtoError::internal(format!(
            "a split scan has no way to read the {:?} namespace",
            char::from(other)
        ))),
    }
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
///
/// # Every family the region spans, and every namespace in it
///
/// One [`Db::approximate_size`] per (data column family × physical namespace), summed. The
/// families are `DATA_CFS` and the ranges are `keyspace::physical_ranges`, the mapping a snapshot
/// ships a region by. Both are private, so they are named here rather than linked.
///
/// Both namespaces are asked of both families rather than kept in a table of which family may hold
/// which — `write` holds only `'x'` today, and asking it for `['r', 's')` costs an overlap check
/// that matches no file. It is a cheaper guarantee than the table, and one that cannot go stale.
///
/// Reading `default` under `'r'` alone is what made a SQL table one region for ever, and reading
/// `default` under both namespaces would not have fixed it: a value of
/// `esker_txn::SHORT_VALUE_MAX_LEN` or under is inlined into its `write` record and costs no
/// `default` entry at all, and ordinary SQL rows are short
/// ([ADR 0073](../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
pub fn approximate_size(db: &Db, region: &Region) -> Result<u64, ProtoError> {
    let mut total: u64 = 0;
    for name in DATA_CFS {
        for (low, high) in physical_ranges(region) {
            let bytes = db
                .approximate_size(name, Some(&low), Some(&high))
                .map_err(|error| engine_to_proto(&error))?;
            total = total.saturating_add(bytes);
        }
    }
    Ok(total)
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
        const ROWS: u32 = 5_000;
        const VALUE_LEN: usize = 200;

        let (_dir, db) = open();
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
        const ROWS: u32 = 5_000;

        let (_dir, db) = open();
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
            (1_000..=4_000).contains(&at),
            "the boundary landed at row {at} of {ROWS}"
        );
    }

    /// A key's versions are one key. Without that the sample is weighted by how often a row was
    /// updated rather than by how many rows there are, and a table where one row is rewritten a
    /// thousand times splits inside that row's versions — a boundary that divides no rows at all.
    ///
    /// **The versions are deliberately lopsided**, and the first shape of this test is why: ten
    /// rows of a hundred versions each cannot tell the two apart, because the 500th *version* is
    /// row five's and row five is also the answer counting *keys* gives. One row carrying almost
    /// every version is the distribution where the two answers differ — row zero without the
    /// deduplication (checked by taking it out: "the boundary landed at row 0 of 10"), the middle
    /// row with it.
    #[test]
    fn the_versions_of_one_row_count_once() {
        const ROWS: u32 = 10;
        const REWRITES: u64 = 1_000;

        let (_dir, db) = open();
        commit_rows(&db, (0..ROWS).map(row_key), 32, 20);
        for version in 0..REWRITES {
            commit_rows(&db, [row_key(0)], 32, 22 + version * 2);
        }

        let whole = region(b"", b"");
        // A cap above the version count, so nothing but the deduplication decides the answer.
        let split = choose_split_key(&db, &whole, 4 * 1024)
            .unwrap()
            .expect("ten rows can be split");
        let at = (0..ROWS)
            .position(|n| row_key(n) == split)
            .expect("the boundary is one of the ten rows");
        assert!(
            (2..=8).contains(&at),
            "the boundary landed at row {at} of {ROWS}: row 0 holds {REWRITES} of the {} versions \
             in this region, so versions were sampled as keys",
            REWRITES + u64::from(ROWS)
        );
    }

    /// A value too long to inline lives in `default` under `'x'`, keyed by `start_ts`. Both its
    /// bytes and its key have to be visible, or a table of large rows is the same bug again.
    #[test]
    fn a_value_too_long_to_inline_is_counted_and_can_be_split_at() {
        const ROWS: u32 = 500;
        const VALUE_LEN: usize = SHORT_VALUE_MAX_LEN + 1_000;

        let (_dir, db) = open();
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
