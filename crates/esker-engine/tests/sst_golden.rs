//! Golden sorted string tables, and what survives damage to them.
//!
//! `tests/golden/sst/*.sst` are frozen files: real tables, byte for byte. Two tests hold them
//! there — one rebuilds them from the same seeded generator and compares the bytes, the other
//! opens the *committed* file and reads every key out of it. The first alone would only prove
//! the writer is deterministic; the second alone would not notice the format drifting. A
//! change to either file is a format change: it needs an ADR and a format version bump, never
//! a re-bless.
//!
//! To add cases, extend `entries()` and re-generate with
//! `ESKER_BLESS=1 cargo test -p esker-engine --test sst_golden`. That is for *new* cases only.
//!
//! The third test is the one that matters most for `CLAUDE.md` invariant 2: it flips every
//! single byte of both files in turn and requires each flip to be either reported as an error
//! or read back as exactly the right data. Never a wrong answer, never a panic.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_base::rng::Pcg32;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Compression, StripSuffix};
use esker_engine::range_del::{RangeTombstone, RangeTombstones};
use esker_engine::sst::{TableBuilder, TableOptions, TableReader};

/// Seed of the generator behind every golden file. Changing it invalidates them.
const SEED: u64 = 0xE5CE_5551;

/// The footer's fixed size, mirrored from `esker_engine::format` so the corruption test can
/// point at the one region of a table that carries no checksum of its own.
const FOOTER_SIZE: usize = esker_engine::format::SST_FOOTER_SIZE;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sst")
}

/// The entries every golden table holds: deterministic, sorted, and deliberately awkward.
///
/// An empty key, keys sharing long prefixes (which is what the block's prefix compression is
/// for), keys full of `0xFF` (which is where a length-prefixed decoder goes wrong), empty
/// values, and values long enough to force several block cuts.
fn entries() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rng = Pcg32::from_seed(SEED);
    let mut map = std::collections::BTreeMap::new();

    // The empty key: sorts first, and is out of any prefix extractor's domain.
    map.insert(Vec::new(), b"the empty key".to_vec());

    // Long shared prefixes: 8 families of 8 keys differing only in the last two bytes.
    for family in 0..8u8 {
        for member in 0..8u8 {
            let mut key = vec![b'p'; 24];
            key[23] = member;
            key[22] = family;
            let mut value = vec![0u8; usize::try_from(rng.below(48)).unwrap()];
            rng.fill_bytes(&mut value);
            map.insert(key, value);
        }
    }

    // All-0xFF keys of every length up to 12, and keys that end in 0xFF.
    for len in 0..12usize {
        map.insert(vec![0xff; len], vec![0xfe; len]);
        let mut key = b"ff-".to_vec();
        key.extend(std::iter::repeat_n(0xffu8, len));
        map.insert(key, Vec::new());
    }

    // Random keys, to spread the filter out and cut more blocks.
    for _ in 0..48 {
        let mut key = vec![0u8; usize::try_from(rng.range_inclusive(9, 20)).unwrap()];
        rng.fill_bytes(&mut key);
        let mut value = vec![0u8; usize::try_from(rng.below(80)).unwrap()];
        rng.fill_bytes(&mut value);
        map.insert(key, value);
    }

    map.into_iter().collect()
}

/// The two shapes worth freezing: the defaults, and the other side of every knob.
fn cases() -> Vec<(&'static str, TableOptions, RangeTombstones)> {
    vec![
        // `docs/DESIGN.md` §14 defaults: 4 KiB blocks, restart every 16, lz4, bloom 10.
        (
            "default.sst",
            TableOptions::default(),
            RangeTombstones::new(),
        ),
        // Uncompressed, small blocks, restart every entry, and a prefix-extracted filter —
        // so the golden files between them cover both codecs and both filter shapes.
        (
            "plain-prefix.sst",
            TableOptions {
                block_size: 512,
                restart_interval: 1,
                compression: Compression::None,
                prefix_extractor: Some(Arc::new(StripSuffix::new(8))),
                ..TableOptions::default()
            },
            RangeTombstones::new(),
        ),
        // The same entries with range tombstones attached: the block
        // [ADR 0017](../../../docs/adr/0017-range-tombstones.md) adds, and the widened key
        // bounds that make it findable. A separate file rather than a field on the two above,
        // because those two are frozen and adding a block to them would be a format change to
        // bytes that have not changed.
        ("range-del.sst", TableOptions::default(), tombstones()),
    ]
}

