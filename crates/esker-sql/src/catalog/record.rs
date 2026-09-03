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
//! 'm' ++ "sql" ++ 'j' ++ tenant:u64 ++ id:u64  a schema-change job, and how far its backfill got
//! 'm' ++ "sql" ++ 'f' ++ tenant:u64 ++ id:u64  a flashback in progress, and how far it got
//! 'm' ++ "sql" ++ 'q' ++ tenant:u64 ++ table:u64 ++ column:u64   a sequence, by the column it fills
//! 'm' ++ "sql" ++ 'e' ++ tenant:u64 ++ seq:u64  that sequence's next unhanded-out value
//! ```
//!
//! **A sequence is keyed by the column it fills, not by its own id.** Every sequence this node has
//! is owned by one column — `bigserial` and `GENERATED AS IDENTITY` are the only two spellings
//! that make one, and a standalone `CREATE SEQUENCE` is `0A000` — so the owner is the natural key,
//! and a prefix scan of one table's is what lets a `TableDef` carry its sequences without the
//! *table record* growing a field. That matters more than it sounds: the table record has a format
//! version and readers on both sides of it, and a feature that can be added without touching it is
//! a feature that cannot break one.
//!
//! Its **value** is a separate record for the reason [`row_id_key`] gives: the definition is
//! written once and the counter on every allocation, so putting them together would rewrite a
//! definition to hand out a number.
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

use crate::catalog::{
    CheckDef, ColumnDef, ExcludeDef, ExprShape, ForeignKeyDef, FunctionDef, Identity, IndexDef,
    IndexKey, KeyOrder, KeyPart, PartitionBound, PartitionKey, PartitionStrategy, RangeBound,
    ReferentialAction, Relation, SchemaState, SequenceDef, TableDef, TriggerDef, UniqueKind,
};
use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum, NO_TYPMOD};

/// The version byte on every catalog record.
///
/// Version 2 added a table's schema version (ADR 0019). Version 3 added a column's default and its
/// missing value, and the schema state on every column and index (ADR 0020,
/// `docs/plans/phase-6e.md` §4). Version 4 added a column's **typmod** — the length of a
/// `varchar(n)` or `character(n)` and the precision of a `timestamp(p)` — which
/// [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md) named as the one version bump
/// tier 1 owes. A version 3 column reads back `-1`, which is what a column declared without a
/// number means, and is what every column a version 3 catalog could hold was.
///
/// Version 5 added an expression default (`DEFAULT CURRENT_TIMESTAMP`), version 6 the table's
/// `CHECK` constraints, version 7 a partial index's predicate, version 8 an index's key
/// **expressions**, version 9 each key part's **order** — `DESC` and where its NULLs go — and
/// version 10 the table's `FOREIGN KEY` constraints, and version 11 each index's
/// `NULLS NOT DISTINCT`. Each is appended at the end, so a record of
/// every earlier version is a prefix of a later one's and the goldens below still decode.
///
/// Version 21 added the **comments**: the table's, one per column and one per index, in that
/// order and at the very end like every section before it. A table written before 21 has none,
/// which is what every table had while `COMMENT ON` was `0A000` naming itself (ADR 0049).
///
/// Version 1 is not read: nothing had ever persisted a catalog when version 2 landed, so a
/// compatibility path for it was one nothing could check. **Version 2 is different** — this crate
/// has had a real backend since phase 6a unit 11, so v2 records exist and [`decode_table`] reads
/// them: a v2 column has no default and no missing value, which is what a column that was never
/// given one means.
pub(crate) const CATALOG_FORMAT_VERSION: u8 = 21;

/// The oldest catalog record this crate reads.
///
/// Version 2. Version 1 is refused because no version 1 data has ever existed outside a test on the
/// same commit (ADR 0019 Decision 2); version 2 data **does** exist — this crate has had a real
/// backend since phase 6a unit 11 — so it is read rather than refused.
const OLDEST_TABLE_VERSION: u8 = 2;

/// The catalog version an **extension** record first existed at. Nothing older can hold one, and
/// a reader newer than 12 must still accept the ones version 12 wrote — which is what this floor
/// says and `CATALOG_FORMAT_VERSION` would not.
const OLDEST_EXTENSION_VERSION: u8 = 12;

/// The version a **function** record was introduced at, read as its own floor for the reason the
/// extension's is: a reader newer than 18 must still accept what 18 wrote.
const OLDEST_FUNCTION_VERSION: u8 = 18;

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
const KIND_JOB: u8 = b'j';
const KIND_FLASHBACK: u8 = b'f';
const KIND_SEQUENCE: u8 = b'q';
const KIND_SEQUENCE_VALUE: u8 = b'e';
/// A `FOREIGN KEY`'s **back**-reference: parent id first, so "who references me" is a prefix scan.
const KIND_FK_BACKREF: u8 = b'k';
/// An **installed** extension, keyed by name. The available ones are a property of the build and
/// are not stored; which of them a tenant has installed is state, and a real server's outlives the
/// session that installed it.
const KIND_EXTENSION: u8 = b'x';
/// A stored function, keyed by name.
const KIND_FUNCTION: u8 = b'f';

/// Tags for [`ColumnType`] as stored. Ours rather than PostgreSQL's OIDs, because these are a
/// format we own and must never move; the OIDs stay on the wire where they belong.
const TAG_INT8: u8 = 1;
const TAG_TEXT: u8 = 2;
const TAG_BOOL: u8 = 3;
const TAG_BYTEA: u8 = 4;
const TAG_TIMESTAMPTZ: u8 = 5;
const TAG_DOUBLE: u8 = 6;
/// Appended by [ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md), never
/// renumbered: a record written before it has no tag above 6 and decodes unchanged.
const TAG_INT4: u8 = 7;
/// Appended by ADR 0033 with `varchar`; a record written before it has no tag above 7.
const TAG_VARCHAR: u8 = 8;
/// Appended by ADR 0033 with `timestamp`; a record written before it has no tag above 8.
const TAG_TIMESTAMP: u8 = 9;
/// Appended by ADR 0033 with `smallint`; a record written before it has no tag above 9.
const TAG_INT2: u8 = 10;
/// Appended by ADR 0033 with `real`; a record written before it has no tag above 10.
const TAG_REAL: u8 = 11;
/// `character(n)`, whose internal name is `bpchar`. Version 4's type, and the reason version 4
/// exists: it is the one type that cannot be declared without a typmod.
const TAG_BPCHAR: u8 = 12;
/// `json`, tier 2's first type ([ADR 0042](../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)).
const TAG_JSON: u8 = 13;
/// `jsonb`, its canonicalising twin.
const TAG_JSONB: u8 = 14;
/// Appended by tier 2's first type, never renumbered: a record written before it has no tag above
/// 14 and decodes unchanged.
const TAG_DATE: u8 = 15;
/// Appended by tier 2's hard half, never renumbered.
const TAG_NUMERIC: u8 = 16;
/// `time` without time zone.
const TAG_TIME: u8 = 17;
/// `uuid`.
const TAG_UUID: u8 = 18;
/// `interval`.
const TAG_INTERVAL: u8 = 19;
/// `oid`.
const TAG_OID: u8 = 20;

