//! Where a catalog entry lives in the `'m'` space, and what its bytes are.
//!
//! ```text
//! 'm' ++ "sql" ++ 'v'                          the catalog version, one counter for the cluster
//! 'm' ++ "sql" ++ 's' ++ tenant:u64            the next relation id, one counter per tenant
//! 'm' ++ "sql" ++ 't' ++ tenant:u64 ++ id:u64  a table, with its indexes inside it
//! 'm' ++ "sql" ++ 'n' ++ tenant:u64 ++ name    a name, and what relation it is
//! ```
//!
//! Ids in keys are memcomparable (`esker_keys::codec`), so a scan of one tenant's tables visits
//! them in id order. Record *bodies* are hand-written little-endian, like every other value this
//! project writes (`CLAUDE.md`), behind a version byte that an unknown value makes an error rather
//! than a guess.
//!
//! **A name maps to a table or to an index, through one map.** PostgreSQL keeps both in `pg_class`
//! and it really is one namespace: `CREATE INDEX dup ON t (a)` where a table `dup` exists answers
//! `42P07 relation "dup" already exists`. Confirmed against the server, and it is the reason there
//! is one name map here instead of the two this started as.

use esker_base::varint;
use esker_keys::{codec, prefix};

use crate::catalog::{ColumnDef, IndexDef, Relation, TableDef};
use crate::error::{Result, SqlError};
use crate::value::ColumnType;

/// The version byte on every catalog record.
///
/// Version 2 added a table's schema version (ADR 0019). Version 1 is not read: nothing has ever
/// persisted a catalog outside a test, and a compatibility path for data that does not exist is
/// one nothing can check.
pub(crate) const CATALOG_FORMAT_VERSION: u8 = 2;

/// What every catalog key begins with, after the `'m'` namespace byte.
const SQL: &[u8] = b"sql";

const KIND_VERSION: u8 = b'v';
const KIND_NEXT_ID: u8 = b's';
const KIND_TABLE: u8 = b't';
const KIND_NAME: u8 = b'n';
const KIND_INDEX: u8 = b'i';
const KIND_PRIMARY_KEY: u8 = b'p';

/// Tags for [`ColumnType`] as stored. Ours rather than PostgreSQL's OIDs, because these are a
/// format we own and must never move; the OIDs stay on the wire where they belong.
const TAG_INT8: u8 = 1;
const TAG_TEXT: u8 = 2;
const TAG_BOOL: u8 = 3;
const TAG_BYTEA: u8 = 4;
const TAG_TIMESTAMPTZ: u8 = 5;
const TAG_DOUBLE: u8 = 6;

fn tag_of(ty: ColumnType) -> u8 {
    match ty {
        ColumnType::Int8 => TAG_INT8,
        ColumnType::Text => TAG_TEXT,
        ColumnType::Bool => TAG_BOOL,
        ColumnType::Bytea => TAG_BYTEA,
        ColumnType::TimestampTz => TAG_TIMESTAMPTZ,
        ColumnType::Double => TAG_DOUBLE,
    }
}

fn type_of(tag: u8) -> Result<ColumnType> {
    Ok(match tag {
        TAG_INT8 => ColumnType::Int8,
        TAG_TEXT => ColumnType::Text,
        TAG_BOOL => ColumnType::Bool,
        TAG_BYTEA => ColumnType::Bytea,
        TAG_TIMESTAMPTZ => ColumnType::TimestampTz,
        TAG_DOUBLE => ColumnType::Double,
        other => return Err(corrupt(format!("column type tag {other}"))),
    })
}

/// `'m' ++ "sql" ++ 'v'`. One counter, read once per transaction.
#[must_use]
pub(super) fn version_key() -> Vec<u8> {
    prefix::meta_key(&[SQL, &[KIND_VERSION]].concat())
}

/// `'m' ++ "sql" ++ 's' ++ tenant`. Table and index ids come from one sequence per tenant, so no
/// two relations of one tenant can share an id whatever they are.
#[must_use]
pub(super) fn next_id_key(tenant: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_NEXT_ID]].concat();
    codec::encode_u64(tenant, &mut suffix);
    prefix::meta_key(&suffix)
}

