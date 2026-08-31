//! What tables exist, and a cache that cannot serve a stale one.
//!
//! Definitions live in the `'m'` space (`record`) and are read through a transaction like any
//! other data, which is what makes DDL and DML consistent with each other: a `CREATE TABLE` that
//! has not committed is invisible, and one that has is visible to every transaction that starts
//! after it and to none that started before.
//!
//! # The cache, and the one thing it must never do
//!
//! Every node caches definitions, because resolving a name on every statement would put a network
//! round trip in front of every query. A cache that can serve a *stale* definition is not a
//! performance problem, it is a correctness one: writing a row against an old column list produces
//! bytes that decode wrong, and writing it against an old index list produces a row that is
//! missing from an index that exists.
//!
//! So there is a monotone `catalog_version` in the store, every DDL statement bumps it, and each
//! transaction reads it **once** — [`Catalog::view`]. Every lookup a transaction makes is then
//! answered at that one version, so a statement cannot see two different shapes of the same table
//! and cannot see a table that appeared halfway through it. The cache is keyed by that version and
//! is discarded, not repaired, when the version moves.
//!
//! A transaction whose snapshot is *older* than what the cache holds reads through to the store
//! instead: its snapshot is real, the cache is just newer than it, and serving from the cache
//! would show it a definition from its own future.
//!
//! # And the second thing it must never do: cache a definition nobody has committed
//!
//! A transaction that has run DDL reads its *own* catalog writes back, version bump included, so
//! its view sits at a version no committed transaction has reached yet. Caching what it reads
//! would publish an uncommitted definition to every session on the node — and version numbers are
//! reused after a rollback, because the bump is `read + 1` inside the transaction, so the next DDL
//! that really does commit lands on the same number and finds the abandoned entry waiting for it.
//! The failure is a `SELECT` against a table that was never created, or worse, a row written
//! against a column list that was rolled back.
//!
//! So a transaction that has written the catalog gets [`Catalog::view_uncached`]: it reads through
//! for every lookup, which is also what makes its own uncommitted DDL visible to itself, and it
//! puts nothing back. DDL is rare and this is the statement that just paid for a round trip
//! anyway.

mod record;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum};

/// The version byte on every catalog record. An unknown one is an error, never a guess
/// (`CLAUDE.md` invariant 2).
pub const CATALOG_FORMAT_VERSION: u8 = record::CATALOG_FORMAT_VERSION;

/// How long old MVCC versions are kept when nothing says otherwise: **one hour**.
///
/// This number is two things at once and they pull in opposite directions
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md)). It is the depth of the *time machine* — a
/// read `AS OF` an instant older than this has nothing left to read — and it is the depth of every
/// version chain the storage engine has to walk past to answer an ordinary read. An hour is chosen
/// to be long enough that "what did this look like before the last run" is answerable out of the
/// box and short enough that a hot-rewritten key does not accumulate a chain nobody wanted.
pub const DEFAULT_RETENTION_MS: u64 = 60 * 60 * 1000;

/// A retention that never collects. Reserved rather than derived, because every other value is
/// subtracted from a timestamp and this one cannot be.
pub const RETENTION_FOREVER: u64 = u64::MAX;

/// The longest identifier PostgreSQL keeps. Longer ones are truncated with a `42622` notice, not
/// refused — measured against the server, which truncated a 70-character name to 63.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// One column of one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// As the user wrote it, already folded (see [`fold_identifier`]).
    pub name: String,
    /// One of the six types phase 6a stores.
    pub ty: ColumnType,
    /// Whether a NULL is refused. Primary key columns are always `NOT NULL`.
    pub not_null: bool,
    /// What an `INSERT` that omits this column writes. `None` is NULL.
    ///
    /// A **constant**, not an expression: `DEFAULT 7` and `DEFAULT 'x'` are stored, `DEFAULT
    /// random()` is refused by name (`docs/plans/phase-6e.md` §5 unit 1). PostgreSQL folds a
    /// constant expression like `(1+1)` to `2` before storing it; there is no folder here, so a
    /// parenthesised expression is refused rather than half-read.
    pub default: Option<Datum>,
    /// What a **row narrower than this column** reads as. `None` is NULL.
    ///
    /// PostgreSQL 11's `attmissingval`, and the reason `ALTER TABLE ... ADD COLUMN ... DEFAULT` is
    /// instant on a populated table: rather than rewriting every row to carry the value, the
    /// *catalog* remembers it and the decoder pads with it. ADR 0019's pad rule generalised — that
    /// rule pads with NULL, which is this field's `None`.
    ///
    /// **Separate from [`ColumnDef::default`], and the two really do diverge.** Measured on
    /// PostgreSQL 19beta1: after `ADD COLUMN c text DEFAULT 'old'`, a later
    /// `ALTER COLUMN c SET DEFAULT 'new'` leaves `attmissingval` at `old` — rows that predate the
    /// column still read `old` and new rows get `new`. Storing one field for both would rewrite
    /// history the first time somebody changed a default.
    pub missing: Option<Datum>,
}

/// Where an index or a column is in a staged schema change (ADR 0020).
///
/// Four states, moved one at a time, and each exists because the pair on either side of it is safe
/// together and the pair you would get by skipping it is not. The variants carry that argument;
/// `tests/schema_change.rs` carries the repro for each, written so that it **fails** if the state
/// it guards is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaState {
    /// Nobody reads it, nobody writes it, nobody removes from it. What an index is before a
    /// `CREATE INDEX` starts and after a `DROP INDEX` finishes.
    Absent,
    /// A **delete** removes an entry if one is there; nothing reads and nothing inserts.
    ///
    /// Exists so that *every* node removes entries before *any* node creates them. Skipping it
    /// leaves an entry pointing at a row a node that did not know about the index deleted — and
    /// the moment the index goes public, a scan through it returns a row the table does not
    /// contain (ADR 0020, "skip delete-only").
    DeleteOnly,
    /// Inserts, updates and deletes all maintain it; nothing reads it.
    ///
    /// Exists so that *every* node maintains the index before *any* node trusts it. Skipping it
    /// lets a node behind insert a row and write no entry, while a node ahead answers a query
    /// from the index and does not find it (ADR 0020, "skip write-only").
    WriteOnly,
    /// Read, written and removed from: an ordinary index.
    ///
    /// Reached only after the **backfill**, which is what makes it complete. Skipping that leaves
    /// every row written before write-only invisible to an index scan — the same wrong answer with
    /// a wider blast radius (ADR 0020, "skip the backfill").
    Public,
}

impl SchemaState {
    /// Whether a query may answer *from* this index.
    #[must_use]
    pub fn readable(self) -> bool {
        self == SchemaState::Public
    }

    /// Whether an insert or an update writes an entry into it.
    #[must_use]
    pub fn written(self) -> bool {
        matches!(self, SchemaState::WriteOnly | SchemaState::Public)
    }

