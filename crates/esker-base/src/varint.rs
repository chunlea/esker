//! LEB128 variable-length integers, the length prefix of every hand-rolled format in Esker.
//!
//! Unsigned values are little-endian base-128: seven payload bits per byte, the high bit set
//! on every byte but the last. Signed values are zigzagged first, so small magnitudes of
//! either sign stay short.
//!
//! Decoding is **strict**. A truncated buffer, a value that overflows the target width, and a
//! non-canonical (overlong) encoding are all errors. Esker's own encoder never produces an
//! overlong form, so accepting one would only mean accepting corruption — and these bytes
//! come off disk and off the network, where `CLAUDE.md` invariant 9 says we must not panic
//! and must not silently continue.

use thiserror::Error;

/// The longest LEB128 encoding of a `u64`: 64 bits at 7 bits per byte.
pub const MAX_LEN_U64: usize = 10;

/// The longest LEB128 encoding of a `u32`.
pub const MAX_LEN_U32: usize = 5;

/// Why a varint could not be decoded.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VarintError {
    /// The buffer ended before a byte with the continuation bit clear.
    #[error("truncated varint: buffer ended after {read} bytes with no terminator")]
    Truncated {
        /// How many bytes were consumed before the buffer ran out.
        read: usize,
    },
    /// The encoded value does not fit in the requested width.
    #[error("varint overflows {bits} bits")]
    Overflow {
        /// Width of the target integer.
        bits: u32,
    },
    /// The value was encoded in more bytes than it needs. Esker's encoder is canonical, so
    /// this means the bytes were damaged or written by something else.
    #[error("non-canonical varint: {len} bytes encode a value that needs {want}")]
    NotCanonical {
        /// Length actually read.
        len: usize,
        /// Length the canonical encoding would have used.
        want: usize,
    },
}

/// Bytes the canonical encoding of `value` occupies.
#[must_use]
pub fn encoded_len_u64(value: u64) -> usize {
    // 1 byte per 7 bits, rounded up, with zero encoded in one byte.
    let significant = u64::BITS - value.leading_zeros();
    (significant as usize).max(1).div_ceil(7)
}

/// Appends the canonical LEB128 encoding of `value` to `out`.
pub fn put_u64(value: u64, out: &mut Vec<u8>) {
    let mut rest = value;
    while rest >= 0x80 {
        // The cast keeps the low seven bits; the continuation bit marks "more follows".
        #[allow(clippy::cast_possible_truncation, reason = "the mask keeps 7 bits")]
        out.push((rest as u8) | 0x80);
        rest >>= 7;
    }
    #[allow(clippy::cast_possible_truncation, reason = "rest < 0x80 here")]
    out.push(rest as u8);
}

/// Appends the canonical LEB128 encoding of a `u32`.
pub fn put_u32(value: u32, out: &mut Vec<u8>) {
    put_u64(u64::from(value), out);
}

/// Decodes a `u64` from the front of `buf`, returning the value and the bytes consumed.
///
/// Fails on truncation, on a value wider than 64 bits, and on an overlong encoding.
pub fn get_u64(buf: &[u8]) -> Result<(u64, usize), VarintError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;

    for (index, &byte) in buf.iter().enumerate() {
        if index == MAX_LEN_U64 {
            return Err(VarintError::Overflow { bits: 64 });
        }
        let payload = u64::from(byte & 0x7F);
        // The tenth byte carries a single bit of a 64-bit value; anything above it overflows.
        if shift == 63 && payload > 1 {
            return Err(VarintError::Overflow { bits: 64 });
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            let len = index + 1;
            let want = encoded_len_u64(value);
            if len != want {
                return Err(VarintError::NotCanonical { len, want });
            }
            return Ok((value, len));
        }
        shift += 7;
    }

    Err(VarintError::Truncated { read: buf.len() })
}

/// Decodes a `u32`. A value that does not fit in 32 bits is an error, not a truncation.
pub fn get_u32(buf: &[u8]) -> Result<(u32, usize), VarintError> {
    let (value, len) = get_u64(buf)?;
    let narrowed = u32::try_from(value).map_err(|_| VarintError::Overflow { bits: 32 })?;
    Ok((narrowed, len))
}

/// Maps a signed value onto an unsigned one so that small magnitudes encode short:
/// `0, -1, 1, -2, 2 …` become `0, 1, 2, 3, 4 …`.
#[must_use]
pub fn zigzag_encode(value: i64) -> u64 {
    // Arithmetic shift produces all-ones for negatives, all-zeros for non-negatives.
    #[allow(clippy::cast_sign_loss, reason = "the xor is the definition of zigzag")]
    {
        ((value << 1) ^ (value >> 63)) as u64
    }
}

