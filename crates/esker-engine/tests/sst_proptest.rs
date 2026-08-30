//! A table is a `BTreeMap` that lives in a file. These properties say exactly that.
//!
//! One table is built from a random sorted map under a random combination of every knob —
//! block size, restart interval, codec, filter density, prefix extractor — and then the map is
//! the oracle for every question the table can be asked: point lookups for keys that are there
//! and keys that are not, full scans in both directions, and range scans in both directions.
//!
//! The knobs are randomised together on purpose. Most SST bugs are not in one component but at
//! a boundary between two settings: a block cut exactly at a restart point, a restart interval
//! larger than a block, a prefix extractor whose domain excludes some of the keys. Fixing the
//! options would test the middle of the space and miss all of it.
//!
//! Every property runs at least 1,000 cases (`prompts/00-scaffold.md` acceptance).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use esker_engine::cache::ShardedLruCache;
use esker_engine::cache_api::BlockCache;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Compression, StripSuffix};
use esker_engine::sst::{TableBuilder, TableIter, TableOptions, TableReader};
use proptest::prelude::*;

const CASES: u32 = 1_000;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    }
}

/// The acceptance criterion is a number, so it is asserted rather than trusted.
#[test]
fn the_property_configuration_runs_at_least_a_thousand_cases() {
    assert!(CASES >= 1_000);
    assert_eq!(config().cases, CASES);
}

/// Keys that collide in their prefixes, run to `0xFF`, and are sometimes empty — the shapes
/// that break a prefix-compressed, length-prefixed encoding.
fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => prop::collection::vec(prop_oneof![Just(0u8), Just(0xffu8), any::<u8>()], 0..12),
        2 => (0..6usize, 0..6usize).prop_map(|(shared, tail)| {
            let mut k = vec![b'k'; shared];
            k.extend(std::iter::repeat_n(0xffu8, tail));
            k
        }),
        1 => Just(Vec::new()),
    ]
}

fn model() -> impl Strategy<Value = BTreeMap<Vec<u8>, Vec<u8>>> {
    prop::collection::btree_map(key(), prop::collection::vec(any::<u8>(), 0..48), 0..48)
}

/// All six knobs at once. `strip` is the suffix length of a [`StripSuffix`] extractor, or
/// `None` for no extractor at all.
#[derive(Debug, Clone)]
struct Knobs {
    block_size: usize,
    restart_interval: usize,
    lz4: bool,
    bloom_bits: usize,
    strip: Option<usize>,
    cache: bool,
}

fn knobs() -> impl Strategy<Value = Knobs> {
    (
        prop_oneof![Just(1usize), Just(64), Just(256), Just(4096)],
        1usize..20,
        any::<bool>(),
        prop_oneof![Just(0usize), Just(1), Just(10), Just(20)],
        prop_oneof![Just(None), Just(Some(0usize)), Just(Some(3)), Just(Some(9))],
        any::<bool>(),
    )
        .prop_map(
            |(block_size, restart_interval, lz4, bloom_bits, strip, cache)| Knobs {
                block_size,
                restart_interval,
                lz4,
                bloom_bits,
                strip,
                cache,
            },
        )
}

impl Knobs {
    fn options(&self) -> TableOptions {
        TableOptions {
            block_size: self.block_size,
            restart_interval: self.restart_interval,
            bloom_bits_per_key: self.bloom_bits,
            prefix_extractor: self
                .strip
                .map(|len| Arc::new(StripSuffix::new(len)) as Arc<_>),
            compression: if self.lz4 {
                Compression::Lz4
            } else {
                Compression::None
            },
            ..TableOptions::default()
        }
    }
}

/// Turns an engine error into a proptest failure, so a case that cannot even build its table
/// reports why rather than unwinding.
trait OrFail<T> {
    fn or_fail(self) -> Result<T, TestCaseError>;
}

impl<T> OrFail<T> for esker_engine::Result<T> {
    fn or_fail(self) -> Result<T, TestCaseError> {
        self.map_err(|error| TestCaseError::fail(error.to_string()))
    }
}