    /// Whether a delete removes an entry from it.
    ///
    /// True one state *earlier* than [`SchemaState::written`], and that asymmetry is the whole
    /// design: removal has to lead creation, or an entry outlives the row it names.
    #[must_use]
    pub fn maintained(self) -> bool {
        matches!(
            self,
            SchemaState::DeleteOnly | SchemaState::WriteOnly | SchemaState::Public
        )
    }

    /// The next state towards `Absent`, or `None` at it — the direction a **removal** runs, and
    /// the direction a failed change unwinds in.
    #[must_use]
    pub fn backward(self) -> Option<SchemaState> {
        match self {
            SchemaState::Public => Some(SchemaState::WriteOnly),
            SchemaState::WriteOnly => Some(SchemaState::DeleteOnly),
            SchemaState::DeleteOnly => Some(SchemaState::Absent),
            SchemaState::Absent => None,
        }
    }

    /// The next state towards `Public`, or `None` at it.
    #[must_use]
    pub fn forward(self) -> Option<SchemaState> {
        match self {
            SchemaState::Absent => Some(SchemaState::DeleteOnly),
            SchemaState::DeleteOnly => Some(SchemaState::WriteOnly),
            SchemaState::WriteOnly => Some(SchemaState::Public),
            SchemaState::Public => None,
        }
    }

    /// How it is stored, and how it reads in `esker_schema_jobs()`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SchemaState::Absent => "absent",
            SchemaState::DeleteOnly => "delete-only",
            SchemaState::WriteOnly => "write-only",
            SchemaState::Public => "public",
        }
    }
}

/// One index on one table. Its columns are positions into the table's column list, so renaming a
/// column cannot orphan an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// From the tenant's relation-id sequence, like a table's.
    pub id: u64,
    /// Unique across the tenant, in the same namespace as table names.
    pub name: String,
    /// Whether a duplicate is refused with `23505`.
    pub unique: bool,
    /// Positions into [`TableDef::columns`].
    pub columns: Vec<usize>,
    /// Where this index is in a staged schema change (ADR 0020).
    ///
    /// [`SchemaState::Public`] for an index that was built the old way — one statement, one
    /// transaction — and for every index a version 2 catalog holds, which is what a table that has
    /// never staged a change means.
    pub state: SchemaState,
    /// The [`TableDef::schema_version`] this index entered [`IndexDef::state`] at.
    ///
    /// The number ADR 0020's two-version invariant is stated over: the step clock may advance only
    /// when no node can still be acting on a state two behind this one, and this is what "behind"
    /// is measured in. It is written on every transition and read by the job, never by a read or a
    /// write of a row.
    pub state_since: u64,
}

/// The name the internal row id column carries: **no name at all**.
///
/// It cannot collide with anything a user writes, and that is a fact about PostgreSQL rather than
/// a convention of ours: a zero-length delimited identifier is `42601 zero-length delimited
/// identifier`, measured against the server, so `""` is a name no statement can ever mention. That
/// makes the column unnameable by construction instead of by a reserved word somebody could
/// legitimately want — no `_rowid` that a user is then forbidden to call their own column.
pub const INTERNAL_ROW_ID_NAME: &str = "";

/// How many row ids a session reserves at a time (see [`allocate_row_ids`]).
pub const ROW_ID_BATCH: u64 = 256;

/// A table, its columns, its primary key and its indexes — everything needed to write a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    /// From the tenant's relation-id sequence. Part of every key of every row.
    pub id: u64,
    /// Unique across the tenant.
    pub name: String,
    /// In declaration order, which is the order a row's values are encoded in.
    pub columns: Vec<ColumnDef>,
    /// Positions into [`TableDef::columns`], in key order.
    pub primary_key: Vec<usize>,
    /// Every index, including the unique ones a `UNIQUE` constraint creates.
    pub indexes: Vec<IndexDef>,
    /// The primary key constraint's name, which is a relation name like any other even though it
    /// has no index behind it. It is what a `23505` on the key quotes back.
    ///
    /// **Empty means the user declared no primary key**, in which case [`TableDef::row_id`] is
    /// `Some` and the key is an internal row id. Empty is unambiguous rather than convenient: a
    /// zero-length identifier is a syntax error in PostgreSQL (see [`INTERNAL_ROW_ID_NAME`]), so
    /// no constraint a user names can be spelled this way.
    pub primary_key_name: String,
    /// How many times this table's *shape* has changed. `1` for a table as `CREATE TABLE` left
    /// it; each `ALTER TABLE ADD COLUMN` adds one.
    ///
    /// Nothing needs it to read a row — a row carries its own column count (`crate::row`) — so it
    /// is not load-bearing yet. It is here because a schema change is the table's own event and
    /// the cluster-wide `catalog_version` cannot say which table moved, which is what the staged
    /// online-DDL design in `docs/adr/0020-online-schema-change.md` needs to attach per-column
    /// states to.
    pub schema_version: u64,
}

impl TableDef {
    /// The position of a column by name, or `None`.
    ///
    /// A user's name never finds the internal row id, because that column's name is one no
    /// statement can contain ([`INTERNAL_ROW_ID_NAME`]).
    #[must_use]
    pub fn column(&self, name: &str) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        self.columns.iter().position(|column| column.name == name)
    }

    /// The position of the internal row id, or `None` for a table whose user declared a primary
    /// key.
    ///
    /// It is always column 0 when it is there, so a later `ALTER TABLE ADD COLUMN` appends after
    /// the user's columns and cannot move it.
    #[must_use]
    pub fn row_id(&self) -> Option<usize> {
        self.primary_key_name.is_empty().then_some(0)
    }

    /// The columns a user can see: every column except the internal row id.
    ///
    /// This is what `SELECT *` expands to and what an `INSERT` with no column list fills, which is
    /// the whole of what makes the row id *hidden* rather than merely unnamed.
    pub fn user_columns(&self) -> impl Iterator<Item = (usize, &ColumnDef)> {
        let skip = usize::from(self.row_id().is_some());
        self.columns.iter().enumerate().skip(skip)
    }

    /// Every column's type, in encoding order — what [`crate::row`] needs.
    #[must_use]
    pub fn column_types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(|column| column.ty).collect()
    }

    /// How this table's rows decode: [`TableDef::column_types`] and [`TableDef::column_missing`]
    /// as the one value [`crate::row::decode_row`] takes.
    #[must_use]
    pub fn row_schema(&self) -> crate::row::RowSchema {
        crate::row::RowSchema::new(self.column_types(), self.column_missing())
    }

    /// What each column reads as in a row too narrow to hold it, in the same order.
    ///
    /// The second half of what [`crate::row::decode_row`] needs for a **table row**, and the reason
    /// `ALTER TABLE ADD COLUMN ... DEFAULT <constant>` rewrites nothing: the value lives here
    /// rather than in every row (`ColumnDef::missing`).
    #[must_use]
    pub fn column_missing(&self) -> Vec<Option<Datum>> {
        self.columns
            .iter()
            .map(|column| column.missing.clone())
            .collect()
    }

    /// The types of the primary key columns, in key order.
    #[must_use]
    pub fn primary_key_types(&self) -> Vec<ColumnType> {
        self.primary_key
            .iter()
            .map(|&ordinal| self.columns[ordinal].ty)
            .collect()
    }
}

