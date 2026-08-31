//! Arbitrary bytes into every decode entry point. No panic, ever.
//!
//! `CLAUDE.md` invariant 9 is the requirement: nothing panics on user input or on-disk data. That
//! is a statement about *every* byte sequence, not about the ones a test author thought of, so it
//! is checked the only way such a statement can be — by generating byte sequences nobody chose.
//!
//! Two campaigns, because they reach different code:
//!
//! * **Blind.** Random bytes straight into [`Trailer::decode`], [`Footer::decode`],
//!   `decode_chunk` and `decode_column`, at every type and every encoding tag. Cheap, and it
//!   covers the outermost checks thoroughly.
//! * **Structure-aware.** A *valid* file, mutated — bits flipped, regions zeroed, lengths
//!   rewritten, bytes spliced from elsewhere in the same file — then opened and read to the end.
//!   This is the campaign that matters: blind bytes bounce off the trailing magic and never
//!   reach a bit-packed integer run or a dictionary code, and those are where a decoder that
//!   trusts a length would actually fail.
//!
//! # The other half: memory
//!
//! A decoder that does not panic but allocates whatever a corrupt length asks for has failed just
//! as completely — the process dies, it simply dies elsewhere. Every count in this format is read
//! through a cursor that refuses one whose items cannot fit in the bytes behind it, every chunk
//! length is validated against the file's own size when it is opened, and an LZ4 payload's stored
//! size is checked against what its compressed bytes could plausibly produce. The campaign below
//! would not survive without those; it is as much a test of them as of the panics.
//!
//! # Running it for longer
//!
//! The committed budget is small so that `just check` stays fast. `ESKER_FUZZ_SECONDS=60 cargo
//! test -p esker-columnar --test fuzz_decode -- --nocapture` runs a real campaign and prints how
//! many cases it got through. The generator is a seeded [`Pcg32`], so a failure found that way
//! reproduces from the seed it prints.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_columnar::encode::{Encoding, decode_column};
use esker_columnar::frame::decode_chunk;
use esker_columnar::{ColumnType, Footer, Reader, Trailer, Value, Writer, WriterOptions};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use proptest::prelude::*;

#[path = "corpus.rs"]
mod corpus;

/// Seed of the mutation campaign. Printed on failure so a case reproduces.
const SEED: u64 = 0xF0_0D_BE_EF;

/// Rows in the file the structure-aware campaign mutates. Small enough that a mutation lands
/// somewhere interesting rather than in the middle of a long incompressible run.
const ROWS: usize = 200;

