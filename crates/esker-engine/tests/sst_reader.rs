//! Opening a table, and reading one back: the reader's own behaviour.
//!
//! The two checks `TableReader::open` makes are the point of most of this file. A comparator
//! mismatch is refused, because a table sorted one way and searched another silently misses
//! keys rather than erroring. A prefix-extractor mismatch instead disables the filter, because
//! a filter built over different bytes than the probe uses reports present keys as absent —
//! and losing a key is worse than reading a block.
//!
//! The rest is the two-level iterator: crossing block seams in both directions, landing on the
//! right side of a gap, and the ends of a table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::cache::ShardedLruCache;
use esker_engine::cache_api::BlockCache;
use esker_engine::dbformat::Comparator;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Compression, StripSuffix};
use esker_engine::sst::{TableBuilder, TableOptions};
use esker_engine::sst::{TableIter, TableReader};
use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;

/// Reverse bytewise order, to prove the reader takes its order from the comparator.
#[derive(Debug)]
struct ReverseComparator;

impl Comparator for ReverseComparator {
    fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        b.cmp(a)
    }
    fn name(&self) -> &'static str {
        "test.ReverseComparator"
    }
}

fn kv(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|i| {
            (
                format!("key{i:06}").into_bytes(),
                format!("value-{i}-{}", "p".repeat(i % 37)).into_bytes(),
            )
        })
        .collect()
}

/// Writes a table into a fresh in-memory filesystem and opens it again.
fn round_trip(
    write: TableOptions,
    read: TableOptions,
    entries: &[(Vec<u8>, Vec<u8>)],
    cache: Option<Arc<dyn BlockCache>>,
) -> esker_engine::Result<TableReader> {
    let fs = MemFileSystem::new();
    let mut builder = TableBuilder::new(write, fs.create(Path::new("/t.sst")).unwrap());
    for (key, value) in entries {
        builder.add(key, value).unwrap();
    }
    builder.finish().unwrap();
    TableReader::open(fs.open(Path::new("/t.sst")).unwrap(), 7, read, cache)
}

fn open_default(entries: &[(Vec<u8>, Vec<u8>)]) -> TableReader {
    round_trip(
        TableOptions::default(),
        TableOptions::default(),
        entries,
        None,
    )
    .expect("a table this writer built opens")
}

fn collect(iter: &mut TableIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        out.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    out
}

/// Every key written comes back, by `get` and by walking, in both directions, across many
/// data blocks.
#[test]
fn every_key_round_trips_across_block_boundaries() {
    let entries = kv(2_000);
    let table = open_default(&entries);
    assert!(
        table.properties().data_block_count > 10,
        "the test needs many blocks, got {}",
        table.properties().data_block_count
    );

    for (key, value) in &entries {
        assert_eq!(table.get(key).unwrap().as_ref(), Some(value), "get {key:?}");
    }
    assert_eq!(table.get(b"key999999").unwrap(), None);
    assert_eq!(table.get(b"").unwrap(), None);

    let mut iter = table.iter();
    assert_eq!(collect(&mut iter), entries);

    let mut backwards = Vec::new();
    iter.seek_to_last();
    while iter.valid() {
        backwards.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.prev();
    }
    backwards.reverse();
    assert_eq!(backwards, entries);
    iter.status().unwrap();
}

/// Seeking lands on the right entry even when the target falls between two data blocks,
/// which is where a two-level iterator goes wrong.
#[test]
fn seeking_between_blocks() {
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..600u32)
        .map(|i| (format!("k{:06}", i * 10).into_bytes(), b"v".to_vec()))
        .collect();
    let options = TableOptions {
        block_size: 128,
        ..TableOptions::default()
    };
    let table = round_trip(options.clone(), options, &entries, None).unwrap();
    assert!(table.properties().data_block_count > 30);

    let mut iter = table.iter();
    for (i, (key, _)) in entries.iter().enumerate() {
        // Exactly on a key.
        iter.seek(key);
        assert!(iter.valid());
        assert_eq!(iter.key(), &key[..], "seek to {key:?}");

        // Between this key and the next: k000005 sits between k000000 and k000010.
        let mut between = key.clone();
        between.extend_from_slice(b"5");
        iter.seek(&between);
        let expected = entries.get(i + 1).map(|(k, _)| k.clone());
        assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);

        iter.seek_for_prev(&between);
        assert!(iter.valid());
        assert_eq!(iter.key(), &key[..], "seek_for_prev past {key:?}");
    }

    iter.seek(b"a");
    assert_eq!(iter.key(), &entries[0].0[..], "before the first key");
    iter.seek_for_prev(b"a");
    assert!(!iter.valid(), "nothing at or before the first key");
    iter.seek(b"zzz");
    assert!(!iter.valid(), "past the last key");
    iter.seek_for_prev(b"zzz");
    assert_eq!(iter.key(), &entries[599].0[..]);
    iter.status().unwrap();
}