/// The four array types. Appended, never renumbered, for the reason every tag above is: a record
/// written by an older build has no tag above 20 and reads unchanged.
const TAG_INT8_ARRAY: u8 = 21;
const TAG_INT4_ARRAY: u8 = 22;
const TAG_NUMERIC_ARRAY: u8 = 23;
const TAG_TEXT_ARRAY: u8 = 24;

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
        ColumnType::Int4 => TAG_INT4,
        ColumnType::Varchar => TAG_VARCHAR,
        ColumnType::Timestamp => TAG_TIMESTAMP,
        ColumnType::Int2 => TAG_INT2,
        ColumnType::Real => TAG_REAL,
        ColumnType::Bpchar => TAG_BPCHAR,
        ColumnType::Json => TAG_JSON,
        ColumnType::Jsonb => TAG_JSONB,
        ColumnType::Date => TAG_DATE,
        ColumnType::Numeric => TAG_NUMERIC,
        ColumnType::Time => TAG_TIME,
        ColumnType::Uuid => TAG_UUID,
        ColumnType::Interval => TAG_INTERVAL,
        ColumnType::Oid => TAG_OID,
        ColumnType::Int8Array => TAG_INT8_ARRAY,
        ColumnType::Int4Array => TAG_INT4_ARRAY,
        ColumnType::NumericArray => TAG_NUMERIC_ARRAY,
        ColumnType::TextArray => TAG_TEXT_ARRAY,
    }
}

/// The version 5 byte, read as the expression it used to stand for.
///
/// It held a closed set: `0` no expression default, `1` `CURRENT_TIMESTAMP`, and — from version
/// 13 — `2` and `3` for the two UUID functions. Version 14 stores the expression as **text**
/// instead, because PostgreSQL allows any expression there and a tag per function cannot follow;
/// this is what the tags a written record already holds mean, so those records read back
/// unchanged. Version 14 and later write `0` here and the text in their own section.
///
/// `1` becomes `CURRENT_TIMESTAMP` rather than `now()`: the tag could not tell the two spellings
/// apart and that was the divergence it forced, so the canonical one is what a record written
/// before the text existed can honestly claim.
fn legacy_volatile_default(tag: u8) -> Result<Option<String>> {
    Ok(match tag {
        0 => None,
        1 => Some("CURRENT_TIMESTAMP".to_owned()),
        2 => Some("gen_random_uuid()".to_owned()),
        3 => Some("uuid_generate_v4()".to_owned()),
        other => return Err(corrupt(format!("volatile default tag {other}"))),
    })
}

/// Tags for [`ExprShape`] as stored, ours like every other tag in this record.
const SHAPE_CALL: u8 = 1;
const SHAPE_VALUE: u8 = 2;
const SHAPE_OPERATOR: u8 = 3;

fn shape_tag(shape: ExprShape) -> u8 {
    match shape {
        ExprShape::Call => SHAPE_CALL,
        ExprShape::Value => SHAPE_VALUE,
        ExprShape::Operator => SHAPE_OPERATOR,
    }
}

fn shape_of(tag: u8) -> Result<ExprShape> {
    Ok(match tag {
        SHAPE_CALL => ExprShape::Call,
        SHAPE_VALUE => ExprShape::Value,
        SHAPE_OPERATOR => ExprShape::Operator,
        other => return Err(corrupt(format!("index expression shape tag {other}"))),
    })
}

fn type_of(tag: u8) -> Result<ColumnType> {
    Ok(match tag {
        TAG_INT8 => ColumnType::Int8,
        TAG_TEXT => ColumnType::Text,
        TAG_BOOL => ColumnType::Bool,
        TAG_BYTEA => ColumnType::Bytea,
        TAG_TIMESTAMPTZ => ColumnType::TimestampTz,
        TAG_DOUBLE => ColumnType::Double,
        TAG_INT4 => ColumnType::Int4,
        TAG_VARCHAR => ColumnType::Varchar,
        TAG_TIMESTAMP => ColumnType::Timestamp,
        TAG_INT2 => ColumnType::Int2,
        TAG_REAL => ColumnType::Real,
        TAG_BPCHAR => ColumnType::Bpchar,
        TAG_JSON => ColumnType::Json,
        TAG_JSONB => ColumnType::Jsonb,
        TAG_DATE => ColumnType::Date,
        TAG_NUMERIC => ColumnType::Numeric,
        TAG_TIME => ColumnType::Time,
        TAG_UUID => ColumnType::Uuid,
        TAG_INTERVAL => ColumnType::Interval,
        TAG_OID => ColumnType::Oid,
        TAG_INT8_ARRAY => ColumnType::Int8Array,
        TAG_INT4_ARRAY => ColumnType::Int4Array,
        TAG_NUMERIC_ARRAY => ColumnType::NumericArray,
        TAG_TEXT_ARRAY => ColumnType::TextArray,
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

/// The relation name out of a key [`name_key`] wrote.
///
/// The name is the whole tail of the key, so it needs no length and cannot be confused with a
/// longer one — the same property the checkpoint keys rely on.
pub(super) fn name_of(tenant: u64, key: &[u8]) -> Result<String> {
    let prefix = name_key(tenant, "");
    let tail = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("a catalog name key for another tenant"))?;
    String::from_utf8(tail.to_vec()).map_err(|error| corrupt(format!("a relation name: {error}")))
}

/// Every relation name a tenant has, as a scan range over the same key space [`name_key`] writes.
///
/// One scan is the whole of `pg_class`: a name record exists for every table, index, primary key
/// and sequence, and its value says which. That is the same shape `columnar_range` gives the
/// placement driver — the catalog is already a scannable key space, so a computed relation over it
/// needs no second copy of anything.
#[must_use]
pub(super) fn name_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_NAME]].concat();
    codec::encode_u64(tenant, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    // The successor of the prefix: every key that starts with it sorts below this.
    end.push(0xff);
    (start, end)
}

/// `'m' ++ "sql" ++ 'k' ++ tenant ++ parent_id ++ child_id`. Value: empty.
///
/// The **reverse** of a `FOREIGN KEY`, and the only reason a `DELETE` on a parent row is not a
/// scan of the whole catalog. A table's own record holds the constraints it is the *child* of,
/// because that is the direction an `INSERT` checks; a `DELETE` asks the opposite question — who
/// points at me — and no table can answer it about itself. One key per (parent, child) pair,
/// whatever the number of constraints between them, because the child's record is read anyway to
/// find which of them apply.
pub(super) fn fk_backref_key(tenant: u64, parent: u64, child: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_FK_BACKREF]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(parent, &mut suffix);
    codec::encode_u64(child, &mut suffix);
    prefix::meta_key(&suffix)
}

/// Every child of one parent: the range [`fk_backref_key`] writes into.
pub(super) fn fk_backref_range(tenant: u64, parent: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_FK_BACKREF]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(parent, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    end.push(0xff);
    (start, end)
}

/// The child id out of a key [`fk_backref_key`] wrote.
pub(super) fn fk_backref_child(tenant: u64, parent: u64, key: &[u8]) -> Result<u64> {
    let prefix = fk_backref_range(tenant, parent).0;
    let tail = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("a foreign-key back-reference outside its own range"))?;
    let (child, rest) = codec::decode_u64(tail)
        .map_err(|_| corrupt("a foreign-key back-reference with no child id"))?;
    if !rest.is_empty() {
        return Err(corrupt("a foreign-key back-reference with a tail"));
    }
    Ok(child)
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

/// One installed extension: the tenant, then the name.
///
/// The name rather than an id, because that is what every statement and every view looks it up by
/// — `CREATE EXTENSION "hstore"`, `WHERE extname = 'hstore'` — and an extension has no other
/// identity a client can see.
#[must_use]
pub(super) fn extension_key(tenant: u64, name: &str) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_EXTENSION]].concat();
    codec::encode_u64(tenant, &mut suffix);
    suffix.extend_from_slice(name.as_bytes());
    prefix::meta_key(&suffix)
}