/// What a name resolves to. Tables and indexes share one namespace, as they do in PostgreSQL's
/// `pg_class`: creating an index over a table's name answers `42P07`, confirmed against a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// A table.
    Table {
        /// Its id.
        table_id: u64,
    },
    /// An index, and the table it is on.
    Index {
        /// The table the index is defined on.
        table_id: u64,
        /// The index's own id.
        index_id: u64,
    },
    /// A primary key constraint's name — `<table>_pkey`.
    ///
    /// It points at no index because there is none to point at: the row key *is* the primary key
    /// (`crate::row`), so the constraint is enforced by the key space itself. The name is still
    /// taken, though, and has to be: PostgreSQL answers `42P07` to `CREATE INDEX t_pkey ON t (a)`,
    /// and it is the name a `23505` on the primary key quotes back.
    PrimaryKey {
        /// The table whose primary key it is.
        table_id: u64,
    },
}

/// An identifier as PostgreSQL stores it: folded when it was written unquoted, and truncated to
/// [`MAX_IDENTIFIER_BYTES`].
///
/// **Only ASCII `A`–`Z` fold.** A UTF-8 server leaves `Ébc` alone — measured, and the reason this
/// is not `str::to_lowercase`, which would have quietly renamed every non-ASCII identifier.
/// The second return value is whether truncation happened, which PostgreSQL reports as a `42622`
/// notice rather than an error.
#[must_use]
pub fn fold_identifier(name: &str, quoted: bool) -> (String, bool) {
    let folded = if quoted {
        name.to_owned()
    } else {
        name.chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c
                }
            })
            .collect()
    };
    if folded.len() <= MAX_IDENTIFIER_BYTES {
        return (folded, false);
    }
    // Truncate on a character boundary: cutting a multi-byte character in half would leave a name
    // that is not UTF-8, which the catalog would then refuse to read back.
    let mut end = MAX_IDENTIFIER_BYTES;
    while !folded.is_char_boundary(end) {
        end -= 1;
    }
    (folded[..end].to_owned(), true)
}

/// The per-node cache of definitions. One per SQL node, shared by every session.
#[derive(Debug, Default)]
pub struct Catalog {
    cached: Mutex<Cache>,
}

#[derive(Debug, Default)]
struct Cache {
    /// The catalog version everything below was read at.
    version: u64,
    /// `(tenant, name)` to what it resolves to, `None` for a name that is known not to exist.
    names: BTreeMap<(u64, String), Option<Relation>>,
    /// `(tenant, table_id)` to the definition.
    tables: BTreeMap<(u64, u64), Arc<TableDef>>,
}

impl Catalog {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Catalog::default()
    }

    /// Reads the catalog version once and returns the view every lookup in this transaction goes
    /// through.
    ///
    /// This is the whole of the consistency mechanism: one read, at the transaction's snapshot,
    /// and every definition it then sees belongs to that version.
    ///
    /// Only for a transaction that has **not** written the catalog itself — see
    /// [`Catalog::view_uncached`], which is what a transaction that has run DDL must use.
    pub fn view<'a>(&'a self, txn: &'a dyn Txn, tenant: u64) -> Result<View<'a>> {
        self.view_at(txn, tenant, true)
    }

    /// The same view, reading through the cache and never filling it.
    ///
    /// This is what a transaction that has run DDL gets. Its reads answer with its own
    /// uncommitted definitions, which are exactly the ones that must not be published to the
    /// other sessions sharing this node (see the module docs).
    pub fn view_uncached<'a>(&'a self, txn: &'a dyn Txn, tenant: u64) -> Result<View<'a>> {
        self.view_at(txn, tenant, false)
    }

    fn view_at<'a>(&'a self, txn: &'a dyn Txn, tenant: u64, cached: bool) -> Result<View<'a>> {
        let version = match txn.get(&record::version_key())? {
            Some(bytes) => record::decode_counter(&bytes)?,
            // No DDL has ever run. Version 0 is the empty catalog.
            None => 0,
        };
        Ok(View {
            catalog: cached.then_some(self),
            txn,
            tenant,
            version,
        })
    }

    /// A poisoned lock means a thread panicked holding it; the maps behind it are still sound and
    /// taking them back beats propagating a panic into a session (`CLAUDE.md` invariant 9).
    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops everything older than `version`, and reports whether the cache may now be used for
    /// it. A view older than the cache is told no: its snapshot is real and the cache is ahead of
    /// it.
    fn usable_at(&self, version: u64) -> bool {
        let mut cache = self.lock();
        if version > cache.version {
            *cache = Cache {
                version,
                ..Cache::default()
            };
        }
        cache.version == version
    }
}

/// One transaction's consistent view of the catalog.
#[derive(Debug)]
pub struct View<'a> {
    /// The cache to answer from and fill, or `None` for a transaction that has written the
    /// catalog and whose reads are therefore its own uncommitted DDL.
    catalog: Option<&'a Catalog>,
    txn: &'a dyn Txn,
    tenant: u64,
    version: u64,
}

impl View<'_> {
    /// The catalog version this view is pinned to.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// What a name is, or `None` when nothing of that name exists.
    pub fn relation(&self, name: &str) -> Result<Option<Relation>> {
        let cache = self.cache();
        let key = (self.tenant, name.to_owned());
        if let Some(cache) = cache
            && let Some(hit) = cache.lock().names.get(&key)
        {
            return Ok(*hit);
        }
        let found = match self.txn.get(&record::name_key(self.tenant, name))? {
            Some(bytes) => Some(record::decode_relation(&bytes)?),
            None => None,
        };
        if let Some(cache) = cache {
            cache.lock().names.insert(key, found);
        }
        Ok(found)
    }

    /// A table by name, or `None`. An index's name resolves to no table: `SELECT * FROM an_index`
    /// is `42P01` in PostgreSQL too.
    pub fn table(&self, name: &str) -> Result<Option<Arc<TableDef>>> {
        match self.relation(name)? {
            Some(Relation::Table { table_id }) => self.table_by_id(table_id),
            Some(Relation::Index { .. } | Relation::PrimaryKey { .. }) | None => Ok(None),
        }
    }

    /// A table by id.
    pub fn table_by_id(&self, table_id: u64) -> Result<Option<Arc<TableDef>>> {
        let cache = self.cache();
        let key = (self.tenant, table_id);
        if let Some(cache) = cache
            && let Some(hit) = cache.lock().tables.get(&key)
        {
            return Ok(Some(Arc::clone(hit)));
        }
        let Some(bytes) = self.txn.get(&record::table_key(self.tenant, table_id))? else {
            return Ok(None);
        };
        let table = Arc::new(record::decode_table(&bytes)?);
        if let Some(cache) = cache {
            cache.lock().tables.insert(key, Arc::clone(&table));
        }
        Ok(Some(table))
    }

    /// The cache this view may use, or `None` — either because the transaction has written the
    /// catalog, or because the cache holds a version this view cannot be answered from.
    fn cache(&self) -> Option<&Catalog> {
        self.catalog
            .filter(|catalog| catalog.usable_at(self.version))
    }

    /// A table by name, or `42P01` — the shape almost every statement wants.
    pub fn require_table(&self, name: &str) -> Result<Arc<TableDef>> {
        self.table(name)?
            .ok_or_else(|| SqlError::UndefinedTable(name.to_owned()))
    }
}