/// An empty table opens, reads as empty, and does not panic on any cursor operation.
#[test]
fn an_empty_table_reads_as_empty() {
    let table = open_default(&[]);
    assert_eq!(table.properties().entry_count, 0);
    assert!(!table.has_filter());
    assert_eq!(table.get(b"anything").unwrap(), None);

    let mut iter = table.iter();
    assert_eq!(collect(&mut iter), vec![]);
    iter.seek_to_last();
    assert!(!iter.valid());
    iter.seek(b"k");
    assert!(!iter.valid());
    iter.seek_for_prev(b"k");
    assert!(!iter.valid());
    iter.next();
    iter.prev();
    assert!(!iter.valid());
    iter.status().unwrap();
}

/// A single entry is the first, the last, and the whole of its only block.
#[test]
fn a_single_entry_table() {
    let entries = vec![(b"only".to_vec(), b"value".to_vec())];
    let table = open_default(&entries);
    assert_eq!(table.get(b"only").unwrap().as_deref(), Some(&b"value"[..]));
    assert_eq!(table.get(b"onlz").unwrap(), None);
    assert_eq!(table.get(b"onlx").unwrap(), None);

    let mut iter = table.iter();
    assert_eq!(collect(&mut iter), entries);
    iter.seek_to_last();
    assert_eq!(iter.key(), b"only");
    iter.next();
    assert!(!iter.valid());
    iter.prev();
    assert!(!iter.valid(), "prev from invalid must stay invalid");
}

/// Order comes from the injected comparator, not from `memcmp`, all the way through the
/// reader.
#[test]
fn a_reverse_comparator_is_honoured_end_to_end() {
    let options = TableOptions {
        comparator: Arc::new(ReverseComparator),
        block_size: 64,
        ..TableOptions::default()
    };
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..200u32)
        .rev()
        .map(|i| (format!("k{i:04}").into_bytes(), b"v".to_vec()))
        .collect();
    let table = round_trip(options.clone(), options, &entries, None).unwrap();

    for (key, _) in &entries {
        assert!(table.get(key).unwrap().is_some(), "get {key:?}");
    }
    let mut iter = table.iter();
    assert_eq!(
        collect(&mut iter),
        entries,
        "iteration follows reverse order"
    );
}

/// A table read with the wrong comparator would silently miss keys, so opening it fails.
#[test]
fn a_comparator_mismatch_is_refused_at_open() {
    let error = round_trip(
        TableOptions::default(),
        TableOptions {
            comparator: Arc::new(ReverseComparator),
            ..TableOptions::default()
        },
        &kv(20),
        None,
    )
    .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("comparator"), "{text}");
    assert!(text.contains("wrong order"), "{text}");
}

