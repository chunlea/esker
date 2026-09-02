//! The fragment **result** format: what a columnar node answers with.
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 3,
//! `docs/plans/phase-8-learner.md` §wire. Milestone 2 built the fragment *request* as a
//! versioned, fuzzed, golden-tested format and left the answer as Rust types —
//! `FragmentResult { output, stats }` over `FragmentOutput::Rows`/`Groups` — with no way to put
//! it on a wire. This is that way.
//!
//! ```text
//! result := version:u8 ++ kind:u8 ++ body ++ crc32c:u32     // crc covers version..body
//! kind   := 0 rows | 1 groups
//!
//! rows   := ncols:varint ++ type:u8 * ncols
//!         ++ nrows:varint ++ (value * ncols) * nrows
//!
//! groups := nkeys:varint ++ type:u8 * nkeys
//!         ++ naggs:varint ++ (kind:u8 ++ type:u8) * naggs
//!         ++ ngroups:varint ++ (value * nkeys ++ partial * naggs) * ngroups
//!
//! value   := 0x00 | tag:u8 ++ payload
//! partial := count:varint | value
//! ```
//!
//! # Why the types are declared, and declared once
//!
//! A result could be decoded against the column list the caller sent, with no types on the wire
//! at all. It is not, and the reason is a lesson this project has already paid for: format
//! version 2 of the columnar file exists because **a checksum proves a block is intact, not that
//! it is the block that was asked for** — a fuzz laid one valid chunk over another and the scan
//! answered with the wrong stripe's rows, with no error anywhere.
//!
//! A result decoded against the wrong column list is the same failure with a different cause:
//! intact bytes, wrong meaning, no error. Declaring the shape in the result turns that into a
//! decode that refuses. Once in a header rather than per value, because the per-value cost would
//! be a byte on every value of a million-row answer and the header costs one per column.
//!
//! # Why there is a checksum inside a frame that already has one
//!
//! `esker-proto`'s frame is checksummed, so a result that arrives intact in a good frame is
//! intact. The checksum here is for every other path: a result cached, spilled to disk by a node
//! finishing a two-level aggregate, or forwarded between nodes without being re-framed. ADR 0002
//! asks every format to carry a version and a checksum; this is a format, so it carries both.
//!
//! # `sum(double)` does not associate
//!
//! Combining partial aggregates adds numbers in an order a single-level fold would not, so a
//! `sum(double)` finished from many fragments may differ in its last bits from the same query run
//! over one. `esker_columnar`'s `Partial::combine` documents it where partials are combined; this
//! is the other place a reader meets it, because a wire is where "many fragments" stops being
//! hypothetical. It is inherent to two-level aggregation rather than a defect, and it is why a
//! fragment folds its own answer in row order across every stripe rather than per stripe and
//! combined.

use esker_base::crc32c;

use crate::codec::{DecodeError, Decoder, Encoder};

/// Format version of a fragment result. Bumping it needs an ADR and a new golden beside the old.
pub const RESULT_FORMAT_VERSION: u8 = 1;

const KIND_ROWS: u8 = 0;
const KIND_GROUPS: u8 = 1;

/// A NULL, which is why no type may use tag zero.
const TAG_NULL: u8 = 0;

const AGG_COUNT: u8 = 0;
const AGG_SUM: u8 = 1;
const AGG_MIN: u8 = 2;
const AGG_MAX: u8 = 3;

