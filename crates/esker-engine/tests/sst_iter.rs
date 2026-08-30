//! Iterator edges: the ends of a table, and every boundary between its blocks.
//!
//! A two-level iterator is mostly correct in the middle of a block and wrong at its seams, so
//! these tests spend their effort there. Rather than guess where a block was cut, they check
//! *every* position — for each entry in turn, the cursor's neighbours must be the map's
//! neighbours — which covers every seam whatever the block size did.
//!
//! The other edges are the ones with no entry on the far side: before the first key, past the
//! last, a table with one entry, and a table with none.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Compression, StripSuffix};
use esker_engine::sst::{TableBuilder, TableIter, TableOptions, TableReader};

/// Entries whose keys are 6 bytes and values 10, so a 64-byte block holds a handful and a
/// table of 400 has dozens of seams.
fn entries(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|i| {
            (
                format!("k{i:05}").into_bytes(),
                format!("v{i:09}").into_bytes(),
            )
        })
        .collect()
}

fn open(options: TableOptions, entries: &[(Vec<u8>, Vec<u8>)]) -> TableReader {
    let fs = MemFileSystem::new();
    let mut builder = TableBuilder::new(options.clone(), fs.create(Path::new("/t.sst")).unwrap());
    for (key, value) in entries {
        builder.add(key, value).unwrap();
    }
    builder.finish().unwrap();
    TableReader::open(fs.open(Path::new("/t.sst")).unwrap(), 3, options, None).unwrap()
}

/// Every shape worth walking a seam in: tiny and large blocks, restart intervals above and
/// below the entries a block holds, both codecs, and a filter over prefixes.
fn shapes() -> Vec<(&'static str, TableOptions)> {
    vec![
        (
            "one entry per block",
            TableOptions {
                block_size: 1,
                restart_interval: 16,
                ..TableOptions::default()
            },
        ),
        (
            "restart every entry",
            TableOptions {
                block_size: 64,
                restart_interval: 1,
                ..TableOptions::default()
            },
        ),
        (
            "restart interval past the block",
            TableOptions {
                block_size: 64,
                restart_interval: 1_000,
                compression: Compression::None,
                ..TableOptions::default()
            },
        ),
        (
            "prefix filter, small blocks",
            TableOptions {
                block_size: 128,
                restart_interval: 4,
                prefix_extractor: Some(Arc::new(StripSuffix::new(2))),
                ..TableOptions::default()
            },
        ),
        ("defaults", TableOptions::default()),
    ]
}

/// Walks the cursor to `index` and returns what it sees.
fn key_at(iter: &TableIter) -> Option<Vec<u8>> {
    iter.valid().then(|| iter.key().to_vec())
}

/// At every position in the table, `next` and `prev` must agree with the entry list — which
/// means every block seam is crossed in both directions.
#[test]
fn every_position_has_the_right_neighbours() {
    let entries = entries(400);
    for (name, options) in shapes() {
        let table = open(options, &entries);
        let blocks = table.properties().data_block_count;
        assert!(blocks >= 1, "{name}");

        let mut iter = table.iter();
        for (i, (key, value)) in entries.iter().enumerate() {
            iter.seek(key);
            assert!(iter.valid(), "{name}: seek to entry {i}");
            assert_eq!(iter.key(), &key[..], "{name}: entry {i}");
            assert_eq!(iter.value(), &value[..], "{name}: entry {i}");

            iter.next();
            assert_eq!(
                key_at(&iter),
                entries.get(i + 1).map(|(k, _)| k.clone()),
                "{name}: next from entry {i} of {blocks} blocks"
            );

            // Back to i, then back again to i - 1.
            iter.seek(key);
            iter.prev();
            assert_eq!(
                key_at(&iter),
                i.checked_sub(1).map(|j| entries[j].0.clone()),
                "{name}: prev from entry {i}"
            );
        }
        iter.status().unwrap();
    }
}

/// Crossing a seam and immediately turning round must land back where it started. This is the
/// move that a two-level iterator gets wrong by forgetting which block it was in.
///
/// The exception is turning round at an end: a cursor that steps off the front or the back is
/// off, and `next`/`prev` on an invalid cursor are no-ops rather than a way back on. Getting
/// back on takes a seek, which is `LevelDB`'s rule and the one the merge iterator expects. So
/// the first entry does not survive a `prev` then `next`, and the last does not survive a
/// `next` then `prev`; every entry between them survives both.
#[test]
fn turning_round_at_every_position_returns_to_it() {
    let entries = entries(300);
    for (name, options) in shapes() {
        let table = open(options, &entries);
        let mut iter = table.iter();
        for (i, (key, _)) in entries.iter().enumerate() {
            iter.seek(key);
            iter.next();
            iter.prev();
            // Symmetrically: stepping off the back leaves the cursor off, so only positions
            // with something after them return.
            let expected = (i + 1 < entries.len()).then(|| key.clone());
            assert_eq!(key_at(&iter), expected, "{name}: next/prev at {i}");

            iter.seek(key);
            iter.prev();
            iter.next();
            // Stepping off the front leaves the cursor off: `next` on an invalid cursor is a
            // no-op, not a way back on. So only positions with something before them return.
            let expected = (i > 0).then(|| key.clone());
            assert_eq!(key_at(&iter), expected, "{name}: prev/next at {i}");
        }
        iter.status().unwrap();
    }
}