/// `'m' ++ "sql" ++ 't' ++ tenant ++ id`.
#[must_use]
pub(super) fn table_key(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_TABLE]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// `'m' ++ "sql" ++ 'n' ++ tenant ++ name`. The name is the whole rest of the key, so nothing has
/// to be prefix-free about it and a name cannot be confused with a longer one.
#[must_use]
pub(super) fn name_key(tenant: u64, name: &str) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_NAME]].concat();
    codec::encode_u64(tenant, &mut suffix);
    suffix.extend_from_slice(name.as_bytes());
    prefix::meta_key(&suffix)
}

/// A monotone counter as stored: little-endian, like every other record body here.
#[must_use]
pub(super) fn encode_counter(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Reads a counter, or says the bytes are not one.
pub(super) fn decode_counter(bytes: &[u8]) -> Result<u64> {
    bytes
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| corrupt(format!("a counter is {} bytes, not 8", bytes.len())))
}

/// A table and everything an executor needs to write a row into it — its columns, its primary key
/// and **its indexes**, all in one record.
///
/// The indexes are inside rather than beside on purpose. A cache entry is a whole table, and if
/// the index list lived under its own keys a cached table could be current while its index list
/// was stale — which is the one kind of staleness that corrupts data rather than returning old
/// data, because a row would be written without an entry in an index that exists.
pub(super) fn encode_table(table: &TableDef) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&table.id.to_le_bytes());
    put_str(&table.name, &mut out);

    put_str(&table.primary_key_name, &mut out);

    varint::put_u64(table.schema_version, &mut out);

    varint::put_u64(table.columns.len() as u64, &mut out);
    for column in &table.columns {
        put_str(&column.name, &mut out);
        out.push(tag_of(column.ty));
        out.push(u8::from(column.not_null));
    }

    varint::put_u64(table.primary_key.len() as u64, &mut out);
    for &ordinal in &table.primary_key {
        varint::put_u64(ordinal as u64, &mut out);
    }

    varint::put_u64(table.indexes.len() as u64, &mut out);
    for index in &table.indexes {
        out.extend_from_slice(&index.id.to_le_bytes());
        put_str(&index.name, &mut out);
        out.push(u8::from(index.unique));
        varint::put_u64(index.columns.len() as u64, &mut out);
        for &ordinal in &index.columns {
            varint::put_u64(ordinal as u64, &mut out);
        }
    }
    out
}

/// Reads a table record back. Every failure is typed: a catalog is on-disk data like any other.
pub(super) fn decode_table(bytes: &[u8]) -> Result<TableDef> {
    let mut reader = Reader::new(bytes)?;
    let id = reader.u64_le()?;
    let name = reader.string()?;
    let primary_key_name = reader.string()?;
    let schema_version = reader.varint()?;

    let mut columns = Vec::with_capacity(reader.count()?);
    for _ in 0..columns.capacity() {
        columns.push(ColumnDef {
            name: reader.string()?,
            ty: type_of(reader.byte()?)?,
            not_null: reader.flag()?,
        });
    }

    let mut primary_key = Vec::with_capacity(reader.count()?);
    for _ in 0..primary_key.capacity() {
        primary_key.push(reader.ordinal(columns.len())?);
    }

    let mut indexes = Vec::with_capacity(reader.count()?);
    for _ in 0..indexes.capacity() {
        let id = reader.u64_le()?;
        let name = reader.string()?;
        let unique = reader.flag()?;
        let mut index_columns = Vec::with_capacity(reader.count()?);
        for _ in 0..index_columns.capacity() {
            index_columns.push(reader.ordinal(columns.len())?);
        }
        indexes.push(IndexDef {
            id,
            name,
            unique,
            columns: index_columns,
        });
    }

    reader.finish()?;
    Ok(TableDef {
        id,
        name,
        columns,
        primary_key,
        indexes,
        primary_key_name,
        schema_version,
    })
}

/// What a name resolves to.
pub(super) fn encode_relation(relation: &Relation) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    match relation {
        Relation::Table { table_id } => {
            out.push(KIND_TABLE);
            out.extend_from_slice(&table_id.to_le_bytes());
        }
        Relation::Index { table_id, index_id } => {
            out.push(KIND_INDEX);
            out.extend_from_slice(&table_id.to_le_bytes());
            out.extend_from_slice(&index_id.to_le_bytes());
        }
        Relation::PrimaryKey { table_id } => {
            out.push(KIND_PRIMARY_KEY);
            out.extend_from_slice(&table_id.to_le_bytes());
        }
    }
    out
}

