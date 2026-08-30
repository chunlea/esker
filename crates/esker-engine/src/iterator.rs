//! The cursor shape every layer of the engine iterates through.
//!
//! Not a `std::iter::Iterator`, and it cannot be one: `next` yields nothing, the cursor also
//! goes backwards, and it can be repositioned at an arbitrary key. That is the shape
//! `docs/DESIGN.md` §4.1 specifies, and it is what a merge iterator needs — merging requires
//! looking at each child's current key without consuming it, which a pull iterator does not
//! allow.
//!
//! # Errors are a separate question from validity
//!
//! A cursor that has run off the end and a cursor that could not read a block are both
//! invalid, and confusing the two turns an unreadable file into an empty one. Every
//! implementation reports the difference through [`Cursor::status`], and every caller checks
//! it before believing that iteration finished.

use crate::error::Result;

/// A two-directional cursor over sorted key-value pairs.
///
/// Keys are whatever the layer stores: internal keys inside the engine, user keys at the
/// [`Db`](crate::Db) surface.
pub trait Cursor {
    /// Whether the cursor is on an entry. False at either end **and** after a read error;
    /// [`status`](Cursor::status) tells them apart.
    fn valid(&self) -> bool;

    /// The key under the cursor. Only meaningful while [`valid`](Cursor::valid).
    fn key(&self) -> &[u8];

    /// The value under the cursor. Only meaningful while [`valid`](Cursor::valid).
    fn value(&self) -> &[u8];

    /// Positions on the first entry at or after `target`.
    fn seek(&mut self, target: &[u8]);

    /// Positions on the last entry at or before `target`.
    fn seek_for_prev(&mut self, target: &[u8]);

    /// Positions on the first entry.
    fn seek_to_first(&mut self);

    /// Positions on the last entry.
    fn seek_to_last(&mut self);

    /// Moves forward one entry. A no-op on an invalid cursor: getting back on takes a seek.
    fn next(&mut self);

    /// Moves back one entry. A no-op on an invalid cursor.
    fn prev(&mut self);

    /// Whether everything read so far was readable. Checked after iteration ends, because an
    /// unreadable block ends it exactly as reaching the last entry does.
    fn status(&self) -> Result<()>;
}