/// Inverse of [`zigzag_encode`].
#[must_use]
pub fn zigzag_decode(value: u64) -> i64 {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "the xor is the definition of zigzag"
    )]
    {
        ((value >> 1) as i64) ^ -((value & 1) as i64)
    }
}

/// Appends a zigzagged LEB128 `i64`.
pub fn put_i64(value: i64, out: &mut Vec<u8>) {
    put_u64(zigzag_encode(value), out);
}

/// Decodes a zigzagged LEB128 `i64`.
pub fn get_i64(buf: &[u8]) -> Result<(i64, usize), VarintError> {
    let (raw, len) = get_u64(buf)?;
    Ok((zigzag_decode(raw), len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_u64(value, &mut out);
        out
    }

    /// Frozen byte layouts. These bytes are on disk and on the wire; a change here is a
    /// format change and needs an ADR.
    #[test]
    fn golden_encodings() {
        assert_eq!(encode(0), [0x00]);
        assert_eq!(encode(1), [0x01]);
        assert_eq!(encode(127), [0x7F]);
        assert_eq!(encode(128), [0x80, 0x01]);
        assert_eq!(encode(300), [0xAC, 0x02]);
        assert_eq!(encode(16_383), [0xFF, 0x7F]);
        assert_eq!(encode(16_384), [0x80, 0x80, 0x01]);
        assert_eq!(
            encode(u64::MAX),
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]
        );
    }

    #[test]
    fn round_trips_at_every_length_boundary() {
        let mut cases = vec![0u64, 1, u64::MAX];
        for shift in 0..64 {
            let boundary = 1u64 << shift;
            cases.push(boundary - 1);
            cases.push(boundary);
            cases.push(boundary + 1);
        }
        for value in cases {
            let bytes = encode(value);
            assert_eq!(bytes.len(), encoded_len_u64(value), "length of {value}");
            assert!(bytes.len() <= MAX_LEN_U64);
            assert_eq!(get_u64(&bytes), Ok((value, bytes.len())));
        }
    }

    #[test]
    fn decodes_only_the_prefix_it_needs() {
        let mut buf = encode(300);
        buf.extend_from_slice(b"trailing");
        assert_eq!(get_u64(&buf), Ok((300, 2)));
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        assert_eq!(get_u64(&[]), Err(VarintError::Truncated { read: 0 }));
        assert_eq!(get_u64(&[0x80]), Err(VarintError::Truncated { read: 1 }));
        assert_eq!(
            get_u64(&[0x80, 0x80, 0x80]),
            Err(VarintError::Truncated { read: 3 })
        );
    }

    #[test]
    fn overflow_is_rejected() {
        // Eleven continuation bytes: wider than any u64.
        assert_eq!(
            get_u64(&[0x80; 11]),
            Err(VarintError::Overflow { bits: 64 })
        );
        // Ten bytes whose last byte carries more than the single bit that is left.
        let mut too_wide = [0xFFu8; 10];
        too_wide[9] = 0x02;
        assert_eq!(get_u64(&too_wide), Err(VarintError::Overflow { bits: 64 }));
        // Fits in 64 but not in 32.
        assert_eq!(
            get_u32(&encode(u64::from(u32::MAX) + 1)),
            Err(VarintError::Overflow { bits: 32 })
        );
    }

    #[test]
    fn overlong_encodings_are_rejected() {
        // `0` written in two bytes: valid LEB128, not canonical, so it is corruption.
        assert_eq!(
            get_u64(&[0x80, 0x00]),
            Err(VarintError::NotCanonical { len: 2, want: 1 })
        );
        assert_eq!(
            get_u64(&[0xFF, 0x80, 0x00]),
            Err(VarintError::NotCanonical { len: 3, want: 1 })
        );
    }

    #[test]
    fn zigzag_round_trips_and_keeps_small_values_small() {
        for value in [
            0i64,
            -1,
            1,
            -2,
            2,
            i64::MIN,
            i64::MAX,
            -1_000_000,
            1_000_000,
        ] {
            assert_eq!(zigzag_decode(zigzag_encode(value)), value);
        }
        assert_eq!(zigzag_encode(0), 0);
        assert_eq!(zigzag_encode(-1), 1);
        assert_eq!(zigzag_encode(1), 2);
        assert_eq!(zigzag_encode(-2), 3);
        // A small negative must not cost ten bytes.
        let mut out = Vec::new();
        put_i64(-1, &mut out);
        assert_eq!(out, [0x01]);
    }
}
