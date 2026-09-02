//! A decoded column chunk: where its NULLs are, and its values densely behind them.
//!
//! This is the crate's in-memory shape and it is used in **both** directions — the writer builds
//! one with [`ColumnBuilder`] and encodes it, the reader decodes one out of a chunk. That is not
//! a convenience: it means a round-trip test is `decode(encode(c)) == c`, over the same type,
//! with no adapter in between that could quietly correct a mistake.
//!
//! Values are stored for the rows that are not NULL and no others, so a NULL costs one bit in the
//! mask and nothing else. Row order is recovered by walking the mask, which is what [`Column::iter`]
//! does and why there is no random-access accessor: a scan wants one pass, and an index-by-row
//! would need a rank structure that milestone 2 can build if it turns out to need one.
//!
//! # `identical`, and why `==` is not enough
//!
//! [`Column`] derives `PartialEq`, which for a double column compares values — and `NaN != NaN`,
//! so a round-trip test over floats would pass without proving anything. [`Column::identical`]
//! compares floating point by bits instead, and it is what the goldens and the round-trip
//! proptests use.

use crate::error::{Error, Result};
use crate::value::{ColumnType, Value, ValueRef};

/// Which rows of a chunk are NULL, one bit each, least-significant bit first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NullMask {
    bits: Vec<u8>,
    rows: usize,
    nulls: usize,
}

impl NullMask {
    /// A mask with no NULLs at all, which costs no bytes.
    #[must_use]
    pub fn none(rows: usize) -> Self {
        Self {
            bits: Vec::new(),
            rows,
            nulls: 0,
        }
    }

    /// A mask from one flag per row, `true` meaning NULL.
    #[must_use]
    pub fn from_bools(nulls: &[bool]) -> Self {
        let count = nulls.iter().filter(|null| **null).count();
        if count == 0 {
            return Self::none(nulls.len());
        }
        let mut bits = vec![0u8; nulls.len().div_ceil(8)];
        for (row, null) in nulls.iter().enumerate() {
            if *null {
                bits[row / 8] |= 1 << (row % 8);
            }
        }
        Self {
            bits,
            rows: nulls.len(),
            nulls: count,
        }
    }

    /// Whether row `row` is NULL. A row past the end is not NULL; it is not a row.
    #[must_use]
    pub fn is_null(&self, row: usize) -> bool {
        if row >= self.rows {
            return false;
        }
        self.bits
            .get(row / 8)
            .is_some_and(|byte| byte & (1 << (row % 8)) != 0)
    }

    /// How many rows the mask covers.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// How many of them are NULL.
    #[must_use]
    pub fn nulls(&self) -> usize {
        self.nulls
    }

    /// One flag per row, `true` meaning NULL.
    #[must_use]
    pub fn to_bools(&self) -> Vec<bool> {
        (0..self.rows).map(|row| self.is_null(row)).collect()
    }
}

/// The values of a chunk, densely, for the rows that are not NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    /// An `Int8` or a `TimestampTz` column.
    Ints(Vec<i64>),
    /// A `Double` column.
    Doubles(Vec<f64>),
    /// A `Real` column: four bytes each, never widened into [`ColumnData::Doubles`].
    /// `encode::float` says why the widening an `Int4` gets is wrong one width down.
    Floats(Vec<f32>),
    /// A `Bool` column.
    Bools(Vec<bool>),
    /// A `Text` or `Bytea` column: value `i` is `data[offsets[i]..offsets[i + 1]]`.
    Bytes {
        /// One entry per value plus a final total; always at least one entry long.
        offsets: Vec<u32>,
        /// Every value's bytes, end to end.
        data: Vec<u8>,
    },
}

impl ColumnData {
    /// An empty run of the shape `ty` needs.
    #[must_use]
    pub fn empty(ty: ColumnType) -> Self {
        match ty {
            // An `Int4` rides in the `Ints` run, widened. The *column* carries its type tag, so
            // there is nothing ambiguous about it, and a second integer run would be a second
            // encoding to keep in step for no gain — the values are the same values.
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Int4
            | ColumnType::Int2
            | ColumnType::Date
            | ColumnType::Time => ColumnData::Ints(Vec::new()),
            ColumnType::Double => ColumnData::Doubles(Vec::new()),
            // A `Real` gets its **own** run rather than riding in the doubles one widened, which
            // is the one place the integer trick above does not carry over: widening an `f32` is
            // exact for every value except a `NaN` payload, where it is unspecified, and it would
            // write eight bytes for a four-byte type. `encode::float` has the argument.
            ColumnType::Real => ColumnData::Floats(Vec::new()),
            ColumnType::Bool => ColumnData::Bools(Vec::new()),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Numeric
            | ColumnType::Bytea => ColumnData::Bytes {
                offsets: vec![0],
                data: Vec::new(),
            },
        }
    }