/// The tombstones the third golden table carries.
///
/// Deliberately awkward: one starting at the empty key, two sharing a `begin` at different
/// sequence numbers (which is what makes the block's sort order load-bearing), and one whose
/// `end` reaches *above* every key in the table — the case that forces the bounds to widen.
fn tombstones() -> RangeTombstones {
    let comparator = esker_engine::dbformat::BytewiseComparator;
    let mut set = RangeTombstones::new();
    for (begin, end, seqno) in [
        (&b""[..], &b"a"[..], 100u64),
        (&b"key/0000"[..], &b"key/0100"[..], 200),
        (&b"key/0000"[..], &b"key/0100"[..], 300),
        (&b"z"[..], &[0xffu8; 16][..], 400),
    ] {
        set.push(RangeTombstone::new(begin, end, seqno), &comparator);
    }
    set
}

/// Builds one table and returns its bytes.
fn build(
    options: TableOptions,
    entries: &[(Vec<u8>, Vec<u8>)],
    tombstones: RangeTombstones,
) -> Vec<u8> {
    let fs = MemFileSystem::new();
    let mut builder = TableBuilder::new(options, fs.create(Path::new("/t.sst")).unwrap());
    builder.set_seqno_range(1, 4096);
    builder.set_range_tombstones(tombstones);
    for (key, value) in entries {
        builder.add(key, value).unwrap();
    }
    builder.finish().unwrap();
    fs.contents("/t.sst").unwrap()
}

/// Opens `bytes` as a table.
fn open(bytes: Vec<u8>, options: TableOptions) -> esker_engine::Result<TableReader> {
    let fs = MemFileSystem::new();
    fs.install("/t.sst", bytes).unwrap();
    TableReader::open(fs.open(Path::new("/t.sst")).unwrap(), 42, options, None)
}

/// The bytes of a table are the on-disk format. If this fails, the format changed.
#[test]
fn golden_bytes_are_frozen() {
    let entries = entries();
    let bless = std::env::var_os("ESKER_BLESS").is_some();
    if bless {
        std::fs::create_dir_all(golden_dir()).unwrap();
    }

    for (name, options, tombstones) in cases() {
        let built = build(options, &entries, tombstones);
        let path = golden_dir().join(name);
        if bless {
            std::fs::write(&path, &built).unwrap();
            continue;
        }
        let frozen = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; re-generate with ESKER_BLESS=1", path.display()));
        assert_eq!(
            built.len(),
            frozen.len(),
            "{name} is {} bytes but the frozen file is {}; that is a format change (ADR + \
             version bump), not a re-bless",
            built.len(),
            frozen.len()
        );
        if built != frozen {
            let at = built
                .iter()
                .zip(&frozen)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            panic!(
                "{name} differs from the frozen file at byte {at}: built {:#04x}, frozen \
                 {:#04x}. That is a format change (ADR + version bump), not a re-bless.",
                built[at], frozen[at]
            );
        }
    }
}

/// The committed bytes must still be readable by this build — which is the half that catches
/// a reader and writer drifting together.
#[test]
fn golden_files_read_back() {
    let entries = entries();
    for (name, options, tombstones) in cases() {
        let bytes = std::fs::read(golden_dir().join(name)).unwrap();
        let table = open(bytes, options.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));

        let props = table.properties();
        assert_eq!(props.entry_count, entries.len() as u64, "{name}");
        assert_eq!(props.smallest_key, entries[0].0, "{name}");
        assert_eq!(props.range_del_count, tombstones.len() as u64, "{name}");
        assert_eq!(table.range_tombstones(), &tombstones, "{name}");
        // The table's *properties* record its entries, tombstones or not: widening the bounds
        // to span a tombstone is the engine's job, in internal-key space, because the bounds a
        // reader picks files by are internal keys and building one here would mean
        // understanding a key — which `CLAUDE.md` invariant 7 forbids the SST layer
        // ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)). `db/flush.rs::widen` does it,
        // and `tests/range_del.rs` is where that is checked.
        assert_eq!(props.largest_key, entries[entries.len() - 1].0, "{name}");
        assert_eq!(props.smallest_seqno, 1, "{name}");
        assert_eq!(props.largest_seqno, 4096, "{name}");
        assert_eq!(props.compression, options.compression, "{name}");
        assert_eq!(
            props.prefix_extractor_name,
            options
                .prefix_extractor
                .as_ref()
                .map(|extractor| extractor.name().to_string()),
            "{name}"
        );
        assert!(table.has_filter(), "{name}: the filter was not usable");

        for (key, value) in &entries {
            assert_eq!(
                table.get(key).unwrap().as_ref(),
                Some(value),
                "{name}: get {key:?}"
            );
        }

        let mut iter = table.iter();
        let mut forward = Vec::new();
        iter.seek_to_first();
        while iter.valid() {
            forward.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.next();
        }
        iter.status().unwrap();
        assert_eq!(forward, entries, "{name}: forward scan");

        let mut backward = Vec::new();
        iter.seek_to_last();
        while iter.valid() {
            backward.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.prev();
        }
        iter.status().unwrap();
        backward.reverse();
        assert_eq!(backward, entries, "{name}: reverse scan");
    }
}

