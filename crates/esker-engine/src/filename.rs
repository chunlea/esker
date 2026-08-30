//! Every file name the engine uses, in one place.
//!
//! ```text
//! NNNNNN.wal          one write-ahead-log segment per memtable generation
//! NNNNNN.sst          one sorted string table, immutable once finished
//! MANIFEST-NNNNNN     a log of VersionEdits (docs/DESIGN.md §4.6)
//! CURRENT             one line naming the live manifest, replaced by rename
//! NNNNNN.tmp          a file being built; never read, deleted at startup
//! ```
//!
//! Names are derived from a **file number**, never from content, and a number is never reused
//! within a database. That is what makes a cached block impossible to stale
//! ([`crate::cache_api`]) and what lets recovery decide which logs to replay by comparing
//! numbers rather than timestamps.
//!
//! Numbers are zero-padded to six digits so a directory listing usually reads in order, but
//! nothing depends on that: recovery sorts by the parsed number, because the padding stops
//! being sufficient at a million files and a listing that silently reorders after that would
//! be a spectacular bug to find later.

use std::path::{Path, PathBuf};

/// What a name in the database directory refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    /// `NNNNNN.wal`
    Wal(u64),
    /// `NNNNNN.sst`
    Sst(u64),
    /// `MANIFEST-NNNNNN`
    Manifest(u64),
    /// `CURRENT`
    Current,
    /// `NNNNNN.tmp`, a partially built file left by a crash.
    Temp(u64),
}

/// The path of write-ahead-log segment `number`.
pub fn wal(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.wal"))
}

/// The path of sorted string table `number`.
pub fn sst(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.sst"))
}

/// The path of manifest `number`.
pub fn manifest(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("MANIFEST-{number:06}"))
}

/// The path of the file naming the live manifest.
pub fn current(dir: &Path) -> PathBuf {
    dir.join("CURRENT")
}

/// The path of a scratch file numbered `number`, used to build `CURRENT` before renaming it.
pub fn temp(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("{number:06}.tmp"))
}

/// The contents `CURRENT` holds when it names manifest `number`.
///
/// The trailing newline is not decoration: it makes a truncated `CURRENT` detectable, since a
/// name without one was not fully written.
pub fn current_contents(number: u64) -> String {
    format!("MANIFEST-{number:06}\n")
}

/// The manifest number a `CURRENT` file names, or `None` if it is not a well-formed one.
pub fn parse_current(contents: &str) -> Option<u64> {
    let line = contents.strip_suffix('\n')?;
    match classify(line) {
        Some(FileKind::Manifest(number)) => Some(number),
        _ => None,
    }
}

/// What `name` — a bare file name, not a path — refers to, or `None` for anything the engine
/// did not create.
pub fn classify(name: &str) -> Option<FileKind> {
    if name == "CURRENT" {
        return Some(FileKind::Current);
    }
    if let Some(digits) = name.strip_prefix("MANIFEST-") {
        return digits.parse().ok().map(FileKind::Manifest);
    }
    let (digits, extension) = name.rsplit_once('.')?;
    let number = digits.parse().ok()?;
    match extension {
        "wal" => Some(FileKind::Wal(number)),
        "sst" => Some(FileKind::Sst(number)),
        "tmp" => Some(FileKind::Temp(number)),
        _ => None,
    }
}

/// [`classify`] for a full path, ignoring anything without a file name.
pub fn classify_path(path: &Path) -> Option<FileKind> {
    classify(path.file_name()?.to_str()?)
}

#[cfg(test)]
mod tests {
    use super::{
        FileKind, classify, classify_path, current, current_contents, manifest, parse_current, sst,
        temp, wal,
    };
    use std::path::Path;

    #[test]
    fn names_round_trip_through_classify() {
        let dir = Path::new("/db");
        let cases = [
            (wal(dir, 1), FileKind::Wal(1)),
            (sst(dir, 42), FileKind::Sst(42)),
            (manifest(dir, 7), FileKind::Manifest(7)),
            (current(dir), FileKind::Current),
            (temp(dir, 9), FileKind::Temp(9)),
            (wal(dir, 12_345_678), FileKind::Wal(12_345_678)),
        ];
        for (path, kind) in cases {
            assert_eq!(classify_path(&path), Some(kind), "{}", path.display());
        }
    }

    #[test]
    fn numbers_are_padded_to_six_digits() {
        assert_eq!(wal(Path::new("/db"), 1).file_name().unwrap(), "000001.wal");
        assert_eq!(
            manifest(Path::new("/db"), 3).file_name().unwrap(),
            "MANIFEST-000003"
        );
        // Past a million the padding simply stops helping; the number is still exact.
        assert_eq!(
            sst(Path::new("/db"), 1_234_567).file_name().unwrap(),
            "1234567.sst"
        );
    }

    #[test]
    fn foreign_names_are_not_ours() {
        for name in [
            "",
            "LOG",
            "lost+found",
            "000001.log",
            "MANIFEST-",
            "MANIFEST-abc",
            "x.wal",
            ".wal",
        ] {
            assert_eq!(classify(name), None, "{name} should not be recognised");
        }
    }

    #[test]
    fn current_points_at_a_manifest() {
        let contents = current_contents(5);
        assert_eq!(contents, "MANIFEST-000005\n");
        assert_eq!(parse_current(&contents), Some(5));
    }

    /// A `CURRENT` without its newline was not fully written, which is exactly the state a
    /// crash mid-rename must not be mistaken for a valid pointer.
    #[test]
    fn a_truncated_current_is_not_a_pointer() {
        assert_eq!(parse_current("MANIFEST-000005"), None);
        assert_eq!(parse_current(""), None);
        assert_eq!(parse_current("\n"), None);
        assert_eq!(parse_current("MANIFEST-0000\n"), Some(0));
        assert_eq!(parse_current("something else\n"), None);
        assert_eq!(parse_current("MANIFEST-000005\nMANIFEST-000006\n"), None);
    }
}