/// Every installed extension of one tenant: the range [`extension_key`] writes into.
#[must_use]
pub(super) fn extension_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let start = extension_key(tenant, "");
    let mut end = start.clone();
    end.push(0xff);
    (start, end)
}

/// The name out of a key [`extension_key`] wrote.
pub(super) fn extension_name_of(tenant: u64, key: &[u8]) -> Result<String> {
    let prefix = extension_key(tenant, "");
    let name = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("an extension key outside its own range"))?;
    String::from_utf8(name.to_vec()).map_err(|_| corrupt("an extension name that is not UTF-8"))
}

/// An installed extension's version, behind the same version byte as every other record.
#[must_use]
pub(super) fn encode_extension(version: &str) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    put_str(version, &mut out);
    out
}

/// Reads one back.
pub(super) fn decode_extension(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::at_least(bytes, OLDEST_EXTENSION_VERSION)?;
    let version = reader.string()?;
    reader.finish()?;
    Ok(version)
}

/// One stored function, keyed by name — a record of its own, as an extension is.
///
/// Not in a table record, because a function belongs to no table: the trigger function statement
/// 790 defines is named by a trigger on one table and could be named by a trigger on another.
#[must_use]
pub(super) fn function_key(tenant: u64, name: &str) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_FUNCTION]].concat();
    codec::encode_u64(tenant, &mut suffix);
    suffix.extend_from_slice(name.as_bytes());
    prefix::meta_key(&suffix)
}

/// Every stored function of one tenant: the range [`function_key`] writes into.
#[must_use]
pub(super) fn function_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let start = function_key(tenant, "");
    let mut end = start.clone();
    end.push(0xff);
    (start, end)
}

/// The name out of a key [`function_key`] wrote.
pub(super) fn function_name_of(tenant: u64, key: &[u8]) -> Result<String> {
    let prefix = function_key(tenant, "");
    let name = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("a function key outside its own range"))?;
    String::from_utf8(name.to_vec()).map_err(|_| corrupt("a function name that is not UTF-8"))
}

/// One function: `version ++ id ++ language ++ body`. The name is in the key.
#[must_use]
pub(super) fn encode_function(function: &FunctionDef) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&function.id.to_le_bytes());
    put_str(&function.language, &mut out);
    // **Verbatim** — semicolons, newlines and all. `pg_proc.prosrc` holds exactly this and
    // `length(prosrc)` counts it, which is what the capture pins.
    put_str(&function.body, &mut out);
    out
}

/// Reads one back. `name` comes from the key the caller read it under.
pub(super) fn decode_function(bytes: &[u8], name: String) -> Result<FunctionDef> {
    let mut reader = Reader::at_least(bytes, OLDEST_FUNCTION_VERSION)?;
    let id = reader.u64_le()?;
    let language = reader.string()?;
    let body = reader.string()?;
    reader.finish()?;
    Ok(FunctionDef {
        id,
        name,
        body,
        language,
    })
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

/// The columnar record's key, and the codec, live in [`esker_keys::columnar`].
///
/// Moved there in [ADR 0030](../../../../docs/adr/0030-the-row-codec-moves-down.md)'s second
/// application: a store holding a columnar learner has to read this record and cannot link this
/// crate. What stays here is the *writing* of it, because the `TableDef` it is written from is
/// this crate's, and the pinning test below that says the constants the two crates share have
/// not drifted.
pub(super) use esker_keys::columnar::{
    decode as decode_columnar, key as columnar_key, range as columnar_range,
    replicas as decode_columnar_replicas, table_id as columnar_table_id,
};

/// Writes a columnar record from a table definition.
///
/// The one direction that stays: the layout is `esker-keys`', and turning a `TableDef` into the
/// `(type, missing)` pairs it holds is this crate's, because `TableDef` is.
pub(super) fn encode_columnar(replicas: u8, table: Option<&TableDef>) -> Result<Vec<u8>> {
    let published = table.map(|table| esker_keys::columnar::Published {
        schema_version: table.schema_version,
        columns: table
            .columns
            .iter()
            .map(|column| (column.ty, column.missing.clone()))
            .collect(),
    });
    Ok(esker_keys::columnar::encode(replicas, published.as_ref())?)
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

/// `'m' ++ "sql" ++ 'j' ++ tenant ++ index_id`. A schema-change job in flight.
///
/// Keyed by the **index**, not the table, because a job is about one index and two jobs on one
/// table are two records. One scan of the kind byte lists everything in flight, which is what
/// `esker_schema_jobs()` reads and what a node picks up after a restart.
#[must_use]
pub(super) fn job_key(tenant: u64, index_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_JOB]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(index_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// Every job of one tenant: `[start, end)` over the `'j'` space.
#[must_use]
pub(super) fn job_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_JOB]].concat();
    codec::encode_u64(tenant, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    let last = end.len() - 1;
    end[last] += 1;
    (start, end)
}

/// A job: which table and index, and **how far the backfill got**.
///
/// The **direction** is stored beside the cursor because the two ends of a schema change are not
/// symmetric in what they cost: an adding step waits the ordinary interval, and a removing one
/// waits the retention window on top of it (ADR 0020, as amended; `docs/plans/phase-6e.md` §1).
/// A driver that could not tell them apart would have to assume the expensive one always.
///
/// The cursor is the other reason this record exists. A backfill is many small transactions rather
/// than one — one transaction over a large table holds locks for its whole duration, conflicts
/// with everything, and outlives the lock TTL the step arithmetic depends on (ADR 0020) — and many
/// small transactions need somewhere durable to say where they got to, or a node that dies
/// restarts instead of resuming.
#[must_use]
pub(super) fn encode_job(table_id: u64, cursor: &[u8], done: bool, removing: bool) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&table_id.to_le_bytes());
    out.push(u8::from(done));
    out.push(u8::from(removing));
    varint::put_u64(cursor.len() as u64, &mut out);
    out.extend_from_slice(cursor);
    out
}

/// Reads a job back: `(table_id, cursor, done, removing)`.
pub(super) fn decode_job(bytes: &[u8]) -> Result<(u64, Vec<u8>, bool, bool)> {
    let mut reader = Reader::new(bytes)?;
    let table_id = reader.u64_le()?;
    let done = reader.flag()?;
    let removing = reader.flag()?;
    let len = reader.count()?;
    let (cursor, rest) = reader
        .bytes
        .split_at_checked(len)
        .ok_or_else(|| corrupt(format!("a job cursor of {len} bytes is truncated")))?;
    let cursor = cursor.to_vec();
    reader.bytes = rest;
    reader.finish()?;
    Ok((table_id, cursor, done, removing))
}

/// The index id out of a job key, for listing.
pub(super) fn job_index_id(tenant: u64, key: &[u8]) -> Result<u64> {
    let (prefix, _) = job_range(tenant);
    let rest = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("a job key outside the job range"))?;
    codec::decode_u64(rest)
        .map(|(id, _)| id)
        .map_err(|error| corrupt(format!("a job key with no index id: {error}")))
}