/// The unforgivable bloom bug, at the table level: a filter built over whole keys and a
/// reader configured with a prefix extractor (or the reverse) must not be used, and every
/// key must still be found.
#[test]
fn a_prefix_extractor_mismatch_disables_the_filter_rather_than_losing_keys() {
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..500u32)
        .map(|i| (format!("k{i:04}ts{i:04}").into_bytes(), b"v".to_vec()))
        .collect();
    let with = TableOptions {
        prefix_extractor: Some(Arc::new(StripSuffix::new(6))),
        ..TableOptions::default()
    };
    let without = TableOptions::default();

    for (write, read) in [
        (with.clone(), without.clone()),
        (without.clone(), with.clone()),
        (
            with.clone(),
            TableOptions {
                prefix_extractor: Some(Arc::new(StripSuffix::new(4))),
                ..TableOptions::default()
            },
        ),
    ] {
        let table = round_trip(write, read, &entries, None).unwrap();
        assert!(
            !table.has_filter(),
            "a filter built over different bytes was kept"
        );
        for (key, value) in &entries {
            assert_eq!(
                table.get(key).unwrap().as_ref(),
                Some(value),
                "mismatched filter lost {key:?}"
            );
        }
    }

    // Matching extractors keep the filter, and it still finds every key.
    let table = round_trip(with.clone(), with, &entries, None).unwrap();
    assert!(table.has_filter());
    for (key, value) in &entries {
        assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
    }
}

/// The filter's job: an absent key must usually be answered without reading a block.
#[test]
fn the_filter_answers_absent_keys_without_reading_a_block() {
    let entries = kv(2_000);
    let cache = Arc::new(ShardedLruCache::new(8 * 1024 * 1024));
    let table = round_trip(
        TableOptions::default(),
        TableOptions::default(),
        &entries,
        Some(cache.clone()),
    )
    .unwrap();
    assert!(table.has_filter());

    let before = cache.stats();
    for i in 0..2_000u32 {
        assert_eq!(table.get(format!("absent{i:06}").as_bytes()).unwrap(), None);
    }
    let after = cache.stats();
    let block_reads = (after.hits + after.misses) - (before.hits + before.misses);
    assert!(
        block_reads * 50 < 2_000,
        "{block_reads} block lookups for 2000 absent keys; the filter is not being used"
    );
}

/// A cache in front of the file must be filled and then hit, and the table must read the
/// same with a cache as without one.
#[test]
fn the_block_cache_is_filled_and_then_hit() {
    let entries = kv(500);
    let cache = Arc::new(ShardedLruCache::new(4 * 1024 * 1024));
    let table = round_trip(
        TableOptions::default(),
        TableOptions::default(),
        &entries,
        Some(cache.clone()),
    )
    .unwrap();

    assert_eq!(
        cache.stats().entries,
        0,
        "opening should not fill the cache"
    );
    for (key, value) in &entries {
        assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
    }
    let warm = cache.stats();
    assert!(warm.entries > 0, "no data block was cached");
    assert!(warm.misses > 0);

    let before_hits = warm.hits;
    for (key, value) in &entries {
        assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
    }
    let after = cache.stats();
    assert!(
        after.hits > before_hits,
        "a second pass read nothing from the cache"
    );
    assert_eq!(after.misses, warm.misses, "a warm cache still missed");
}

/// Compression is invisible above the block trailer: the same entries come back either way.
#[test]
fn both_codecs_read_the_same() {
    let entries = kv(400);
    for compression in [Compression::None, Compression::Lz4] {
        let options = TableOptions {
            compression,
            ..TableOptions::default()
        };
        let table = round_trip(options.clone(), options, &entries, None).unwrap();
        let mut iter = table.iter();
        assert_eq!(collect(&mut iter), entries, "{compression:?}");
    }
}

/// Files that are not tables must be named as such rather than parsed into nonsense.
#[test]
fn files_that_are_not_tables_are_refused() {
    let fs = MemFileSystem::new();
    let open = |name: &str| {
        TableReader::open(
            fs.open(Path::new(name)).unwrap(),
            1,
            TableOptions::default(),
            None,
        )
    };

    fs.install("/empty.sst", Vec::new()).unwrap();
    assert!(open("/empty.sst").is_err());

    fs.install("/short.sst", vec![0u8; 47]).unwrap();
    assert!(open("/short.sst").is_err());

    fs.install("/garbage.sst", vec![0xab; 4096]).unwrap();
    assert!(open("/garbage.sst").is_err());

    // A real table with its magic rewritten.
    let mut builder = TableBuilder::new(
        TableOptions::default(),
        fs.create(Path::new("/good.sst")).unwrap(),
    );
    builder.add(b"k", b"v").unwrap();
    builder.finish().unwrap();
    let mut bytes = fs.contents("/good.sst").unwrap();
    let last = bytes.len() - 1;
    bytes[last] = b'9';
    fs.install("/bad-magic.sst", bytes).unwrap();
    let error = open("/bad-magic.sst").unwrap_err();
    assert!(error.is_corruption(), "{error}");
}

