//! The two halves of every message body: an [`Encoder`] that appends fields and a [`Decoder`]
//! that reads them back.
//!
//! There is no schema and no derive. Every message in [`crate::messages`] writes its own
//! `encode`/`decode` pair against these primitives, which is what ADR 0002 chose over `serde`
//! or `prost`: the bytes are the specification, and a golden test pins them.
//!
//! Three rules the primitives enforce so that no message has to remember them:
//!
//! * **A decode never reads past the end.** Every getter checks first and returns
//!   [`DecodeError::Truncated`] otherwise, so a body cut short by a corrupt length is an error
//!   value rather than a panic (`CLAUDE.md` invariant 9).
//! * **Trailing bytes are an error.** [`Decoder::finish`] refuses a body with anything left in
//!   it. A message with extra bytes on the end is a *different* message — probably one written
//!   by a peer at another version — and guessing which fields to believe is how a protocol
//!   silently splits in two (`docs/DESIGN.md` §9).
//! * **The encoding is canonical.** Integers are LEB128 varints with no redundant
//!   continuation bytes, so one value has exactly one encoding and a golden file means
//!   something.

use esker_base::varint;

/// Why a body could not be read.
///
/// Separate from [`crate::ProtoError`] because the same failure means different things in
/// different places: a server that cannot decode a request answers
/// [`crate::ProtoError::InvalidRequest`], while the frame layer, which is reading bytes whose
/// checksum already passed, reports [`crate::ProtoError::Corrupt`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The body ended in the middle of a field.
    #[error("{field}: needed {needed} more bytes, found {found}")]
    Truncated {
        /// The field being read.
        field: &'static str,
        /// How many bytes it needed.
        needed: usize,
        /// How many were left.
        found: usize,
    },

    /// A varint was longer than ten bytes, or its value did not fit the field.
    #[error("{field}: malformed varint")]
    Varint {
        /// The field being read.
        field: &'static str,
    },

    /// A string field held bytes that are not UTF-8.
    #[error("{field}: not valid UTF-8")]
    Utf8 {
        /// The field being read.
        field: &'static str,
    },

    /// A tag byte or word this version does not define: an unknown method, frame kind, error
    /// code or enum discriminant. Never ignored, never treated as a default.
    #[error("{what}: unknown tag {tag}")]
    UnknownTag {
        /// What was being decoded — `method`, `peer role`, `error code`.
        what: &'static str,
        /// The value that was not recognised.
        tag: u64,
    },

    /// The field decoded, but its value is impossible: a count larger than the body could
    /// hold, a boolean that is neither 0 nor 1.
    #[error("{field}: {detail}")]
    InvalidValue {
        /// The field being read.
        field: &'static str,
        /// What was wrong with it.
        detail: String,
    },

    /// The message decoded and the body was not finished. See the module docs.
    #[error("{remaining} trailing bytes after the message")]
    Trailing {
        /// How many bytes were left over.
        remaining: usize,
    },
}

impl DecodeError {
    /// Names an impossible value, for the messages that check their own invariants.
    pub fn invalid(field: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidValue {
            field,
            detail: detail.into(),
        }
    }
}