/// `'m' ++ "sql" ++ 'f' ++ tenant ++ table_id`. A flashback in progress.
///
/// Keyed by the **table**, because a flashback is about one table and two of them on one table
/// would be two people undoing each other. Its own kind rather than a field on the job record: a
/// schema-change job is about an index and a flashback is about rows, and one record holding
/// either would be a record whose meaning depends on which fields are set.
#[must_use]
pub(super) fn flashback_key(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_FLASHBACK]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// A flashback: the instant it is putting the table back to, and how far it has got.
///
/// The **target** is stored, not just the cursor, and that is the point of storing anything: a
/// resume that picked up a cursor without checking what it was a cursor *for* would finish
/// somebody else's flashback with its own target, leaving a table that was never in either state.
#[must_use]
pub(super) fn encode_flashback(target_ts: u64, cursor: &[u8], changed: u64) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&target_ts.to_le_bytes());
    varint::put_u64(changed, &mut out);
    varint::put_u64(cursor.len() as u64, &mut out);
    out.extend_from_slice(cursor);
    out
}

/// Reads a flashback back: `(target_ts, cursor, changed)`.
pub(super) fn decode_flashback(bytes: &[u8]) -> Result<(u64, Vec<u8>, u64)> {
    let mut reader = Reader::new(bytes)?;
    let target_ts = reader.u64_le()?;
    let changed = reader.varint()?;
    let len = reader.count()?;
    let (cursor, rest) = reader
        .bytes
        .split_at_checked(len)
        .ok_or_else(|| corrupt(format!("a flashback cursor of {len} bytes is truncated")))?;
    let cursor = cursor.to_vec();
    reader.bytes = rest;
    reader.finish()?;
    Ok((target_ts, cursor, changed))
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

/// `'m' ++ "sql" ++ 'q' ++ tenant ++ table_id ++ column`. One sequence, by the column it fills.
///
/// A prefix of `'m' ++ "sql" ++ 'q' ++ tenant ++ table_id` is exactly one table's sequences, in
/// column order, which is how a `TableDef` gets them at load without the table record knowing.
#[must_use]
pub(super) fn sequence_key(tenant: u64, table_id: u64, sequence_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_SEQUENCE]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    codec::encode_u64(sequence_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// The half-open range of one table's sequence records.
#[must_use]
pub(super) fn table_sequence_range(tenant: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = [SQL, &[KIND_SEQUENCE]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(table_id, &mut suffix);
    let start = prefix::meta_key(&suffix);
    let mut end = start.clone();
    end.push(0xff);
    (start, end)
}

/// `'m' ++ "sql" ++ 'e' ++ tenant ++ sequence_id`. The next unhanded-out value.
#[must_use]
pub(super) fn sequence_value_key(tenant: u64, sequence_id: u64) -> Vec<u8> {
    let mut suffix = [SQL, &[KIND_SEQUENCE_VALUE]].concat();
    codec::encode_u64(tenant, &mut suffix);
    codec::encode_u64(sequence_id, &mut suffix);
    prefix::meta_key(&suffix)
}

/// One sequence: `version ++ id ++ name ++ identity`, and from version 15 the four fields
/// `CREATE SEQUENCE` brought.
///
/// **The key held the column until version 15 and now holds the id.** A column can own more than
/// one sequence — `CREATE SEQUENCE s OWNED BY t.c` on a table whose `c` is already a `bigserial`
/// is accepted by a real server, measured — so keying by column could not represent the statement
/// this format version exists for. The column moved into the body, where it is now two separate
/// facts that a single ordinal used to conflate:
///
/// * `column` is the one this sequence **fills** — its `nextval` is that column's default;
/// * `owner_column` is the one that **owns** it — dropping that column drops the sequence.
///
/// A `bigserial` sets both to the same column. `CREATE SEQUENCE … OWNED BY t.c` sets only the
/// second, and `pg_attrdef` proves the difference: creating one leaves the column's default alone.
pub(super) fn encode_sequence(sequence: &SequenceDef) -> Vec<u8> {
    let mut out = vec![CATALOG_FORMAT_VERSION];
    out.extend_from_slice(&sequence.id.to_le_bytes());
    put_str(&sequence.name, &mut out);
    out.push(sequence.identity.as_u8());
    // Version 15. An ordinal plus one, so that `0` is `None` and every older record — which had
    // no bytes here at all — reads as the column its key carried.
    varint::put_u64(optional_ordinal(sequence.column), &mut out);
    varint::put_u64(optional_ordinal(sequence.owner_column), &mut out);
    out.extend_from_slice(&sequence.start.to_le_bytes());
    out.extend_from_slice(&sequence.increment.to_le_bytes());
    out
}

/// An optional column ordinal as a varint: `0` for none, and the ordinal plus one otherwise.
fn optional_ordinal(column: Option<usize>) -> u64 {
    column.map_or(0, |at| at as u64 + 1)
}

fn ordinal_of(raw: u64, columns: usize) -> Result<Option<usize>> {
    let Some(at) = raw.checked_sub(1) else {
        return Ok(None);
    };
    let at = usize::try_from(at).map_err(|_| corrupt("a sequence column that is not a usize"))?;
    if columns > 0 && at >= columns {
        return Err(corrupt("a sequence column past the end of its table"));
    }
    Ok(Some(at))
}

/// Reads a sequence. `table_id` comes from the key the caller read it under, and `column` too for
/// a record written before version 15 — which is why the caller passes the key's ordinal in.
pub(super) fn decode_sequence(
    bytes: &[u8],
    table_id: u64,
    key_column: usize,
) -> Result<SequenceDef> {
    let mut reader = Reader::new(bytes)?;
    let id = reader.u64_le()?;
    let name = reader.string()?;
    let identity = Identity::from_u8(reader.byte()?)?;
    // A record written before version 15 was keyed by the column it filled, and that column both
    // filled and owned it — a `bigserial`'s is the only kind that existed.
    let (column, owner_column, start, increment) = if reader.version >= 15 {
        let column = ordinal_of(reader.varint()?, 0)?;
        let owner_column = ordinal_of(reader.varint()?, 0)?;
        (column, owner_column, reader.i64_le()?, reader.i64_le()?)
    } else {
        (Some(key_column), Some(key_column), 1, 1)
    };
    reader.finish()?;
    Ok(SequenceDef {
        id,
        name,
        table_id,
        column,
        owner_column,
        identity,
        start,
        increment,
    })
}

/// The sequence id a sequence key ends with.
pub(super) fn sequence_id_of(tenant: u64, table_id: u64, key: &[u8]) -> Result<u64> {
    let (start, _) = table_sequence_range(tenant, table_id);
    let rest = key
        .strip_prefix(start.as_slice())
        .ok_or_else(|| corrupt("a sequence key outside the range it was read from"))?;
    let (id, _) = codec::decode_u64(rest).map_err(|_| corrupt("a sequence key with no id"))?;
    Ok(id)
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
#[allow(
    clippy::too_many_lines,
    reason = "one block per format version, in version order; that order is the invariant"
)]
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
        // Version 4. Appended rather than placed beside the type tag it belongs to, so that a
        // version 3 column's bytes are a prefix of a version 4 one's and the diff between the two
        // goldens is one field at one end.
        out.extend_from_slice(&column.typmod.to_le_bytes());
        // Version 5. One byte, appended for the same reason the typmod was: a version 4 column's
        // bytes are a prefix of a version 5 one's.
        // Version 5 wrote a **bool** here and version 13 a tag over four values. Version 14 always
        // writes `0` and puts the default expression in its own section as text, because the set
        // a tag can name is closed and the set of expressions PostgreSQL allows in a `DEFAULT` is
        // not. The byte stays because removing it would move every field after it, and a record
        // written by version 5 through 13 still reads through `legacy_volatile_default`.
        out.push(0);
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
        varint::put_u64(index.keys.len() as u64, &mut out);
        for key in &index.keys {
            // An expression part writes ordinal **0**, which is the number PostgreSQL's own
            // `indkey` reserves for one, and the expression itself is written in the version 8
            // section below. Every table has a column 0, so the number a version 7 reader would
            // take this for is at least in range — it is the wrong column rather than a corrupt
            // record, and a version 7 reader is a binary older than the record it is reading,
            // which `Reader::at_least` already refuses at the top.
            varint::put_u64(key.position().unwrap_or(0) as u64, &mut out);
        }
    }

    // Version 6. At the very end, so a version 5 record's bytes are a prefix of a version 6 one's
    // — the same shape every bump in this record has taken.
    varint::put_u64(table.checks.len() as u64, &mut out);
    for check in &table.checks {
        put_str(&check.name, &mut out);
        put_str(&check.expr, &mut out);
    }

    // Version 7. The predicates come **after** the checks rather than beside each index, so that
    // a version 6 record's bytes stay a prefix of a version 7 one's — the property every bump in
    // this record has kept, and the only reason four old goldens still decode.
    for index in &table.indexes {
        put_str(index.predicate.as_deref().unwrap_or(""), &mut out);
    }

    // Version 8. The expressions come after the predicates, for the third time and the same
    // reason: a version 7 record's bytes stay a prefix of a version 8 one's.
    //
    // One string per key part, empty for a column, and the `call` flag **only after a non-empty
    // one** — a column part costs the one byte a zero-length string costs, which is what keeps a
    // table of ordinary indexes the same size it was.
    for index in &table.indexes {
        for key in &index.keys {
            match &key.part {
                KeyPart::Column(_) => put_str("", &mut out),
                KeyPart::Expression { expr, shape, ty } => {
                    put_str(expr, &mut out);
                    out.push(shape_tag(*shape));
                    out.push(tag_of(*ty));
                }
            }
        }
    }

    // Version 9. One byte per key part, for the fourth time in the same place and for the same
    // reason — `pg_index.indoption`'s own two bits, which is what makes an ascending part cost a
    // zero byte and every table written before this read back as one.
    for index in &table.indexes {
        for key in &index.keys {
            out.push(u8::try_from(key.order.indoption()).unwrap_or(0));
        }
    }

    // Version 10. The `FOREIGN KEY` constraints, last, like every bump before it.
    varint::put_u64(table.foreign_keys.len() as u64, &mut out);
    for key in &table.foreign_keys {
        put_str(&key.name, &mut out);
        varint::put_u64(key.columns.len() as u64, &mut out);
        for &ordinal in &key.columns {
            varint::put_u64(ordinal as u64, &mut out);
        }
        out.extend_from_slice(&key.parent.to_le_bytes());
        // The parent's ordinals are **not** checked against this table's column count when they
        // are read back, because they are positions in another table's record which this one
        // cannot see. They are validated where they are made (`crate::exec::ddl`).
        varint::put_u64(key.parent_columns.len() as u64, &mut out);
        for &ordinal in &key.parent_columns {
            varint::put_u64(ordinal as u64, &mut out);
        }
        out.push(action_tag(key.on_update));
        out.push(action_tag(key.on_delete));
        out.push(u8::from(key.deferrable));
    }

    // Version 11. One flag per index, at the **very end** — after the foreign keys, not beside
    // the indexes, for the reason every bump before it went to the end: a version 10 record's
    // bytes have to stay a prefix of a version 11 one's, and a section inserted in the middle
    // would break that for every golden below.
    for index in &table.indexes {
        out.push(u8::from(index.nulls_not_distinct));
    }
    // Version 12. One byte, last, like every bump before it: whether
    // `ALTER TABLE … DISABLE TRIGGER ALL` has suspended this table's referential checks. Every
    // table written before version 12 read back `false`, which is what a table nobody disabled
    // means and what the statement answered until now (`0A000`).
    out.push(u8::from(table.triggers_disabled));

    // Version 13. One string per column, **after** version 12's byte — sections are appended in
    // version order, and a reader takes them in that order too. Writing this one first put the
    // triggers byte where the decoder expected a string length, which read a generation
    // expression onto the wrong column and refused an ordinary `INSERT`.
    //
    // The expression of a `GENERATED ALWAYS AS (…) STORED` column, empty for every other. A
    // version 12 table has none, which is what every table written before it had — the clause was
    // `0A000` until now.
    for column in &table.columns {
        put_str(column.generated.as_deref().unwrap_or(""), &mut out);
    }

    // Version 14. One string per column, **after** version 13's, for the same reason version 13's
    // came after version 12's byte: sections go on in version order and come off in that order.
    //
    // The default that stays an expression, empty for a column with none or with a folded one.
    // Version 5 through 13 named one of three expressions with a byte and this replaces that
    // byte's job, because `DEFAULT` takes any expression and a tag per function cannot follow.
    for column in &table.columns {
        put_str(column.default_expr.as_deref().unwrap_or(""), &mut out);
    }

    // Version 16. The inheritance edges, parents then children, each a count and that many ids —
    // **after** version 14's section, for the fourth time the same reason: sections go on in
    // version order and come off in that order. A table written before 16 has neither, which is
    // what every table had while `INHERITS` was `0A000`.
    varint::put_u64(table.parents.len() as u64, &mut out);
    for parent in &table.parents {
        out.extend_from_slice(&parent.to_le_bytes());
    }
    varint::put_u64(table.children.len() as u64, &mut out);
    for child in &table.children {
        out.extend_from_slice(&child.to_le_bytes());
    }

    // Version 17. One byte per index, in index order and **after** version 16's edges — the fifth
    // section to go on the end for the same reason as the four before it.
    //
    // `0` is an index that is not a constraint, which is what every index written before 17 was
    // read as: a `UNIQUE` constraint's index and a `CREATE UNIQUE INDEX`'s were indistinguishable
    // until this byte, so a record that predates it cannot claim to be one.
    for index in &table.indexes {
        out.push(unique_kind_tag(index.constraint));
    }

    // Version 18. The triggers registered on this table: a count, then each one's fields — after
    // version 17's bytes, the sixth section on the end and in version order like every one before
    // it. A table written before 18 has none, which is what every table had while `CREATE TRIGGER`
    // was `0A000`.
    varint::put_u64(table.triggers.len() as u64, &mut out);
    for trigger in &table.triggers {
        put_str(&trigger.name, &mut out);
        put_str(&trigger.function, &mut out);
        out.push(u8::from(trigger.before));
        out.push(u8::from(trigger.for_each_row));
        out.push(u8::from(trigger.enabled));
        // Non-negative by construction: the mask is built from PostgreSQL's own event bits.
        varint::put_u64(trigger.events.unsigned_abs().into(), &mut out);
    }

    // Version 19. The partition key and this table's own bound, in that order and after version
    // 18's bytes — the seventh section on the end, and the rule has not changed: sections go on in
    // version order and come off in that order.
    //
    // A table written before 19 has neither, which is what every table had while `PARTITION BY`
    // was `0A000`.
    match &table.partition_by {
        None => out.push(0),
        Some(key) => {
            out.push(1);
            out.push(match key.strategy {
                PartitionStrategy::List => b'l',
                PartitionStrategy::Range => b'r',
            });
            varint::put_u64(key.columns.len() as u64, &mut out);
            for &column in &key.columns {
                varint::put_u64(column as u64, &mut out);
            }
        }
    }
    match &table.partition_bound {
        None => out.push(0),
        Some(PartitionBound::Default) => out.push(1),
        Some(PartitionBound::Values(values)) => {
            out.push(2);
            varint::put_u64(values.len() as u64, &mut out);
            for value in values {
                put_bound_value(value, &mut out);
            }
        }
        Some(PartitionBound::Range { from, to }) => {
            out.push(3);
            for side in [from, to] {
                varint::put_u64(side.len() as u64, &mut out);
                for end in side {
                    // One tag byte per end, because `MINVALUE` and `MAXVALUE` are not values: a
                    // range that stored them as the extremes of the key's type would print
                    // numbers where a real server prints the words.
                    match end {
                        RangeBound::MinValue => out.push(0),
                        RangeBound::MaxValue => out.push(2),
                        RangeBound::Value(value) => {
                            out.push(1);
                            put_bound_value(value, &mut out);
                        }
                    }
                }
            }
        }
    }

    // Version 19, still: one list per index, the `INCLUDE (…)` columns by position in the order
    // written — the eighth section on the end, after the partition key and bound.
    //
    // **Two sections under one version number, because they arrived in one release.** A version
    // is what a *reader* branches on, and no reader can ever see a record with the partitioning
    // section and not this one; giving them separate numbers would claim a state that has never
    // existed on disk and would spend a number another lane needs. A table written before 19 has
    // neither, which is what every table had while `PARTITION BY` and `INCLUDE` were both `0A000`.
    for index in &table.indexes {
        varint::put_u64(index.include.len() as u64, &mut out);
        for &at in &index.include {
            varint::put_u64(at as u64, &mut out);
        }
    }

    // Version 20. The `EXCLUDE` constraints, after version 19's two sections — the ninth on the
    // end, and the rule has not changed: sections go on in version order and come off in that
    // order. This one was written as 19 while it was the only section claiming that number; the
    // partition and `INCLUDE` sections landed on main first, so it moved to 20 and moved *after*
    // them here. A table written before 20 — anything main-with-partitioning wrote — has none,
    // which is what every table had while an `EXCLUDE` constraint was a *syntax error* rather
    // than a refusal.
    varint::put_u64(table.excludes.len() as u64, &mut out);
    for exclude in &table.excludes {
        put_str(&exclude.name, &mut out);
        put_str(&exclude.key, &mut out);
        put_str(&exclude.operator, &mut out);
        put_str(&exclude.method, &mut out);
        put_str(exclude.predicate.as_deref().unwrap_or(""), &mut out);
        out.push(u8::from(exclude.deferrable));
        out.push(u8::from(exclude.deferred));
    }

    // Version 21. The comments, on the end for the tenth time and the same reason. **An empty
    // string is "no comment"** and needs no present-or-absent byte in front of it, because
    // PostgreSQL cannot store an empty comment: `COMMENT ON … IS ''` deletes the row exactly as
    // `IS NULL` does. Measured, and it is why this section costs one byte per object rather than
    // two.
    put_str(table.comment.as_deref().unwrap_or(""), &mut out);
    put_str(table.primary_key_comment.as_deref().unwrap_or(""), &mut out);
    for column in &table.columns {
        put_str(column.comment.as_deref().unwrap_or(""), &mut out);
    }
    for index in &table.indexes {
        put_str(index.comment.as_deref().unwrap_or(""), &mut out);
    }

    Ok(out)
}