fn budget() -> Duration {
    let seconds = std::env::var("ESKER_FUZZ_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2);
    Duration::from_secs(seconds)
}

/// A valid file's bytes, to be mutated.
fn valid_file() -> Vec<u8> {
    let fs = MemFileSystem::new();
    let path = Path::new("/f/valid.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let options = WriterOptions {
        stripe_rows: 48,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, corpus::schema(), options).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    writer.finish().unwrap();
    fs.contents(path).unwrap()
}

fn installed(bytes: Vec<u8>) -> (MemFileSystem, PathBuf) {
    let fs = MemFileSystem::new();
    let path = PathBuf::from("/f/case.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    fs.install(&path, bytes).unwrap();
    (fs, path)
}

/// Opens and reads everything, and checks that whatever comes back is self-consistent.
///
/// A mutated file is allowed to be rejected at any point. What it is not allowed to do is answer
/// with a column that does not match the stripe that named it — a decoder that returned 40 rows
/// where the footer said 48 would produce a row set nothing would ever flag.
fn open_and_read(fs: &MemFileSystem, path: &Path) {
    let Ok(reader) = Reader::open(fs, path) else {
        return;
    };
    for (index, stripe) in reader.stripes().iter().enumerate() {
        for column in 0..reader.schema().len() {
            if let Ok(values) = reader.read_column(index, column) {
                assert_eq!(
                    values.rows() as u64,
                    stripe.rows,
                    "stripe {index} column {column} decoded a different number of rows than \
                     the footer claims"
                );
                assert_eq!(values.ty(), reader.schema().columns()[column].ty);
                // Walking it is what proves the offsets inside are consistent.
                let seen = values.iter().count();
                assert_eq!(seen, values.rows());
                let _ = values.to_values();
            }
        }
    }
}

/// One mutation of `bytes`, chosen by `rng`.
fn mutate(rng: &mut Pcg32, bytes: &mut Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    let len = bytes.len();
    match rng.below(7) {
        // A flipped bit.
        0 => {
            let at = rng.below(len as u32) as usize;
            bytes[at] ^= 1 << rng.below(8);
        }
        // A byte replaced outright, which reaches tags and widths a bit flip rarely does.
        1 => {
            let at = rng.below(len as u32) as usize;
            bytes[at] = rng.below(256) as u8;
        }
        // A short run of noise: several fields at once.
        2 => {
            let at = rng.below(len as u32) as usize;
            let run = (rng.below(16) as usize + 1).min(len - at);
            rng.fill_bytes(&mut bytes[at..at + run]);
        }
        // A region zeroed, which is what a lost disk sector looks like.
        3 => {
            let at = rng.below(len as u32) as usize;
            let run = (rng.below(64) as usize + 1).min(len - at);
            bytes[at..at + run].fill(0);
        }
        // A varint rewritten to something enormous: the shape that becomes an allocation.
        4 => {
            let at = rng.below(len as u32) as usize;
            let run = 10.min(len - at);
            for byte in &mut bytes[at..at + run - 1] {
                *byte = 0xff;
            }
            if run > 0 {
                bytes[at + run - 1] = 0x7f;
            }
        }
        // Bytes copied from elsewhere in the same file: a plausible but misplaced region.
        5 => {
            let from = rng.below(len as u32) as usize;
            let to = rng.below(len as u32) as usize;
            let run = (rng.below(48) as usize + 1).min(len - from).min(len - to);
            let slice = bytes[from..from + run].to_vec();
            bytes[to..to + run].copy_from_slice(&slice);
        }
        // Truncation, which the crash test covers exhaustively but which composes with the rest.
        _ => bytes.truncate(rng.below(len as u32) as usize),
    }
}

/// The campaign that reaches the deep decoders.
#[test]
fn mutating_a_valid_file_never_panics() {
    let good = valid_file();
    let deadline = Instant::now() + budget();
    let mut rng = Pcg32::from_seed(SEED);
    let mut cases = 0u64;

    while Instant::now() < deadline {
        for _ in 0..64 {
            let mut bytes = good.clone();
            // One to four mutations: one finds the shallow checks, several compose into files
            // whose footer is valid and whose chunks are not.
            for _ in 0..=rng.below(4) {
                mutate(&mut rng, &mut bytes);
            }
            let (fs, path) = installed(bytes);
            open_and_read(&fs, &path);
            cases += 1;
        }
    }
    println!("mutating a valid file: {cases} cases from seed {SEED:#x}");
    assert!(cases > 1000, "the campaign only managed {cases} cases");
}

/// The same, but starting from a file whose footer is deliberately left intact, so the mutations
/// land in the stripe data and every chunk decoder is entered with damaged bytes.
#[test]
fn mutating_only_the_stripe_data_never_panics() {
    let good = valid_file();
    let (fs, path) = installed(good.clone());
    let data_end = usize::try_from(
        Reader::open(&fs, &path)
            .unwrap()
            .stripes()
            .last()
            .map_or(0, |stripe| stripe.offset + stripe.len),
    )
    .unwrap();
    assert!(data_end > 0 && data_end < good.len());

    let deadline = Instant::now() + budget();
    let mut rng = Pcg32::from_seed(SEED ^ 0x5555);
    let mut cases = 0u64;

    while Instant::now() < deadline {
        for _ in 0..64 {
            let mut bytes = good.clone();
            let mut region = bytes[..data_end].to_vec();
            for _ in 0..=rng.below(3) {
                mutate(&mut rng, &mut region);
            }
            region.resize(data_end, 0);
            bytes[..data_end].copy_from_slice(&region);

            let (fs, path) = installed(bytes);
            open_and_read(&fs, &path);
            cases += 1;
        }
    }
    println!(
        "mutating stripe data only: {cases} cases from seed {:#x}",
        SEED ^ 0x5555
    );
    assert!(cases > 1000, "the campaign only managed {cases} cases");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Blind bytes into the two fixed layouts.
    #[test]
    fn arbitrary_bytes_into_the_trailer_and_footer(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = Trailer::decode(&bytes);
        let _ = Footer::decode(&bytes);
        // And at exactly the trailer's size, where the length check stops shielding the rest.
        let mut fixed = bytes.clone();
        fixed.resize(32, 0);
        let _ = Trailer::decode(&fixed);
    }

    /// Blind bytes into the chunk framing and into every column decoder.
    #[test]
    fn arbitrary_bytes_into_a_chunk(
        bytes in prop::collection::vec(any::<u8>(), 0..200),
        rows in 0u64..200,
        tag in any::<u8>(),
    ) {
        let _ = decode_chunk(&bytes, "fuzz");
        if let Some(encoding) = Encoding::from_u8(tag) {
            for ty in ColumnType::ALL {
                if let Ok(column) = decode_column(ty, rows, encoding, &bytes) {
                    prop_assert_eq!(column.rows() as u64, rows);
                    prop_assert_eq!(column.iter().count() as u64, rows);
                }
            }
        }
    }

    /// Blind bytes as a whole file, including ones that end in the magic.
    #[test]
    fn arbitrary_bytes_as_a_file(
        mut bytes in prop::collection::vec(any::<u8>(), 0..400),
        seal in any::<bool>(),
    ) {
        if seal && bytes.len() >= 8 {
            let at = bytes.len() - 8;
            bytes[at..].copy_from_slice(b"ESKERCOL");
        }
        let (fs, path) = installed(bytes);
        open_and_read(&fs, &path);
    }
}

/// A sanity check on the campaign itself: the unmutated file still reads, so a campaign that
/// found nothing was actually exercising a working decoder rather than an empty one.
#[test]
fn the_file_the_campaign_starts_from_is_valid() {
    let (fs, path) = installed(valid_file());
    let reader = Reader::open(&fs, &path).unwrap();
    assert_eq!(reader.rows() as usize, ROWS);

    let mut rows = Vec::new();
    let projection: Vec<usize> = (0..reader.schema().len()).collect();
    for (index, stripe) in reader.stripes().iter().enumerate() {
        let decoded: Vec<Vec<Value>> = reader
            .read_stripe(index, &projection)
            .unwrap()
            .iter()
            .map(|column| column.to_values().unwrap())
            .collect();
        for offset in 0..stripe.rows as usize {
            rows.push(
                decoded
                    .iter()
                    .map(|values| values[offset].clone())
                    .collect::<Vec<_>>(),
            );
        }
    }
    assert_eq!(rows, corpus::rows(ROWS));
}
