//! The write-ahead log: the first place every write goes and the last place recovery looks.
//!
//! Invariant 1 — log before state, fsync before ack — is implemented here and nowhere else.
//! A batch becomes durable when [`LogWriter::sync`] returns; only then may it be acknowledged
//! or inserted into a memtable, and [`LogReader`] is what turns those bytes back into batches
//! after a crash.
//!
//! * [`format`] — the frozen byte layout, and why the checksum is seeded with the record type
//! * [`writer`] — fragmenting records across 32 KiB blocks, one `write` per group commit
//! * [`reader`] — reassembling them, and telling a torn tail from corruption

pub mod format;
pub mod reader;
pub mod writer;

pub use format::{BLOCK_SIZE, HEADER_SIZE, MAX_FRAGMENT_LEN, RecordType};
pub use reader::{LogReader, ReadOutcome};
pub use writer::LogWriter;