/// Version 21: the table's comment, the primary key's, then one per column and one per index.
///
/// **An empty string is `None`.** PostgreSQL deletes a `pg_description` row for `IS ''` rather
/// than storing an empty comment, so the two cannot be told apart on a real server either and
/// there is nothing to lose by not distinguishing them here.
fn read_comments(
    reader: &mut Reader<'_>,
    columns: &mut [ColumnDef],
    indexes: &mut [IndexDef],
) -> Result<(Option<String>, Option<String>)> {
    if reader.version < 21 {
        return Ok((None, None));
    }
    let table = some_comment(reader.string()?);
    let primary_key = some_comment(reader.string()?);
    for column in columns {
        column.comment = some_comment(reader.string()?);
    }
    for index in indexes {
        index.comment = some_comment(reader.string()?);
    }
    Ok((table, primary_key))
}

/// An empty comment is no comment — see [`read_comments`].
fn some_comment(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// The second half of the version 19 section: each index's `INCLUDE (…)` columns.
///
/// Read into the indexes that have already been decoded, the way the version 17 byte is: one list
/// per index, in the order the indexes were written. It shares 19 with the partitioning section
/// above because the two arrived in one release and no record can carry one without the other.
fn read_index_include(reader: &mut Reader<'_>, indexes: &mut [IndexDef]) -> Result<()> {
    if reader.version < 19 {
        return Ok(());
    }
    for index in indexes {
        let count = reader.count()?;
        let mut include = Vec::with_capacity(count);
        for _ in 0..count {
            include.push(
                usize::try_from(reader.varint()?)
                    .map_err(|_| corrupt("an included column that is not a usize"))?,
            );
        }
        index.include = include;
    }
    Ok(())
}

/// The version 17 byte: whether an index is a `UNIQUE` constraint's, and whether it is deferrable.
fn unique_kind_tag(kind: Option<UniqueKind>) -> u8 {
    match kind {
        None => 0,
        Some(UniqueKind::Immediate) => 1,
        Some(UniqueKind::Deferrable) => 2,
        // Appended, never renumbered: a record written before deferred constraints existed has no
        // tag above 2 and reads back as the kind it was.
        Some(UniqueKind::Deferred) => 3,
    }
}

fn unique_kind_of(tag: u8) -> Result<Option<UniqueKind>> {
    Ok(match tag {
        0 => None,
        1 => Some(UniqueKind::Immediate),
        2 => Some(UniqueKind::Deferrable),
        3 => Some(UniqueKind::Deferred),
        other => return Err(corrupt(format!("unique constraint tag {other}"))),
    })
}

/// Tags for [`ReferentialAction`] as stored. PostgreSQL's own `confdeltype` characters would do,
/// and are not used: this is a format we own, and its bytes are ours the way the type tags are.
const ACTION_NO_ACTION: u8 = 1;
const ACTION_RESTRICT: u8 = 2;
const ACTION_CASCADE: u8 = 3;

fn action_tag(action: ReferentialAction) -> u8 {
    match action {
        ReferentialAction::NoAction => ACTION_NO_ACTION,
        ReferentialAction::Restrict => ACTION_RESTRICT,
        ReferentialAction::Cascade => ACTION_CASCADE,
    }
}

fn action_of(tag: u8) -> Result<ReferentialAction> {
    Ok(match tag {
        ACTION_NO_ACTION => ReferentialAction::NoAction,
        ACTION_RESTRICT => ReferentialAction::Restrict,
        ACTION_CASCADE => ReferentialAction::Cascade,
        other => return Err(corrupt(format!("referential action tag {other}"))),
    })
}

/// The two per-index sections that were appended after version 6: predicates, then expressions.
///
/// They are at the end of the record and in that order, so a version 6 record's bytes are a prefix
/// of a version 7 one's and a version 7 one's of a version 8 one's — which is what keeps every
/// golden below decoding.
fn read_index_tails(reader: &mut Reader<'_>, indexes: &mut [IndexDef]) -> Result<()> {
    // An empty string is "no predicate": a partial index whose `WHERE` was empty is not a thing
    // the parser can produce, so the two cannot be confused.
    if reader.version >= 7 {
        for index in indexes.iter_mut() {
            let predicate = reader.string()?;
            index.predicate = (!predicate.is_empty()).then_some(predicate);
        }
    }
    // An empty string is "a column part", for the same reason and with the same guarantee: the
    // parser cannot produce an empty index expression. A version 7 index reads back with every
    // part a column, which is what every index a version 7 catalog could hold was — an expression
    // index was `0A000` until version 8.
    if reader.version >= 8 {
        for index in indexes.iter_mut() {
            for key in &mut index.keys {
                let expr = reader.string()?;
                if !expr.is_empty() {
                    key.part = KeyPart::Expression {
                        expr,
                        shape: shape_of(reader.byte()?)?,
                        ty: type_of(reader.byte()?)?,
                    };
                }
            }
        }
    }
    // A version 8 key part is **ascending with its NULLs last**, which is what every key part a
    // version 8 catalog could hold was: a `DESC` index column was `0A000` until version 9.
    if reader.version >= 9 {
        for index in indexes.iter_mut() {
            for key in &mut index.keys {
                key.order = order_of(reader.byte()?)?;
            }
        }
    }
    Ok(())
}

/// The other half of [`KeyOrder::indoption`], which is PostgreSQL's own bitmask.
fn order_of(indoption: u8) -> Result<KeyOrder> {
    if indoption & !0b11 != 0 {
        return Err(corrupt(format!("index key order bits {indoption}")));
    }
    Ok(KeyOrder {
        descending: indoption & 1 != 0,
        nulls_first: indoption & 2 != 0,
    })
}

/// The `FOREIGN KEY` section, appended by version 10.
///
/// A version 9 table has none, which is what every table written before version 10 had:
/// `FOREIGN KEY` was `0A000` until then.
///
/// The **parent's** ordinals are not checked against a column count, because they are positions in
/// another table's record which this one cannot see; they are validated where they are made
/// (`crate::exec::ddl::resolve_foreign_key`).
fn read_foreign_keys(reader: &mut Reader<'_>, columns: usize) -> Result<Vec<ForeignKeyDef>> {
    if reader.version < 10 {
        return Ok(Vec::new());
    }
    let mut keys = Vec::with_capacity(reader.count()?);
    for _ in 0..keys.capacity() {
        let name = reader.string()?;
        let mut referencing = Vec::with_capacity(reader.count()?);
        for _ in 0..referencing.capacity() {
            referencing.push(reader.ordinal(columns)?);
        }
        let parent = reader.u64_le()?;
        let mut parent_columns = Vec::with_capacity(reader.count()?);
        for _ in 0..parent_columns.capacity() {
            parent_columns
                .push(usize::try_from(reader.varint()?).map_err(|_| {
                    corrupt("a referenced column ordinal larger than this machine")
                })?);
        }
        keys.push(ForeignKeyDef {
            name,
            columns: referencing,
            parent,
            parent_columns,
            on_update: action_of(reader.byte()?)?,
            on_delete: action_of(reader.byte()?)?,
            deferrable: reader.flag()?,
        });
    }
    Ok(keys)
}

/// The version 13 tail: one generation expression per column.
///
/// A version 12 column has none, which is what every column written before version 13 had — the
/// clause was `0A000` until then. An **empty string is "not generated"**, and it is unambiguous:
/// a generated column's expression is never empty, because the grammar has no empty parentheses.
fn read_generation_expressions(reader: &mut Reader<'_>, columns: &mut [ColumnDef]) -> Result<()> {
    if reader.version < 13 {
        return Ok(());
    }
    for column in columns {
        let expr = reader.string()?;
        column.generated = (!expr.is_empty()).then_some(expr);
    }
    Ok(())
}

/// The version 14 section: each column's default **expression**, or the empty string for none.
///
/// Read after the version 13 one and written after it, because the reader takes the sections in
/// version order and a section read out of order takes the next field's bytes for its own.
fn read_default_expressions(reader: &mut Reader<'_>, columns: &mut [ColumnDef]) -> Result<()> {
    if reader.version < 14 {
        return Ok(());
    }
    for column in columns {
        let expr = reader.string()?;
        column.default_expr = (!expr.is_empty()).then_some(expr);
    }
    Ok(())
}

/// The version 16 section: the tables this one inherits from, and the tables that inherit from it.
fn read_inheritance(reader: &mut Reader<'_>) -> Result<(Vec<u64>, Vec<u64>)> {
    if reader.version < 16 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut edges = [Vec::new(), Vec::new()];
    for side in &mut edges {
        let count = reader.count()?;
        side.reserve(count);
        for _ in 0..count {
            side.push(reader.u64_le()?);
        }
    }
    let [parents, children] = edges;
    Ok((parents, children))
}

/// The version 18 section: the triggers registered on this table.
fn read_triggers(reader: &mut Reader<'_>) -> Result<Vec<TriggerDef>> {
    if reader.version < 18 {
        return Ok(Vec::new());
    }
    let count = reader.count()?;
    let mut triggers = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string()?;
        let function = reader.string()?;
        let before = reader.flag()?;
        let for_each_row = reader.flag()?;
        let enabled = reader.flag()?;
        let events = i16::try_from(reader.varint()?)
            .map_err(|_| corrupt("a trigger event mask that is not an i16"))?;
        triggers.push(TriggerDef {
            name,
            before,
            events,
            for_each_row,
            function,
            enabled,
        });
    }
    Ok(triggers)
}