/// Writes the map into a table and opens it again.
fn build_and_open(
    knobs: &Knobs,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<TableReader, TestCaseError> {
    let fs = MemFileSystem::new();
    let mut builder = TableBuilder::new(knobs.options(), fs.create(Path::new("/t.sst")).unwrap());
    for (key, value) in model {
        builder.add(key, value).or_fail()?;
    }
    builder.finish().or_fail()?;

    let cache = knobs
        .cache
        .then(|| Arc::new(ShardedLruCache::new(64 * 1024)) as Arc<dyn BlockCache>);
    TableReader::open(
        fs.open(Path::new("/t.sst")).unwrap(),
        11,
        knobs.options(),
        cache,
    )
    .or_fail()
}

fn forward(iter: &mut TableIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    out
}

fn backward(iter: &mut TableIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    iter.seek_to_last();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    out.reverse();
    out
}

proptest! {
    #![proptest_config(config())]

    /// Everything that is in the map is in the table, by point lookup and by scanning either
    /// way, under every combination of the knobs.
    #[test]
    fn a_table_holds_exactly_its_map(model in model(), knobs in knobs()) {
        let table = build_and_open(&knobs, &model)?;
        let expected: Vec<(Vec<u8>, Vec<u8>)> =
            model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        prop_assert_eq!(table.properties().entry_count, model.len() as u64);

        for (key, value) in &model {
            let found = table.get(key).or_fail()?;
            prop_assert_eq!(found.as_ref(), Some(value), "get({:?})", key);
        }

        let mut iter = table.iter();
        prop_assert_eq!(forward(&mut iter), expected.clone());
        prop_assert_eq!(backward(&mut iter), expected);
        iter.status().or_fail()?;
    }

    /// Nothing that is not in the map is in the table. This is the half a filter can get
    /// wrong: a false negative shows up here as a key that is present being reported absent,
    /// and a broken filter shows up as an absent key coming back with a value.
    #[test]
    fn absent_keys_are_absent(
        model in model(),
        knobs in knobs(),
        probes in prop::collection::vec(key(), 1..24),
    ) {
        let table = build_and_open(&knobs, &model)?;
        for probe in &probes {
            let found = table.get(probe).or_fail()?;
            prop_assert_eq!(found.as_ref(), model.get(probe), "get({:?})", probe);
        }
    }

    /// A seek lands where the map says it should — the first key at or after the target — and
    /// `seek_for_prev` lands on the last key at or before it. Targets are drawn from the same
    /// distribution as the keys, so they land on keys, between keys, and outside the table.
    #[test]
    fn seeks_land_where_the_map_says(
        model in model(),
        knobs in knobs(),
        targets in prop::collection::vec(key(), 1..16),
    ) {
        let table = build_and_open(&knobs, &model)?;
        let mut iter = table.iter();

        for target in &targets {
            iter.seek(target);
            let expected = model.range(target.clone()..).next().map(|(k, _)| k.clone());
            prop_assert_eq!(
                iter.valid().then(|| iter.key().to_vec()),
                expected,
                "seek({:?})", target
            );

            iter.seek_for_prev(target);
            let expected = model.range(..=target.clone()).next_back().map(|(k, _)| k.clone());
            prop_assert_eq!(
                iter.valid().then(|| iter.key().to_vec()),
                expected,
                "seek_for_prev({:?})", target
            );
        }
        iter.status().or_fail()?;
    }

    /// Every range of the table equals the same range of the map, scanned forwards from the
    /// low end and backwards from the high end. Ranges cross block boundaries at every knob
    /// setting, which is where a two-level iterator drops or repeats an entry.
    #[test]
    fn ranges_match_the_map(
        model in model(),
        knobs in knobs(),
        bounds in prop::collection::vec((key(), key()), 1..12),
    ) {
        let table = build_and_open(&knobs, &model)?;
        let mut iter = table.iter();

        for (a, b) in &bounds {
            let (low, high) = if a <= b { (a, b) } else { (b, a) };
            let expected: Vec<(Vec<u8>, Vec<u8>)> = model
                .range(low.clone()..=high.clone())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let mut ascending = Vec::new();
            iter.seek(low);
            while iter.valid() && iter.key() <= &high[..] {
                ascending.push((iter.key().to_vec(), iter.value().to_vec()));
                iter.next();
            }
            prop_assert_eq!(&ascending, &expected, "[{:?}, {:?}] forwards", low, high);

            let mut descending = Vec::new();
            iter.seek_for_prev(high);
            while iter.valid() && iter.key() >= &low[..] {
                descending.push((iter.key().to_vec(), iter.value().to_vec()));
                iter.prev();
            }
            descending.reverse();
            prop_assert_eq!(&descending, &expected, "[{:?}, {:?}] backwards", low, high);
        }
        iter.status().or_fail()?;
    }

    /// A cache in front of the table changes performance, never answers. Same map, same knobs,
    /// with and without: the two must read identically.
    #[test]
    fn a_cache_never_changes_an_answer(
        model in model(),
        knobs in knobs(),
        probes in prop::collection::vec(key(), 1..12),
    ) {
        let cached = build_and_open(&Knobs { cache: true, ..knobs.clone() }, &model)?;
        let uncached = build_and_open(&Knobs { cache: false, ..knobs }, &model)?;

        for probe in &probes {
            prop_assert_eq!(
                cached.get(probe).or_fail()?,
                uncached.get(probe).or_fail()?
            );
        }
        // Twice, so the second pass is served from a warm cache.
        for _ in 0..2 {
            let mut a = cached.iter();
            let mut b = uncached.iter();
            prop_assert_eq!(forward(&mut a), forward(&mut b));
        }
    }
}
