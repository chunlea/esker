//! Every relation this tenant has, read once, with the **one** function that says what its oid is.
//!
//! `pg_class`, `pg_attribute`, `pg_index`, `pg_constraint` and `information_schema` are five views
//! over one set of facts, and every statement `ActiveRecord` writes joins them by oid:
//! `t.oid = d.indrelid`, `d.indexrelid = i.oid`, `a.attrelid = d.adrelid`, `c.conrelid = t.oid`.
//! An oid that differs between two views breaks all of them silently; an oid that **collides**
//! breaks them in a way that looks like it works. So there is one snapshot and one `oid`, and no
//! view computes either for itself.
//!
//! # What an oid is
//!
//! The id the catalog record already carries. Table and index ids come from one sequence per
//! tenant (`catalog::record`, which is private), so no two relations of one tenant can share one.
//!
//! | Relation | oid |
//! |---|---|
//! | table | `table_id` |
//! | index | `index_id` |
//! | sequence | `SequenceDef::id` |
//! | primary key | [`PRIMARY_KEY_OID_BASE`] `+ table_id` |
//!
//! **Two of those were the table's own id until this module.** `9de9519` built `pg_class` out of
//! the name records and read the *key* rather than the relation for a primary key and a sequence,
//! so `t`, `t_pkey` and `t_id_seq` were three rows of `pg_class` with one oid between them.
//! Nothing read `pg_class.oid` before phase 13, which is why it survived; every statement in the
//! schema-dump path reads it. Measured on 19beta1: `SELECT c.oid <> p.oid FROM pg_class c, pg_class
//! p WHERE c.relname = 'cb' AND p.relname = 'cb_pkey'` is `t`, and `pg_index` for `cb` carries
//! `indexrelid` distinct from `indrelid`.
//!
//! A **sequence** had an id already and it was simply not being read. A **primary key** has none:
//! its name record is `Relation::PrimaryKey { table_id }` and giving it a field would bump
//! `CATALOG_FORMAT_VERSION` for a number that can be derived. So it is derived, in a band nothing
//! a user creates can reach, and reversibly — `oid - PRIMARY_KEY_OID_BASE` is the table whose key
//! it is, which is what `pg_index.indrelid` and `pg_constraint.conrelid` need.
//!
//! # Read once, and bounded
//!
//! One scan of the name records, then one point read per table and per sequence. Past
//! [`MAX_CATALOG_RELATIONS`] it is `53400` rather than an unbounded allocation on the client's
//! behalf — the rule `Sort`, the group table, a materialised join side and the savepoint block
//! already follow. A catalog scan is a scan.

use std::collections::BTreeMap;

use crate::backend::Txn;
use crate::catalog::{Relation, SequenceDef, TableDef};
use crate::error::{Result, SqlError};

/// Where a primary key's oid comes from, since it has no record of its own.
///
/// `1 << 62`. Relation ids come from a per-tenant sequence that starts at 1, so a tenant would have
/// to create four and a half quintillion relations to reach it, and the reserved view ids
/// (`crate::catalog::pg_catalog`) are far above it again. The value is **not** arbitrary in one
/// respect: it must stay inside `i64`, because `pg_class.oid` is a `bigint` here.
pub const PRIMARY_KEY_OID_BASE: u64 = 0x4000_0000_0000_0000;

/// The most relations one snapshot will hold before `53400`.
///
/// A real server's `pg_class` has some hundreds of system relations and a large schema has
/// thousands of user ones; this is generous for a tenant and finite, which is the property that
/// matters. `crate::exec::cursor` explains the rule this follows.
pub const MAX_CATALOG_RELATIONS: usize = 100_000;

/// What kind of relation a row is. PostgreSQL's `relkind`, as this node's catalog can spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    /// A table: `relkind` `r`.
    Table,
    /// An index: `relkind` `i`.
    Index,
    /// A primary key constraint, which **is** an index as far as a client can see: `relkind` `i`.
    ///
    /// Measured on 19beta1 — `r4a_pkey` is in `pg_class` with `relkind` `i` — and it is the right
    /// answer here even though the row key *is* the primary key and no separate index exists: what
    /// `relkind` describes is a relation a client can name, and a client can name `r4a_pkey`.
    PrimaryKey,
    /// A sequence: `relkind` `S`.
    Sequence,
    /// The index behind an `EXCLUDE` constraint: `relkind` `i`, and `pg_am` says `gist`.
    ///
    /// **Synthesised, not stored.** Every other relation here comes from a name record; this one
    /// is derived in [`Relations::read`] from the table's own `excludes`, because a real server
    /// makes an index relation for each exclusion constraint and a client names it — the capture
    /// reads `pg_get_indexdef('…_date_overlap'::regclass)`. No record format changes for it, and
    /// the constraint's oid is the index's, the arrangement a primary key already has.
    ///
    /// There is no `GiST` behind it: the constraint is enforced by a scan
    /// (`crate::exec::dml::check_exclusions`). What the catalog reports is what the constraint
    /// *is*, which is what a client reads it for.
    Exclusion,
}