/// Writes a new table, its name, and its indexes' names, and bumps the catalog version.
///
/// The duplicate check is the same composition `crate::backend` documents for a unique index: read
/// the name inside the transaction and require it absent, then write it. That catches a name that
/// is already committed; a *concurrent* `CREATE TABLE` of the same name is caught by the write
/// conflict on the same key, and one of the two loses at commit.
pub fn create_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    let names = [&table.name, &table.primary_key_name]
        .into_iter()
        .chain(table.indexes.iter().map(|index| &index.name));
    for name in names {
        if txn.get(&record::name_key(tenant, name))?.is_some() {
            return Err(SqlError::DuplicateTable(name.clone()));
        }
    }
    write_table(txn, tenant, table)?;
    bump_version(txn)
}

/// Rewrites a table that already exists — `CREATE INDEX` and `DROP INDEX` — and bumps the version.
///
/// `previous` is what the table looked like before, so names that are no longer in use are
/// removed. Passing the wrong one would leave a name pointing at an index that is gone.
pub fn replace_table(
    txn: &mut dyn Txn,
    tenant: u64,
    previous: &TableDef,
    table: &TableDef,
) -> Result<()> {
    for index in &previous.indexes {
        if !table.indexes.iter().any(|kept| kept.id == index.id) {
            txn.delete(&record::name_key(tenant, &index.name));
        }
    }
    for index in &table.indexes {
        if !previous.indexes.iter().any(|had| had.id == index.id)
            && txn.get(&record::name_key(tenant, &index.name))?.is_some()
        {
            return Err(SqlError::DuplicateTable(index.name.clone()));
        }
    }
    write_table(txn, tenant, table)?;
    bump_version(txn)
}

/// Removes a table, its name and its indexes' names, and bumps the version. The table's *rows* are
/// the caller's to delete; this is the catalog only.
pub fn drop_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    txn.delete(&record::table_key(tenant, table.id));
    // A relation id is never reused, so an orphan override could not be mistaken for another
    // table's -- but it would sit in the collector's scan of every override for ever.
    clear_table_retention(txn, tenant, table.id);
    txn.delete(&record::row_id_key(tenant, table.id));
    txn.delete(&record::name_key(tenant, &table.name));
    if !table.primary_key_name.is_empty() {
        txn.delete(&record::name_key(tenant, &table.primary_key_name));
    }
    for index in &table.indexes {
        txn.delete(&record::name_key(tenant, &index.name));
    }
    bump_version(txn)
}

fn write_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    txn.put(
        &record::table_key(tenant, table.id),
        &record::encode_table(table)?,
    );
    txn.put(
        &record::name_key(tenant, &table.name),
        &record::encode_relation(&Relation::Table { table_id: table.id }),
    );
    // The primary key constraint's name is a relation name and has to be taken, even though there
    // is no index behind it -- the row key is the primary key. PostgreSQL answers `42P07` to
    // `CREATE INDEX t_pkey ON t (a)` and so must this.
    // A table with no declared key has no constraint and so takes no name: `CREATE TABLE t (a
    // int8); CREATE INDEX t_pkey ON t (a);` succeeds on a real server.
    if !table.primary_key_name.is_empty() {
        txn.put(
            &record::name_key(tenant, &table.primary_key_name),
            &record::encode_relation(&Relation::PrimaryKey { table_id: table.id }),
        );
    }
    for index in &table.indexes {
        txn.put(
            &record::name_key(tenant, &index.name),
            &record::encode_relation(&Relation::Index {
                table_id: table.id,
                index_id: index.id,
            }),
        );
    }
    Ok(())
}

/// Moves one index to a new state, bumping the table's schema version.
///
/// **One step is all a caller may take**, and the check is here rather than in the caller because
/// this is the primitive every path goes through — the job, and a test staging a state by hand.
/// Two steps at once is exactly the two-version invariant broken (ADR 0020): a node whose snapshot
/// predates the write would be two states behind a node whose snapshot follows it, and no pair of
/// states two apart is safe together.
///
/// It bumps `schema_version`, which is what makes the transition a *table* event that
/// `IndexDef::state_since` can be compared against — the cluster-wide catalog version says only
/// that something moved.
pub fn advance_index_state(
    txn: &mut dyn Txn,
    tenant: u64,
    table: &TableDef,
    index_id: u64,
    to: SchemaState,
) -> Result<TableDef> {
    let next_version = table.schema_version + 1;
    let mut updated = table.clone();
    updated.schema_version = next_version;
    let index = updated
        .indexes
        .iter_mut()
        .find(|index| index.id == index_id)
        .ok_or_else(|| {
            SqlError::Internal(format!(
                "index {index_id} is not on table \"{}\"",
                table.name
            ))
        })?;
    // **One step, in either direction.** Forwards is a change being added and backwards is one
    // being removed — or one being unwound after it failed — and both need the same rule: a node
    // one step behind must never be two, whichever way the cluster is moving.
    //
    // Idempotent for the state it is already in, so a job that crashed after writing and before
    // recording resumes rather than failing.
    let one_step = index.state == to
        || index.state.forward() == Some(to)
        || index.state.backward() == Some(to);
    if !one_step {
        return Err(SqlError::Internal(format!(
            "index \"{}\" cannot go from {} to {} in one step",
            index.name,
            index.state.name(),
            to.name()
        )));
    }
    index.state = to;
    index.state_since = next_version;
    replace_table(txn, tenant, table, &updated)?;
    Ok(updated)
}

/// A schema-change job in flight: which table, how far its backfill got, and whether it finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// The index being built.
    pub index_id: u64,
    /// The table it is on.
    pub table_id: u64,
    /// The row key the next batch starts at. Empty means "from the beginning".
    ///
    /// **Durable, and that is the point.** A backfill is many small transactions, and a node that
    /// dies mid-job must resume rather than restart — on a large table, restarting is how a job
    /// never finishes.
    pub cursor: Vec<u8>,
    /// Whether the backfill has reached the end of the table.
    pub done: bool,
}

/// Records a job, or moves its cursor on.
pub fn put_job(txn: &mut dyn Txn, tenant: u64, job: &JobRecord) {
    txn.put(
        &record::job_key(tenant, job.index_id),
        &record::encode_job(job.table_id, &job.cursor, job.done),
    );
}

/// One job, or `None`.
pub fn job(txn: &dyn Txn, tenant: u64, index_id: u64) -> Result<Option<JobRecord>> {
    let Some(bytes) = txn.get(&record::job_key(tenant, index_id))? else {
        return Ok(None);
    };
    let (table_id, cursor, done) = record::decode_job(&bytes)?;
    Ok(Some(JobRecord {
        index_id,
        table_id,
        cursor,
        done,
    }))
}