/// The ends: before the first key, past the last, and stepping off either one and back.
#[test]
fn the_ends_of_the_table() {
    let entries = entries(200);
    let first = entries[0].0.clone();
    let last = entries[199].0.clone();

    for (name, options) in shapes() {
        let table = open(options, &entries);
        let mut iter = table.iter();

        iter.seek_to_first();
        assert_eq!(iter.key(), &first[..], "{name}");
        iter.prev();
        assert!(!iter.valid(), "{name}: prev off the front");
        iter.prev();
        assert!(!iter.valid(), "{name}: prev on an invalid cursor");
        iter.seek_to_first();
        assert_eq!(iter.key(), &first[..], "{name}: recovers after running off");

        iter.seek_to_last();
        assert_eq!(iter.key(), &last[..], "{name}");
        iter.next();
        assert!(!iter.valid(), "{name}: next off the back");
        iter.next();
        assert!(!iter.valid(), "{name}: next on an invalid cursor");
        iter.seek_to_last();
        assert_eq!(iter.key(), &last[..], "{name}: recovers after running off");

        // A target before every key, and after every key.
        iter.seek(b"a");
        assert_eq!(iter.key(), &first[..], "{name}: seek before the first");
        iter.seek_for_prev(b"a");
        assert!(!iter.valid(), "{name}: nothing at or before the first");
        iter.seek(b"zzzzzz");
        assert!(!iter.valid(), "{name}: nothing at or after the last");
        iter.seek_for_prev(b"zzzzzz");
        assert_eq!(iter.key(), &last[..], "{name}: seek_for_prev past the last");

        iter.status().unwrap();
    }
}

/// One entry is simultaneously the first, the last, and the whole of its only block.
#[test]
fn a_single_entry_table_at_every_shape() {
    let entries = vec![(b"solo".to_vec(), b"value".to_vec())];
    for (name, options) in shapes() {
        let table = open(options, &entries);
        assert_eq!(table.properties().data_block_count, 1, "{name}");

        let mut iter = table.iter();
        iter.seek_to_first();
        assert_eq!(iter.key(), b"solo", "{name}");
        iter.next();
        assert!(!iter.valid(), "{name}");
        iter.seek_to_last();
        assert_eq!(iter.key(), b"solo", "{name}");
        iter.prev();
        assert!(!iter.valid(), "{name}");

        iter.seek(b"solo");
        assert_eq!(iter.key(), b"solo", "{name}");
        iter.seek(b"sola");
        assert_eq!(iter.key(), b"solo", "{name}: before it");
        iter.seek(b"solz");
        assert!(!iter.valid(), "{name}: after it");
        iter.seek_for_prev(b"sola");
        assert!(!iter.valid(), "{name}");
        iter.seek_for_prev(b"solz");
        assert_eq!(iter.key(), b"solo", "{name}");
        iter.status().unwrap();
    }
}

/// A table with no entries is legal, and every cursor operation on it is well defined: it is
/// invalid, and it is not an error.
#[test]
fn an_empty_table_at_every_shape() {
    for (name, options) in shapes() {
        let table = open(options, &[]);
        assert_eq!(table.properties().entry_count, 0, "{name}");
        assert_eq!(table.properties().data_block_count, 0, "{name}");
        assert_eq!(table.get(b"anything").unwrap(), None, "{name}");

        let mut iter = table.iter();
        for _ in 0..2 {
            iter.seek_to_first();
            assert!(!iter.valid(), "{name}");
            iter.seek_to_last();
            assert!(!iter.valid(), "{name}");
            iter.seek(b"k");
            assert!(!iter.valid(), "{name}");
            iter.seek_for_prev(b"k");
            assert!(!iter.valid(), "{name}");
            iter.next();
            iter.prev();
            assert!(!iter.valid(), "{name}");
        }
        iter.status().unwrap();
    }
}

/// A cursor owns a share of the table, so the merge iterator above can keep one alive after
/// dropping the reader it came from.
#[test]
fn a_cursor_outlives_the_reader_it_came_from() {
    let entries = entries(300);
    let mut iter = {
        let table = open(
            TableOptions {
                block_size: 64,
                ..TableOptions::default()
            },
            &entries,
        );
        table.iter()
    };

    let mut seen = Vec::new();
    iter.seek_to_first();
    while iter.valid() {
        seen.push((iter.key().to_vec(), iter.value().to_vec()));
        iter.next();
    }
    iter.status().unwrap();
    assert_eq!(seen, entries);
}
