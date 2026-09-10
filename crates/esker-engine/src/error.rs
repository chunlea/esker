//! The engine's error type.
//!
//! One enum for the whole crate, as `CLAUDE.md` requires. Two of its variants carry the
//! weight:
//!
//! * [`Error::Io`] always names the path. An engine failure that says only "No such file or
//!   directory" is unusable in a crash report, and the caller of a [`crate::fs::FileSystem`]
//!   method always knows the path it passed.
//! * [`Error::Corruption`] is how invariant 2 is honoured: a bad checksum, a wrong magic or a
//!   truncated record comes back as a value. It is never a panic and never a silent skip.
//!
//! A torn record at the tail of the last log segment is *not* corruption — it is the expected
//! shape of a crash — so it is not represented here at all. The log reader reports it as a
//! separate status and the caller decides whether it is tolerable at that position
//! (`docs/DESIGN.md` §4.3).

use std::io;
use std::path::{Path, PathBuf};

/// The result of every fallible engine operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong inside the engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem refused an operation. The path is part of the message because a bare
    /// `io::Error` is not actionable.
    #[error("io error on {path}: {source}")]
    Io {
        /// The file or directory the operation named.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// On-disk bytes did not mean what the format says they must: a checksum mismatch, a bad
    /// magic, an impossible length, an unknown record type (invariant 2).
    #[error("corruption in {context}: {detail}")]
    Corruption {
        /// Where it was found — a file name, or a component such as `wal reader`.
        context: String,
        /// What was wrong, specifically enough to debug from a log line alone.
        detail: String,
    },

    /// The caller asked for something the engine cannot do with these arguments: an unknown
    /// column family, an inverted range, a key that is too large.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A documented limitation of this version, distinguished from a caller error so that it
    /// can be found and removed later. `DeleteRange` across an SST boundary is the phase-1
    /// example (`docs/DESIGN.md` §4.7).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Something the engine expected to exist does not: a manifest named by `CURRENT`, a WAL
    /// segment named by the manifest, an SST named by a version.
    #[error("not found: {0}")]
    NotFound(String),

    /// The database is closing, so the operation was not started. Background work returns
    /// this rather than a partial result.
    #[error("database is shutting down")]
    ShuttingDown,

    /// This write shared a group commit with another that failed.
    ///
    /// Group commit means one writer does the log append and the `fsync` for everyone queued
    /// behind it, so one failure belongs to all of them. The leader gets the original error;
    /// everyone else gets this, carrying its message. Reporting success to a follower whose
    /// bytes never reached the log would break invariant 1 for a write that looked fine.
    #[error("group commit failed: {0}")]
    GroupCommit(String),

    /// An update failed part-way through, so what is in memory and what is on disk may no
    /// longer agree. Nothing further is attempted; the database has to be reopened, which
    /// re-derives the state from what actually reached the disk.
    ///
    /// This is the honest answer to a failed manifest sync or a failed `CURRENT` rename. The
    /// alternative — carrying on with an in-memory version the disk does not share — is how a
    /// storage engine starts returning keys that are not there.
    #[error("database must be reopened: {0}")]
    Poisoned(String),

    /// The data directory already has a writer.
    ///
    /// A directory holds one database and a database has one writer, and until this existed
    /// nothing said so: a second process opened the same tree and wrote its own WAL segments and
    /// manifests into it. An error and never a panic (invariant 9).
    ///
    /// **It arrives late on purpose.** `Db::open` waits a few seconds for a directory somebody is
    /// still letting go of before it answers this, because a claim held by a live writer is held
    /// for ever and one held by a shutdown is held for a moment — so waiting separates them
    /// without ever admitting a second live writer. The wait is bounded: a node that blocked here
    /// for ever would be a node an operator reads as hung.
    #[error("{dir} is open in another process")]
    InUse {
        /// The directory somebody else holds.
        dir: PathBuf,
    },
}

impl Error {
    /// Attaches a path to an [`io::Error`].
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Reports corrupt bytes found in `context`.
    pub fn corruption(context: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Corruption {
            context: context.into(),
            detail: detail.into(),
        }
    }

    /// True when this error means the bytes on disk are not what the format allows. Recovery
    /// paths branch on it: with `paranoid_checks` off, some corruption is survivable at the
    /// tail of a log, while an `Io` error never is.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corruption { .. })
    }
}

/// Adds path context to an `io::Result`, which is what [`crate::fs::FileSystem`] returns.
///
/// Public because anything built on the [`crate::fs`] traits needs the same one-line
/// conversion, and because a `FileSystem` implementation lives outside this crate.
pub trait IoResultExt<T> {
    /// Converts to an engine [`Result`], naming `path`.
    fn at(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|source| Error::io(path, source))
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, IoResultExt};
    use std::io;

    #[test]
    fn io_errors_name_the_path() {
        let err = Error::io("/db/000001.wal", io::Error::from(io::ErrorKind::NotFound));
        let text = err.to_string();
        assert!(text.contains("/db/000001.wal"), "{text}");
        assert!(!err.is_corruption());
    }

    #[test]
    fn corruption_is_distinguishable() {
        let err = Error::corruption("000001.wal", "checksum mismatch at offset 4096");
        assert!(err.is_corruption());
        assert!(err.to_string().contains("offset 4096"));
    }

    /// The extension trait exists so that call sites stay one line; if it stopped attaching
    /// the path it would still compile, so the behaviour is pinned here.
    #[test]
    fn the_extension_trait_attaches_the_path() {
        let result: io::Result<()> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let err = result.at("/db/CURRENT").unwrap_err();
        assert!(err.to_string().contains("/db/CURRENT"));
    }
}
