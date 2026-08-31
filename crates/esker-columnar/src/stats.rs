//! Per-column-chunk statistics: how many NULLs, and the range the values span.
//!
//! They live in the **footer**, not in the chunk. That is the whole point of them: a reader that
//! wants to know whether a stripe can contain a matching row must be able to answer from what it
//! already read when it opened the file. Statistics inside the chunk would mean reading the chunk
//! to find out whether to read the chunk.
//!
//! ```text
//! stats := null_count:varint ++ flags:u8 ++ [min_len:varint ++ min] ++ [max_len:varint ++ max]
//! ```
//!
//! `flags` bit 0 says a minimum is present, bit 1 a maximum, and bits 2 and 3 say that the
//! respective bound is **truncated** — a lower bound at or below the true minimum, an upper
//! bound at or above the true maximum, rather than the value itself. Any other bit set is
//! corruption rather than something to ignore: a reader that skips a flag it does not understand
//! is a reader that has silently disagreed with the writer (ADR 0002).
//!
//! The truncation bits exist now, before anything prunes, because the first pruner to conclude
//! `column = 'x'` from an *approximate* bound returns the wrong rows, and the bit that stops it
//! costs nothing to write today and a format version to add later.
//!
//! Bounds are stored as bytes whose meaning is the column's type: eight little-endian bytes for
//! an integer or a timestamp, one byte for a boolean, the raw value for text and bytes. Their
//! *order* is the type's own — [`crate::stats`] does not compare them, milestone 2 does — so
//! this module fixes only the width and the truncation rule.

use esker_base::varint;

use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::format::MAX_BOUND_LEN;

/// Bit 0 of `flags`: a minimum is present.
const FLAG_HAS_MIN: u8 = 1 << 0;
/// Bit 1: a maximum is present.
const FLAG_HAS_MAX: u8 = 1 << 1;
/// Bit 2: the minimum is a lower bound rather than a value.
const FLAG_MIN_TRUNCATED: u8 = 1 << 2;
/// Bit 3: the maximum is an upper bound rather than a value.
const FLAG_MAX_TRUNCATED: u8 = 1 << 3;
/// Every bit this version defines. A byte with any other bit set is corruption.
const FLAGS_KNOWN: u8 = FLAG_HAS_MIN | FLAG_HAS_MAX | FLAG_MIN_TRUNCATED | FLAG_MAX_TRUNCATED;

/// One end of a column chunk's range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The bound's bytes, in the column type's own stored form.
    pub bytes: Vec<u8>,
    /// Whether this is an approximation rather than a value found in the chunk.
    ///
    /// A truncated minimum sorts at or below every value present; a truncated maximum sorts at or
    /// above every value present. Neither may be compared for equality with a literal.
    pub truncated: bool,
}

impl Bound {
    /// An exact bound: a value that really is in the chunk.
    #[must_use]
    pub fn exact(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            truncated: false,
        }
    }
}

/// What is known about one column chunk without reading it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnStats {
    /// How many of the chunk's rows are NULL.
    pub null_count: u64,
    /// The smallest value present, absent when every row is NULL — or, for a floating-point
    /// column, when every value present is `NaN`.
    pub min: Option<Bound>,
    /// The largest value present, absent for the same reasons as [`ColumnStats::min`] and for one
    /// more: a truncated maximum that no 64-byte prefix can bound from above.
    pub max: Option<Bound>,
}

impl ColumnStats {
    /// Statistics for a chunk nothing is known about: no NULLs counted, no bounds.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether both bounds are values found in the chunk rather than approximations.
    ///
    /// The question a pruner asks before concluding equality from a bound.
    #[must_use]
    pub fn bounds_are_exact(&self) -> bool {
        !self.min.as_ref().is_some_and(|bound| bound.truncated)
            && !self.max.as_ref().is_some_and(|bound| bound.truncated)
    }