    /// How many values are present.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            ColumnData::Ints(values) => values.len(),
            ColumnData::Doubles(values) => values.len(),
            ColumnData::Floats(values) => values.len(),
            ColumnData::Bools(values) => values.len(),
            ColumnData::Bytes { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }

    /// Whether no value is present. Every row of the chunk is then NULL.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this run can hold a column of type `ty`.
    #[must_use]
    pub fn fits(&self, ty: ColumnType) -> bool {
        matches!(
            (self, ty),
            (
                ColumnData::Ints(_),
                ColumnType::Int8
                    | ColumnType::TimestampTz
                    | ColumnType::Timestamp
                    | ColumnType::Int4
                    | ColumnType::Int2
                    | ColumnType::Date
                    | ColumnType::Time
            ) | (ColumnData::Doubles(_), ColumnType::Double)
                | (ColumnData::Floats(_), ColumnType::Real)
                | (ColumnData::Bools(_), ColumnType::Bool)
                | (
                    ColumnData::Bytes { .. },
                    ColumnType::Text
                        | ColumnType::Varchar
                        | ColumnType::Bpchar
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        // A `numeric` is stored as its text, so it rides the byte run like a
                        // string; `stats::fit` says why its *bounds* are not a number's.
                        | ColumnType::Numeric
                        | ColumnType::Bytea
                )
        )
    }
}

/// One column of one stripe, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    ty: ColumnType,
    nulls: NullMask,
    data: ColumnData,
}

impl Column {
    /// A column, or an error if the pieces do not describe one.
    ///
    /// The three checks here — the run matches the type, the mask covers the rows, and there are
    /// exactly as many values as there are non-NULL rows — are what every accessor below relies
    /// on, so a decoder cannot produce a `Column` that would index out of bounds later.
    pub fn new(ty: ColumnType, nulls: NullMask, data: ColumnData) -> Result<Self> {
        if !data.fits(ty) {
            return Err(Error::corruption(
                "column",
                format!("a {} column holding {data:?}", ty.name()),
            ));
        }
        let present = nulls.rows() - nulls.nulls();
        if data.len() != present {
            return Err(Error::corruption(
                "column",
                format!(
                    "{} values for {present} non-null rows of {}",
                    data.len(),
                    nulls.rows()
                ),
            ));
        }
        if let ColumnData::Bytes { offsets, data } = &data
            && (offsets.first() != Some(&0)
                || offsets.last().copied().unwrap_or(0) as usize != data.len()
                || offsets.windows(2).any(|pair| pair[0] > pair[1]))
        {
            return Err(Error::corruption(
                "column",
                "byte offsets are not a non-decreasing cover of the data".to_string(),
            ));
        }
        Ok(Self { ty, nulls, data })
    }

    /// Builds a column from one value per row, which is what a test or a small writer wants.
    pub fn build(ty: ColumnType, values: &[Value]) -> Result<Self> {
        let mut builder = ColumnBuilder::new(ty);
        for value in values {
            builder.push(value)?;
        }
        builder.finish()
    }

    /// What the column holds.
    #[must_use]
    pub fn ty(&self) -> ColumnType {
        self.ty
    }