/// One bound value: its **type tag** and then its text.
///
/// The type is written because the bound is compared, not only printed. A value stored as text and
/// read back as text compares equal to another text and to nothing else — so an `int4` range bound
/// decoded without its type would say `10` and `10` are different values, and the second partition
/// of a two-partition range would be refused as overlapping the first. A bound's type is the key
/// column's, which belongs to the **parent**, and the parent is not loaded here: writing the tag is
/// what makes the record self-describing rather than needing one.
fn put_bound_value(value: &Datum, out: &mut Vec<u8>) {
    // A NULL bound value cannot be written by any statement — `FOR VALUES IN (NULL)` is refused
    // where it is lowered — so `text` is a tag that will never be read back, not a coercion.
    out.push(tag_of(value.column_type().unwrap_or(ColumnType::Text)));
    put_str(
        &crate::value::PgDatum::to_text(value).unwrap_or_default(),
        out,
    );
}

/// One bound value, read back as the type it was written with.
fn bound_value(reader: &mut Reader<'_>) -> Result<Datum> {
    let ty = type_of(reader.byte()?)?;
    let text = reader.string()?;
    <Datum as crate::value::PgDatum>::from_text(ty, &text)
        .map_err(|_| corrupt("a partition bound value of the wrong type"))
}

/// The version 19 section: the partition key, then this table's own bound.
///
/// A bound's values come back as **text**, to be read as the key columns' types where the parent
/// is in hand — which is not here.
fn read_partitioning(
    reader: &mut Reader<'_>,
) -> Result<(Option<PartitionKey>, Option<PartitionBound>)> {
    if reader.version < 19 {
        return Ok((None, None));
    }
    let partition_by = match reader.byte()? {
        0 => None,
        1 => {
            let strategy = match reader.byte()? {
                b'l' => PartitionStrategy::List,
                b'r' => PartitionStrategy::Range,
                other => return Err(corrupt(format!("partition strategy byte {other}"))),
            };
            let count = reader.count()?;
            let mut columns = Vec::with_capacity(count);
            for _ in 0..count {
                columns.push(
                    usize::try_from(reader.varint()?)
                        .map_err(|_| corrupt("a partition key column that is not a usize"))?,
                );
            }
            Some(PartitionKey { strategy, columns })
        }
        other => return Err(corrupt(format!("partition key tag {other}"))),
    };
    let partition_bound = match reader.byte()? {
        0 => None,
        1 => Some(PartitionBound::Default),
        2 => {
            let count = reader.count()?;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(bound_value(reader)?);
            }
            Some(PartitionBound::Values(values))
        }
        3 => {
            let mut sides = [Vec::new(), Vec::new()];
            for side in &mut sides {
                let count = reader.count()?;
                for _ in 0..count {
                    side.push(match reader.byte()? {
                        0 => RangeBound::MinValue,
                        1 => RangeBound::Value(bound_value(reader)?),
                        2 => RangeBound::MaxValue,
                        other => return Err(corrupt(format!("range bound tag {other}"))),
                    });
                }
            }
            let [from, to] = sides;
            Some(PartitionBound::Range { from, to })
        }
        other => return Err(corrupt(format!("partition bound tag {other}"))),
    };
    Ok((partition_by, partition_bound))
}