    /// Appends the encoding the module documents.
    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        varint::put_u64(self.null_count, out);
        let mut flags = 0u8;
        if let Some(min) = &self.min {
            flags |= FLAG_HAS_MIN;
            if min.truncated {
                flags |= FLAG_MIN_TRUNCATED;
            }
        }
        if let Some(max) = &self.max {
            flags |= FLAG_HAS_MAX;
            if max.truncated {
                flags |= FLAG_MAX_TRUNCATED;
            }
        }
        out.push(flags);
        for bound in [self.min.as_ref(), self.max.as_ref()].into_iter().flatten() {
            varint::put_u64(bound.bytes.len() as u64, out);
            out.extend_from_slice(&bound.bytes);
        }
    }

    /// Reads statistics from the front of a footer's chunk entry.
    pub(crate) fn decode_from(cursor: &mut Cursor<'_>) -> Result<Self> {
        let null_count = cursor.varint("chunk null count")?;
        let flags = cursor.u8("chunk stats flags")?;
        if flags & !FLAGS_KNOWN != 0 {
            return Err(Error::corruption(
                "chunk stats",
                format!("flags {flags:#04x} set a bit this version does not define"),
            ));
        }

        let mut read_bound = |present: bool,
                              truncated: bool,
                              field: &str|
         -> Result<Option<Bound>> {
            if !present {
                return Ok(None);
            }
            let len = cursor.count(field, 1)?;
            if len > MAX_BOUND_LEN {
                return Err(Error::corruption(
                    "chunk stats",
                    format!("{field} is {len} bytes, over the {MAX_BOUND_LEN} this format stores"),
                ));
            }
            Ok(Some(Bound {
                bytes: cursor.bytes(len, field)?.to_vec(),
                truncated,
            }))
        };

        let min = read_bound(
            flags & FLAG_HAS_MIN != 0,
            flags & FLAG_MIN_TRUNCATED != 0,
            "chunk min",
        )?;
        let max = read_bound(
            flags & FLAG_HAS_MAX != 0,
            flags & FLAG_MAX_TRUNCATED != 0,
            "chunk max",
        )?;

        // A truncation bit without its presence bit is a writer and a reader disagreeing about
        // what was written, which is exactly what a flags byte must not be allowed to hide.
        if (flags & FLAG_MIN_TRUNCATED != 0 && min.is_none())
            || (flags & FLAG_MAX_TRUNCATED != 0 && max.is_none())
        {
            return Err(Error::corruption(
                "chunk stats",
                format!("flags {flags:#04x} mark a bound truncated that is not present"),
            ));
        }

        Ok(Self {
            null_count,
            min,
            max,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Bound, ColumnStats};
    use crate::cursor::Cursor;
    use crate::format::MAX_BOUND_LEN;

    fn round_trip(stats: &ColumnStats) -> ColumnStats {
        let mut bytes = Vec::new();
        stats.encode_to(&mut bytes);
        let mut cursor = Cursor::new(&bytes, "test");
        let decoded = ColumnStats::decode_from(&mut cursor).unwrap();
        cursor.finish().unwrap();
        decoded
    }

    #[test]
    fn every_shape_of_statistics_round_trips() {
        let cases = [
            ColumnStats::empty(),
            ColumnStats {
                null_count: 7,
                min: None,
                max: None,
            },
            ColumnStats {
                null_count: 0,
                min: Some(Bound::exact(1i64.to_le_bytes().to_vec())),
                max: Some(Bound::exact(9i64.to_le_bytes().to_vec())),
            },
            ColumnStats {
                null_count: 3,
                min: Some(Bound {
                    bytes: b"aaa".to_vec(),
                    truncated: true,
                }),
                max: None,
            },
            ColumnStats {
                null_count: 0,
                min: Some(Bound::exact(Vec::new())),
                max: Some(Bound {
                    bytes: vec![0xff; MAX_BOUND_LEN],
                    truncated: true,
                }),
            },
        ];
        for stats in cases {
            assert_eq!(round_trip(&stats), stats);
            assert_eq!(
                stats.bounds_are_exact(),
                !stats.min.as_ref().is_some_and(|b| b.truncated)
                    && !stats.max.as_ref().is_some_and(|b| b.truncated)
            );
        }
    }

    #[test]
    fn an_unknown_flag_bit_is_corruption_not_a_skip() {
        let mut bytes = Vec::new();
        ColumnStats::empty().encode_to(&mut bytes);
        let flags_at = bytes.len() - 1;
        bytes[flags_at] |= 0x80;
        let error = ColumnStats::decode_from(&mut Cursor::new(&bytes, "test")).unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("does not define"), "{error}");
    }

    #[test]
    fn a_truncation_bit_without_its_bound_is_corruption() {
        let mut bytes = Vec::new();
        ColumnStats::empty().encode_to(&mut bytes);
        let flags_at = bytes.len() - 1;
        bytes[flags_at] |= 0x04;
        let error = ColumnStats::decode_from(&mut Cursor::new(&bytes, "test")).unwrap_err();
        assert!(error.to_string().contains("not present"), "{error}");
    }

    #[test]
    fn an_over_long_bound_is_refused() {
        let stats = ColumnStats {
            null_count: 0,
            min: Some(Bound::exact(vec![0u8; MAX_BOUND_LEN + 1])),
            max: None,
        };
        let mut bytes = Vec::new();
        stats.encode_to(&mut bytes);
        let error = ColumnStats::decode_from(&mut Cursor::new(&bytes, "test")).unwrap_err();
        assert!(error.to_string().contains("over the"), "{error}");
    }

    #[test]
    fn truncated_bytes_never_look_like_a_panic() {
        let stats = ColumnStats {
            null_count: 1,
            min: Some(Bound::exact(b"abc".to_vec())),
            max: Some(Bound::exact(b"xyz".to_vec())),
        };
        let mut bytes = Vec::new();
        stats.encode_to(&mut bytes);
        for cut in 0..bytes.len() {
            let mut cursor = Cursor::new(&bytes[..cut], "test");
            let _ = ColumnStats::decode_from(&mut cursor);
        }
    }
}