impl RelKind {
    /// The one character `pg_class.relkind` carries.
    #[must_use]
    pub fn relkind(self) -> &'static str {
        match self {
            RelKind::Table => "r",
            RelKind::Index | RelKind::PrimaryKey | RelKind::Exclusion => "i",
            RelKind::Sequence => "S",
        }
    }
}

/// One relation, with everything the five views need to describe it.
#[derive(Debug, Clone)]
pub struct RelationRow {
    /// Its oid, from the table above. Equal in every view by construction.
    pub oid: i64,
    /// Its name, **bare** — the relation's own, with no schema on it. A relation in `public` is
    /// stored exactly this way, which is what keeps every answer about `public` unchanged.
    pub name: String,
    /// The schema it is in, `public` unless the stored name carried one.
    pub schema: String,
    /// What it is.
    pub kind: RelKind,
    /// The table it belongs to: itself for a table, the indexed table for an index, the keyed
    /// table for a primary key, the owning table for a sequence.
    pub table_id: u64,
    /// Which of [`TableDef::indexes`] it is, for an index.
    pub index_at: Option<usize>,
    /// Which of [`TableDef::excludes`] it is, for an exclusion constraint's index.
    pub exclude_at: Option<usize>,
    /// Which column it fills, for a sequence.
    pub column: Option<usize>,
}

/// Every relation of one tenant, and every table definition behind them.
#[derive(Debug, Default)]
pub struct Relations {
    /// In name order, which is the order the name scan returns them in.
    rows: Vec<RelationRow>,
    /// Every table, by id, so a view can reach its columns without a second read.
    tables: BTreeMap<u64, TableDef>,
}

impl Relations {
    /// Reads the tenant's whole catalog: one scan, then one point read per table and per sequence.
    pub fn read(txn: &dyn Txn, tenant: u64) -> Result<Relations> {
        let (start, end) = super::record::name_range(tenant);
        let mut rows = Vec::new();
        let mut tables = BTreeMap::new();
        for (key, value) in txn.scan(&start, &end, 0)? {
            if rows.len() >= MAX_CATALOG_RELATIONS {
                return Err(SqlError::ConfigurationLimitExceeded(format!(
                    "a catalog scan would hold more than {MAX_CATALOG_RELATIONS} relations"
                )));
            }
            let stored = super::record::name_of(tenant, &key)?;
            let relation = super::record::decode_relation(&value)?;
            rows.push(row_of(txn, tenant, &stored, relation, &mut tables)?);
        }
        // The `EXCLUDE` constraints' indexes, which have no name record of their own — see
        // [`RelKind::Exclusion`]. Appended after the scan and then re-sorted, so the whole list
        // stays in the name order every view reads it in.
        for table in tables.values() {
            for (at, exclude) in table.excludes.iter().enumerate() {
                rows.push(RelationRow {
                    oid: super::pg_constraint::exclude_oid(table.id, at),
                    name: exclude.name.clone(),
                    // Synthesised beside its table, so it is in the table's schema.
                    schema: super::split_qualified(&table.name).0.to_owned(),
                    kind: RelKind::Exclusion,
                    table_id: table.id,
                    index_at: None,
                    exclude_at: Some(at),
                    column: None,
                });
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Relations { rows, tables })
    }

    /// Every relation, in name order.
    pub fn rows(&self) -> impl Iterator<Item = &RelationRow> {
        self.rows.iter()
    }

    /// Every relation of one kind, in name order.
    pub fn of_kind(&self, kind: RelKind) -> impl Iterator<Item = &RelationRow> {
        self.rows.iter().filter(move |row| row.kind == kind)
    }

    /// Every table definition, in id order — for the views whose answer about one relation is a
    /// property of **all** of them. `pg_class.relhastriggers` is the first: a table is either side
    /// of a foreign key, and the referenced side is not written down on the table that is
    /// referenced.
    pub fn tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.values()
    }

    /// The table a row belongs to. `None` only for a record that names a table with no definition,
    /// which [`Relations::read`] already refused to build.
    #[must_use]
    pub fn table(&self, row: &RelationRow) -> Option<&TableDef> {
        self.tables.get(&row.table_id)
    }