/// Invariant 2, mechanised: every single byte of a table, flipped in turn, must be either
/// reported as an error or read back as exactly the right data.
///
/// A flip that lands in the footer's zero padding is genuinely harmless, so "not detected" is
/// a legal outcome — but only together with completely correct data. What this test exists to
/// rule out is the third possibility: a table that opens, reads without complaint, and returns
/// something that was never written.
#[test]
fn flipping_any_byte_is_detected_or_harmless() {
    let entries = entries();
    for (name, options, _) in cases() {
        let original = std::fs::read(golden_dir().join(name)).unwrap();
        let mut detected = 0usize;
        let mut harmless = Vec::new();

        for position in 0..original.len() {
            let mut corrupt = original.clone();
            corrupt[position] ^= 0xff;

            let Ok(table) = open(corrupt, options.clone()) else {
                detected += 1;
                continue;
            };

            // Read everything the table claims to hold. Any error is detection; anything that
            // comes back has to be exactly what was written.
            let mut scanned = Vec::new();
            let mut iter = table.iter();
            iter.seek_to_first();
            while iter.valid() {
                scanned.push((iter.key().to_vec(), iter.value().to_vec()));
                iter.next();
            }
            if iter.status().is_err() {
                detected += 1;
                continue;
            }

            let mut failed = false;
            for (key, value) in &entries {
                match table.get(key) {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(found) => assert_eq!(
                        found.as_ref(),
                        Some(value),
                        "{name}: flipping byte {position} made get({key:?}) return the wrong \
                         value instead of an error"
                    ),
                }
            }
            if failed {
                detected += 1;
                continue;
            }

            assert_eq!(
                scanned, entries,
                "{name}: flipping byte {position} changed what the table scans to, without \
                 reporting an error"
            );
            harmless.push(position);
        }

        assert_eq!(
            detected + harmless.len(),
            original.len(),
            "{name}: not every byte was accounted for"
        );

        // Every byte a flip can leave harmless is footer padding, and nothing else. The footer
        // is the one structure with no checksum of its own, and its handle region is
        // deliberately over-sized so the three handles always fit (see `sst::footer`); the
        // bytes past them are read by nobody. Anything undetected *outside* that window would
        // mean a checksum stopped being verified, so the window is asserted, not a count.
        let padding = (original.len() - FOOTER_SIZE)..(original.len() - FOOTER_SIZE + 35);
        for position in &harmless {
            assert!(
                padding.contains(position),
                "{name}: flipping byte {position} was not detected and did not change the \
                 data, but it is outside the footer's padding ({padding:?}) -- some checksum \
                 is no longer being verified"
            );
        }
        assert!(
            harmless.len() < 35,
            "{name}: {} undetected flips is more padding than the footer has",
            harmless.len()
        );
        assert!(
            detected > original.len() / 2,
            "{name}: only {detected} detected"
        );
    }
}

/// Truncating a golden table at every offset must never produce a readable table that has
/// silently lost entries.
#[test]
fn truncating_at_any_offset_is_detected_or_complete() {
    let entries = entries();
    for (name, options, _) in cases() {
        let original = std::fs::read(golden_dir().join(name)).unwrap();
        for cut in 0..original.len() {
            let Ok(table) = open(original[..cut].to_vec(), options.clone()) else {
                continue;
            };
            let mut scanned = 0usize;
            let mut iter = table.iter();
            iter.seek_to_first();
            while iter.valid() {
                scanned += 1;
                iter.next();
            }
            if iter.status().is_ok() {
                assert_eq!(
                    scanned,
                    entries.len(),
                    "{name}: truncating to {cut} bytes read cleanly but lost entries"
                );
            }
        }
    }
}
