//! Where a catalog entry lives in the `'m'` space, and what its bytes are.
//!
//! ```text
//! 'm' ++ "sql" ++ 'v'                          the catalog version, one counter for the cluster
//! 'm' ++ "sql" ++ 's' ++ tenant:u64            the next relation id, one counter per tenant
//! 'm' ++ "sql" ++ 't' ++ tenant:u64 ++ id:u64  a table, with its indexes inside it
//! 'm' ++ "sql" ++ 'n' ++ tenant:u64 ++ name    a name, and what relation it is
//! 'm' ++ "sql" ++ 'd'                          the cluster's default MVCC retention
//! 'm' ++ "sql" ++ 'r' ++ tenant:u64 ++ id:u64  one table's retention override
//! 'm' ++ "sql" ++ 'a' ++ tenant:u64 ++ id:u64  one table's next internal row id
//! 'm' ++ "sql" ++ 'c' ++ tenant:u64 ++ name    a checkpoint: a name and the timestamp it means
//! ```
//!
//! The two retention records are read by the **garbage collector**, which lives below this crate
//! and does not link it ([ADR 0021](../../../../docs/adr/0021-time-machine.md)). That is why they
//! are records of their own rather than fields of the table record: the GC filter must read them
//! without decoding a table definition, whose format it has no business knowing, and a prefix scan
//! of `'m' ++ "sql" ++ 'r'` returns every override and nothing else. The cluster default is under
//! its own kind byte for the same reason — under `'r'` it would be inside that scan.
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

use crate::catalog::{ColumnDef, IndexDef, Relation, SchemaState, TableDef};
use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum};

/// The version byte on every catalog record.
///
/// Version 2 added a table's schema version (ADR 0019). Version 3 added a column's default and its
/// missing value, and the schema state on every column and index (ADR 0020,
/// `docs/plans/phase-6e.md` §4).
///
/// Version 1 is not read: nothing had ever persisted a catalog when version 2 landed, so a
/// compatibility path for it was one nothing could check. **Version 2 is different** — this crate
/// has had a real backend since phase 6a unit 11, so v2 records exist and [`decode_table`] reads
/// them: a v2 column has no default and no missing value, which is what a column that was never
/// given one means.
pub(crate) const CATALOG_FORMAT_VERSION: u8 = 3;

/// The oldest catalog record this crate reads.
///
/// Version 2. Version 1 is refused because no version 1 data has ever existed outside a test on the
/// same commit (ADR 0019 Decision 2); version 2 data **does** exist — this crate has had a real
/// backend since phase 6a unit 11 — so it is read rather than refused.
const OLDEST_TABLE_VERSION: u8 = 2;

/// What every catalog key begins with, after the `'m'` namespace byte.
const SQL: &[u8] = b"sql";

const KIND_VERSION: u8 = b'v';
const KIND_NEXT_ID: u8 = b's';
const KIND_TABLE: u8 = b't';
const KIND_NAME: u8 = b'n';
const KIND_INDEX: u8 = b'i';
const KIND_PRIMARY_KEY: u8 = b'p';
const KIND_RETENTION_DEFAULT: u8 = b'd';
const KIND_RETENTION: u8 = b'r';
const KIND_ROW_ID: u8 = b'a';
const KIND_CHECKPOINT: u8 = b'c';

/// Tags for [`ColumnType`] as stored. Ours rather than PostgreSQL's OIDs, because these are a
/// format we own and must never move; the OIDs stay on the wire where they belong.
const TAG_INT8: u8 = 1;
const TAG_TEXT: u8 = 2;
const TAG_BOOL: u8 = 3;
const TAG_BYTEA: u8 = 4;
const TAG_TIMESTAMPTZ: u8 = 5;
const TAG_DOUBLE: u8 = 6;

/// Tags for [`SchemaState`] as stored. Ours, and they must never move: an index read as the wrong
/// state is an index a node writes when it should not, which is the whole failure ADR 0020 is about.
const TAG_ABSENT: u8 = 0;
const TAG_DELETE_ONLY: u8 = 1;
const TAG_WRITE_ONLY: u8 = 2;
const TAG_PUBLIC: u8 = 3;

fn state_tag(state: SchemaState) -> u8 {
    match state {
        SchemaState::Absent => TAG_ABSENT,
        SchemaState::DeleteOnly => TAG_DELETE_ONLY,
        SchemaState::WriteOnly => TAG_WRITE_ONLY,
        SchemaState::Public => TAG_PUBLIC,
    }
}

fn state_of(tag: u8) -> Result<SchemaState> {
    Ok(match tag {
        TAG_ABSENT => SchemaState::Absent,
        TAG_DELETE_ONLY => SchemaState::DeleteOnly,
        TAG_WRITE_ONLY => SchemaState::WriteOnly,
        TAG_PUBLIC => SchemaState::Public,
        // Never a guess: an unknown state is not "probably public", it is a record this build did
        // not write.
        other => {
            return Err(corrupt(format!(
                "schema state tag {other} is not one of ours"
            )));
        }
    })
}

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

