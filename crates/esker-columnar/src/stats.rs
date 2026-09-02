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

use crate::column::{Column, ColumnData};
use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::format::MAX_BOUND_LEN;
use crate::value::{ColumnType, Value, pg_cmp_f32, pg_cmp_f64};

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

    /// The bound of an `Int8` or `TimestampTz` column, or `None` if the bytes are not one.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.bytes
            .as_slice()
            .try_into()
            .ok()
            .map(i64::from_le_bytes)
    }

    /// The bound of a `Double` column, or `None` if the bytes are not one.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.bytes
            .as_slice()
            .try_into()
            .ok()
            .map(f64::from_le_bytes)
    }

    /// The bound of a `Bool` column, or `None` if the bytes are not one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self.bytes.as_slice() {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }

    /// This bound as a value of type `ty`, comparable with [`crate::Value::pg_cmp`].
    ///
    /// A text bound comes back as [`Value::Bytea`] rather than [`Value::Text`], on purpose: a
    /// truncated one is a prefix and may split a code point, so it is not valid UTF-8 and is not
    /// a *value* at all. Comparison does not care — text and bytes both compare as bytes here —
    /// and pretending otherwise would mean a bound that cannot be read back.
    #[must_use]
    pub fn as_value(&self, ty: ColumnType) -> Option<Value> {
        let fixed = || <[u8; 8]>::try_from(self.bytes.as_slice()).ok();
        Some(match ty {
            ColumnType::Int8 => Value::Int8(i64::from_le_bytes(fixed()?)),
            ColumnType::Int4 => Value::Int4(i32::from_le_bytes(
                <[u8; 4]>::try_from(self.bytes.as_slice()).ok()?,
            )),
            ColumnType::Int2 => Value::Int2(i16::from_le_bytes(
                <[u8; 2]>::try_from(self.bytes.as_slice()).ok()?,
            )),
            ColumnType::TimestampTz => Value::TimestampTz(i64::from_le_bytes(fixed()?)),
            ColumnType::Timestamp => Value::Timestamp(i64::from_le_bytes(fixed()?)),
            ColumnType::Double => Value::Double(f64::from_le_bytes(fixed()?)),
            ColumnType::Real => Value::Real(f32::from_le_bytes(
                <[u8; 4]>::try_from(self.bytes.as_slice()).ok()?,
            )),
            ColumnType::Date => Value::Date(i32::from_le_bytes(
                <[u8; 4]>::try_from(self.bytes.as_slice()).ok()?,
            )),
            ColumnType::Bool => Value::Bool(self.as_bool()?),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Numeric
            | ColumnType::Bytea => Value::Bytea(self.bytes.clone()),
        })
    }

    /// A lower bound for `value`: the value itself, or a prefix of it, which sorts no higher.
    fn lower(value: &[u8]) -> Self {
        if value.len() <= MAX_BOUND_LEN {
            return Self::exact(value.to_vec());
        }
        Self {
            bytes: value[..MAX_BOUND_LEN].to_vec(),
            truncated: true,
        }
    }

    /// An upper bound for `value`, or `None` when no short one exists.
    ///
    /// Take the first [`MAX_BOUND_LEN`] bytes, drop the trailing `0xFF`s and increment what is
    /// left: the result sorts above every string that starts with that prefix, which includes the
    /// value it came from. A prefix that is *all* `0xFF` has no such successor, and the honest
    /// answer is then no upper bound at all rather than one that is subtly wrong — pruning with a
    /// max that is too low is how a query loses rows.
    fn upper(value: &[u8]) -> Option<Self> {
        if value.len() <= MAX_BOUND_LEN {
            return Some(Self::exact(value.to_vec()));
        }
        let mut bytes = value[..MAX_BOUND_LEN].to_vec();
        while let Some(last) = bytes.last_mut() {
            if *last == 0xff {
                bytes.pop();
            } else {
                *last += 1;
                return Some(Self {
                    bytes,
                    truncated: true,
                });
            }
        }
        None
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

    /// Computes the statistics of a decoded column.
    ///
    /// Called by the writer on the same [`Column`] it is about to encode, so the statistics
    /// describe what was actually stored rather than what was handed in. Bounds are over the
    /// values **present**: a NULL is counted, never compared.
    ///
    /// Two floating-point rules are not the obvious ones, and both are what a pruner needs:
    ///
    /// * **`NaN` is the largest value, not an excluded one.** Parquet excludes it, and Parquet is
    ///   right for IEEE semantics, where every comparison with `NaN` is false and a `NaN` row can
    ///   never match a range predicate. This system uses [`pg_cmp_f64`], PostgreSQL's ordering,
    ///   where `NaN` sorts above `Infinity` and `WHERE x > 5` genuinely matches it — so a chunk
    ///   of `[1.0, NaN]` whose maximum was `1.0` would be pruned out of a query it belongs to,
    ///   and the row would vanish with no error anywhere. **Bounds are computed in the ordering
    ///   the query engine uses**, which is the only rule that makes them safe to prune with.
    /// * **Zero is signed and comparison is not.** A minimum of `0.0` is written as `-0.0` and a
    ///   maximum of `0.0` as `+0.0`, so the bound holds whether a reader compares numerically or
    ///   bitwise.
    ///
    /// Text and byte bounds are compared as **bytes**, which is the order the row side's keys use
    /// (`esker_sql::row`: text sorts by bytes, not by a collation). A truncated text bound may
    /// therefore split a code point and is not valid UTF-8 — it is a bound, not a value, and its
    /// truncation flag says so.
    #[must_use]
    pub fn of(column: &Column) -> Self {
        let null_count = column.nulls().nulls() as u64;
        let (min, max) = match column.data() {
            // At the **column's** width, not the run's: `Int4` and `Int2` ride in the widened
            // integers run, and a bound eight bytes wide is one `Bound::as_value` answers `None`
            // for — which the scan reads as "no bound" and silently stops pruning with.
            ColumnData::Ints(values) => match (values.iter().min(), values.iter().max()) {
                (Some(low), Some(high)) => (
                    Some(Bound::exact(int_bound(*low, column.ty()))),
                    Some(Bound::exact(int_bound(*high, column.ty()))),
                ),
                _ => (None, None),
            },
            ColumnData::Doubles(values) => double_bounds(values),
            ColumnData::Floats(values) => float_bounds(values),
            ColumnData::Bools(values) => {
                if values.is_empty() {
                    (None, None)
                } else {
                    let any_false = values.iter().any(|value| !*value);
                    let any_true = values.iter().any(|value| *value);
                    (
                        Some(Bound::exact(vec![u8::from(!any_false)])),
                        Some(Bound::exact(vec![u8::from(any_true)])),
                    )
                }
            }
            ColumnData::Bytes { offsets, data } => {
                let values = offsets
                    .windows(2)
                    .map(|pair| &data[pair[0] as usize..pair[1] as usize]);
                match values.clone().min().zip(values.max()) {
                    Some((low, high)) => (Some(Bound::lower(low)), Bound::upper(high)),
                    None => (None, None),
                }
            }
        };
        Self {
            null_count,
            min,
            max,
        }
    }

    /// Whether the bounds are shaped for a column of type `ty`.
    ///
    /// A fixed-width type's bound is always its own width and is never truncated; a variable
    /// length one's is at most [`MAX_BOUND_LEN`]. Checked by the writer's tests rather than on the
    /// read path, where the type is already known and a wrong width simply fails to interpret.
    #[must_use]
    pub fn fit(&self, ty: ColumnType) -> bool {
        let width = match ty {
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Double => Some(8),
            // Each at its own width, which is what makes it a different type.
            ColumnType::Int4 | ColumnType::Real | ColumnType::Date => Some(4),
            ColumnType::Int2 => Some(2),
            ColumnType::Bool => Some(1),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            // A `numeric` is stored as its text, so its bounds are **byte bounds of that text**
            // and their order is not the type's: `"10" < "9"` and `"-1.5" < "0"` as bytes, both
            // backwards as numbers. Nothing prunes yet; the first pruner that does must either
            // decode both bounds and compare with `numeric`'s own ordering or skip this type.
            | ColumnType::Numeric
            | ColumnType::Bytea => None,
        };
        [self.min.as_ref(), self.max.as_ref()]
            .into_iter()
            .flatten()
            .all(|bound| match width {
                Some(width) => bound.bytes.len() == width && !bound.truncated,
                None => bound.bytes.len() <= MAX_BOUND_LEN,
            })
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

/// The minimum and maximum of a run of doubles, under the two rules [`ColumnStats::of`] documents.
///
/// Everything here is decided by [`pg_cmp_f64`] rather than by `<`, which is the whole correction:
/// a fold with `partial_cmp` silently drops `NaN`, and dropping it is what made these bounds
/// unsound to prune with.
fn double_bounds(values: &[f64]) -> (Option<Bound>, Option<Bound>) {
    let mut low: Option<f64> = None;
    let mut high: Option<f64> = None;
    for value in values {
        if low.is_none_or(|current| pg_cmp_f64(*value, current).is_lt()) {
            low = Some(*value);
        }
        if high.is_none_or(|current| pg_cmp_f64(*value, current).is_gt()) {
            high = Some(*value);
        }
    }
    let (Some(mut low), Some(mut high)) = (low, high) else {
        return (None, None);
    };

    // `-0.0 == 0.0` under this ordering, so the fold may have kept either. Widen each bound to
    // the side that also holds if somebody compares the stored bytes rather than the values.
    if low == 0.0 {
        low = -0.0;
    }
    if high == 0.0 {
        high = 0.0;
    }
    (
        Some(Bound::exact(low.to_le_bytes().to_vec())),
        Some(Bound::exact(high.to_le_bytes().to_vec())),
    )
}

/// One integer bound, at the width of the column rather than of the run it rides in.
///
/// The value came out of the widened `i64` run and was written there by a column of `ty`, so it
/// fits `ty` by construction; `try_from` is how that is proved rather than assumed, and a value
/// that somehow did not fit keeps the eight-byte form, where [`ColumnStats::fit`] rejects it
/// instead of a narrowing silently inventing a bound that excludes real rows.
fn int_bound(value: i64, ty: ColumnType) -> Vec<u8> {
    match ty {
        ColumnType::Int4 => i32::try_from(value).map_or_else(
            |_| value.to_le_bytes().to_vec(),
            |narrow| narrow.to_le_bytes().to_vec(),
        ),
        ColumnType::Int2 => i16::try_from(value).map_or_else(
            |_| value.to_le_bytes().to_vec(),
            |narrow| narrow.to_le_bytes().to_vec(),
        ),
        // A `date` rides in the same widened run and is four bytes at its own width, like an
        // `int4` — which `ColumnStats::fit` checks, so a bound left at eight would be rejected
        // rather than silently excluding rows.
        ColumnType::Date => i32::try_from(value).map_or_else(
            |_| value.to_le_bytes().to_vec(),
            |narrow| narrow.to_le_bytes().to_vec(),
        ),
        _ => value.to_le_bytes().to_vec(),
    }
}

/// The same as [`double_bounds`] one width down, over a `real` column's own four-byte run.
fn float_bounds(values: &[f32]) -> (Option<Bound>, Option<Bound>) {
    let mut low: Option<f32> = None;
    let mut high: Option<f32> = None;
    for value in values {
        if low.is_none_or(|current| pg_cmp_f32(*value, current).is_lt()) {
            low = Some(*value);
        }
        if high.is_none_or(|current| pg_cmp_f32(*value, current).is_gt()) {
            high = Some(*value);
        }
    }
    let (Some(mut low), Some(mut high)) = (low, high) else {
        return (None, None);
    };

    // As above: `-0.0 == 0.0` under this ordering, so widen each bound to the side that also
    // holds for a reader comparing the stored bytes.
    if low == 0.0 {
        low = -0.0;
    }
    if high == 0.0 {
        high = 0.0;
    }
    (
        Some(Bound::exact(low.to_le_bytes().to_vec())),
        Some(Bound::exact(high.to_le_bytes().to_vec())),
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Bound, ColumnStats};
    use crate::column::Column;
    use crate::cursor::Cursor;
    use crate::format::MAX_BOUND_LEN;
    use crate::value::{ColumnType, Value, pg_cmp_f32, pg_cmp_f64};

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

    fn stats_of(ty: ColumnType, values: &[Value]) -> ColumnStats {
        ColumnStats::of(&Column::build(ty, values).unwrap())
    }

    #[test]
    fn integer_bounds_are_the_extremes_that_are_there() {
        let stats = stats_of(
            ColumnType::Int8,
            &[
                Value::Int8(7),
                Value::Null,
                Value::Int8(i64::MIN),
                Value::Int8(i64::MAX),
            ],
        );
        assert_eq!(stats.null_count, 1);
        assert_eq!(stats.min.as_ref().unwrap().as_i64(), Some(i64::MIN));
        assert_eq!(stats.max.as_ref().unwrap().as_i64(), Some(i64::MAX));
        assert!(stats.bounds_are_exact());
        assert!(stats.fit(ColumnType::Int8));
    }

    /// A chunk with nothing in it has nothing to bound, and says so rather than guessing.
    #[test]
    fn a_column_of_nothing_but_nulls_has_no_bounds() {
        for ty in ColumnType::ALL {
            let stats = stats_of(ty, &vec![Value::Null; 20]);
            assert_eq!(stats.null_count, 20, "{ty:?}");
            assert!(stats.min.is_none() && stats.max.is_none(), "{ty:?}");
            assert!(stats.fit(ty));

            let empty = stats_of(ty, &[]);
            assert_eq!(empty, ColumnStats::empty(), "{ty:?}");
        }
    }

    #[test]
    fn boolean_bounds_say_which_values_occur() {
        let all_true = stats_of(ColumnType::Bool, &vec![Value::Bool(true); 3]);
        assert_eq!(all_true.min.as_ref().unwrap().as_bool(), Some(true));
        assert_eq!(all_true.max.as_ref().unwrap().as_bool(), Some(true));

        let all_false = stats_of(ColumnType::Bool, &vec![Value::Bool(false); 3]);
        assert_eq!(all_false.min.as_ref().unwrap().as_bool(), Some(false));
        assert_eq!(all_false.max.as_ref().unwrap().as_bool(), Some(false));

        let both = stats_of(
            ColumnType::Bool,
            &[Value::Bool(false), Value::Null, Value::Bool(true)],
        );
        assert_eq!(both.min.as_ref().unwrap().as_bool(), Some(false));
        assert_eq!(both.max.as_ref().unwrap().as_bool(), Some(true));
        assert_eq!(both.null_count, 1);
    }

    /// The rule a pruner depends on, and the one M1 got wrong: `NaN` is the top of the range.
    ///
    /// Parquet excludes it, which is right where every comparison with `NaN` is false. Here
    /// `WHERE x > 5` matches a `NaN` row, so a maximum that excluded it would prune away a stripe
    /// the query wants and lose the row silently. See [`crate::value::pg_cmp_f64`].
    #[test]
    fn nan_is_the_largest_value() {
        let stats = stats_of(
            ColumnType::Double,
            &[
                Value::Double(f64::NAN),
                Value::Double(3.0),
                Value::Double(-1.0),
                Value::Double(f64::NAN),
            ],
        );
        assert_eq!(stats.min.as_ref().unwrap().as_f64(), Some(-1.0));
        assert!(
            stats.max.as_ref().unwrap().as_f64().unwrap().is_nan(),
            "a chunk holding a NaN has NaN as its maximum"
        );

        // The case that made this a bug rather than a preference: without it, `x > 5` prunes this
        // stripe away and the NaN row it contains is lost with no error anywhere.
        let hazard = stats_of(
            ColumnType::Double,
            &[Value::Double(1.0), Value::Double(f64::NAN)],
        );
        let max = hazard.max.as_ref().unwrap().as_f64().unwrap();
        assert!(
            pg_cmp_f64(max, 5.0).is_gt(),
            "the maximum does not admit a row that matches x > 5"
        );

        // Nothing but NaN is still a range — of NaN, which is equal to itself.
        let only_nan = stats_of(ColumnType::Double, &vec![Value::Double(f64::NAN); 5]);
        assert!(only_nan.min.as_ref().unwrap().as_f64().unwrap().is_nan());
        assert!(only_nan.max.as_ref().unwrap().as_f64().unwrap().is_nan());
        assert_eq!(only_nan.null_count, 0, "a NaN is not a NULL");

        // And a chunk with no NaN is bounded by its ordinary extremes, infinities included.
        let infinities = stats_of(
            ColumnType::Double,
            &[
                Value::Double(f64::NEG_INFINITY),
                Value::Double(f64::INFINITY),
            ],
        );
        assert_eq!(
            infinities.min.as_ref().unwrap().as_f64(),
            Some(f64::NEG_INFINITY)
        );
        assert_eq!(
            infinities.max.as_ref().unwrap().as_f64(),
            Some(f64::INFINITY)
        );
    }

    /// The other rule: a zero bound is widened to the side that holds bitwise as well.
    #[test]
    fn a_zero_bound_is_signed_to_the_safe_side() {
        let stats = stats_of(
            ColumnType::Double,
            &[Value::Double(0.0), Value::Double(5.0)],
        );
        assert_eq!(
            stats.min.as_ref().unwrap().as_f64().unwrap().to_bits(),
            (-0.0f64).to_bits(),
            "a minimum of zero must be negative zero"
        );

        let stats = stats_of(
            ColumnType::Double,
            &[Value::Double(-5.0), Value::Double(-0.0)],
        );
        assert_eq!(
            stats.max.as_ref().unwrap().as_f64().unwrap().to_bits(),
            0.0f64.to_bits(),
            "a maximum of zero must be positive zero"
        );
    }

    /// A long value is bounded, not stored: the flags say the bounds are approximate.
    #[test]
    fn long_values_are_truncated_into_bounds_that_still_hold() {
        let low = vec![b'a'; MAX_BOUND_LEN + 40];
        let high = {
            let mut bytes = vec![b'z'; MAX_BOUND_LEN + 40];
            bytes[MAX_BOUND_LEN - 1] = b'm';
            bytes
        };
        let stats = stats_of(
            ColumnType::Bytea,
            &[Value::Bytea(low.clone()), Value::Bytea(high.clone())],
        );

        let min = stats.min.as_ref().unwrap();
        let max = stats.max.as_ref().unwrap();
        assert!(min.truncated && max.truncated);
        assert!(!stats.bounds_are_exact());
        assert_eq!(min.bytes.len(), MAX_BOUND_LEN);
        assert!(
            min.bytes.as_slice() <= low.as_slice(),
            "the minimum does not bound"
        );
        assert!(
            max.bytes.as_slice() >= high.as_slice(),
            "the maximum does not bound"
        );
        assert!(stats.fit(ColumnType::Bytea));

        // A value exactly at the limit is stored whole and is exact.
        let exact = stats_of(ColumnType::Text, &[Value::Text("x".repeat(MAX_BOUND_LEN))]);
        assert!(exact.bounds_are_exact());
    }

    /// The one case with no short upper bound at all, answered honestly rather than wrongly.
    #[test]
    fn a_value_of_all_ones_has_no_upper_bound() {
        let stats = stats_of(
            ColumnType::Bytea,
            &[Value::Bytea(vec![0xff; MAX_BOUND_LEN + 1])],
        );
        assert!(stats.min.is_some(), "a lower bound always exists");
        assert!(
            stats.max.is_none(),
            "no 64-byte string sorts above one of all 0xff"
        );

        // One byte below the top still has one.
        let mut nearly = vec![0xff; MAX_BOUND_LEN + 1];
        nearly[MAX_BOUND_LEN - 1] = 0xfe;
        let stats = stats_of(ColumnType::Bytea, &[Value::Bytea(nearly.clone())]);
        let max = stats.max.as_ref().unwrap();
        assert!(max.truncated);
        assert!(max.bytes.as_slice() >= nearly.as_slice());
    }

    fn value_of(ty: ColumnType) -> impl Strategy<Value = Value> {
        let present = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Value::Int8).boxed(),
            ColumnType::Int4 => any::<i32>().prop_map(Value::Int4).boxed(),
            ColumnType::Int2 => any::<i16>().prop_map(Value::Int2).boxed(),
            ColumnType::Date => any::<i32>().prop_map(Value::Date).boxed(),
            ColumnType::Real => prop_oneof![
                Just(Value::Real(f32::NAN)),
                Just(Value::Real(-0.0)),
                any::<f32>().prop_map(Value::Real),
            ]
            .boxed(),
            ColumnType::TimestampTz => any::<i64>().prop_map(Value::TimestampTz).boxed(),
            ColumnType::Timestamp => any::<i64>().prop_map(Value::Timestamp).boxed(),
            ColumnType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
            ColumnType::Double => prop_oneof![
                Just(Value::Double(f64::NAN)),
                Just(Value::Double(-0.0)),
                any::<f64>().prop_map(Value::Double),
            ]
            .boxed(),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb => prop::collection::vec(any::<char>(), 0..90)
                .prop_map(|chars| Value::Text(chars.into_iter().collect()))
                .boxed(),
            ColumnType::Bytea => prop::collection::vec(any::<u8>(), 0..90)
                .prop_map(Value::Bytea)
                .boxed(),
            ColumnType::Numeric => (0usize..6)
                .prop_map(|pick| {
                    Value::Numeric(
                        [
                            "0",
                            "0.00",
                            "-1.5",
                            "12345678901234567890.5",
                            "NaN",
                            "-Infinity",
                        ][pick]
                            .to_owned(),
                    )
                })
                .boxed(),
        };
        prop_oneof![1 => Just(Value::Null), 5 => present]
    }

    /// Statistics are checked against the values themselves, never against the accumulator that
    /// produced them: a bound that is subtly false is worse than one that is absent, because a
    /// pruner would drop rows rather than report an error.
    fn bounds_hold(ty: ColumnType, values: &[Value], stats: &ColumnStats) -> Result<(), String> {
        let present: Vec<&Value> = values.iter().filter(|value| !value.is_null()).collect();
        let expected_nulls = (values.len() - present.len()) as u64;
        if stats.null_count != expected_nulls {
            return Err(format!("{} nulls, not {expected_nulls}", stats.null_count));
        }
        if !stats.fit(ty) {
            return Err("bounds of the wrong shape for the type".into());
        }

        let (Some(min), Some(max)) = (stats.min.as_ref(), stats.max.as_ref()) else {
            return Ok(());
        };
        for value in present {
            let inside = match value {
                Value::Int8(v) | Value::TimestampTz(v) | Value::Timestamp(v) => {
                    min.as_i64() <= Some(*v) && Some(*v) <= max.as_i64()
                }
                // A four-byte bound, read as its own width: `as_i64` wants eight and answers
                // `None` for one, which would make this arm vacuously true.
                Value::Int4(v) => {
                    let read = |bound: &Bound| match bound.as_value(ColumnType::Int4) {
                        Some(Value::Int4(value)) => Some(value),
                        _ => None,
                    };
                    read(min) <= Some(*v) && Some(*v) <= read(max)
                }
                Value::Int2(v) => {
                    let read = |bound: &Bound| match bound.as_value(ColumnType::Int2) {
                        Some(Value::Int2(value)) => Some(value),
                        _ => None,
                    };
                    read(min) <= Some(*v) && Some(*v) <= read(max)
                }
                Value::Date(v) => {
                    let read = |bound: &Bound| match bound.as_value(ColumnType::Date) {
                        Some(Value::Date(value)) => Some(value),
                        _ => None,
                    };
                    read(min) <= Some(*v) && Some(*v) <= read(max)
                }
                Value::Bool(v) => {
                    min.as_bool().is_some_and(|low| low <= *v)
                        && max.as_bool().is_some_and(|high| *v <= high)
                }
                // Under this system's ordering a NaN is *inside* the range, at the top of it.
                Value::Double(v) => {
                    min.as_f64().is_some_and(|low| pg_cmp_f64(low, *v).is_le())
                        && max
                            .as_f64()
                            .is_some_and(|high| pg_cmp_f64(*v, high).is_le())
                }
                // Its own four-byte bound, read as its own width; `as_f64` wants eight and would
                // answer `None`, making this arm vacuously false.
                Value::Real(v) => {
                    let read = |bound: &Bound| match bound.as_value(ColumnType::Real) {
                        Some(Value::Real(value)) => Some(value),
                        _ => None,
                    };
                    read(min).is_some_and(|low| pg_cmp_f32(low, *v).is_le())
                        && read(max).is_some_and(|high| pg_cmp_f32(*v, high).is_le())
                }
                // A numeric shares this arm because **its bound is its text's bound, not its
                // value's**; see the warning on the byte-run list in `fit` for why a pruner may
                // not read it as a number.
                Value::Text(v) | Value::Numeric(v) => {
                    min.bytes.as_slice() <= v.as_bytes() && v.as_bytes() <= max.bytes.as_slice()
                }
                Value::Bytea(v) => {
                    min.bytes.as_slice() <= v.as_slice() && v.as_slice() <= max.bytes.as_slice()
                }
                Value::Null => true,
            };
            if !inside {
                return Err(format!("{value:?} is outside {min:?}..{max:?}"));
            }
        }
        Ok(())
    }

    proptest! {
        /// Every type there is, so a type cannot be added to this crate and left without
        /// statistics that were ever checked. The named tests below stay because a failure that
        /// names its type is easier to read; this is the one that cannot be forgotten.
        ///
        /// It is the test the widened runs needed and did not have: `Int4`, `Int2` and `Real` all
        /// ride in a run wider than themselves or did, and each wrote a bound at the run's width
        /// that `Bound::as_value` then answered `None` for — a scan reads that as "no bound" and
        /// stops pruning, which is slow rather than wrong and so had no other way to be noticed.
        #[test]
        fn every_type_s_statistics_hold(
            (ty, values) in prop::sample::select(ColumnType::ALL.as_slice())
                .prop_flat_map(|ty| (Just(ty), prop::collection::vec(value_of(ty), 0..60))),
        ) {
            let stats = stats_of(ty, &values);
            if let Err(why) = bounds_hold(ty, &values, &stats) {
                prop_assert!(false, "{}: {}", ty.name(), why);
            }
        }

        #[test]
        fn int_statistics_hold(values in prop::collection::vec(value_of(ColumnType::Int8), 0..60)) {
            let stats = stats_of(ColumnType::Int8, &values);
            prop_assert!(bounds_hold(ColumnType::Int8, &values, &stats).is_ok());
        }

        #[test]
        fn timestamp_statistics_hold(
            values in prop::collection::vec(value_of(ColumnType::TimestampTz), 0..60),
        ) {
            let stats = stats_of(ColumnType::TimestampTz, &values);
            prop_assert!(bounds_hold(ColumnType::TimestampTz, &values, &stats).is_ok());
        }

        #[test]
        fn bool_statistics_hold(values in prop::collection::vec(value_of(ColumnType::Bool), 0..60)) {
            let stats = stats_of(ColumnType::Bool, &values);
            prop_assert!(bounds_hold(ColumnType::Bool, &values, &stats).is_ok());
        }

        #[test]
        fn double_statistics_hold(
            values in prop::collection::vec(value_of(ColumnType::Double), 0..60),
        ) {
            let stats = stats_of(ColumnType::Double, &values);
            prop_assert!(bounds_hold(ColumnType::Double, &values, &stats).is_ok());
        }

        #[test]
        fn text_statistics_hold(values in prop::collection::vec(value_of(ColumnType::Text), 0..40)) {
            let stats = stats_of(ColumnType::Text, &values);
            if let Err(why) = bounds_hold(ColumnType::Text, &values, &stats) {
                prop_assert!(false, "{}", why);
            }
        }

        #[test]
        fn bytea_statistics_hold(
            values in prop::collection::vec(value_of(ColumnType::Bytea), 0..40),
        ) {
            let stats = stats_of(ColumnType::Bytea, &values);
            if let Err(why) = bounds_hold(ColumnType::Bytea, &values, &stats) {
                prop_assert!(false, "{}", why);
            }
        }

        /// And the statistics survive the footer they are written into.
        #[test]
        fn statistics_round_trip_through_the_footer(
            values in prop::collection::vec(value_of(ColumnType::Bytea), 0..40),
        ) {
            let stats = stats_of(ColumnType::Bytea, &values);
            prop_assert_eq!(round_trip(&stats), stats);
        }
    }
}