    /// How many rows, NULLs included.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.nulls.rows()
    }

    /// Whether the column has no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Which rows are NULL.
    #[must_use]
    pub fn nulls(&self) -> &NullMask {
        &self.nulls
    }

    /// The values present, densely.
    #[must_use]
    pub fn data(&self) -> &ColumnData {
        &self.data
    }

    /// Every row in order, NULLs included.
    #[must_use]
    pub fn iter(&self) -> ColumnIter<'_> {
        ColumnIter {
            column: self,
            row: 0,
            present: 0,
        }
    }

    /// Every row as an owned [`Value`], which validates the UTF-8 of a text column.
    pub fn to_values(&self) -> Result<Vec<Value>> {
        self.iter().map(|value| value.to_value(self.ty)).collect()
    }

    /// Whether two columns hold the same rows, comparing floating point **by bits**.
    ///
    /// `==` would say a `NaN` column differs from itself, which makes a round-trip assertion pass
    /// without proving anything. This is what the goldens and the proptests compare with.
    #[must_use]
    pub fn identical(&self, other: &Self) -> bool {
        if self.ty != other.ty || self.nulls != other.nulls {
            return false;
        }
        match (&self.data, &other.data) {
            (ColumnData::Doubles(left), ColumnData::Doubles(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(a, b)| a.to_bits() == b.to_bits())
            }
            (ColumnData::Floats(left), ColumnData::Floats(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(a, b)| a.to_bits() == b.to_bits())
            }
            (left, right) => left == right,
        }
    }
}

impl<'a> IntoIterator for &'a Column {
    type Item = ValueRef<'a>;
    type IntoIter = ColumnIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Walks a column in row order, weaving the values back through the null mask.
#[derive(Debug)]
pub struct ColumnIter<'a> {
    column: &'a Column,
    row: usize,
    present: usize,
}

impl<'a> Iterator for ColumnIter<'a> {
    type Item = ValueRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.row >= self.column.rows() {
            return None;
        }
        let row = self.row;
        self.row += 1;
        if self.column.nulls.is_null(row) {
            return Some(ValueRef::Null);
        }
        let index = self.present;
        self.present += 1;
        // `Column::new` proved there are exactly as many values as non-NULL rows, and this is the
        // `index`-th of them, so every access below is in bounds.
        Some(match &self.column.data {
            ColumnData::Ints(values) => ValueRef::Int(values[index]),
            ColumnData::Doubles(values) => ValueRef::Double(values[index]),
            ColumnData::Floats(values) => ValueRef::Real(values[index]),
            ColumnData::Bools(values) => ValueRef::Bool(values[index]),
            ColumnData::Bytes { offsets, data } => {
                ValueRef::Bytes(&data[offsets[index] as usize..offsets[index + 1] as usize])
            }
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.column.rows() - self.row;
        (left, Some(left))
    }
}

impl ExactSizeIterator for ColumnIter<'_> {}

/// Accumulates one column of a stripe, value by value, and hands over a [`Column`].
///
/// The writer keeps one per column and calls [`ColumnBuilder::finish`] when it seals a stripe,
/// which empties the builder for the next one — a stripe's buffers are reused rather than
/// reallocated.
#[derive(Debug)]
pub struct ColumnBuilder {
    ty: ColumnType,
    nulls: Vec<bool>,
    ints: Vec<i64>,
    doubles: Vec<f64>,
    floats: Vec<f32>,
    bools: Vec<bool>,
    offsets: Vec<u32>,
    data: Vec<u8>,
}

impl ColumnBuilder {
    /// An empty builder for a column of type `ty`.
    #[must_use]
    pub fn new(ty: ColumnType) -> Self {
        Self {
            ty,
            nulls: Vec::new(),
            ints: Vec::new(),
            doubles: Vec::new(),
            floats: Vec::new(),
            bools: Vec::new(),
            offsets: vec![0],
            data: Vec::new(),
        }
    }

    /// Appends one row's value, which must be NULL or of the column's own type.
    pub fn push(&mut self, value: &Value) -> Result<()> {
        if !value.fits(self.ty) {
            return Err(Error::InvalidArgument(format!(
                "a {} column was given {value:?}",
                self.ty.name()
            )));
        }
        self.nulls.push(value.is_null());
        match value {
            Value::Null => {}
            Value::Int8(v) | Value::TimestampTz(v) | Value::Timestamp(v) | Value::Time(v) => {
                self.ints.push(*v);
            }
            // A day is an integer to the encoder, the way a timestamp is: the schema says which.
            Value::Int4(v) | Value::Date(v) => self.ints.push(i64::from(*v)),
            Value::Int2(v) => self.ints.push(i64::from(*v)),
            Value::Double(v) => self.doubles.push(*v),
            Value::Real(v) => self.floats.push(*v),
            Value::Bool(v) => self.bools.push(*v),
            Value::Text(v) | Value::Numeric(v) => self.push_bytes(v.as_bytes())?,
            Value::Bytea(v) => self.push_bytes(v)?,
        }
        Ok(())
    }