/// `'m' ++ "sql" ++ 'd'`. One number for the cluster, absent until somebody sets it.
#[must_use]
pub(super) fn default_retention_key() -> Vec<u8> {
    prefix::meta_key(&[SQL, &[KIND_RETENTION_DEFAULT]].concat())
}

/// `'m' ++ "sql" ++ 'r' ++ tenant ++ table_id`. Absent for a table that takes the default.
///
/// Ids are memcomparable, so one scan of `'m' ++ "sql" ++ 'r'` visits every override in id order,
/// which is what the collector wants: it reads the whole map once per compaction rather than
/// asking per key.
#[must_use]
pub(super) fn retention_key(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_RETENTION]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// A retention, in milliseconds, behind the same version byte as every other record.
///
/// Milliseconds because that is the unit of a timestamp's physical half (`esker_pd::tso`), so
/// turning a retention into a timestamp distance is a shift and not a conversion anybody can get
/// wrong. [`super::RETENTION_FOREVER`] is `u64::MAX` and means *never collect*, which a reader has
/// to test for rather than subtract.
#[must_use]
pub(super) fn encode_retention(retention_ms: u64) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&retention_ms.to_le_bytes());
    out
}

/// Reads a retention back.
pub(super) fn decode_retention(bytes: &[u8]) -> Result<u64> {
    let mut reader = Reader::new(bytes)?;
    let retention = reader.u64_le()?;
    reader.finish()?;
    Ok(retention)
}

/// `'m' ++ "sql" ++ 'c' ++ tenant ++ name`. A checkpoint, absent until somebody names one.
///
/// The name is the whole rest of the key, like a relation name, so one scan of
/// `'m' ++ "sql" ++ 'c' ++ tenant` lists a tenant's checkpoints in name order and a name cannot be
/// confused with a longer one.
///
/// **A checkpoint is a claim, not a guarantee** ([ADR 0021](../../../../docs/adr/0021-time-machine.md)
/// Decision 3). It costs one record: no snapshot, no copy, no flush, because the data it refers to
/// is kept by retention whether anybody named it or not. The catch is exactly that — a checkpoint
/// older than the window names history that is gone, and reading at it is refused with the window
/// named, which is where the claim gets checked.
#[must_use]
pub(super) fn checkpoint_key(tenant: u64, name: &str) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_CHECKPOINT]].concat();
    codec::encode_u64(tenant, &mut suffix);
    suffix.extend_from_slice(name.as_bytes());
    prefix::meta_key(&suffix)
}

/// Every checkpoint of one tenant: `[start, end)` over the `'c'` space.
#[must_use]
pub(super) fn checkpoint_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_CHECKPOINT]].concat();
    codec::encode_u64(tenant, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    // One past the last key with this prefix. The tenant id is fixed-width and memcomparable, so
    // incrementing the last byte of the prefix is exact; a name cannot carry it past the boundary
    // because every key here begins with the whole prefix.
    let last = end.len() - 1;
    end[last] += 1;
    (start, end)
}

/// A checkpoint's timestamp, behind the same version byte as every other record.
#[must_use]
pub(super) fn encode_checkpoint(start_ts: u64) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&start_ts.to_le_bytes());
    out
}

/// Reads a checkpoint's timestamp back.
pub(super) fn decode_checkpoint(bytes: &[u8]) -> Result<u64> {
    let mut reader = Reader::new(bytes)?;
    let start_ts = reader.u64_le()?;
    reader.finish()?;
    Ok(start_ts)
}

/// The name out of a checkpoint key, for listing them.
///
/// A decode and not a parse: the prefix is fixed-width, so the name is whatever follows it.
pub(super) fn checkpoint_name(tenant: u64, key: &[u8]) -> Result<String> {
    let (prefix, _) = checkpoint_range(tenant);
    let Some(name) = key.strip_prefix(prefix.as_slice()) else {
        return Err(SqlError::DataCorrupted(
            "a checkpoint key outside the checkpoint range".to_owned(),
        ));
    };
    String::from_utf8(name.to_vec())
        .map_err(|_| SqlError::DataCorrupted("a checkpoint name that is not UTF-8".to_owned()))
}

/// `'m' ++ "sql" ++ 'a' ++ tenant ++ table_id`. The next unhanded-out row id for one table.
///
/// Its own key per table, and not the tenant's relation-id sequence: row ids are handed out per
/// *insert* rather than per relation, so one counter for a tenant would be a key every writer in
/// the tenant conflicts on.
#[must_use]
pub(super) fn row_id_key(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_ROW_ID]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
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
pub(super) fn encode_table(table: &TableDef) -> Result<Vec<u8>> {
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
        // Version 3. Two constants, each present-or-absent, each in the column's own type — so a
        // reader that knows the type knows the length, and neither needs a tag of its own.
        put_value(column.default.as_ref(), column.ty, &mut out)?;
        put_value(column.missing.as_ref(), column.ty, &mut out)?;
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
        // Version 3. A version 2 index has neither, and reads back `Public` at the table's own
        // schema version — which is what an index that never staged a change means.
        out.push(state_tag(index.state));
        varint::put_u64(index.state_since, &mut out);
        varint::put_u64(index.columns.len() as u64, &mut out);
        for &ordinal in &index.columns {
            varint::put_u64(ordinal as u64, &mut out);
        }
    }
    Ok(out)
}