/// Forgets a job. Called when it reaches `public`, or when it unwinds.
pub fn drop_job(txn: &mut dyn Txn, tenant: u64, index_id: u64) {
    txn.delete(&record::job_key(tenant, index_id));
}

/// The `[start, end)` key range holding one tenant's jobs, in index-id order.
#[must_use]
pub fn job_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    record::job_range(tenant)
}

/// One listed job, out of its key and its value.
pub fn decode_job(tenant: u64, key: &[u8], value: &[u8]) -> Result<JobRecord> {
    let index_id = record::job_index_id(tenant, key)?;
    let (table_id, cursor, done) = record::decode_job(value)?;
    Ok(JobRecord {
        index_id,
        table_id,
        cursor,
        done,
    })
}

/// A flashback in progress: where it is putting the table back to, and how far it has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashbackRecord {
    /// The table being put back.
    pub table_id: u64,
    /// The snapshot it is being put back to.
    pub target_ts: u64,
    /// The row key the next batch starts at. Empty means "from the beginning".
    pub cursor: Vec<u8>,
    /// How many rows have been changed so far, for the answer the statement gives.
    pub changed: u64,
}

/// Records a flashback, or moves its cursor on.
pub fn put_flashback(txn: &mut dyn Txn, tenant: u64, record: &FlashbackRecord) {
    txn.put(
        &record::flashback_key(tenant, record.table_id),
        &record::encode_flashback(record.target_ts, &record.cursor, record.changed),
    );
}