/// The type of a value in a fragment result.
///
/// **These tag bytes are copied, deliberately, and must not move.** `esker_sql::catalog::record`'s
/// `TAG_*` constants are the original and they are on disk; `esker_columnar::value::ColumnType`
/// already copies them rather than linking, pinned by a test called `tags_match_the_row_side`.
/// This is the third copy and it is pinned the same way, because the alternatives are worse in
/// both directions: this crate cannot depend on `esker-sql`, which sits above it, and must not
/// depend on `esker-columnar`, a leaf whose format versions would then be able to break the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueType {
    /// A 64-bit signed integer.
    Int8,
    /// UTF-8 text.
    Text,
    /// A boolean.
    Bool,
    /// Opaque bytes.
    Bytea,
    /// Microseconds from 2000-01-01 UTC, as a 64-bit signed integer.
    TimestampTz,
    /// A 64-bit float.
    Double,
    /// Microseconds from 2000-01-01, with **no** zone: PostgreSQL's `timestamp`.
    Timestamp,
    /// A 32-bit signed integer.
    ///
    /// Appended by [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md), never
    /// renumbered: a peer that has not learned it answers `unknown type tag` rather than reading
    /// four bytes as eight, which is the direction this vocabulary is built to fail in.
    Int4,
}

impl ValueType {
    /// Every type, so a test cannot silently skip one.
    pub const ALL: [ValueType; 8] = [
        ValueType::Int8,
        ValueType::Int4,
        ValueType::Timestamp,
        ValueType::Text,
        ValueType::Bool,
        ValueType::Bytea,
        ValueType::TimestampTz,
        ValueType::Double,
    ];

    /// The tag byte. Frozen — see the type's docs.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            ValueType::Int8 => 1,
            ValueType::Text => 2,
            ValueType::Bool => 3,
            ValueType::Bytea => 4,
            ValueType::TimestampTz => 5,
            ValueType::Double => 6,
            ValueType::Int4 => 7,
            ValueType::Timestamp => 8,
        }
    }

    /// The type a tag names, or an error for one no version has written.
    pub fn from_tag(tag: u8) -> Result<Self, DecodeError> {
        Ok(match tag {
            1 => ValueType::Int8,
            2 => ValueType::Text,
            3 => ValueType::Bool,
            4 => ValueType::Bytea,
            5 => ValueType::TimestampTz,
            6 => ValueType::Double,
            7 => ValueType::Int4,
            8 => ValueType::Timestamp,
            _ => return Err(DecodeError::invalid("result.type", "unknown type tag")),
        })
    }
}

/// One value in a fragment result.
///
/// Mirrors `esker_columnar::Value` without linking it. The mapping between the two is a total
/// match in whichever crate owns the conversion, which is what makes a new type on either side a
/// compile error rather than a silent hole.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// No value.
    Null,
    /// [`ValueType::Int8`].
    Int8(i64),
    /// [`ValueType::Text`].
    Text(String),
    /// [`ValueType::Bool`].
    Bool(bool),
    /// [`ValueType::Bytea`].
    Bytea(Vec<u8>),
    /// [`ValueType::TimestampTz`].
    TimestampTz(i64),
    /// [`ValueType::Double`].
    Double(f64),
    /// [`ValueType::Int4`].
    Int4(i32),
    /// [`ValueType::Timestamp`].
    Timestamp(i64),
}

impl Value {
    /// The type of this value, or `None` for NULL, which fits every type.
    #[must_use]
    pub fn value_type(&self) -> Option<ValueType> {
        Some(match self {
            Value::Null => return None,
            Value::Int8(_) => ValueType::Int8,
            Value::Text(_) => ValueType::Text,
            Value::Bool(_) => ValueType::Bool,
            Value::Bytea(_) => ValueType::Bytea,
            Value::TimestampTz(_) => ValueType::TimestampTz,
            Value::Double(_) => ValueType::Double,
            Value::Int4(_) => ValueType::Int4,
            Value::Timestamp(_) => ValueType::Timestamp,
        })
    }

