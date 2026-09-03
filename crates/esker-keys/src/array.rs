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
    /// ([ADR 0047](../../docs/adr/0047-an-array-is-a-column-type-over-one-element-type.md)).
    #[must_use]
    pub fn array_of(element: ColumnType) -> Option<ColumnType> {
        Some(match element {
            ColumnType::Int8 => ColumnType::Int8Array,
            ColumnType::Int4 => ColumnType::Int4Array,
            ColumnType::Numeric => ColumnType::NumericArray,
            ColumnType::Text => ColumnType::TextArray,
            _ => return None,
        })
    }

    /// The element type an array type is over, or `None` for a type that is not an array.
    #[must_use]
    pub fn element_of(array: ColumnType) -> Option<ColumnType> {
        Some(match array {
            ColumnType::Int8Array => ColumnType::Int8,
            ColumnType::Int4Array => ColumnType::Int4,
            ColumnType::NumericArray => ColumnType::Numeric,
            ColumnType::TextArray => ColumnType::Text,
            _ => return None,
        })
    }
}