    /// A table by its id, for the edges a `TableDef` records as ids rather than as rows — a
    /// partition's parent, whose name its inherited constraints are reported under.
    #[must_use]
    pub fn table_by_id(&self, table_id: u64) -> Option<&TableDef> {
        self.tables.get(&table_id)
    }

    /// The relation an oid names, if this tenant has one.
    #[must_use]
    pub fn by_oid(&self, oid: i64) -> Option<&RelationRow> {
        self.rows.iter().find(|row| row.oid == oid)
    }

    /// `obj_description(oid, …)`: the comment on the relation an oid names.
    ///
    /// **The catalog-name argument is not consulted**, because it filters `pg_description.classoid`
    /// on a real server and every relation here is in `pg_class` — so `'pg_class'` matches and
    /// anything else matches nothing, which the caller decides. NULL for an oid that names nothing
    /// is the caller's answer too: there is no not-found error anywhere in this surface.
    #[must_use]
    pub fn comment_of(&self, oid: i64) -> Option<&str> {
        let row = self.by_oid(oid)?;
        let table = self.table(row)?;
        match row.kind {
            RelKind::Table => table.comment.as_deref(),
            RelKind::PrimaryKey => table.primary_key_comment.as_deref(),
            RelKind::Index => table.indexes.get(row.index_at?)?.comment.as_deref(),
            // An `EXCLUDE` constraint's index is synthesised from the constraint and has no record
            // to keep a comment in; a sequence's record has no field for one either.
            RelKind::Exclusion | RelKind::Sequence => None,
        }
    }

    /// `col_description(oid, attnum)`: the comment on one column.
    ///
    /// **Attnum 0 is the table's own comment**, which is not a special case anywhere but here:
    /// `pg_description` keys a table comment as `objsubid = 0` and a real server's
    /// `col_description` does not filter it out. Measured. Everything else out of range — a
    /// negative attnum, one past the last column, an oid that is not a table — is NULL.
    #[must_use]
    pub fn column_comment(&self, oid: i64, attnum: i64) -> Option<&str> {
        let row = self.by_oid(oid)?;
        let table = self.table(row)?;
        if !matches!(row.kind, RelKind::Table) {
            return None;
        }
        if attnum == 0 {
            return table.comment.as_deref();
        }
        let at = usize::try_from(attnum - 1).ok()?;
        table.columns.get(at)?.comment.as_deref()
    }

    /// The relation a name names, if this tenant has one.
    #[must_use]
    /// A relation by the name it is **stored** under, which carries its schema.
    ///
    /// A bare name is `public`'s, so every existing caller keeps working unchanged; a qualified one
    /// finds the relation in its own schema and not a relation of that name somewhere else.
    pub fn by_name(&self, name: &str) -> Option<&RelationRow> {
        let (schema, bare) = super::split_qualified(name);
        self.rows
            .iter()
            .find(|row| row.name == bare && row.schema == schema)
    }
}

/// One name record, turned into a row — reading whatever second record its oid lives in.
fn row_of(
    txn: &dyn Txn,
    tenant: u64,
    stored: &str,
    relation: Relation,
    tables: &mut BTreeMap<u64, TableDef>,
) -> Result<RelationRow> {
    // **The stored name carries the schema and every view wants them apart**: `pg_class.relname`
    // is the bare one and `relnamespace` is the other half. A relation in `public` has no
    // separator, so both come back exactly as they always did.
    let (schema, name) = super::split_qualified(stored);
    let (schema, name) = (schema.to_owned(), name.to_owned());
    Ok(match relation {
        Relation::Table { table_id } => {
            load_table(txn, tenant, table_id, tables)?;
            RelationRow {
                oid: as_oid(table_id),
                name: name.clone(),
                schema: schema.clone(),
                kind: RelKind::Table,
                table_id,
                index_at: None,
                exclude_at: None,
                column: None,
            }
        }
        Relation::Index { table_id, index_id } => {
            let table = load_table(txn, tenant, table_id, tables)?;
            let index_at = table.indexes.iter().position(|index| index.id == index_id);
            RelationRow {
                oid: as_oid(index_id),
                name: name.clone(),
                schema: schema.clone(),
                kind: RelKind::Index,
                table_id,
                index_at,
                exclude_at: None,
                column: None,
            }
        }
        Relation::PrimaryKey { table_id } => {
            load_table(txn, tenant, table_id, tables)?;
            RelationRow {
                // Derived: a primary key has no record of its own to carry one. See the module
                // note — it was the table's own id until this module, which made two rows of
                // `pg_class` join as one.
                oid: as_oid(PRIMARY_KEY_OID_BASE.wrapping_add(table_id)),
                name: name.clone(),
                schema: schema.clone(),
                kind: RelKind::PrimaryKey,
                table_id,
                index_at: None,
                exclude_at: None,
                column: None,
            }
        }
        Relation::Sequence {
            table_id,
            sequence_id,
        } => {
            // A sequence no column owns has no table to load, and `pg_class` still lists it.
            if table_id != crate::catalog::STANDALONE_SEQUENCE_OWNER {
                load_table(txn, tenant, table_id, tables)?;
            }
            // **The sequence's own id, which the name record now carries.** It used to live only
            // in the sequence record, so `pg_class` reported the table's id instead and every
            // `bigserial` table had two relations with one oid; the version 15 name entry holds
            // the id where the column ordinal used to be.
            let id = sequence_id;
            RelationRow {
                oid: as_oid(id),
                name: name.clone(),
                schema: schema.clone(),
                kind: RelKind::Sequence,
                table_id,
                index_at: None,
                exclude_at: None,
                column: None,
            }
        }
    })
}

