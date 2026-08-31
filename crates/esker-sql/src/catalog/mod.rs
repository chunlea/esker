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

mod record;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::value::ColumnType;

/// The version byte on every catalog record. An unknown one is an error, never a guess
/// (`CLAUDE.md` invariant 2).
pub const CATALOG_FORMAT_VERSION: u8 = record::CATALOG_FORMAT_VERSION;

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
}

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
}

impl TableDef {
    /// The position of a column by name, or `None`.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// Every column's type, in encoding order — what [`crate::row`] needs.
    #[must_use]
    pub fn column_types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(|column| column.ty).collect()
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
    pub fn view<'a>(&'a self, txn: &'a dyn Txn, tenant: u64) -> Result<View<'a>> {
        let version = match txn.get(&record::version_key())? {
            Some(bytes) => record::decode_counter(&bytes)?,
            // No DDL has ever run. Version 0 is the empty catalog.
            None => 0,
        };
        Ok(View {
            catalog: self,
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
    catalog: &'a Catalog,
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
        let cacheable = self.catalog.usable_at(self.version);
        let key = (self.tenant, name.to_owned());
        if cacheable && let Some(hit) = self.catalog.lock().names.get(&key) {
            return Ok(*hit);
        }
        let found = match self.txn.get(&record::name_key(self.tenant, name))? {
            Some(bytes) => Some(record::decode_relation(&bytes)?),
            None => None,
        };
        if cacheable {
            self.catalog.lock().names.insert(key, found);
        }
        Ok(found)
    }

    /// A table by name, or `None`. An index's name resolves to no table: `SELECT * FROM an_index`
    /// is `42P01` in PostgreSQL too.
    pub fn table(&self, name: &str) -> Result<Option<Arc<TableDef>>> {
        match self.relation(name)? {
            Some(Relation::Table { table_id }) => self.table_by_id(table_id),
            Some(Relation::Index { .. }) | None => Ok(None),
        }
    }

    /// A table by id.
    pub fn table_by_id(&self, table_id: u64) -> Result<Option<Arc<TableDef>>> {
        let cacheable = self.catalog.usable_at(self.version);
        let key = (self.tenant, table_id);
        if cacheable && let Some(hit) = self.catalog.lock().tables.get(&key) {
            return Ok(Some(Arc::clone(hit)));
        }
        let Some(bytes) = self.txn.get(&record::table_key(self.tenant, table_id))? else {
            return Ok(None);
        };
        let table = Arc::new(record::decode_table(&bytes)?);
        if cacheable {
            self.catalog.lock().tables.insert(key, Arc::clone(&table));
        }
        Ok(Some(table))
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
    for name in std::iter::once(&table.name).chain(table.indexes.iter().map(|i| &i.name)) {
        if txn.get(&record::name_key(tenant, name))?.is_some() {
            return Err(SqlError::DuplicateTable(name.clone()));
        }
    }
    write_table(txn, tenant, table);
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
    write_table(txn, tenant, table);
    bump_version(txn)
}

/// Removes a table, its name and its indexes' names, and bumps the version. The table's *rows* are
/// the caller's to delete; this is the catalog only.
pub fn drop_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    txn.delete(&record::table_key(tenant, table.id));
    txn.delete(&record::name_key(tenant, &table.name));
    for index in &table.indexes {
        txn.delete(&record::name_key(tenant, &index.name));
    }
    bump_version(txn)
}

fn write_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) {
    txn.put(
        &record::table_key(tenant, table.id),
        &record::encode_table(table),
    );
    txn.put(
        &record::name_key(tenant, &table.name),
        &record::encode_relation(&Relation::Table { table_id: table.id }),
    );
    for index in &table.indexes {
        txn.put(
            &record::name_key(tenant, &index.name),
            &record::encode_relation(&Relation::Index {
                table_id: table.id,
                index_id: index.id,
            }),
        );
    }
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
        Catalog, ColumnDef, IndexDef, MAX_IDENTIFIER_BYTES, Relation, TableDef, allocate_id,
        create_table, drop_table, fold_identifier, record, replace_table,
    };
    use crate::backend::{Backend, MemoryBackend};
    use crate::sqlstate;
    use crate::value::ColumnType;

    fn accounts(id: u64) -> TableDef {
        TableDef {
            id,
            name: "accounts".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: ColumnType::Int8,
                    not_null: true,
                },
                ColumnDef {
                    name: "email".into(),
                    ty: ColumnType::Text,
                    not_null: false,
                },
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef {
                id: id + 1,
                name: "accounts_email_key".into(),
                unique: true,
                columns: vec![1],
            }],
        }
    }

    /// The golden. A catalog record is an on-disk format like any other, and these bytes are it.
    #[test]
    fn a_table_record_is_a_version_and_then_the_definition() {
        let encoded = record::encode_table(&accounts(7));
        let hex: String = {
            use std::fmt::Write as _;
            encoded.iter().fold(String::new(), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
        };
        assert_eq!(
            hex,
            concat!(
                "01",                 // catalog format version
                "0700000000000000",   // table id 7
                "086163636f756e7473", // varint 8, "accounts"
                "02",                 // two columns
                "026964",
                "01",
                "01", // "id", INT8, NOT NULL
                "05656d61696c",
                "02",
                "00", // "email", TEXT, nullable
                "01",
                "00",                                     // primary key: one column, column 0
                "01",                                     // one index
                "0800000000000000",                       // index id 8
                "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
                "01",                                     // unique
                "01",
                "01", // one column, column 1
            )
        );
        assert_eq!(record::decode_table(&encoded).unwrap(), accounts(7));
    }

    /// Invariant 2 again: a record from a version we do not know is an error, and every truncation
    /// of a record we do know is one too (invariant 9).
    #[test]
    fn a_record_that_is_not_ours_is_refused_rather_than_guessed_at() {
        let mut encoded = record::encode_table(&accounts(1));
        encoded[0] = super::CATALOG_FORMAT_VERSION + 1;
        assert_eq!(
            record::decode_table(&encoded).unwrap_err().sqlstate(),
            sqlstate::DATA_CORRUPTED
        );

        let encoded = record::encode_table(&accounts(1));
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
            record::encode_table(&on_first),
            record::encode_table(&on_second),
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
            let mut encoded = record::encode_table(&accounts(1));
            if at < encoded.len() {
                encoded[at] = byte;
            }
            let _ = record::decode_table(&encoded);
        }
    }
}
