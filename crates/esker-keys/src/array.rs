//! An array value: the elements, the shape they are in, and where the subscripts start.
//!
//! PostgreSQL's own model, and each of its three parts is load-bearing rather than incidental:
//!
//! * **the elements are flat**, row-major, however many dimensions the value has. A
//!   two-dimensional array is not an array of arrays here or on a real server — it is one element
//!   sequence with a shape, which is why `('{{1,2},{3,4}}')[1]` is NULL rather than `{1,2}`;
//! * **the shape is a list of lengths**, empty for an empty array. An empty array has *no*
//!   dimensions at all, which is why `array_length('{}', 1)` is NULL where `cardinality('{}')`
//!   is 0 — the two disagree only here, and only because of this;
//! * **the lower bound is part of the value.** `'[0:2]={1,2,3}'` prints back as `[0:2]={1,2,3}`,
//!   and an implementation that stored only the elements would print `{1,2,3}` and be wrong about
//!   its own text. Measured.
//!
//! An element may be NULL, and that is not the same as the array being NULL: `'{NULL}'::int[] IS
//! NULL` is false. Both states exist here — `Datum::Null` for the array, `None` for an element.
//!
//! # What it is *not*
//!
//! Not a new scalar type: an array is a **constructor over** one. Its element type is carried in
//! the value because a `Datum` has to be able to say what it is, and every question about an
//! element — how it parses, how it prints, how two of them compare — is the element type's to
//! answer. `'{2147483648}'::int[]` fails with `int4`'s own overflow message for that reason.

use crate::value::{ColumnType, Datum};

/// One array value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayValue {
    /// What the elements are. Not derivable from them: an empty `text[]` and an empty `int[]`
    /// hold the same nothing and are different values.
    pub element: ColumnType,
    /// The first subscript of the first dimension. One unless the literal said otherwise.
    pub lower: i32,
    /// The length of each dimension, outermost first. **Empty for an empty array**, which has no
    /// dimensions rather than one of length zero.
    pub dims: Vec<i32>,
    /// Every element, row-major. `None` is a NULL element.
    pub values: Vec<Option<Datum>>,
}

impl ArrayValue {
    /// An empty array of `element`: no dimensions, no elements, lower bound one.
    #[must_use]
    pub fn empty(element: ColumnType) -> Self {
        ArrayValue {
            element,
            lower: 1,
            dims: Vec::new(),
            values: Vec::new(),
        }
    }

    /// A one-dimensional array of `values`, subscripted from `lower`.
    #[must_use]
    pub fn one_dimensional(element: ColumnType, lower: i32, values: Vec<Option<Datum>>) -> Self {
        let dims = if values.is_empty() {
            Vec::new()
        } else {
            vec![i32::try_from(values.len()).unwrap_or(i32::MAX)]
        };
        ArrayValue {
            element,
            lower,
            dims,
            values,
        }
    }

    /// How many elements in total, which is `cardinality` — **0 for an empty array**, where
    /// `array_length` is NULL.
    #[must_use]
    pub fn cardinality(&self) -> usize {
        self.values.len()
    }