/// Appends fields to a body.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    /// An empty body.
    #[must_use]
    pub fn new() -> Self {
        Self { out: Vec::new() }
    }

    /// An empty body with room for `capacity` bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
        }
    }

    /// One byte.
    pub fn put_u8(&mut self, value: u8) {
        self.out.push(value);
    }

    /// Two bytes, little-endian. Used for the tag every body starts with.
    pub fn put_u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Four bytes, little-endian.
    pub fn put_u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Eight bytes, little-endian.
    pub fn put_u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// A LEB128 varint. Small numbers — which is nearly all of them — cost one byte.
    pub fn put_varint(&mut self, value: u64) {
        varint::put_u64(value, &mut self.out);
    }

    /// One byte, `0` or `1`.
    pub fn put_bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }

    /// A length-prefixed byte string.
    pub fn put_bytes(&mut self, value: &[u8]) {
        self.put_varint(value.len() as u64);
        self.out.extend_from_slice(value);
    }

    /// A length-prefixed string. UTF-8 on the wire, checked on the way back in.
    pub fn put_str(&mut self, value: &str) {
        self.put_bytes(value.as_bytes());
    }

    /// A present flag followed by the value if there is one.
    ///
    /// An absent optional is one byte. This is how "the key was not there" is told from "the
    /// key was there and its value is empty", a distinction `RawKv` `Get` depends on.
    pub fn put_opt_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(bytes) => {
                self.put_bool(true);
                self.put_bytes(bytes);
            }
            None => self.put_bool(false),
        }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.out
    }

    /// How many bytes have been written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.out.len()
    }

    /// True when nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// The finished body.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Bytes a varint occupies, which is what [`Encoder::put_varint`] writes.
///
/// Public because a client cutting a batch into frames has to know what one more key costs
/// (#99), and a second implementation of that arithmetic is what #98 was.
#[must_use]
pub fn varint_len(value: u64) -> usize {
    varint::encoded_len_u64(value)
}

/// Bytes a length-prefixed byte string occupies: its length as a varint, then the bytes
/// ([`Encoder::put_bytes`]). Public for the reason [`varint_len`] is.
#[must_use]
pub fn bytes_len(value: &[u8]) -> usize {
    varint_len(value.len() as u64) + value.len()
}

/// Bytes an optional byte string occupies: the present flag, and the string when there is one
/// ([`Encoder::put_opt_bytes`]).
pub(crate) fn opt_bytes_len(value: Option<&[u8]>) -> usize {
    BOOL_LEN + value.map_or(0, bytes_len)
}

/// One byte, `0` or `1` ([`Encoder::put_bool`]).
pub(crate) const BOOL_LEN: usize = 1;

/// One byte: the tag a mutation or a request kind starts with.
pub(crate) const TAG_LEN: usize = 1;

