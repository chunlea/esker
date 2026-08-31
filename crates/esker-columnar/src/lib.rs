//! `esker-columnar` — the columnar file format.
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) argues for a second copy of a
//! table laid out by column, and ranks the cost of building one. The file format is first on that
//! list and this crate is it: *"a file format with a footer, a version and golden tests; a set of
//! per-type encodings each with a round-trip proptest; a stripe/row-group layout with statistics
//! for predicate pruning; ... a crash test ...; and a fuzz test proving that no arbitrary byte
//! sequence panics a decoder."*
//!
//! It is a **file format and nothing else**. No Raft, no learner, no fragment protocol, no
//! planner, no SQL — those are ADR 0022's milestones 2 through 5 and
//! `docs/plans/phase-7-columnar.md` names every one of them under what this phase does not do.
//! Both ways of delivering a columnar copy — a Raft learner applying the log, or phase 6b
//! rewriting a cold SST on its way to object storage — need these bytes and nothing about
//! either is decided here.
//!
//! # The shape
//!
//! ```text
//! file   := stripe* ++ footer ++ trailer
//! stripe := chunk*                        one chunk per column, in schema order
//! chunk  := payload ++ codec:u8 ++ crc32c:u32
//! ```
//!
//! A **stripe** is a row group: a bounded run of rows, stored one column at a time. It is the
//! unit of pruning (a stripe is skipped or read) and the unit of decoding (a chunk is decoded
//! whole). A **chunk** is one column of one stripe: an encoding tag, the rows it covers, where
//! its NULLs are, and its values densely packed — then LZ4 over the lot, if that pays.
//!
//! The [`footer`] carries the schema, every chunk's position, and every chunk's
//! [statistics](stats), so that deciding what to read costs no I/O beyond opening the file. The
//! **trailer** is 32 fixed bytes at the very end, and its magic is what makes a file a file:
//! everything a crash left half-written lacks it, and is reported as [`Error::Unsealed`] rather
//! than as damage.
//!
//! # Invariants this crate holds
//!
//! * **Every region is checksummed** (invariant 2). Each chunk carries a CRC32C over its payload
//!   and codec byte together; the footer's CRC is in the trailer; the trailer checksums itself.
//! * **Immutable files, atomic pointers** (invariant 3). A file is written to a temporary name,
//!   synced, then renamed into place; nothing is ever modified after the trailer lands.
//! * **Nothing panics on on-disk data** (invariant 9). Every decode path reads through
//!   one bounds-checked cursor, which refuses counts larger than the bytes behind
//!   them. `tests/fuzz_decode.rs` is what proves it, and `tests/crash.rs` proves that no
//!   truncation of a file is ever read back as complete.
//! * **The type set is the row side's** — the six `esker_sql::value::ColumnType` variants, with
//!   the row side's own tag bytes. A columnar copy of a row holds no more than the row could.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod column;
pub mod encode;
pub mod error;
pub mod footer;
pub mod fragment;
pub mod frame;
pub mod reader;
pub mod scan;
pub mod stats;
pub mod value;
pub mod writer;

pub(crate) mod cursor;

pub use column::{Column, ColumnBuilder, ColumnData, NullMask};
pub use error::{Error, Result};
pub use footer::{ChunkMeta, Footer, StripeMeta, Trailer};
pub use fragment::{Aggregate, CompareOp, Expr, Fragment, KeyRange, Output, TableRef};
pub use frame::Compression;
pub use reader::{ReadCounters, Reader};
pub use scan::{
    FragmentOutput, FragmentResult, Group, Partial, ScanOptions, ScanStats, evaluate,
    evaluate_merged, evaluate_with,
};
pub use stats::{Bound, ColumnStats};
pub use value::{ColumnDef, ColumnType, Schema, Value, ValueRef};
pub use writer::{FileSummary, Writer, WriterOptions};

/// Byte layouts that are frozen. Changing any of these is a format change: it needs an ADR, a
/// format-version bump and a migration story (ADR 0002).
pub mod format {
    /// Trailing magic of a columnar file: the ASCII bytes `ESKERCOL`.
    pub const COLUMNAR_MAGIC: [u8; 8] = *b"ESKERCOL";

    /// Fixed trailer size: footer handle, two checksums, version, magic.
    pub const COLUMNAR_TRAILER_SIZE: usize = 32;

    /// Version of the layout this build reads and writes.
    ///
    /// Version 2 binds each chunk's checksum to its offset, so a chunk copied elsewhere in a file
    /// fails instead of answering with another stripe's rows. Version 1 was never written outside
    /// this repository's own golden files and is not read.
    pub const COLUMNAR_FORMAT_VERSION: u32 = 2;

    /// Every chunk ends with `codec:u8 ++ crc32c:u32`.
    pub const CHUNK_TRAILER_SIZE: usize = 5;

    /// Most rows one stripe may hold: 4 Mi.
    ///
    /// A pruning granularity nothing wants to exceed — the writer's default is 64Ki and its byte
    /// budget usually seals sooner — and, more to the point, a decode guard. A chunk's row count
    /// sizes the null mask and, for a one-bit encoding, an array 64 times the size of the bytes
    /// behind it: without a cap, a corrupt count of two billion asks for sixteen gigabytes from a
    /// chunk that is merely large. The cursor's "no count larger than the bytes behind it" rule
    /// does not catch that one on its own, because those bits really are there.
    pub const MAX_STRIPE_ROWS: usize = 4 * 1024 * 1024;

    /// Longest single text or bytea value this format stores: 64 MiB.
    ///
    /// A decode guard as much as a limit. A corrupt length prefix asking for more than this is
    /// refused before a byte is allocated, which is half of why an arbitrary byte sequence cannot
    /// exhaust memory in a decoder (the other half is that no count may exceed the bytes behind
    /// it).
    pub const MAX_VALUE_LEN: usize = 64 * 1024 * 1024;

    /// Most bytes one column chunk may decode to: 256 MiB.
    ///
    /// A dictionary makes expansion possible — a thousand codes into one long entry decode to a
    /// thousand copies of it — so the bound is on the *decoded* size and is checked as the total
    /// accumulates, not after.
    pub const MAX_COLUMN_BYTES: usize = 256 * 1024 * 1024;

    /// Longest min/max bound stored in a chunk's statistics.
    ///
    /// A bound is there to prune with, not to reproduce a value, so one enormous string must not
    /// be able to inflate the footer every reader loads. Longer values are truncated to a bound
    /// that still holds — see [`crate::stats`].
    pub const MAX_BOUND_LEN: usize = 64;
}

#[cfg(test)]
mod tests {
    use super::format::{COLUMNAR_FORMAT_VERSION, COLUMNAR_MAGIC, COLUMNAR_TRAILER_SIZE};

    /// The magic is written into every file, so its bytes are part of the format.
    #[test]
    fn the_format_constants_are_frozen() {
        assert_eq!(&COLUMNAR_MAGIC, b"ESKERCOL");
        assert_eq!(COLUMNAR_TRAILER_SIZE, 32);
        assert_eq!(COLUMNAR_FORMAT_VERSION, 2);
        assert!(COLUMNAR_MAGIC.len() < COLUMNAR_TRAILER_SIZE);
    }
}