    /// The array type a value of this element type belongs to, or `None` for an element type this
    /// node has no array of.
    ///
    /// The four are the ones `ActiveRecord`'s schemas declare. Each is a `ColumnType` of its own
    /// rather than a constructor applied to another, because [`ColumnType`] is `Copy` and used by
    /// value everywhere — a recursive variant would be a `Box` in every one of those places
    /// ([ADR 0047](../../../docs/adr/0047-an-array-is-a-column-type-over-one-element-type.md)).
    #[must_use]
    pub fn array_of(element: ColumnType) -> Option<ColumnType> {
        Some(match element {
            ColumnType::Int8 => ColumnType::Int8Array,
            ColumnType::RegType => ColumnType::RegTypeArray,
            ColumnType::RegProc => ColumnType::RegProcArray,
            ColumnType::RegClass => ColumnType::RegClassArray,
            ColumnType::Int4 => ColumnType::Int4Array,
            ColumnType::Int2 => ColumnType::Int2Array,
            ColumnType::Numeric => ColumnType::NumericArray,
            ColumnType::Text => ColumnType::TextArray,
            ColumnType::Hstore => ColumnType::HstoreArray,
            ColumnType::TsVector => ColumnType::TsVectorArray,
            ColumnType::TsQuery => ColumnType::TsQueryArray,
            ColumnType::TsRange => ColumnType::TsRangeArray,
            ColumnType::TstzRange => ColumnType::TstzRangeArray,
            ColumnType::Int4Range => ColumnType::Int4RangeArray,
            ColumnType::DateRange => ColumnType::DateRangeArray,
            ColumnType::NumRange => ColumnType::NumRangeArray,
            ColumnType::Int8Range => ColumnType::Int8RangeArray,
            ColumnType::Point => ColumnType::PointArray,
            ColumnType::Box => ColumnType::BoxArray,
            ColumnType::Bool => ColumnType::BoolArray,
            ColumnType::Bytea => ColumnType::ByteaArray,
            ColumnType::Bpchar => ColumnType::BpcharArray,
            ColumnType::Varchar => ColumnType::VarcharArray,
            ColumnType::Name => ColumnType::NameArray,
            ColumnType::Char => ColumnType::CharArray,
            ColumnType::Lseg => ColumnType::LsegArray,
            ColumnType::Path => ColumnType::PathArray,
            ColumnType::Polygon => ColumnType::PolygonArray,
            ColumnType::Circle => ColumnType::CircleArray,
            ColumnType::Line => ColumnType::LineArray,
            ColumnType::Date => ColumnType::DateArray,
            ColumnType::Time => ColumnType::TimeArray,
            ColumnType::Timestamp => ColumnType::TimestampArray,
            ColumnType::TimestampTz => ColumnType::TimestampTzArray,
            ColumnType::Interval => ColumnType::IntervalArray,
            ColumnType::Real => ColumnType::RealArray,
            ColumnType::Double => ColumnType::DoubleArray,
            ColumnType::Uuid => ColumnType::UuidArray,
            ColumnType::Json => ColumnType::JsonArray,
            ColumnType::Xml => ColumnType::XmlArray,
            ColumnType::Ltree => ColumnType::LtreeArray,
            ColumnType::LQuery => ColumnType::LQueryArray,
            ColumnType::Int2Vector => ColumnType::Int2VectorArray,
            ColumnType::OidVector => ColumnType::OidVectorArray,
            ColumnType::Jsonb => ColumnType::JsonbArray,
            ColumnType::Oid => ColumnType::OidArray,
            ColumnType::Citext => ColumnType::CitextArray,
            ColumnType::Money => ColumnType::MoneyArray,
            ColumnType::Inet => ColumnType::InetArray,
            ColumnType::Cidr => ColumnType::CidrArray,
            ColumnType::MacAddr => ColumnType::MacAddrArray,
            ColumnType::Bit => ColumnType::BitArray,
            ColumnType::VarBit => ColumnType::VarBitArray,
            _ => return None,
        })
    }

    /// Stack `parts` into one array with one more dimension — the value half of
    /// [`array_over`](Self::array_over).
    ///
    /// `None` is a NULL operand and has **no** dimensions, which is why `ARRAY[NULL::int[]]` is
    /// the empty array and `ARRAY['{1,2}'::int[], NULL::int[]]` is a mismatch rather than an array
    /// with a NULL in it. Every part must carry the same dimensions; the result's are the number
    /// of parts followed by theirs, and its elements are theirs end to end — which is all a
    /// dimension is here, the values being flat already.
    ///
    /// Answers `None` when the dimensions disagree. **The caller says which sentence that is**:
    /// PostgreSQL raises one from `ExecEvalArrayExpr` for a constructor and a different one from
    /// `accumArrayResultArr` for `array_agg`, both `2202E`.
    ///
    /// `fallback` is the element type to use when every part is NULL and none can say.
    #[must_use]
    pub fn stacked(parts: &[Option<&ArrayValue>], fallback: ColumnType) -> Option<ArrayValue> {
        let mut dims: Option<&[i32]> = None;
        for part in parts {
            let theirs: &[i32] = part.map_or(&[], |array| array.dims.as_slice());
            match dims {
                None => dims = Some(theirs),
                Some(seen) if seen == theirs => {}
                Some(_) => return None,
            }
        }
        let element = parts
            .iter()
            .flatten()
            .next()
            .map_or(fallback, |array| array.element);
        let inner = dims.unwrap_or(&[]);
        // Nothing had a dimension — every part was NULL or itself empty — so neither has the
        // answer. `ARRAY[NULL::int[]]` and `ARRAY['{}'::int[]]` are both `{}`, measured.
        if inner.is_empty() {
            return Some(ArrayValue::empty(element));
        }
        let mut all = Vec::with_capacity(inner.len() + 1);
        all.push(i32::try_from(parts.len()).ok()?);
        all.extend_from_slice(inner);
        let mut values = Vec::new();
        for part in parts.iter().flatten() {
            values.extend(part.values.iter().cloned());
        }
        Some(ArrayValue {
            element,
            lower: 1,
            dims: all,
            values,
        })
    }

