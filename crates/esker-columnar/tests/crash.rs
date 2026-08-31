//! What a crash in the middle of writing a columnar file may cost, and what it may not.
//!
//! `CLAUDE.md` invariant 3 says the only mutable pointer is a file replaced by atomic rename, and
//! invariant 9 says on-disk data never panics. Put together, those give this format a single
//! obligation, and it is the one this file exercises at **every** length a file can have:
//!
//! > a file that is not complete must be *reported* as not complete — never panicked over, and
//! > never read back as though it were whole.
//!
//! "Never read back as whole" is the hard half, and it is why the assertions below do not simply
//! check for an error. A truncated file that opens and answers with three of its four stripes is
//! a wrong answer, not a failure, and no error would ever be logged for it. So every truncation
//! that opens at all is read to the end and its rows compared with the rows the whole file holds.
//!
//! # Three shapes of crash
//!
//! 1. **A truncated finished file.** Every prefix of a complete file, all of them.
//! 2. **A truncated temporary.** What a crash actually leaves behind: the writer's `.tmp`, cut
//!    off at an arbitrary point, with no footer and no trailer.
//! 3. **A crash before the rename.** The final name must not exist at all, and a retry must
//!    produce a complete, correct file over the leftover.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use esker_columnar::{Error, Reader, Value, Writer, WriterOptions};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

#[path = "corpus.rs"]
mod corpus;

const ROWS: usize = 400;
const STRIPE_ROWS: usize = 64;

fn options() -> WriterOptions {
    WriterOptions {
        stripe_rows: STRIPE_ROWS,
        ..WriterOptions::default()
    }
}