/// Reads fields back out of a body.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Decoder<'a> {
    /// Reads from `bytes`, starting at the beginning.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How many bytes are left unread.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// Where the cursor is, for an error message that has to name a position.
    #[must_use]
    pub fn position(&self) -> usize {
        self.at
    }

    fn take(&mut self, field: &'static str, count: usize) -> Result<&'a [u8], DecodeError> {
        let Some(end) = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
        else {
            return Err(DecodeError::Truncated {
                field,
                needed: count,
                found: self.remaining(),
            });
        };
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// One byte.
    pub fn get_u8(&mut self, field: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(field, 1)?[0])
    }

    /// Two bytes, little-endian.
    pub fn get_u16(&mut self, field: &'static str) -> Result<u16, DecodeError> {
        let bytes = self.take(field, 2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Four bytes, little-endian.
    pub fn get_u32(&mut self, field: &'static str) -> Result<u32, DecodeError> {
        let bytes = self.take(field, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Eight bytes, little-endian.
    pub fn get_u64(&mut self, field: &'static str) -> Result<u64, DecodeError> {
        let bytes = self.take(field, 8)?;
        let mut value = [0u8; 8];
        value.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(value))
    }

    /// A LEB128 varint.
    pub fn get_varint(&mut self, field: &'static str) -> Result<u64, DecodeError> {
        let (value, used) =
            varint::get_u64(&self.bytes[self.at..]).map_err(|_| DecodeError::Varint { field })?;
        self.at += used;
        Ok(value)
    }

    /// A varint that has to fit in a `u32`.
    pub fn get_varint_u32(&mut self, field: &'static str) -> Result<u32, DecodeError> {
        let value = self.get_varint(field)?;
        u32::try_from(value).map_err(|_| DecodeError::InvalidValue {
            field,
            detail: format!("{value} does not fit in 32 bits"),
        })
    }

    /// One byte, which must be `0` or `1`. Any other value is an error rather than "truthy":
    /// a byte that is neither is a message this version does not understand.
    pub fn get_bool(&mut self, field: &'static str) -> Result<bool, DecodeError> {
        match self.get_u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::InvalidValue {
                field,
                detail: format!("boolean byte {other}"),
            }),
        }
    }

    /// A length-prefixed byte string, borrowed from the body.
    pub fn get_bytes(&mut self, field: &'static str) -> Result<&'a [u8], DecodeError> {
        let len = self.get_varint(field)?;
        // Checked against what is left rather than against a constant: a length that claims
        // more than the body holds is refused before anything is allocated for it.
        let len = usize::try_from(len).map_err(|_| DecodeError::InvalidValue {
            field,
            detail: format!("length {len} does not fit in memory"),
        })?;
        self.take(field, len)
    }

    /// A length-prefixed UTF-8 string, borrowed from the body.
    pub fn get_str(&mut self, field: &'static str) -> Result<&'a str, DecodeError> {
        let bytes = self.get_bytes(field)?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::Utf8 { field })
    }

    /// An optional byte string: a present flag, then the value.
    pub fn get_opt_bytes(&mut self, field: &'static str) -> Result<Option<&'a [u8]>, DecodeError> {
        if self.get_bool(field)? {
            Ok(Some(self.get_bytes(field)?))
        } else {
            Ok(None)
        }
    }

    /// Reads a repeat count and checks it against what is left.
    ///
    /// A count is attacker-controlled, and `Vec::with_capacity(count)` on a two-byte varint
    /// asks for four gigabytes. Every element costs at least one byte, so a count larger than
    /// the remaining body is impossible and is refused before the allocation.
    pub fn get_count(&mut self, field: &'static str) -> Result<usize, DecodeError> {
        let count = self.get_varint(field)?;
        let count = usize::try_from(count).map_err(|_| DecodeError::InvalidValue {
            field,
            detail: format!("count {count} does not fit in memory"),
        })?;
        if count > self.remaining() {
            return Err(DecodeError::InvalidValue {
                field,
                detail: format!("count {count} exceeds the {} bytes left", self.remaining()),
            });
        }
        Ok(count)
    }

    /// Finishes the body, refusing anything left over. See the module docs.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(DecodeError::Trailing {
                remaining: self.remaining(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DecodeError, Decoder, Encoder};

    #[test]
    fn every_primitive_round_trips() {
        let mut encoder = Encoder::new();
        encoder.put_u8(0xAB);
        encoder.put_u16(0x1234);
        encoder.put_u32(0xDEAD_BEEF);
        encoder.put_u64(u64::MAX);
        encoder.put_varint(300);
        encoder.put_bool(true);
        encoder.put_bytes(b"hello");
        encoder.put_str("wire");
        encoder.put_opt_bytes(Some(b""));
        encoder.put_opt_bytes(None);
        let body = encoder.finish();

        let mut decoder = Decoder::new(&body);
        assert_eq!(decoder.get_u8("a").unwrap(), 0xAB);
        assert_eq!(decoder.get_u16("b").unwrap(), 0x1234);
        assert_eq!(decoder.get_u32("c").unwrap(), 0xDEAD_BEEF);
        assert_eq!(decoder.get_u64("d").unwrap(), u64::MAX);
        assert_eq!(decoder.get_varint("e").unwrap(), 300);
        assert!(decoder.get_bool("f").unwrap());
        assert_eq!(decoder.get_bytes("g").unwrap(), b"hello");
        assert_eq!(decoder.get_str("h").unwrap(), "wire");
        assert_eq!(decoder.get_opt_bytes("i").unwrap(), Some(&b""[..]));
        assert_eq!(decoder.get_opt_bytes("j").unwrap(), None);
        decoder.finish().unwrap();
    }

    /// An empty value and an absent one are different, and a protocol that confuses them
    /// cannot answer "is this key present?".
    #[test]
    fn an_empty_value_is_not_an_absent_one() {
        let mut present = Encoder::new();
        present.put_opt_bytes(Some(b""));
        let mut absent = Encoder::new();
        absent.put_opt_bytes(None);
        assert_ne!(present.finish(), absent.finish());
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut encoder = Encoder::new();
        encoder.put_u8(1);
        let mut body = encoder.finish();
        body.push(0);

        let mut decoder = Decoder::new(&body);
        assert_eq!(decoder.get_u8("a").unwrap(), 1);
        assert_eq!(
            decoder.finish().unwrap_err(),
            DecodeError::Trailing { remaining: 1 }
        );
    }

    /// Every getter has to stop at the end of the body. Truncating a well-formed body at
    /// every offset is the cheapest way to check that none of them reads past it.
    #[test]
    fn truncation_at_any_offset_is_an_error_not_a_panic() {
        let mut encoder = Encoder::new();
        encoder.put_u64(7);
        encoder.put_bytes(b"a longer value than a length byte");
        encoder.put_str("name");
        encoder.put_opt_bytes(Some(b"v"));
        let body = encoder.finish();

        for cut in 0..body.len() {
            let mut decoder = Decoder::new(&body[..cut]);
            let result = (|| {
                decoder.get_u64("a")?;
                decoder.get_bytes("b")?;
                decoder.get_str("c")?;
                decoder.get_opt_bytes("d")?;
                decoder.finish()
            })();
            assert!(result.is_err(), "a body cut to {cut} bytes decoded");
        }
    }

    /// A length prefix is attacker-controlled. A huge one must be refused by what is actually
    /// in the buffer, not by a limit someone remembered to configure.
    #[test]
    fn an_absurd_length_prefix_is_refused_without_allocating() {
        let mut encoder = Encoder::new();
        encoder.put_varint(u64::MAX);
        let body = encoder.finish();
        let error = Decoder::new(&body).get_bytes("value").unwrap_err();
        assert!(matches!(
            error,
            DecodeError::InvalidValue { .. } | DecodeError::Truncated { .. }
        ));
    }

    /// The same reasoning for a repeat count, which would otherwise become a `with_capacity`.
    #[test]
    fn a_count_larger_than_the_body_is_refused() {
        let mut encoder = Encoder::new();
        encoder.put_varint(1_000_000);
        encoder.put_u8(0);
        let body = encoder.finish();
        assert!(matches!(
            Decoder::new(&body).get_count("keys").unwrap_err(),
            DecodeError::InvalidValue { .. }
        ));
    }

    #[test]
    fn a_boolean_that_is_neither_zero_nor_one_is_an_error() {
        for byte in [2u8, 0xFF] {
            let error = Decoder::new(&[byte]).get_bool("sync").unwrap_err();
            assert!(matches!(error, DecodeError::InvalidValue { .. }));
        }
    }

    #[test]
    fn a_string_field_checks_its_utf8() {
        let mut encoder = Encoder::new();
        encoder.put_bytes(&[0xFF, 0xFE]);
        let body = encoder.finish();
        assert_eq!(
            Decoder::new(&body).get_str("reason").unwrap_err(),
            DecodeError::Utf8 { field: "reason" }
        );
    }

    /// Varints are the only variable-width integer here, so the ten-byte limit is what stops
    /// a decoder from walking off the end of a body one continuation bit at a time.
    #[test]
    fn an_endless_varint_is_an_error() {
        let body = [0x80u8; 16];
        assert_eq!(
            Decoder::new(&body).get_varint("id").unwrap_err(),
            DecodeError::Varint { field: "id" }
        );
    }

    #[test]
    fn a_varint_too_wide_for_its_field_is_an_error() {
        let mut encoder = Encoder::new();
        encoder.put_varint(u64::from(u32::MAX) + 1);
        let body = encoder.finish();
        assert!(matches!(
            Decoder::new(&body).get_varint_u32("limit").unwrap_err(),
            DecodeError::InvalidValue { .. }
        ));
    }
}