    /// The type an `ARRAY[…]` constructor, an `array_agg`, an `ARRAY(subquery)` or a written
    /// `T[]` has when its operand is of type `element`.
    ///
    /// **This is not `typarray`, and the difference is the whole of it.** PostgreSQL's arrays are
    /// multidimensional rather than nested: there is no array *of* an array, so `_int4.typarray`
    /// is 0 and [`array_of`](Self::array_of) rightly answers `None` for one. Building an array out
    /// of arrays adds a dimension and keeps the type — `ARRAY['{1,2}'::int[]]` is an `int[]`, and
    /// so is `int[][]` and `ARRAY(SELECT ARRAY(SELECT 1))`, all measured on 19beta1. So an operand
    /// that is already an array answers *itself*.
    ///
    /// Every site that degraded this to `text[]` was one of r1's 196 wire-census rows.
    #[must_use]
    pub fn array_over(element: ColumnType) -> Option<ColumnType> {
        if Self::element_of(element).is_some() {
            return Some(element);
        }
        Self::array_of(element)
    }

    /// The element type an array type is over, or `None` for a type that is not an array.
    #[must_use]
    pub fn element_of(array: ColumnType) -> Option<ColumnType> {
        Some(match array {
            ColumnType::Int8Array => ColumnType::Int8,
            ColumnType::RegTypeArray => ColumnType::RegType,
            ColumnType::RegProcArray => ColumnType::RegProc,
            ColumnType::RegClassArray => ColumnType::RegClass,
            ColumnType::Int4Array => ColumnType::Int4,
            ColumnType::Int2Array => ColumnType::Int2,
            ColumnType::NumericArray => ColumnType::Numeric,
            ColumnType::TextArray => ColumnType::Text,
            ColumnType::HstoreArray => ColumnType::Hstore,
            ColumnType::TsVectorArray => ColumnType::TsVector,
            ColumnType::TsQueryArray => ColumnType::TsQuery,
            ColumnType::TsRangeArray => ColumnType::TsRange,
            ColumnType::TstzRangeArray => ColumnType::TstzRange,
            ColumnType::Int4RangeArray => ColumnType::Int4Range,
            ColumnType::DateRangeArray => ColumnType::DateRange,
            ColumnType::NumRangeArray => ColumnType::NumRange,
            ColumnType::Int8RangeArray => ColumnType::Int8Range,
            ColumnType::PointArray => ColumnType::Point,
            ColumnType::BoxArray => ColumnType::Box,
            ColumnType::BoolArray => ColumnType::Bool,
            ColumnType::ByteaArray => ColumnType::Bytea,
            ColumnType::BpcharArray => ColumnType::Bpchar,
            ColumnType::VarcharArray => ColumnType::Varchar,
            ColumnType::NameArray => ColumnType::Name,
            ColumnType::CharArray => ColumnType::Char,
            ColumnType::LsegArray => ColumnType::Lseg,
            ColumnType::PathArray => ColumnType::Path,
            ColumnType::PolygonArray => ColumnType::Polygon,
            ColumnType::CircleArray => ColumnType::Circle,
            ColumnType::LineArray => ColumnType::Line,
            ColumnType::DateArray => ColumnType::Date,
            ColumnType::TimeArray => ColumnType::Time,
            ColumnType::TimestampArray => ColumnType::Timestamp,
            ColumnType::TimestampTzArray => ColumnType::TimestampTz,
            ColumnType::IntervalArray => ColumnType::Interval,
            ColumnType::RealArray => ColumnType::Real,
            ColumnType::DoubleArray => ColumnType::Double,
            ColumnType::UuidArray => ColumnType::Uuid,
            ColumnType::JsonArray => ColumnType::Json,
            ColumnType::XmlArray => ColumnType::Xml,
            ColumnType::LtreeArray => ColumnType::Ltree,
            ColumnType::LQueryArray => ColumnType::LQuery,
            ColumnType::Int2VectorArray => ColumnType::Int2Vector,
            ColumnType::OidVectorArray => ColumnType::OidVector,
            ColumnType::JsonbArray => ColumnType::Jsonb,
            ColumnType::OidArray => ColumnType::Oid,
            ColumnType::CitextArray => ColumnType::Citext,
            ColumnType::MoneyArray => ColumnType::Money,
            ColumnType::InetArray => ColumnType::Inet,
            ColumnType::CidrArray => ColumnType::Cidr,
            ColumnType::MacAddrArray => ColumnType::MacAddr,
            ColumnType::BitArray => ColumnType::Bit,
            ColumnType::VarBitArray => ColumnType::VarBit,
            _ => return None,
        })
    }
}