/// Reads a table record back. Every failure is typed: a catalog is on-disk data like any other.
pub(super) fn decode_table(bytes: &[u8]) -> Result<TableDef> {
    let mut reader = Reader::at_least(bytes, OLDEST_TABLE_VERSION)?;
    let id = reader.u64_le()?;
    let name = reader.string()?;
    let primary_key_name = reader.string()?;
    let schema_version = reader.varint()?;

    let mut columns = Vec::with_capacity(reader.count()?);
    for _ in 0..columns.capacity() {
        let name = reader.string()?;
        let ty = type_of(reader.byte()?)?;
        let not_null = reader.flag()?;
        // A version 2 column has neither, which is what a column nobody gave a default means.
        let (default, missing) = if reader.version >= 3 {
            (reader.value(ty)?, reader.value(ty)?)
        } else {
            (None, None)
        };
        columns.push(ColumnDef {
            name,
            ty,
            not_null,
            default,
            missing,
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
        let (state, state_since) = if reader.version >= 3 {
            (state_of(reader.byte()?)?, reader.varint()?)
        } else {
            (SchemaState::Public, schema_version)
        };
        let mut index_columns = Vec::with_capacity(reader.count()?);
        for _ in 0..index_columns.capacity() {
            index_columns.push(reader.ordinal(columns.len())?);
        }
        indexes.push(IndexDef {
            id,
            name,
            unique,
            columns: index_columns,
            state,
            state_since,
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

/// A constant of a column's own type, present or absent.
///
/// Encoded as a **one-column row** rather than as a format of its own, so that a value in the
/// catalog is written by exactly the code that writes it in a table and is covered by the same
/// goldens. The presence byte is what distinguishes "no default" from "`DEFAULT NULL`" — which
/// PostgreSQL also treats as the same thing, but a format that could not tell them apart would be
/// deciding that rather than recording it.
fn put_value(value: Option<&Datum>, ty: ColumnType, out: &mut Vec<u8>) -> Result<()> {
    let Some(value) = value else {
        out.push(0);
        return Ok(());
    };
    out.push(1);
    let encoded = crate::row::encode_row(&[ty], std::slice::from_ref(value))?;
    varint::put_u64(encoded.len() as u64, out);
    out.extend_from_slice(&encoded);
    Ok(())
}

/// A cursor that fails rather than panics, whatever the bytes are (`CLAUDE.md` invariant 9).
struct Reader<'a> {
    bytes: &'a [u8],
    /// The version byte this record carried, for the one record whose layout grew.
    version: u8,
}

impl<'a> Reader<'a> {
    /// A reader over a catalog record of any version this crate reads.
    ///
    /// **Every** record, not just the table's. The version byte names the *catalog* format, and
    /// version 3 changed only the table record's layout — a retention, a checkpoint and a counter
    /// are byte-identical under 2 and 3. Refusing them because the byte moved would break a
    /// cluster that had run phase 6d, for a layout change that does not touch them.
    fn new(bytes: &'a [u8]) -> Result<Self> {
        Reader::at_least(bytes, OLDEST_TABLE_VERSION)
    }

    /// A reader over a record of `oldest` or newer.
    fn at_least(bytes: &'a [u8], oldest: u8) -> Result<Self> {
        let (&version, rest) = bytes
            .split_first()
            .ok_or_else(|| corrupt("a catalog record is empty"))?;
        if version < oldest || version > CATALOG_FORMAT_VERSION {
            return Err(corrupt(format!(
                "catalog format version {version} is not {oldest}..={CATALOG_FORMAT_VERSION}"
            )));
        }
        Ok(Reader {
            bytes: rest,
            version,
        })
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

    /// The other half of [`put_value`].
    fn value(&mut self, ty: ColumnType) -> Result<Option<Datum>> {
        if !self.flag()? {
            return Ok(None);
        }
        let len = self.count()?;
        let (body, rest) = self
            .bytes
            .split_at_checked(len)
            .ok_or_else(|| corrupt(format!("a value of {len} bytes is truncated")))?;
        self.bytes = rest;
        let mut row = crate::row::decode_row(&crate::row::RowSchema::nullable(vec![ty]), body)?;
        // One column in, one column out; a row that decoded to another width is corruption in the
        // catalog rather than something to work around.
        row.pop()
            .ok_or_else(|| corrupt("a catalog value decoded to no columns"))
            .map(Some)
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