    /// Appends one variable-length value, refusing a stripe whose bytes would not fit a `u32`
    /// offset. The writer seals stripes on a byte budget far below that, so this is a backstop
    /// rather than a limit anybody meets — but it is a real error, not a silent truncation.
    fn push_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let end = u32::try_from(self.data.len() + bytes.len()).map_err(|_| {
            Error::InvalidArgument(format!(
                "a stripe holding more than {} bytes of one column",
                u32::MAX
            ))
        })?;
        self.data.extend_from_slice(bytes);
        self.offsets.push(end);
        Ok(())
    }

    /// How many rows have been appended.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.nulls.len()
    }

    /// Roughly how many bytes the accumulated values occupy, for the writer's stripe budget.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.nulls.len()
            + self.ints.len() * 8
            + self.doubles.len() * 8
            + self.floats.len() * 4
            + self.bools.len()
            + self.offsets.len() * 4
            + self.data.len()
    }

    /// Takes what has been accumulated as a [`Column`], leaving the builder empty.
    pub fn finish(&mut self) -> Result<Column> {
        let nulls = NullMask::from_bools(&self.nulls);
        let data = match self.ty {
            ColumnType::Int8
            | ColumnType::TimestampTz
            | ColumnType::Timestamp
            | ColumnType::Int4
            | ColumnType::Int2
            | ColumnType::Date
            | ColumnType::Time => ColumnData::Ints(std::mem::take(&mut self.ints)),
            ColumnType::Double => ColumnData::Doubles(std::mem::take(&mut self.doubles)),
            ColumnType::Real => ColumnData::Floats(std::mem::take(&mut self.floats)),
            ColumnType::Bool => ColumnData::Bools(std::mem::take(&mut self.bools)),
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Numeric
            | ColumnType::Bytea => ColumnData::Bytes {
                offsets: std::mem::replace(&mut self.offsets, vec![0]),
                data: std::mem::take(&mut self.data),
            },
        };
        self.nulls.clear();
        Column::new(self.ty, nulls, data)
    }
}

/// Whether every set bit of `mask` is accounted for. Used where a decoder must not trust a count
/// it read somewhere else.
pub(crate) fn count_nulls(nulls: &[bool]) -> usize {
    nulls.iter().filter(|null| **null).count()
}

#[cfg(test)]
mod tests {
    use super::{Column, ColumnBuilder, ColumnData, NullMask, count_nulls};
    use crate::value::{ColumnType, Value, ValueRef};

    #[test]
    fn a_mask_remembers_exactly_which_rows_are_null() {
        let flags = [false, true, false, false, true, true, true, false, true];
        let mask = NullMask::from_bools(&flags);
        assert_eq!(mask.rows(), flags.len());
        assert_eq!(mask.nulls(), 5);
        for (row, flag) in flags.iter().enumerate() {
            assert_eq!(mask.is_null(row), *flag, "row {row}");
        }
        assert!(
            !mask.is_null(flags.len()),
            "a row past the end is not a row"
        );
        assert_eq!(mask.to_bools(), flags);
        assert_eq!(count_nulls(&flags), 5);

        let none = NullMask::none(10);
        assert_eq!(none.nulls(), 0);
        assert!((0..10).all(|row| !none.is_null(row)));
        assert_eq!(NullMask::from_bools(&[false; 10]), none);
    }

    #[test]
    fn a_built_column_reads_back_row_by_row() {
        let values = vec![
            Value::Text("alpha".into()),
            Value::Null,
            Value::Text(String::new()),
            Value::Text("omega".into()),
        ];
        let column = Column::build(ColumnType::Text, &values).unwrap();
        assert_eq!(column.rows(), 4);
        assert_eq!(column.nulls().nulls(), 1);
        assert_eq!(column.data().len(), 3);
        assert_eq!(
            column.iter().collect::<Vec<_>>(),
            vec![
                ValueRef::Bytes(b"alpha"),
                ValueRef::Null,
                ValueRef::Bytes(b""),
                ValueRef::Bytes(b"omega"),
            ]
        );
        assert_eq!(column.to_values().unwrap(), values);
        assert_eq!(column.iter().len(), 4);
    }

