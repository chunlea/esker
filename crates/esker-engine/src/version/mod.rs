//! Versions: the set of files that make up the database at one instant.
//!
//! A `Version` is a snapshot of which SSTs live at which level of which column family. It is
//! immutable and shared behind an `Arc`: a reader pins one and is guaranteed that every file
//! it names still exists, while a compaction installs a new one beside it. Files are deleted
//! only once no version references them (`docs/DESIGN.md` §4.6).
//!
//! * [`edit`] — `VersionEdit`, the delta the manifest is a log of

pub mod edit;

pub use edit::{FileMeta, VersionEdit};
