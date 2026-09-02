//! A bounds-checked reader over a byte slice.
//!
//! Every decode path in this crate goes through one, and that is what makes invariant 9 —
//! *never panic on on-disk data* — a property of the crate rather than of each decoder's
//! discipline. There is no indexing and no slicing anywhere else in the decode paths; a short
//! buffer comes back as [`Error::Corruption`] naming the field that ran off the end.
//!
//! # The allocation guard
//!
//! The dangerous shape in any length-prefixed format is a count that a decoder believes: a
//! corrupt varint says "four billion values follow" and a `Vec::with_capacity` obliges. Every
//! count in this format is therefore read through [`Cursor::count`], which refuses a count whose
//! items cannot fit in the bytes that remain. That check is why the decoder fuzz can feed
//! arbitrary bytes at every entry point without the machine running out of memory, and it is
//! cheap: the smallest possible encoding of an item is a compile-time fact at every call site.

use esker_base::varint;

use crate::error::{Error, Result};

/// A position in a byte slice, and the name of the region for error messages.
#[derive(Debug)]
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
    context: &'static str,
}

impl<'a> Cursor<'a> {
    /// A cursor at the front of `buf`. `context` names the region in every error it produces.
    pub(crate) fn new(buf: &'a [u8], context: &'static str) -> Self {
        Self {
            buf,
            pos: 0,
            context,
        }
    }

    /// Bytes not yet consumed.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn short(&self, field: &str, wanted: usize) -> Error {
        Error::corruption(
            self.context,
            format!(
                "{field} needs {wanted} bytes but only {} remain",
                self.remaining()
            ),
        )
    }

    /// The next `n` bytes.
    pub(crate) fn bytes(&mut self, n: usize, field: &str) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(self.short(field, n));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// One byte.
    pub(crate) fn u8(&mut self, field: &str) -> Result<u8> {
        Ok(self.bytes(1, field)?[0])
    }

    /// A little-endian `u16`. An `int2` literal is two bytes on the wire, because two bytes is
    /// what the type is — widening it would make the framing disagree with `put_literal`.
    pub(crate) fn u16_le(&mut self, field: &str) -> Result<u16> {
        let bytes = self.bytes(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// A little-endian `u32`.
    pub(crate) fn u32_le(&mut self, field: &str) -> Result<u32> {
        let bytes = self.bytes(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A little-endian `u64`.
    pub(crate) fn u64_le(&mut self, field: &str) -> Result<u64> {
        let bytes = self.bytes(8, field)?;
        let mut fixed = [0u8; 8];
        fixed.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(fixed))
    }

    /// A LEB128 varint.
    pub(crate) fn varint(&mut self, field: &str) -> Result<u64> {
        let (value, consumed) = varint::get_u64(&self.buf[self.pos..])
            .map_err(|error| Error::corruption(self.context, format!("{field}: {error}")))?;
        self.pos += consumed;
        Ok(value)
    }

    /// A varint count, refused when its items cannot fit in what remains.
    ///
    /// `min_item_bytes` is the smallest number of bytes one counted item can occupy — `8` for a
    /// fixed-width value, `1` for a length-prefixed one, `0` where the items are bit-packed and
    /// a separate width bounds them. A count that fails this check is corruption, and refusing
    /// it here is what stops a corrupt length from becoming an allocation.
    pub(crate) fn count(&mut self, field: &str, min_item_bytes: usize) -> Result<usize> {
        let count = self.varint(field)?;
        let count = usize::try_from(count).map_err(|_| {
            Error::corruption(
                self.context,
                format!("{field} is {count}, more than this machine can count"),
            )
        })?;
        // An overflowing product is not "unbounded", it is "far too large": say so rather than
        // letting the check fall through, which is how such a guard silently stops guarding.
        match count.checked_mul(min_item_bytes) {
            Some(least) if least <= self.remaining() => Ok(count),
            Some(least) => Err(Error::corruption(
                self.context,
                format!(
                    "{field} is {count}, which needs at least {least} bytes but {} remain",
                    self.remaining()
                ),
            )),
            None => Err(Error::corruption(
                self.context,
                format!("{field} is {count}, whose items cannot be counted in bytes"),
            )),
        }
    }

    /// Consumes the cursor, refusing bytes nobody read.
    ///
    /// Trailing bytes mean the reader and the writer disagree about the layout, which is exactly
    /// the class of bug a format version exists to turn into an error.
    pub(crate) fn finish(self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(Error::corruption(
                self.context,
                format!("{} bytes past the end of the region", self.remaining()),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn reads_what_is_there_and_refuses_what_is_not() {
        let bytes = [0x01, 0x02, 0x00, 0x00, 0x00, 0x80];
        let mut cursor = Cursor::new(&bytes, "test");
        assert_eq!(cursor.u8("a").unwrap(), 1);
        assert_eq!(cursor.u32_le("b").unwrap(), 2);
        assert_eq!(cursor.remaining(), 1);
        // 0x80 is a varint with the continuation bit set and nothing following it.
        assert!(cursor.varint("c").unwrap_err().is_corruption());
    }

    #[test]
    fn an_empty_cursor_never_panics() {
        let mut cursor = Cursor::new(&[], "test");
        assert!(cursor.u8("a").is_err());
        assert!(cursor.u32_le("b").is_err());
        assert!(cursor.u64_le("c").is_err());
        assert!(cursor.varint("d").is_err());
        assert!(cursor.bytes(1, "e").is_err());
        assert!(Cursor::new(&[], "test").finish().is_ok());
    }

    /// The guard the fuzz test depends on: a huge count is refused before anything is sized.
    #[test]
    fn a_count_larger_than_the_bytes_behind_it_is_refused() {
        let mut bytes = Vec::new();
        esker_base::varint::put_u64(1_000_000, &mut bytes);
        bytes.extend_from_slice(&[0u8; 4]);

        let mut cursor = Cursor::new(&bytes, "test");
        let error = cursor.count("values", 8).unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("at least"), "{error}");

        // The same count with no minimum size is a caller's business, not the cursor's.
        let mut cursor = Cursor::new(&bytes, "test");
        assert_eq!(cursor.count("bits", 0).unwrap(), 1_000_000);
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let bytes = [1u8, 2];
        let mut cursor = Cursor::new(&bytes, "test");
        cursor.u8("a").unwrap();
        assert!(cursor.finish().unwrap_err().is_corruption());
    }
}
