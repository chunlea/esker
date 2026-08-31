//! The crate's error type.
//!
//! One enum, as `CLAUDE.md` requires, and three of its variants carry the weight:
//!
//! * [`Error::Corruption`] is how invariant 2 is honoured. A bad checksum, a wrong magic, an
//!   impossible length, a chunk that disagrees with the footer that named it — every one of them
//!   is a value, never a panic and never a silent skip.
//! * [`Error::Unsealed`] is **not** corruption, and keeping them apart is the point of having
//!   both. A columnar file is finished by its trailer: stripes, then the footer, then the
//!   trailer, then a sync and a rename. Every prefix of that is a file a crash left behind, and
//!   the right response is to delete it. A *sealed* file whose bytes have since rotted is an
//!   alarm. One error type for both would make every crash look like disk failure, or — far
//!   worse — make disk failure look survivable.
//! * [`Error::Io`] always names the path, because a bare "No such file or directory" is not
//!   actionable in a crash report and the caller always knew what it asked for.

use std::io;
use std::path::{Path, PathBuf};

/// The result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong reading or writing a columnar file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem refused an operation.
    #[error("io error on {path}: {source}")]
    Io {
        /// The file or directory the operation named.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// On-disk bytes did not mean what the format says they must (invariant 2).
    #[error("corruption in {context}: {detail}")]
    Corruption {
        /// Where it was found — a file name, or a component such as `stripe index`.
        context: String,
        /// What was wrong, specifically enough to debug from one log line.
        detail: String,
    },

    /// The file has no valid trailer, so it was never finished.
    ///
    /// The expected shape of a crash mid-write, and the reason it is not [`Error::Corruption`]:
    /// see the module docs.
    #[error("{path} has no valid columnar trailer: the file was never finished")]
    Unsealed {
        /// The file that has no trailer.
        path: PathBuf,
    },

    /// The caller asked for something this format cannot express: a row of the wrong width, a
    /// value of the wrong type, a column or stripe that does not exist.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// This build will not evaluate this fragment, and has done none of it.
    ///
    /// [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) decision 3: *"a fragment the
    /// columnar node cannot evaluate is refused, never partially honoured."* An unknown expression
    /// node, an aggregate over a type that has no sum, a key range this build cannot restrict to —
    /// each refuses the **whole** fragment, and the caller falls back to a row scan. Honouring the
    /// half it understood would silently drop a filter, which returns extra rows rather than an
    /// error, and is the defect class the rule exists for.
    #[error("fragment refused: {0}")]
    Refused(String),

    /// An aggregate ran out of range.
    ///
    /// A declared divergence from PostgreSQL, which returns `numeric` from `sum(bigint)` and
    /// therefore cannot overflow. Phase 6a has no `numeric` (`esker_sql::plan::expr` refuses
    /// decimal-to-`int8` for the same reason), so the honest answer is an error rather than a
    /// wrapped number — a wrong total is worse than a missing one. A SQL node maps this to
    /// PostgreSQL's `22003 numeric_value_out_of_range`.
    #[error("value out of range: {0}")]
    Overflow(String),
}

impl Error {
    /// A corruption error, with the region that held the bytes and what was wrong with them.
    pub fn corruption(context: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Corruption {
            context: context.into(),
            detail: detail.into(),
        }
    }

    /// Whether this is on-disk bytes failing to mean what they must.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corruption { .. })
    }

    /// Whether this is a file a crash left half-written, which is discardable rather than alarming.
    #[must_use]
    pub fn is_unsealed(&self) -> bool {
        matches!(self, Self::Unsealed { .. })
    }

    /// A refusal, with the reason a caller can log before falling back to a row scan.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused(reason.into())
    }

    /// Whether this build declined the whole fragment, having evaluated none of it.
    #[must_use]
    pub fn is_refused(&self) -> bool {
        matches!(self, Self::Refused(_))
    }

    /// Whether an aggregate ran out of range.
    #[must_use]
    pub fn is_overflow(&self) -> bool {
        matches!(self, Self::Overflow(_))
    }
}

/// Attaches the path to an [`io::Error`], which never carries one of its own.
pub(crate) trait IoResultExt<T> {
    /// Names `path` as where the operation failed.
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