/// The flashback in progress on one table, or `None`.
pub fn flashback(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Option<FlashbackRecord>> {
    let Some(bytes) = txn.get(&record::flashback_key(tenant, table_id))? else {
        return Ok(None);
    };
    let (target_ts, cursor, changed) = record::decode_flashback(&bytes)?;
    Ok(Some(FlashbackRecord {
        table_id,
        target_ts,
        cursor,
        changed,
    }))
}

/// Forgets a flashback, when it has finished.
pub fn drop_flashback(txn: &mut dyn Txn, tenant: u64, table_id: u64) {
    txn.delete(&record::flashback_key(tenant, table_id));
}

/// Sets the retention override for one table, in milliseconds.
///
/// It does **not** bump the catalog version, and that is deliberate. Retention changes nothing
/// about how a row is written or read — it is a housekeeping bound the collector reads directly
/// from the store — so bumping the version would make every node in the cluster throw away its
/// table cache to learn a number none of them uses. The collector picks the change up on its next
/// pass, which is the only place it matters.
pub fn set_table_retention(txn: &mut dyn Txn, tenant: u64, table_id: u64, retention_ms: u64) {
    txn.put(
        &record::retention_key(tenant, table_id),
        &record::encode_retention(retention_ms),
    );
}

/// Removes a table's override, putting it back on the cluster default.
pub fn clear_table_retention(txn: &mut dyn Txn, tenant: u64, table_id: u64) {
    txn.delete(&record::retention_key(tenant, table_id));
}

/// One table's override, or `None` when it takes the cluster default.
pub fn table_retention(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Option<u64>> {
    match txn.get(&record::retention_key(tenant, table_id))? {
        Some(bytes) => Ok(Some(record::decode_retention(&bytes)?)),
        None => Ok(None),
    }
}

/// Sets the cluster's default retention, in milliseconds.
pub fn set_default_retention(txn: &mut dyn Txn, retention_ms: u64) {
    txn.put(
        &record::default_retention_key(),
        &record::encode_retention(retention_ms),
    );
}

/// The cluster's default retention, or [`DEFAULT_RETENTION_MS`] when nobody has set one.
pub fn default_retention(txn: &dyn Txn) -> Result<u64> {
    match txn.get(&record::default_retention_key())? {
        Some(bytes) => record::decode_retention(&bytes),
        None => Ok(DEFAULT_RETENTION_MS),
    }
}

/// Names a timestamp: one record, and nothing else.
///
/// A checkpoint is **free** ([ADR 0021](../../../docs/adr/0021-time-machine.md) Decision 3) — no
/// snapshot, no copy, no flush — because the data it refers to is kept by retention whether
/// anybody named it or not. Re-using a name replaces it, which is what `pg_export_snapshot()`'s
/// named variant should do: a name is a label a user moves, not a unique key they have to free.
pub fn set_checkpoint(txn: &mut dyn Txn, tenant: u64, name: &str, start_ts: u64) {
    txn.put(
        &record::checkpoint_key(tenant, name),
        &record::encode_checkpoint(start_ts),
    );
}

/// The timestamp a checkpoint names, or `None` if there is no such checkpoint.
pub fn checkpoint_at(txn: &dyn Txn, tenant: u64, name: &str) -> Result<Option<u64>> {
    match txn.get(&record::checkpoint_key(tenant, name))? {
        Some(bytes) => Ok(Some(record::decode_checkpoint(&bytes)?)),
        None => Ok(None),
    }
}

/// Forgets a checkpoint. The versions it named are retention's business and are not touched.
pub fn drop_checkpoint(txn: &mut dyn Txn, tenant: u64, name: &str) {
    txn.delete(&record::checkpoint_key(tenant, name));
}

/// The `[start, end)` key range holding one tenant's checkpoints, in name order.
#[must_use]
pub fn checkpoint_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    record::checkpoint_range(tenant)
}

/// One listed checkpoint: the name out of its key, and the timestamp out of its value.
pub fn decode_checkpoint(tenant: u64, key: &[u8], value: &[u8]) -> Result<(String, u64)> {
    Ok((
        record::checkpoint_name(tenant, key)?,
        record::decode_checkpoint(value)?,
    ))
}

/// Reserves `count` consecutive row ids for one table and answers with the first.
///
/// **This is called in a transaction of its own, not the statement's**, and the reason is the
/// whole design. The counter is one key per table; if every `INSERT` bumped it inside its own
/// transaction then two concurrent inserts into the same table would conflict on that key and one
/// would always lose — a table with no primary key would accept one writer at a time. So a session
/// reserves a batch ([`ROW_ID_BATCH`]) in a short transaction of its own and hands ids out from
/// memory.
///
/// The consequence is **gaps**, and they are not a defect: ids reserved by a statement that rolls
/// back, or by a session that ends, are never handed out. That is exactly what a PostgreSQL
/// sequence does — `nextval` is non-transactional and a rolled-back `INSERT` still consumed its
/// value — so a user who knows PostgreSQL already expects it. Nothing user-visible depends on the
/// ids being dense; they are the row's identity and never its data.
pub fn allocate_row_ids(txn: &mut dyn Txn, tenant: u64, table_id: u64, count: u64) -> Result<u64> {
    let key = record::row_id_key(tenant, table_id);
    let next = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        // Ids start at 1, so 0 is never a row.
        None => 1,
    };
    let after = next
        .checked_add(count)
        .ok_or_else(|| SqlError::Internal("the row id sequence is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(after));
    Ok(next)
}

/// Takes the next relation id for a tenant. Ids start at 1, so 0 is never a real relation.
pub fn allocate_id(txn: &mut dyn Txn, tenant: u64) -> Result<u64> {
    let key = record::next_id_key(tenant);
    let next = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        None => 1,
    };
    let after = next
        .checked_add(1)
        .ok_or_else(|| SqlError::Internal("the relation id sequence is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(after));
    Ok(next)
}

/// Moves the catalog version forward, which is what makes every node's cache notice.
///
/// Every DDL statement writes this one key, so two concurrent DDL statements conflict and one of
/// them is told to retry. That is the intended serialisation and not a bottleneck worth removing:
/// DDL is rare and a catalog that two statements changed at once is a catalog nobody can reason
/// about.
pub fn bump_version(txn: &mut dyn Txn) -> Result<()> {
    let key = record::version_key();
    let current = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        None => 0,
    };
    let next = current
        .checked_add(1)
        .ok_or_else(|| SqlError::Internal("the catalog version is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(next));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Catalog, ColumnDef, DEFAULT_RETENTION_MS, IndexDef, MAX_IDENTIFIER_BYTES,
        RETENTION_FOREVER, Relation, SchemaState, TableDef, allocate_id, clear_table_retention,
        create_table, default_retention, drop_table, fold_identifier, record, replace_table,
        set_default_retention, set_table_retention, table_retention,
    };
    use crate::backend::{Backend, MemoryBackend};
    use crate::sqlstate;
    use crate::value::ColumnType;

    /// The other direction, for a golden that is written down rather than produced.
    fn decode_hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    fn accounts(id: u64) -> TableDef {
        TableDef {
            id,
            name: "accounts".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: ColumnType::Int8,
                    not_null: true,
                    default: None,
                    missing: None,
                },
                ColumnDef {
                    name: "email".into(),
                    ty: ColumnType::Text,
                    not_null: false,
                    default: None,
                    missing: None,
                },
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef {
                id: id + 1,
                name: "accounts_email_key".into(),
                unique: true,
                columns: vec![1],
                state: SchemaState::Public,
                state_since: 1,
            }],
            primary_key_name: "accounts_pkey".into(),
            schema_version: 1,
        }
    }

    /// The golden. A catalog record is an on-disk format like any other, and these bytes are it.
    #[test]
    fn a_table_record_is_a_version_and_then_the_definition() {
        let encoded = record::encode_table(&accounts(7)).unwrap();
        assert_eq!(
            hex(&encoded),
            concat!(
                "03",                 // catalog format version
                "0700000000000000",   // table id 7
                "086163636f756e7473", // varint 8, "accounts"
                // varint 13, "accounts_pkey" -- the primary key constraint's name. It is a
                // relation name like any other and has to be reserved, even though there is no
                // index behind it: the row key *is* the primary key.
                "0d6163636f756e74735f706b6579",
                "01", // schema version 1: CREATE TABLE has run and no ALTER has
                "02", // two columns
                "026964",
                "01",
                "01", // "id", INT8, NOT NULL
                "00", // no DEFAULT
                "00", // and no missing value
                "05656d61696c",
                "02",
                "00", // "email", TEXT, nullable
                "00", // no DEFAULT
                "00", // and no missing value
                "01",
                "00",                                     // primary key: one column, column 0
                "01",                                     // one index
                "0800000000000000",                       // index id 8
                "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
                "01",                                     // unique
                "03",                                     // state: public
                "01",                                     // entered at schema version 1
                "01",
                "01", // one column, column 1
            )
        );
        assert_eq!(record::decode_table(&encoded).unwrap(), accounts(7));
    }

    /// The **version 2** golden, kept rather than replaced.
    ///
    /// These are the bytes version 2 wrote, and a cluster that ran phase 6a or 6d has them on
    /// disk. A format change that replaced its golden would be a format change nothing could
    /// prove it had survived, so the old bytes stay and this test decodes them: every column comes
    /// back with no default and no missing value, which is what a column nobody gave one means.
    #[test]
    fn a_version_2_table_record_still_decodes() {
        let v2 = decode_hex(concat!(
            "02",                 // catalog format version 2
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01", // "id", INT8, NOT NULL -- and nothing after it
            "05656d61696c",
            "02",
            "00", // "email", TEXT, nullable
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",
            "01",
            "01",
        ));
        assert_eq!(record::decode_table(&v2).unwrap(), accounts(7));
    }

    /// Version 3 changed the **table** record's layout and nothing else's, so a version 2 record of
    /// any other kind decodes unchanged. Refusing them because the byte moved would break a cluster
    /// that had run phase 6d over a change that does not touch them.
    #[test]
    fn a_version_2_record_of_another_kind_decodes_unchanged() {
        let mut v2 = record::encode_retention(60_000);
        v2[0] = 2;
        assert_eq!(record::decode_retention(&v2).unwrap(), 60_000);

        let mut v2 = record::encode_checkpoint(1_234);
        v2[0] = 2;
        assert_eq!(record::decode_checkpoint(&v2).unwrap(), 1_234);
    }

    /// Invariant 2 again: a record from a version we do not know is an error, and every truncation
    /// of a record we do know is one too (invariant 9).
    #[test]
    fn a_record_that_is_not_ours_is_refused_rather_than_guessed_at() {
        let mut encoded = record::encode_table(&accounts(1)).unwrap();
        encoded[0] = super::CATALOG_FORMAT_VERSION + 1;
        assert_eq!(
            record::decode_table(&encoded).unwrap_err().sqlstate(),
            sqlstate::DATA_CORRUPTED
        );

        let encoded = record::encode_table(&accounts(1)).unwrap();
        for cut in 0..encoded.len() {
            assert!(
                record::decode_table(&encoded[..cut]).is_err(),
                "{cut} bytes decoded as a whole table"
            );
        }
    }

    /// A catalog that points at a column the table does not have would be a wrong column later,
    /// which is worse than an error now.
    #[test]
    fn a_primary_key_pointing_past_the_columns_is_corruption() {
        // Found rather than computed: the one byte that differs between a primary key on column 0
        // and one on column 1 is the ordinal, so the test cannot drift out of the format.
        let mut on_first = accounts(1);
        on_first.primary_key = vec![0];
        let mut on_second = accounts(1);
        on_second.primary_key = vec![1];
        let (a, b) = (
            record::encode_table(&on_first).unwrap(),
            record::encode_table(&on_second).unwrap(),
        );
        let differing: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        assert_eq!(differing.len(), 1, "exactly one byte is the ordinal");

        let mut encoded = a;
        encoded[differing[0]] = 9;
        assert_eq!(
            record::decode_table(&encoded).unwrap_err().sqlstate(),
            sqlstate::DATA_CORRUPTED
        );
    }

    /// Only ASCII folds. `str::to_lowercase` would have renamed every non-ASCII identifier, and a
    /// real server leaves them alone.
    #[test]
    fn identifiers_fold_the_way_a_utf8_server_folds_them() {
        assert_eq!(fold_identifier("Mixed", false).0, "mixed");
        assert_eq!(fold_identifier("Quoted", true).0, "Quoted");
        assert_eq!(fold_identifier("Ébc", false).0, "Ébc", "only A-Z fold");

        let long = "x".repeat(70);
        let (folded, truncated) = fold_identifier(&long, false);
        assert_eq!(folded.len(), MAX_IDENTIFIER_BYTES);
        assert!(truncated, "the caller owes the client a 42622 notice");

        // A multi-byte character straddling the limit is dropped whole, or the name would not be
        // UTF-8 and the catalog could not read it back.
        let wide = format!("{}é", "x".repeat(62));
        let (folded, _) = fold_identifier(&wide, false);
        assert_eq!(folded, "x".repeat(62));
    }

    #[test]
    fn a_table_is_found_by_name_and_by_id_after_it_is_committed() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        let id = allocate_id(&mut *ddl, 1).unwrap();
        let table = accounts(id);
        create_table(&mut *ddl, 1, &table).unwrap();
        ddl.commit().unwrap();

        let txn = backend.begin().unwrap();
        let view = catalog.view(&*txn, 1).unwrap();
        assert_eq!(view.version(), 1, "one DDL statement, one version");
        assert_eq!(*view.table("accounts").unwrap().unwrap(), table);
        assert_eq!(*view.table_by_id(id).unwrap().unwrap(), table);
        assert_eq!(
            view.relation("accounts_email_key").unwrap(),
            Some(Relation::Index {
                table_id: id,
                index_id: id + 1,
            }),
            "an index's name is in the same namespace"
        );
        assert!(
            view.table("accounts_email_key").unwrap().is_none(),
            "and it is not a table"
        );
        assert_eq!(
            view.require_table("nope").unwrap_err().sqlstate(),
            sqlstate::UNDEFINED_TABLE
        );
    }

    /// DDL is invisible until it commits, like any other write. A node that showed uncommitted DDL
    /// would let a query see a table that may never exist.
    #[test]
    fn an_uncommitted_table_is_invisible_to_everyone_else() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        create_table(&mut *ddl, 1, &accounts(1)).unwrap();

        let other = backend.begin().unwrap();
        assert!(
            catalog
                .view(&*other, 1)
                .unwrap()
                .table("accounts")
                .unwrap()
                .is_none()
        );
        // The writer sees its own DDL, because read-your-writes applies to the catalog too.
        assert!(
            catalog
                .view(&*ddl, 1)
                .unwrap()
                .table("accounts")
                .unwrap()
                .is_some()
        );
    }

    /// The cache must never answer with a definition from a version the asking transaction cannot
    /// see. This is the test for that: fill the cache at a new version, then ask an older
    /// transaction, which must read through and get its own snapshot's answer.
    #[test]
    fn a_cache_warmed_by_a_newer_transaction_does_not_leak_into_an_older_one() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut first = backend.begin().unwrap();
        create_table(&mut *first, 1, &accounts(1)).unwrap();
        first.commit().unwrap();

        // Starts before the second table exists.
        let old = backend.begin().unwrap();

        let mut second = backend.begin().unwrap();
        let mut ledger = accounts(3);
        ledger.name = "ledger".into();
        ledger.primary_key_name = "ledger_pkey".into();
        ledger.indexes[0].name = "ledger_email_key".into();
        create_table(&mut *second, 1, &ledger).unwrap();
        second.commit().unwrap();

        // Warm the cache at the new version.
        let new = backend.begin().unwrap();
        let new_view = catalog.view(&*new, 1).unwrap();
        assert_eq!(new_view.version(), 2);
        assert!(new_view.table("ledger").unwrap().is_some());

        let old_view = catalog.view(&*old, 1).unwrap();
        assert_eq!(old_view.version(), 1, "its own snapshot's catalog");
        assert!(
            old_view.table("ledger").unwrap().is_none(),
            "the newer transaction's cache entry must not reach an older snapshot"
        );
        assert!(old_view.table("accounts").unwrap().is_some());
    }

    /// A definition that changed shape must not survive in a cache. Rewriting the table under a
    /// new version has to reach every later reader.
    #[test]
    fn a_changed_table_is_not_served_from_the_cache() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        create_table(&mut *ddl, 1, &accounts(1)).unwrap();
        ddl.commit().unwrap();

        let first = backend.begin().unwrap();
        let before = catalog
            .view(&*first, 1)
            .unwrap()
            .table("accounts")
            .unwrap()
            .unwrap();
        assert_eq!(before.indexes.len(), 1);

        let mut adding = backend.begin().unwrap();
        let mut with_more = accounts(1);
        with_more.indexes.push(IndexDef {
            id: 5,
            name: "accounts_id_idx".into(),
            unique: false,
            columns: vec![0],
            state: SchemaState::Public,
            state_since: 1,
        });
        replace_table(&mut *adding, 1, &accounts(1), &with_more).unwrap();
        adding.commit().unwrap();

        let second = backend.begin().unwrap();
        let after = catalog
            .view(&*second, 1)
            .unwrap()
            .table("accounts")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.indexes.len(),
            2,
            "the cache served a table with a missing index"
        );
    }

    /// Dropping an index frees its name, and dropping a table frees every name it held.
    #[test]
    fn dropping_frees_the_names_it_held() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        let table = accounts(1);
        create_table(&mut *ddl, 1, &table).unwrap();
        let mut without = table.clone();
        without.indexes.clear();
        replace_table(&mut *ddl, 1, &table, &without).unwrap();
        drop_table(&mut *ddl, 1, &without).unwrap();
        ddl.commit().unwrap();

        let txn = backend.begin().unwrap();
        let view = catalog.view(&*txn, 1).unwrap();
        assert_eq!(view.relation("accounts").unwrap(), None);
        assert_eq!(view.relation("accounts_email_key").unwrap(), None);
    }

    /// One namespace: an index cannot take a table's name, which is `42P07` in PostgreSQL and was
    /// confirmed against the server.
    #[test]
    fn an_index_cannot_take_a_name_a_table_already_has() {
        let backend = MemoryBackend::new();
        let mut ddl = backend.begin().unwrap();
        let table = accounts(1);
        create_table(&mut *ddl, 1, &table).unwrap();

        let mut clash = table.clone();
        clash.indexes.push(IndexDef {
            id: 9,
            name: "accounts".into(),
            unique: false,
            columns: vec![0],
            state: SchemaState::Public,
            state_since: 1,
        });
        let error = replace_table(&mut *ddl, 1, &table, &clash).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);

        let mut again = accounts(2);
        again.indexes.clear();
        assert_eq!(
            create_table(&mut *ddl, 1, &again).unwrap_err().sqlstate(),
            sqlstate::DUPLICATE_TABLE
        );
    }

    /// Two nodes creating the same table at once both see the name as free, both write it, and one
    /// of them loses at commit -- the same composition that makes a unique index work, applied to
    /// the catalog.
    #[test]
    fn two_concurrent_creates_of_one_name_leave_one_table() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();

        create_table(&mut *left, 1, &accounts(1)).unwrap();
        let mut other = accounts(2);
        other.indexes[0].name = "accounts_email_key2".into();
        create_table(&mut *right, 1, &other).unwrap();

        assert!(left.commit().is_ok());
        let loser = right.commit().expect_err("one of them must lose");
        assert_eq!(loser.sqlstate(), sqlstate::SERIALIZATION_FAILURE);
    }

    /// Ids are unique across tables and indexes alike, so a debug dump can never confuse the two.
    #[test]
    fn relation_ids_come_from_one_sequence_and_start_at_one() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        assert_eq!(allocate_id(&mut *txn, 1).unwrap(), 1);
        assert_eq!(allocate_id(&mut *txn, 1).unwrap(), 2);
        assert_eq!(allocate_id(&mut *txn, 2).unwrap(), 1, "per tenant");
        txn.commit().unwrap();

        let mut next = backend.begin().unwrap();
        assert_eq!(
            allocate_id(&mut *next, 1).unwrap(),
            3,
            "and it survives a commit"
        );
    }

    /// A rolled-back DDL must leave nothing behind in the cache every session shares.
    ///
    /// The trap is that catalog versions are *reused*: the bump is `read + 1` inside the
    /// transaction, so a DDL that rolls back leaves its number free and the next DDL that really
    /// commits lands on it — and finds the abandoned entry waiting. Before
    /// [`Catalog::view_uncached`] this returned a table nobody had ever created.
    #[test]
    fn a_rolled_back_ddl_leaves_nothing_in_the_cache() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        // Session A: BEGIN; CREATE TABLE accounts; look at it; ROLLBACK.
        let mut a = backend.begin().unwrap();
        create_table(&mut *a, 1, &accounts(1)).unwrap();
        {
            let view = catalog.view_uncached(&*a, 1).unwrap();
            assert_eq!(view.version(), 1);
            assert!(view.table("accounts").unwrap().is_some(), "its own writes");
        }
        a.rollback().unwrap();

        // Session B commits a different DDL, which reaches the same catalog version.
        let mut b = backend.begin().unwrap();
        let mut ledger = accounts(3);
        ledger.name = "ledger".into();
        ledger.primary_key_name = "ledger_pkey".into();
        ledger.indexes[0].name = "ledger_email_key".into();
        create_table(&mut *b, 1, &ledger).unwrap();
        b.commit().unwrap();

        let c = backend.begin().unwrap();
        let view = catalog.view(&*c, 1).unwrap();
        assert_eq!(view.version(), 1);
        assert!(
            view.table("accounts").unwrap().is_none(),
            "a table nobody committed came back from the cache"
        );
    }

    /// The golden for a retention record. The garbage collector reads these bytes from a crate
    /// that does not link this one (ADR 0021), so the layout is a contract between two layers and
    /// not an implementation detail of either.
    #[test]
    fn a_retention_record_is_a_version_and_a_u64_of_milliseconds() {
        let encoded = record::encode_retention(600_000);
        assert_eq!(
            hex(&encoded),
            concat!(
                "03",               // catalog format version
                "c027090000000000", // 600000 ms -- ten minutes, little-endian
            )
        );
        assert_eq!(record::decode_retention(&encoded).unwrap(), 600_000);

        // The key layout is the other half of the contract: one scan of the kind byte visits every
        // override, and the cluster default is not in it.
        let key = record::retention_key(1, 7);
        let default = record::default_retention_key();
        assert_eq!(
            hex(&key),
            concat!(
                "6d",               // 'm', the meta space
                "73716c",           // "sql"
                "72",               // 'r', a retention override
                "0000000000000001", // tenant 1, memcomparable
                "0000000000000007", // table 7
            )
        );
        assert_eq!(hex(&default), concat!("6d", "73716c", "64"));
        assert!(
            !default.starts_with(&key[..key.len() - 16]),
            "the default must not fall inside a scan of the overrides"
        );

        // Forever is a reserved value, not a very large duration: a reader subtracts every other
        // one from a timestamp and cannot subtract this.
        assert_eq!(
            record::decode_retention(&record::encode_retention(RETENTION_FOREVER)).unwrap(),
            u64::MAX
        );

        // And it fails closed like every other record.
        let mut damaged = encoded.clone();
        damaged[0] = record::CATALOG_FORMAT_VERSION + 1;
        assert!(record::decode_retention(&damaged).is_err());
        for cut in 0..encoded.len() {
            assert!(
                record::decode_retention(&encoded[..cut]).is_err(),
                "{cut} bytes decoded as a retention"
            );
        }
        assert!(
            record::decode_retention(&[&encoded[..], &[0]].concat()).is_err(),
            "a trailing byte is not ignored"
        );
    }

    /// Retention is per table with a cluster fallback, and it survives the table being dropped
    /// only in the sense that it does not: an override goes when its table does.
    #[test]
    fn retention_falls_back_to_the_cluster_default_and_dies_with_its_table() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();

        assert_eq!(default_retention(&*txn).unwrap(), DEFAULT_RETENTION_MS);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), None);

        set_default_retention(&mut *txn, 5_000);
        set_table_retention(&mut *txn, 1, 7, 600_000);
        assert_eq!(default_retention(&*txn).unwrap(), 5_000);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), Some(600_000));
        assert_eq!(table_retention(&*txn, 1, 8).unwrap(), None, "another table");
        assert_eq!(
            table_retention(&*txn, 2, 7).unwrap(),
            None,
            "another tenant"
        );

        clear_table_retention(&mut *txn, 1, 7);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), None);

        let table = accounts(7);
        create_table(&mut *txn, 1, &table).unwrap();
        set_table_retention(&mut *txn, 1, 7, RETENTION_FOREVER);
        drop_table(&mut *txn, 1, &table).unwrap();
        assert_eq!(
            table_retention(&*txn, 1, 7).unwrap(),
            None,
            "an override must not outlive its table in the collector's scan"
        );
    }

    proptest::proptest! {
        /// Invariant 9 for the catalog: arbitrary bytes are an error or a definition, never a
        /// panic and never an allocation the length asked for but the record cannot fill.
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            bytes in proptest::collection::vec(proptest::arbitrary::any::<u8>(), 0..128)
        ) {
            let _ = record::decode_table(&bytes);
            let _ = record::decode_relation(&bytes);
            let _ = record::decode_counter(&bytes);
        }

        /// And the same for bytes that start out looking like a record, which is the shape a
        /// truncated or partly-overwritten one actually has.
        #[test]
        fn decoding_a_damaged_record_never_panics(
            at in 0usize..64,
            byte in proptest::arbitrary::any::<u8>(),
        ) {
            let mut encoded = record::encode_table(&accounts(1)).unwrap();
            if at < encoded.len() {
                encoded[at] = byte;
            }
            let _ = record::decode_table(&encoded);
        }
    }
}