/// Writes the corpus to `/c/whole.col` and returns the filesystem and the finished bytes.
fn whole() -> (MemFileSystem, Vec<u8>) {
    let fs = MemFileSystem::new();
    let path = Path::new("/c/whole.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let mut writer = Writer::create(&fs, path, corpus::schema(), options()).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    writer.finish().unwrap();
    let bytes = fs.contents(path).unwrap();
    (fs, bytes)
}

/// Every row of a file, or the error that stopped it.
fn read_all(fs: &MemFileSystem, path: &Path) -> Result<Vec<Vec<Value>>, Error> {
    let reader = Reader::open(fs, path)?;
    let projection: Vec<usize> = (0..reader.schema().len()).collect();
    let mut rows = Vec::new();
    for (index, stripe) in reader.stripes().iter().enumerate() {
        let columns = reader.read_stripe(index, &projection)?;
        let decoded: Vec<Vec<Value>> = columns
            .iter()
            .map(esker_columnar::Column::to_values)
            .collect::<Result<_, _>>()?;
        for offset in 0..stripe.rows as usize {
            rows.push(
                decoded
                    .iter()
                    .map(|values| values[offset].clone())
                    .collect(),
            );
        }
    }
    Ok(rows)
}

fn installed(bytes: Vec<u8>) -> (MemFileSystem, PathBuf) {
    let fs = MemFileSystem::new();
    let path = PathBuf::from("/c/cut.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    fs.install(&path, bytes).unwrap();
    (fs, path)
}

/// Truncation at every one of a finished file's lengths.
#[test]
fn no_truncation_of_a_finished_file_reads_back_as_finished() {
    let (_, good) = whole();
    let expected = corpus::rows(ROWS);
    assert!(good.len() > 4_000, "the corpus is too small to be a test");

    let mut unsealed = 0;
    let mut corrupt = 0;
    for cut in 0..good.len() {
        let (fs, path) = installed(good[..cut].to_vec());
        match read_all(&fs, &path) {
            Err(error) => {
                assert!(
                    error.is_unsealed() || error.is_corruption(),
                    "a file cut at {cut} failed with {error}, which is neither"
                );
                if error.is_unsealed() {
                    unsealed += 1;
                } else {
                    corrupt += 1;
                }
            }
            Ok(rows) => assert_ne!(
                rows,
                expected,
                "a file cut at {cut} of {} read back as complete",
                good.len()
            ),
        }
    }

    // Almost every cut loses the trailing magic, which is the cheap check the format is built
    // around; a handful land inside it. Both are fine, and neither is a panic.
    assert_eq!(unsealed + corrupt, good.len());
    assert!(
        unsealed > good.len() - 64,
        "only {unsealed} of {} cuts were recognised as unfinished",
        good.len()
    );

    // And the whole file, for contrast, reads back exactly.
    let (fs, path) = installed(good);
    assert_eq!(read_all(&fs, &path).unwrap(), expected);
}

/// A file that lost only its trailer is the commonest crash there is, and must be *unsealed*.
#[test]
fn losing_the_trailer_is_unsealed_not_corrupt() {
    let (_, good) = whole();
    for lost in 1..=32usize {
        let (fs, path) = installed(good[..good.len() - lost].to_vec());
        let error = Reader::open(&fs, &path).unwrap_err();
        assert!(
            error.is_unsealed(),
            "losing {lost} trailing bytes reported {error}"
        );
    }
}

/// What a crash actually leaves on disk: a temporary with no footer and no trailer.
#[test]
fn a_half_written_temporary_is_never_a_file() {
    let fs = MemFileSystem::new();
    let path = Path::new("/c/live.col");
    let temp = PathBuf::from("/c/live.col.tmp");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let mut writer = Writer::create(&fs, path, corpus::schema(), options()).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    // Stripes are on disk; the footer and trailer are not. This is the state a `kill -9` leaves.
    drop(writer);

    assert!(!fs.exists(path).unwrap(), "the final name exists");
    let partial = fs.contents(&temp).unwrap();
    assert!(!partial.is_empty(), "nothing was written at all");

    for cut in 0..partial.len() {
        let (fs, path) = installed(partial[..cut].to_vec());
        let error = Reader::open(&fs, &path).unwrap_err();
        assert!(
            error.is_unsealed(),
            "a temporary cut at {cut} reported {error}"
        );
    }
    let (fs, path) = installed(partial);
    assert!(Reader::open(&fs, &path).unwrap_err().is_unsealed());
}

/// A power loss that discards everything unsynced: before the rename nothing appears, and after
/// it the file is whole, because `finish` syncs before it renames.
#[test]
fn a_crash_before_the_rename_leaves_nothing_and_a_retry_succeeds() {
    let fs = MemFileSystem::new();
    let path = Path::new("/c/retry.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let mut writer = Writer::create(&fs, path, corpus::schema(), options()).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    drop(writer);
    fs.lose_unsynced().unwrap();
    assert!(
        !fs.exists(path).unwrap(),
        "an unfinished write produced a file"
    );

    // The retry writes over the leftover temporary rather than tripping on it.
    let mut writer = Writer::create(&fs, path, corpus::schema(), options()).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    writer.finish().unwrap();
    fs.lose_unsynced().unwrap();

    assert!(fs.exists(path).unwrap());
    assert_eq!(read_all(&fs, path).unwrap(), corpus::rows(ROWS));
}

/// Bytes that are not a columnar file at all, and bytes that end in the magic but are nothing
/// else. Neither may panic, and the second may not be mistaken for an unfinished file.
#[test]
fn arbitrary_tails_are_typed_errors() {
    let (fs, path) = installed(b"not a columnar file, not even close".to_vec());
    assert!(Reader::open(&fs, &path).unwrap_err().is_unsealed());

    let mut forged = vec![0u8; 64];
    forged.extend_from_slice(b"ESKERCOL");
    let (fs, path) = installed(forged);
    let error = Reader::open(&fs, &path).unwrap_err();
    assert!(
        error.is_corruption(),
        "a forged magic must be corruption, not an unfinished file: {error}"
    );

    // A trailer that is entirely plausible except that the file behind it is not there.
    let (_, good) = whole();
    let mut short = good[good.len() - 32..].to_vec();
    short.splice(0..0, std::iter::repeat_n(0u8, 8));
    let (fs, path) = installed(short);
    assert!(Reader::open(&fs, &path).unwrap_err().is_corruption());
}