/// The table record, read once per snapshot however many relations point at it.
fn load_table<'a>(
    txn: &dyn Txn,
    tenant: u64,
    table_id: u64,
    tables: &'a mut BTreeMap<u64, TableDef>,
) -> Result<&'a TableDef> {
    if let std::collections::btree_map::Entry::Vacant(slot) = tables.entry(table_id) {
        let Some(bytes) = txn.get(&super::record::table_key(tenant, table_id))? else {
            // A name points at a table whose record is not there. The two keys are written by one
            // transaction, so this is corruption rather than a missing table — the same reading
            // `Executor::table_by_id` takes.
            return Err(SqlError::DataCorrupted(format!(
                "a name points at table {table_id}, which is not there"
            )));
        };
        let mut table = super::record::decode_table(&bytes)?;
        table.sequences = crate::catalog::table_sequences(txn, tenant, table_id)?;
        let inherited =
            crate::catalog::inherited_sequences(txn, tenant, &table, &table.parents.clone())?;
        table.sequences.extend(inherited);
        table.child_scans = crate::catalog::child_scans(txn, tenant, &table)?;
        slot.insert(table);
    }
    tables
        .get(&table_id)
        .ok_or_else(|| SqlError::Internal("a table record that was just inserted is gone".into()))
}

/// A relation id as `pg_class.oid` carries it.
///
/// A real server's `oid` is 32 bits and ours are `u64`, so the column is a `bigint` — the trade
/// `pg_type.oid` already makes and `tests/pg_catalog.rs` already declares. Saturating rather than
/// wrapping: two relations sharing an oid is the failure this whole module exists to prevent, and
/// a saturated one is at least visibly wrong.
pub(super) fn as_oid(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

/// The table a primary key's oid belongs to, or `None` for an oid that is not one.
#[must_use]
pub fn primary_key_table(oid: i64) -> Option<u64> {
    let oid = u64::try_from(oid).ok()?;
    oid.checked_sub(PRIMARY_KEY_OID_BASE)
}

/// The `attnum` a column has: its position among the columns a **user** can see, one-based.
///
/// Not its position in [`TableDef::columns`], and the difference is a whole column: a table with no
/// declared primary key carries an internal row id in slot 0 that `user_columns` hides
/// (`crate::catalog::INTERNAL_ROW_ID_NAME`). `pg_attribute` numbers from the visible list, so
/// anything that reports an attnum — `pg_index.indkey`, `pg_constraint.conkey`,
/// `information_schema.key_column_usage.ordinal_position` — has to number from the same one or the
/// join `a.attnum = ANY(i.indkey)` matches the wrong column. Measured: `ib (a int4, b text)` has
/// `indkey` `1` for an index on `a`, and this node stores `a` at slot 1.
///
/// One function, because two of them would be two chances to disagree.
#[must_use]
pub fn attnum_of(table: &TableDef, at: usize) -> i16 {
    let hidden = usize::from(table.row_id().is_some());
    i16::try_from(at.saturating_sub(hidden) + 1).unwrap_or(i16::MAX)
}

/// One sequence, by the column it fills, for the `nextval` a `pg_attrdef` row prints.
#[must_use]
pub fn sequence_for(table: &TableDef, column: usize) -> Option<&SequenceDef> {
    table
        .sequences
        .iter()
        .find(|sequence| sequence.column == Some(column))
}