/// The version 20 section: the `EXCLUDE` constraints on this table.
fn read_excludes(reader: &mut Reader<'_>) -> Result<Vec<ExcludeDef>> {
    if reader.version < 20 {
        return Ok(Vec::new());
    }
    let count = reader.count()?;
    let mut excludes = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string()?;
        let key = reader.string()?;
        let operator = reader.string()?;
        let method = reader.string()?;
        let predicate = reader.string()?;
        excludes.push(ExcludeDef {
            name,
            key,
            operator,
            method,
            predicate: (!predicate.is_empty()).then_some(predicate),
            deferrable: reader.flag()?,
            deferred: reader.flag()?,
        });
    }
    Ok(excludes)
}

/// Reads a table record back. Every failure is typed: a catalog is on-disk data like any other.
#[allow(
    clippy::too_many_lines,
    reason = "one block per format version, in version order; that order is the invariant"
)]
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
        // A version 3 column has no typmod, which is what a column declared without a number
        // means — and every column a version 3 catalog could hold was declared without one,
        // because no type this crate had before version 4 took a number.
        let typmod = if reader.version >= 4 {
            reader.i32_le()?
        } else {
            NO_TYPMOD
        };
        // A version 4 column has no expression default, which is what every column written before
        // version 5 was: the only default a v4 catalog could hold was a constant. A version 5 to
        // 13 column has one of three, named by a tag; a version 14 column writes `0` here and
        // carries the text in the section below, which overwrites this.
        let default_expr = if reader.version >= 5 {
            legacy_volatile_default(reader.byte()?)?
        } else {
            None
        };
        columns.push(ColumnDef {
            name,
            ty,
            typmod,
            not_null,
            default_expr,
            default,
            missing,
            // Filled from the version 13 section below, after every column has been read.
            generated: None,
            comment: None,
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
        let mut keys = Vec::with_capacity(reader.count()?);
        for _ in 0..keys.capacity() {
            keys.push(IndexKey::column(reader.ordinal(columns.len())?));
        }
        indexes.push(IndexDef {
            id,
            name,
            unique,
            keys,
            state,
            state_since,
            // All three filled after the loop, for versions 7, 8 and 11.
            include: Vec::new(),
            predicate: None,
            nulls_not_distinct: false,
            // Filled from the version 17 section below, after every index has been read.
            constraint: None,
            comment: None,
        });
    }

    // A version 5 table has none, which is what every table written before version 6 had:
    // `CHECK` was `0A000` until then. Read **before** `finish`, which consumes the reader and
    // asserts the record is exhausted.
    let checks = if reader.version >= 6 {
        let mut checks = Vec::with_capacity(reader.count()?);
        for _ in 0..checks.capacity() {
            checks.push(CheckDef {
                name: reader.string()?,
                expr: reader.string()?,
            });
        }
        checks
    } else {
        Vec::new()
    };
    read_index_tails(&mut reader, &mut indexes)?;

    let foreign_keys = read_foreign_keys(&mut reader, columns.len())?;
    // A version 10 index has no flag, which is what every index a version 10 catalog could hold
    // had: `NULLS NOT DISTINCT` was `0A000` until version 11. Read **before** `finish`, which
    // consumes the reader and asserts the record is exhausted.
    if reader.version >= 11 {
        for index in &mut indexes {
            index.nulls_not_distinct = reader.flag()?;
        }
    }
    // A version 11 table has no flag, and `false` is what it meant: `DISABLE TRIGGER` was `0A000`
    // until version 12, so no table written before it could have been disabled.
    let triggers_disabled = reader.version >= 12 && reader.flag()?;
    read_generation_expressions(&mut reader, &mut columns)?;
    read_default_expressions(&mut reader, &mut columns)?;
    let (parents, children) = read_inheritance(&mut reader)?;
    // A table written before version 17 has no byte here, and `None` is what it meant: nothing
    // could tell a constraint's index from a bare one, so none of them may claim to be a
    // constraint.
    if reader.version >= 17 {
        for index in &mut indexes {
            index.constraint = unique_kind_of(reader.byte()?)?;
        }
    }
    let triggers = read_triggers(&mut reader)?;
    let (partition_by, partition_bound) = read_partitioning(&mut reader)?;
    read_index_include(&mut reader, &mut indexes)?;
    // Read **after** version 19's two sections, because it is written after them.
    let excludes = read_excludes(&mut reader)?;
    let (comment, primary_key_comment) = read_comments(&mut reader, &mut columns, &mut indexes)?;
    reader.finish()?;

    Ok(TableDef {
        id,
        name,
        comment,
        primary_key_comment,
        columns,
        primary_key,
        indexes,
        primary_key_name,
        schema_version,
        // Not in the record: a table's sequences are keyed by the columns they fill and are read
        // where the table is loaded (`crate::catalog::View::table_by_id`). A `TableDef` decoded
        // straight from bytes therefore has none, which is what this function is for.
        sequences: Vec::new(),
        parents,
        children,
        triggers,
        excludes,
        child_scans: Vec::new(),
        partition_by,
        partition_bound,
        checks,
        foreign_keys,
        triggers_disabled,
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
        // A sequence's name resolves to the column it fills rather than to an id of its own,
        // because that is the key its record lives under.
        Relation::Sequence {
            table_id,
            sequence_id,
        } => {
            out.push(KIND_SEQUENCE);
            out.extend_from_slice(&table_id.to_le_bytes());
            out.extend_from_slice(&sequence_id.to_le_bytes());
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
        // **The second number was a column ordinal before version 15 and is a sequence id now.**
        // A name entry is rewritten whenever the sequence is, and the two are read together, so
        // there is no record that pairs an old entry with a new sequence — a `bigserial`'s
        // sequence id and its column ordinal are both small numbers and would not have been
        // told apart by a check.
        KIND_SEQUENCE => Relation::Sequence {
            table_id: reader.u64_le()?,
            sequence_id: reader.u64_le()?,
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

    /// A little-endian `i64`, which is what a sequence's start and increment are.
    fn i64_le(&mut self) -> Result<i64> {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "the two's-complement round trip is exact and is how the value was written"
        )]
        Ok(self.u64_le()? as i64)
    }

    fn u64_le(&mut self) -> Result<u64> {
        let (head, rest) = self
            .bytes
            .split_first_chunk::<8>()
            .ok_or_else(|| corrupt("a catalog record ends inside an id"))?;
        self.bytes = rest;
        Ok(u64::from_le_bytes(*head))
    }

    /// A typmod. Signed and fixed-width because `-1` is a real value here, not a sentinel a
    /// varint would be asked to spend ten bytes on.
    fn i32_le(&mut self) -> Result<i32> {
        let (head, rest) = self
            .bytes
            .split_first_chunk::<4>()
            .ok_or_else(|| corrupt("a catalog record ends inside a typmod"))?;
        self.bytes = rest;
        Ok(i32::from_le_bytes(*head))
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