/// Truncating a table at any point must be an error at open or at read, never a wrong
/// answer. The footer is last, so most truncations lose it outright.
#[test]
fn truncation_is_detected() {
    let fs = MemFileSystem::new();
    let mut builder = TableBuilder::new(
        TableOptions {
            block_size: 128,
            ..TableOptions::default()
        },
        fs.create(Path::new("/t.sst")).unwrap(),
    );
    let entries = kv(200);
    for (key, value) in &entries {
        builder.add(key, value).unwrap();
    }
    builder.finish().unwrap();
    let full = fs.contents("/t.sst").unwrap();

    for cut in (1..full.len()).step_by(7) {
        fs.install("/cut.sst", full[..cut].to_vec()).unwrap();
        let opened = TableReader::open(
            fs.open(Path::new("/cut.sst")).unwrap(),
            2,
            TableOptions::default(),
            None,
        );
        let Ok(table) = opened else { continue };
        // Opening a truncated file can only succeed if the footer survived, which means
        // the file is intact; anything else must fail when the missing bytes are needed.
        let mut iter = table.iter();
        let mut seen = 0;
        iter.seek_to_first();
        while iter.valid() {
            seen += 1;
            iter.next();
        }
        if iter.status().is_ok() {
            assert_eq!(
                seen,
                entries.len(),
                "a truncation at {cut} read cleanly but lost entries"
            );
        }
    }
}

/// **A block handle read off disk is a length, and a length from a damaged file is untrusted.**
///
/// The footer carries three of them — index, filter, properties — as plain varints, so a damaged
/// tail produces handles that are perfectly well-formed and point anywhere at all. The reader
/// refuses one that does not fit in the file rather than sizing an allocation from it or slicing
/// past the end (`CLAUDE.md` invariants 2 and 9), and until now nothing asked it to: the footer's
/// own magic and version had tests, and the numbers behind them did not.
///
/// Hand-built rather than truncated. Truncation loses the footer and is caught by the magic, which
/// is a different refusal in a different place; this leaves the footer intact and valid and moves
/// only the index handle, so what is under test is the bounds check and nothing else.
#[test]
fn a_block_handle_pointing_past_the_file_is_refused() {
    use esker_engine::sst::footer::{BlockHandle, Footer};

    // The footer is fixed-width and its size is private, so this is what the reader itself does:
    // take the tail, and let `Footer::decode` say whether it is one.
    const FOOTER_SIZE: usize = 48;

    let fs = MemFileSystem::new();
    let open = |name: &str| {
        TableReader::open(
            fs.open(Path::new(name)).unwrap(),
            1,
            TableOptions::default(),
            None,
        )
    };
    let mut builder = TableBuilder::new(
        TableOptions::default(),
        fs.create(Path::new("/good.sst")).unwrap(),
    );
    builder.add(b"k", b"v").unwrap();
    builder.finish().unwrap();

    let good = fs.contents("/good.sst").unwrap();
    let split = good.len() - FOOTER_SIZE;
    let footer = Footer::decode(&good[split..]).expect("the table this test damages must be good");

    // Everything as written, except an index block that begins a megabyte past the end of a
    // file of a few hundred bytes.
    let moved = Footer {
        index: BlockHandle {
            offset: good.len() as u64 + 1_048_576,
            size: footer.index.size,
        },
        ..footer
    };
    let mut damaged = good[..split].to_vec();
    damaged.extend_from_slice(&moved.encode().expect("a footer of three handles encodes"));
    assert_eq!(
        damaged.len(),
        good.len(),
        "the footer is fixed-width, so damaging it must not change the file's length"
    );

    fs.install("/moved-index.sst", damaged).unwrap();
    let error = open("/moved-index.sst")
        .err()
        .expect("a table whose index block is outside the file must not open");
    assert!(
        error.is_corruption(),
        "a handle past the end of the file is corruption, not an I/O error or a panic: {error}"
    );
}
