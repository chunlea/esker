//! Frozen columnar files, byte for byte.
//!
//! `tests/golden/*.col` are real files produced by the writer from the deterministic corpus in
//! `tests/corpus.rs`. Three tests hold them there, and each one alone would be insufficient:
//!
//! * **Rebuild and compare.** The writer, run again on the same rows, must produce the same
//!   bytes. On its own this only proves the writer is deterministic.
//! * **Read the committed file.** The bytes on disk — not the bytes just produced — must still
//!   decode to exactly the right rows. On its own this would not notice the writer drifting.
//! * **Flip every byte.** Every single-byte change to either file must be *detected*. Not
//!   "detected or harmless": this format checksums every region, so a flip that reads back
//!   successfully means a region nothing covers.
//!
//! A change to either file is a format change: it needs an ADR and a version bump, never a
//! re-bless. `ESKER_BLESS=1 cargo test -p esker-columnar --test golden` regenerates them, and
//! that is for *new* cases only.
//!
//! The LZ4 file also pins `lz4_flex`'s output, so a dependency bump that changes its bytes fails
//! here. That is deliberate: the compressed form is what is on disk, and finding out that it
//! moved is worth one failing test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use esker_columnar::{Compression, Reader, Value, Writer, WriterOptions};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

#[path = "corpus.rs"]
mod corpus;

/// Rows in every golden file, and the stripe size that cuts them into four.
const ROWS: usize = 500;
const STRIPE_ROWS: usize = 128;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The two files, by name and by the codec that produced them.
fn cases() -> [(&'static str, Compression); 2] {
    [
        ("ledger-plain.col", Compression::None),
        ("ledger-lz4.col", Compression::Lz4),
    ]
}

/// Builds one golden file's bytes from the corpus.
fn build(compression: Compression) -> Vec<u8> {
    let fs = MemFileSystem::new();
    let path = Path::new("/g/out.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let options = WriterOptions {
        stripe_rows: STRIPE_ROWS,
        compression,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, corpus::schema(), options).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    let summary = writer.finish().unwrap();
    assert_eq!(summary.rows as usize, ROWS);
    assert_eq!(summary.stripes, ROWS.div_ceil(STRIPE_ROWS));
    fs.contents(path).unwrap()
}

/// Every row of a file at `path`, in order.
fn read_rows(fs: &MemFileSystem, path: &Path) -> Vec<Vec<Value>> {
    let reader = Reader::open(fs, path).unwrap();
    let projection: Vec<usize> = (0..reader.schema().len()).collect();
    let mut rows = Vec::new();
    for (index, stripe) in reader.stripes().iter().enumerate() {
        let decoded: Vec<Vec<Value>> = reader
            .read_stripe(index, &projection)
            .unwrap()
            .iter()
            .map(|column| column.to_values().unwrap())
            .collect();
        for offset in 0..usize::try_from(stripe.rows).unwrap() {
            rows.push(
                decoded
                    .iter()
                    .map(|values| values[offset].clone())
                    .collect(),
            );
        }
    }
    rows
}

fn installed(bytes: Vec<u8>) -> (MemFileSystem, PathBuf) {
    let fs = MemFileSystem::new();
    let path = PathBuf::from("/g/golden.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    fs.install(&path, bytes).unwrap();
    (fs, path)
}

/// The writer, run again, produces the committed bytes exactly.
#[test]
fn golden_files_are_reproduced_byte_for_byte() {
    let bless = std::env::var_os("ESKER_BLESS").is_some();
    for (name, compression) in cases() {
        let path = golden_dir().join(name);
        let built = build(compression);

        if bless && !path.exists() {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, &built).unwrap();
            continue;
        }

        let committed = std::fs::read(&path).unwrap_or_else(|error| {
            panic!("{}: {error} (ESKER_BLESS=1 to create)", path.display())
        });
        assert_eq!(
            built.len(),
            committed.len(),
            "{name} is {} bytes and the committed file is {}: this is a format change",
            built.len(),
            committed.len()
        );
        let first = built.iter().zip(&committed).position(|(a, b)| a != b);
        assert!(
            first.is_none(),
            "{name} differs from the committed file at byte {}: this is a format change, \
             which needs an ADR and a version bump rather than a re-bless",
            first.unwrap()
        );
    }
}

/// The bytes on disk still decode to exactly the rows they were written from.
#[test]
fn the_committed_bytes_still_read() {
    let expected = corpus::rows(ROWS);
    for (name, _) in cases() {
        let bytes = std::fs::read(golden_dir().join(name)).unwrap();
        let (fs, path) = installed(bytes);

        let reader = Reader::open(&fs, &path).unwrap();
        assert_eq!(reader.schema(), &corpus::schema(), "{name}");
        assert_eq!(reader.rows() as usize, ROWS, "{name}");
        assert_eq!(reader.stripes().len(), ROWS.div_ceil(STRIPE_ROWS), "{name}");

        assert_eq!(read_rows(&fs, &path), expected, "{name}");
    }
}

/// Every single-byte change to a golden file is detected.
///
/// Not "detected or harmless". Every region of this format carries a checksum — the chunks their
/// own, the footer's in the trailer, the trailer's over itself — so a flip that reads back
/// successfully would mean a region nothing covers, which is the defect this test exists to catch
/// (`CLAUDE.md` invariant 2). Two bits are flipped per byte, the lowest and the highest, because
/// a bit that only ever appears inside a length is a different case from one inside a value.
#[test]
fn every_flipped_byte_is_caught() {
    for (name, _) in cases() {
        let good = std::fs::read(golden_dir().join(name)).unwrap();
        let expected = corpus::rows(ROWS);

        for at in 0..good.len() {
            for bit in [0u8, 7] {
                let mut bytes = good.clone();
                bytes[at] ^= 1 << bit;
                let (fs, path) = installed(bytes);

                let Ok(reader) = Reader::open(&fs, &path) else {
                    continue;
                };
                let mut noticed = false;
                let mut rows = Vec::new();
                'stripes: for (index, stripe) in reader.stripes().iter().enumerate() {
                    let mut decoded = Vec::new();
                    for column in 0..reader.schema().len() {
                        let Ok(values) = reader.read_column(index, column) else {
                            noticed = true;
                            break 'stripes;
                        };
                        decoded.push(values.to_values().unwrap_or_default());
                    }
                    for offset in 0..usize::try_from(stripe.rows).unwrap() {
                        rows.push(
                            decoded
                                .iter()
                                .map(|values| values.get(offset).cloned().unwrap_or(Value::Null))
                                .collect::<Vec<_>>(),
                        );
                    }
                }
                assert!(
                    noticed || rows == expected,
                    "{name}: flipping bit {bit} of byte {at} changed the data without \
                     any checksum noticing"
                );
            }
        }
    }
}