    #[test]
    fn every_type_builds_and_reads_back() {
        let cases: Vec<(ColumnType, Vec<Value>)> = vec![
            (ColumnType::Int8, vec![Value::Int8(-1), Value::Null]),
            (
                ColumnType::TimestampTz,
                vec![Value::TimestampTz(i64::MIN), Value::TimestampTz(0)],
            ),
            (ColumnType::Bool, vec![Value::Bool(true), Value::Null]),
            (ColumnType::Double, vec![Value::Double(-0.0), Value::Null]),
            (
                ColumnType::Bytea,
                vec![Value::Bytea(vec![0xff]), Value::Null],
            ),
            (ColumnType::Text, vec![Value::Text("x".into())]),
        ];
        for (ty, values) in cases {
            let column = Column::build(ty, &values).unwrap();
            assert_eq!(column.ty(), ty);
            assert_eq!(column.to_values().unwrap(), values, "{ty:?}");
            assert!(column.identical(&column.clone()));
        }
        assert!(Column::build(ColumnType::Int8, &[]).unwrap().is_empty());
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused() {
        let mut builder = ColumnBuilder::new(ColumnType::Int8);
        assert!(builder.push(&Value::Text("no".into())).is_err());
        assert!(builder.push(&Value::Null).is_ok());
        assert_eq!(builder.rows(), 1);
        assert!(builder.heap_bytes() > 0);
    }

    #[test]
    fn a_builder_empties_itself() {
        let mut builder = ColumnBuilder::new(ColumnType::Text);
        builder.push(&Value::Text("first".into())).unwrap();
        let first = builder.finish().unwrap();
        assert_eq!(first.rows(), 1);
        assert_eq!(builder.rows(), 0);

        builder.push(&Value::Text("second".into())).unwrap();
        let second = builder.finish().unwrap();
        assert_eq!(
            second.to_values().unwrap(),
            vec![Value::Text("second".into())]
        );
    }

    /// Every way the three pieces can fail to describe a column.
    #[test]
    fn a_column_that_does_not_add_up_is_corruption() {
        assert!(
            Column::new(
                ColumnType::Int8,
                NullMask::none(2),
                ColumnData::Bools(vec![true, false])
            )
            .unwrap_err()
            .is_corruption(),
            "wrong run for the type"
        );
        assert!(
            Column::new(
                ColumnType::Int8,
                NullMask::none(3),
                ColumnData::Ints(vec![1])
            )
            .unwrap_err()
            .is_corruption(),
            "too few values"
        );
        assert!(
            Column::new(
                ColumnType::Text,
                NullMask::none(1),
                ColumnData::Bytes {
                    offsets: vec![0, 9],
                    data: vec![1, 2],
                }
            )
            .unwrap_err()
            .is_corruption(),
            "offsets past the data"
        );
        assert!(
            Column::new(
                ColumnType::Text,
                NullMask::none(2),
                ColumnData::Bytes {
                    offsets: vec![0, 2, 1],
                    data: vec![1, 2],
                }
            )
            .unwrap_err()
            .is_corruption(),
            "offsets that go backwards"
        );
    }

    /// `NaN` must equal itself here, or a round-trip assertion proves nothing.
    #[test]
    fn identical_compares_floats_by_bits() {
        let nan = Column::build(ColumnType::Double, &[Value::Double(f64::NAN)]).unwrap();
        assert_ne!(
            nan,
            nan.clone(),
            "PartialEq over NaN is why identical exists"
        );
        assert!(nan.identical(&nan.clone()));

        let zero = Column::build(ColumnType::Double, &[Value::Double(0.0)]).unwrap();
        let minus = Column::build(ColumnType::Double, &[Value::Double(-0.0)]).unwrap();
        assert_eq!(zero, minus, "0.0 == -0.0 numerically");
        assert!(!zero.identical(&minus), "and they are different bits");
    }

    #[test]
    fn an_empty_run_exists_for_every_type() {
        for ty in ColumnType::ALL {
            let data = ColumnData::empty(ty);
            assert!(data.is_empty());
            assert!(data.fits(ty));
            assert!(Column::new(ty, NullMask::none(0), data).is_ok());
        }
    }
}