/// Reads a name entry back.
pub(super) fn decode_relation(bytes: &[u8]) -> Result<Relation> {
    let mut reader = Reader::new(bytes)?;
    let relation = match reader.byte()? {
        KIND_TABLE => Relation::Table {
            table_id: reader.u64_le()?,
        },
        KIND_INDEX => Relation::Index {
            table_id: reader.u64_le()?,
            index_id: reader.u64_le()?,
        },
        KIND_PRIMARY_KEY => Relation::PrimaryKey {
            table_id: reader.u64_le()?,
        },
        other => return Err(corrupt(format!("relation kind byte {other}"))),
    };
    reader.finish()?;
    Ok(relation)
}

fn put_str(value: &str, out: &mut Vec<u8>) {
    varint::put_u64(value.len() as u64, out);
    out.extend_from_slice(value.as_bytes());
}

/// A cursor that fails rather than panics, whatever the bytes are (`CLAUDE.md` invariant 9).
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        let (&version, rest) = bytes
            .split_first()
            .ok_or_else(|| corrupt("a catalog record is empty"))?;
        if version != CATALOG_FORMAT_VERSION {
            return Err(corrupt(format!(
                "catalog format version {version} is not {CATALOG_FORMAT_VERSION}"
            )));
        }
        Ok(Reader { bytes: rest })
    }

    fn byte(&mut self) -> Result<u8> {
        let (&byte, rest) = self
            .bytes
            .split_first()
            .ok_or_else(|| corrupt("a catalog record ends early"))?;
        self.bytes = rest;
        Ok(byte)
    }

    /// A boolean field. Any other byte is a value we never wrote.
    fn flag(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(corrupt(format!("flag byte {other} is neither 0 nor 1"))),
        }
    }

    fn u64_le(&mut self) -> Result<u64> {
        let (head, rest) = self
            .bytes
            .split_first_chunk::<8>()
            .ok_or_else(|| corrupt("a catalog record ends inside an id"))?;
        self.bytes = rest;
        Ok(u64::from_le_bytes(*head))
    }

    fn varint(&mut self) -> Result<u64> {
        let (value, consumed) =
            varint::get_u64(self.bytes).map_err(|error| corrupt(format!("varint: {error}")))?;
        self.bytes = &self.bytes[consumed..];
        Ok(value)
    }

    /// A length, bounded so a corrupt one asks for an allocation rather than getting it.
    fn count(&mut self) -> Result<usize> {
        let count = self.varint()?;
        if count > self.bytes.len() as u64 {
            return Err(corrupt(format!(
                "a count of {count} in {} remaining bytes",
                self.bytes.len()
            )));
        }
        usize::try_from(count).map_err(|_| corrupt("a count larger than this machine"))
    }

    /// A column position, checked against the table it indexes into. A catalog that points at a
    /// column that is not there would be a panic later or, worse, the wrong column.
    fn ordinal(&mut self, columns: usize) -> Result<usize> {
        let ordinal = usize::try_from(self.varint()?)
            .map_err(|_| corrupt("a column ordinal larger than this machine"))?;
        if ordinal >= columns {
            return Err(corrupt(format!(
                "column {ordinal} of a table with {columns} columns"
            )));
        }
        Ok(ordinal)
    }

    fn string(&mut self) -> Result<String> {
        let len = self.count()?;
        let (body, rest) = self
            .bytes
            .split_at_checked(len)
            .ok_or_else(|| corrupt(format!("a name of {len} bytes is truncated")))?;
        self.bytes = rest;
        String::from_utf8(body.to_vec()).map_err(|_| corrupt("a name that is not UTF-8"))
    }

    fn finish(self) -> Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        Err(corrupt(format!(
            "{} bytes after the end of a catalog record",
            self.bytes.len()
        )))
    }
}

fn corrupt(what: impl Into<String>) -> SqlError {
    SqlError::DataCorrupted(what.into())
}