    fn encode(&self, out: &mut Encoder) {
        match self {
            Value::Null => out.put_u8(TAG_NULL),
            Value::Int8(v) => {
                out.put_u8(ValueType::Int8.tag());
                out.put_u64(u64::from_le_bytes(v.to_le_bytes()));
            }
            Value::TimestampTz(v) => {
                out.put_u8(ValueType::TimestampTz.tag());
                out.put_u64(u64::from_le_bytes(v.to_le_bytes()));
            }
            Value::Timestamp(v) => {
                out.put_u8(ValueType::Timestamp.tag());
                out.put_u64(u64::from_le_bytes(v.to_le_bytes()));
            }
            Value::Double(v) => {
                out.put_u8(ValueType::Double.tag());
                out.put_u64(v.to_bits());
            }
            // Its own width on the wire, as it is on disk: four bytes, so a reader that knows the
            // tag cannot mistake the framing.
            Value::Int4(v) => {
                out.put_u8(ValueType::Int4.tag());
                out.put_u32(u32::from_le_bytes(v.to_le_bytes()));
            }
            Value::Bool(v) => {
                out.put_u8(ValueType::Bool.tag());
                out.put_u8(u8::from(*v));
            }
            Value::Text(v) => {
                out.put_u8(ValueType::Text.tag());
                out.put_str(v);
            }
            Value::Bytea(v) => {
                out.put_u8(ValueType::Bytea.tag());
                out.put_bytes(v);
            }
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let tag = input.get_u8("result.value.tag")?;
        if tag == TAG_NULL {
            return Ok(Value::Null);
        }
        Ok(match ValueType::from_tag(tag)? {
            ValueType::Int8 => Value::Int8(i64::from_le_bytes(
                input.get_u64("result.value.int8")?.to_le_bytes(),
            )),
            ValueType::TimestampTz => Value::TimestampTz(i64::from_le_bytes(
                input.get_u64("result.value.timestamptz")?.to_le_bytes(),
            )),
            ValueType::Timestamp => Value::Timestamp(i64::from_le_bytes(
                input.get_u64("result.value.timestamp")?.to_le_bytes(),
            )),
            ValueType::Double => {
                Value::Double(f64::from_bits(input.get_u64("result.value.double")?))
            }
            ValueType::Int4 => Value::Int4(i32::from_le_bytes(
                input.get_u32("result.value.int4")?.to_le_bytes(),
            )),
            ValueType::Bool => Value::Bool(input.get_bool("result.value.bool")?),
            ValueType::Text => Value::Text(input.get_str("result.value.text")?.to_owned()),
            ValueType::Bytea => Value::Bytea(input.get_bytes("result.value.bytea")?.to_vec()),
        })
    }
}

/// A partial aggregate, to be folded by whoever finishes the two-level aggregate.
///
/// Mirrors `esker_columnar::scan::group::Partial`. See the module docs for why folding
/// `Sum(Double)` is not associative.
#[derive(Debug, Clone, PartialEq)]
pub enum Partial {
    /// `count(*)` or `count(col)`. Folded by addition.
    Count(u64),
    /// `sum(col)`, NULL over no rows.
    Sum(Option<Value>),
    /// `min(col)`, NULL over no rows.
    Min(Option<Value>),
    /// `max(col)`, NULL over no rows.
    Max(Option<Value>),
}

impl Partial {
    /// Which aggregate this is a partial of, so a builder can check a body against its own
    /// declaration before [`encode`] refuses it.
    #[must_use]
    pub fn kind(&self) -> AggregateKind {
        match self {
            Partial::Count(_) => AggregateKind::Count,
            Partial::Sum(_) => AggregateKind::Sum,
            Partial::Min(_) => AggregateKind::Min,
            Partial::Max(_) => AggregateKind::Max,
        }
    }
}

/// One group's key and its partial aggregates.
///
/// Mirrors `esker_columnar::scan::group::Group`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The grouping values, in the order the fragment named its grouping slots.
    pub key: Vec<Value>,
    /// One partial per aggregate the fragment asked for, in that order.
    pub partials: Vec<Partial>,
}

/// What a fragment produced.
///
/// Mirrors `esker_columnar::FragmentOutput`, plus the type declarations the wire needs and the
/// in-process type did not.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// The projected columns of the rows that matched, in row order.
    Rows {
        /// One type per projected column. Every row has exactly this many values.
        types: Vec<ValueType>,
        /// The rows.
        rows: Vec<Vec<Value>>,
    },
    /// Partial aggregates, one entry per group.
    Groups {
        /// One type per grouping slot.
        key_types: Vec<ValueType>,
        /// The kind and value type of each aggregate, in the order the fragment asked for them.
        /// A `Count`'s type is `None`: it is a `u64` and not a column value.
        aggregates: Vec<(AggregateKind, Option<ValueType>)>,
        /// The groups.
        groups: Vec<Group>,
    },
}

/// Which aggregate a partial is a partial of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    /// `count`.
    Count,
    /// `sum`.
    Sum,
    /// `min`.
    Min,
    /// `max`.
    Max,
}

impl AggregateKind {
    fn tag(self) -> u8 {
        match self {
            AggregateKind::Count => AGG_COUNT,
            AggregateKind::Sum => AGG_SUM,
            AggregateKind::Min => AGG_MIN,
            AggregateKind::Max => AGG_MAX,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, DecodeError> {
        Ok(match tag {
            AGG_COUNT => AggregateKind::Count,
            AGG_SUM => AggregateKind::Sum,
            AGG_MIN => AggregateKind::Min,
            AGG_MAX => AggregateKind::Max,
            _ => {
                return Err(DecodeError::invalid(
                    "result.aggregate.kind",
                    "unknown aggregate",
                ));
            }
        })
    }
}

/// A body that cannot be encoded because it contradicts itself.
///
/// The only way to reach it is a [`Body::Groups`] whose groups do not match the aggregates it
/// declares. That is a bug in whatever built the body, not a wire condition, and it is a hard
/// error rather than a `debug_assert` because the bytes it would otherwise produce pass their own
/// checksum and decode into different partials than were meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A group's partials do not match the aggregates the body declares, in kind or in number.
    #[error("a group's partials do not match the declared aggregates")]
    Mismatch,
}

/// Encodes a fragment result.
pub fn encode(body: &Body) -> Result<Vec<u8>, EncodeError> {
    let mut out = Encoder::new();
    out.put_u8(RESULT_FORMAT_VERSION);
    match body {
        Body::Rows { types, rows } => {
            out.put_u8(KIND_ROWS);
            out.put_varint(types.len() as u64);
            for ty in types {
                out.put_u8(ty.tag());
            }
            out.put_varint(rows.len() as u64);
            for row in rows {
                for value in row {
                    value.encode(&mut out);
                }
            }
        }
        Body::Groups {
            key_types,
            aggregates,
            groups,
        } => {
            out.put_u8(KIND_GROUPS);
            out.put_varint(key_types.len() as u64);
            for ty in key_types {
                out.put_u8(ty.tag());
            }
            out.put_varint(aggregates.len() as u64);
            for (kind, ty) in aggregates {
                out.put_u8(kind.tag());
                out.put_u8(ty.map_or(TAG_NULL, ValueType::tag));
            }
            out.put_varint(groups.len() as u64);
            for group in groups {
                for value in &group.key {
                    value.encode(&mut out);
                }
                // Written against the DECLARED kinds, not against each partial's own variant.
                // The header is what the decoder reads the body by, so a group whose partials
                // disagree with it would be written one way and read another — bytes that pass
                // their own checksum and mean something else, which is the failure the declared
                // header exists to prevent. Refused rather than written.
                for (position, (kind, _)) in aggregates.iter().enumerate() {
                    let partial = group.partials.get(position).ok_or(EncodeError::Mismatch)?;
                    match (kind, partial) {
                        (AggregateKind::Count, Partial::Count(n)) => out.put_varint(*n),
                        (AggregateKind::Sum, Partial::Sum(v))
                        | (AggregateKind::Min, Partial::Min(v))
                        | (AggregateKind::Max, Partial::Max(v)) => {
                            v.as_ref().unwrap_or(&Value::Null).encode(&mut out);
                        }
                        _ => return Err(EncodeError::Mismatch),
                    }
                }
                if group.partials.len() != aggregates.len() {
                    return Err(EncodeError::Mismatch);
                }
            }
        }
    }
    let mut bytes = out.finish();
    let checksum = crc32c::checksum(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    Ok(bytes)
}

/// Reads a fragment result.
///
/// The checksum is verified **first**, so that intact bytes carrying a version or a tag this
/// build does not know are reported as a build that cannot read them rather than as damage. Both
/// look like "an error" from a distance and they call for opposite actions: one is a rolling
/// upgrade, the other is a corrupt store.
pub fn decode(bytes: &[u8]) -> Result<Body, DecodeError> {
    let Some(split) = bytes.len().checked_sub(4) else {
        return Err(DecodeError::invalid("result", "shorter than its checksum"));
    };
    let (body, tail) = bytes.split_at(split);
    let Ok(tail) = <[u8; 4]>::try_from(tail) else {
        return Err(DecodeError::invalid("result", "checksum is not four bytes"));
    };
    let found = u32::from_le_bytes(tail);
    let expected = crc32c::checksum(body);
    if found != expected {
        return Err(DecodeError::invalid("result", "checksum mismatch"));
    }

    let mut input = Decoder::new(body);
    let version = input.get_u8("result.version")?;
    if version != RESULT_FORMAT_VERSION {
        return Err(DecodeError::invalid("result.version", "unknown version"));
    }
    let body = match input.get_u8("result.kind")? {
        KIND_ROWS => {
            let types = decode_types(&mut input, "result.rows.types")?;
            let count = input.get_count("result.rows.len")?;
            let mut rows = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let mut row = Vec::with_capacity(types.len());
                for _ in 0..types.len() {
                    row.push(Value::decode(&mut input)?);
                }
                rows.push(row);
            }
            Body::Rows { types, rows }
        }
        KIND_GROUPS => {
            let key_types = decode_types(&mut input, "result.groups.key_types")?;
            let aggregate_count = input.get_count("result.groups.aggregates")?;
            let mut aggregates = Vec::with_capacity(aggregate_count.min(1024));
            for _ in 0..aggregate_count {
                let kind = AggregateKind::from_tag(input.get_u8("result.aggregate.kind")?)?;
                let tag = input.get_u8("result.aggregate.type")?;
                let ty = if tag == TAG_NULL {
                    None
                } else {
                    Some(ValueType::from_tag(tag)?)
                };
                aggregates.push((kind, ty));
            }
            let count = input.get_count("result.groups.len")?;
            let mut groups = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let mut key = Vec::with_capacity(key_types.len());
                for _ in 0..key_types.len() {
                    key.push(Value::decode(&mut input)?);
                }
                let mut partials = Vec::with_capacity(aggregates.len());
                for (kind, _) in &aggregates {
                    partials.push(match kind {
                        AggregateKind::Count => {
                            Partial::Count(input.get_varint("result.partial.count")?)
                        }
                        AggregateKind::Sum => Partial::Sum(some_or_null(&mut input)?),
                        AggregateKind::Min => Partial::Min(some_or_null(&mut input)?),
                        AggregateKind::Max => Partial::Max(some_or_null(&mut input)?),
                    });
                }
                groups.push(Group { key, partials });
            }
            Body::Groups {
                key_types,
                aggregates,
                groups,
            }
        }
        _ => return Err(DecodeError::invalid("result.kind", "unknown kind")),
    };
    input.finish()?;
    Ok(body)
}

/// A `NULL` aggregate over no rows and an absent value are the same thing on this wire, so the
/// two collapse rather than being distinguished by a presence byte nobody could act on.
fn some_or_null(input: &mut Decoder<'_>) -> Result<Option<Value>, DecodeError> {
    Ok(match Value::decode(input)? {
        Value::Null => None,
        value => Some(value),
    })
}

fn decode_types(
    input: &mut Decoder<'_>,
    field: &'static str,
) -> Result<Vec<ValueType>, DecodeError> {
    let count = input.get_count(field)?;
    let mut types = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        types.push(ValueType::from_tag(input.get_u8(field)?)?);
    }
    Ok(types)
}
