//! `pg_index`, computed from the index definitions — and the primary key, which has none.
//!
//! What `ActiveRecord`'s `indexes()` reads, joined to `pg_class` twice:
//!
//! ```sql
//! SELECT DISTINCT i.relname, d.indisunique, d.indkey, pg_get_indexdef(d.indexrelid),
//!        pg_catalog.obj_description(i.oid, 'pg_class'), d.indisvalid, ARRAY(…) AS columns
//!   FROM pg_class t
//!   INNER JOIN pg_index d ON t.oid = d.indrelid
//!   INNER JOIN pg_class i ON d.indexrelid = i.oid
//!   LEFT JOIN pg_namespace n ON n.oid = t.relnamespace
//!  WHERE i.relkind IN ('i', 'I') AND d.indisprimary = 'f'
//!    AND t.relname = $table AND n.nspname = ANY (current_schemas(false))
//!  ORDER BY i.relname
//! ```
//!
//! Three oids in one statement — `t.oid`, `d.indrelid`, `d.indexrelid`, `i.oid` — which is why
//! [`crate::catalog::pg_relations`] exists and why nothing here computes one.
//!
//! # What the capture settled
//!
//! * **A primary key has a `pg_index` row**, `indisprimary` and `indisunique` both `t`, and its
//!   `indexrelid` is *not* its `indrelid`. Here the row key **is** the primary key and there is no
//!   separate index to describe — and the row is still the right answer, for the reason
//!   `pg_class` reports the constraint as a relation: a client can name `t_pkey`, and every
//!   statement `ActiveRecord` writes assumes it can find it.
//! * **`indkey` is an `int2vector`, and it prints space-separated**: a two-column index is `3 4`.
//!   `ActiveRecord` reads it with `row[2].split(" ").map(&:to_i)` — a *text* operation on the
//!   rendered value — so `text` here carries the same characters and means the same thing. That is
//!   the rule this phase follows for a type it does not have: the column is provided where the
//!   client's use of it is text-shaped, and refused where the use needs the array type
//!   (`pg_constraint.conkey`, which is subscripted in SQL).
//! * **`pg_get_indexdef` never raises.** An oid with no index behind it is **NULL** — including
//!   the oid of a *table*, which is the case a caller is most likely to hit — and so is a NULL
//!   argument.
//! * **The three-argument form is three different functions.** `pg_get_indexdef(oid, 0, true)` is
//!   the whole definition **without** the `public.` qualifier the one-argument form prints;
//!   `(oid, n, …)` for `n >= 1` is the *column name alone*; and past the last column it is the
//!   **empty string**, not NULL and not an error.

use crate::backend::Txn;
use crate::catalog::pg_relations::{RelKind, RelationRow, Relations};
use crate::catalog::{SchemaState, TableDef};
use crate::error::Result;
use crate::value::{ColumnType, Datum};

/// Every `pg_index` row this tenant has: one per index, and one per primary key.
pub fn rows(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for relation in relations.rows() {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        let Some(key) = key_of(relation, table) else {
            continue;
        };
        rows.push(vec![
            Datum::Int8(relation.oid),
            Datum::Int8(oid_of_table(&relations, relation.table_id)),
            Datum::Int2(i16::try_from(key.columns.len()).unwrap_or(i16::MAX)),
            Datum::Bool(key.unique),
            // `NULLS NOT DISTINCT` is a clause this node's `CREATE INDEX` refuses by name, so no
            // index here has it. Measured: the default is `f` and the clause prints after the
            // column list when it is `t`.
            Datum::Bool(false),
            Datum::Bool(key.primary),
            Datum::Bool(key.valid),
            Datum::Text(key.indkey(table)),
            // No expression indexes and no partial indexes: both are `0A000` in
            // `parse::lower::index_columns`, so there is nothing to print and NULL is what a
            // catalog with no such index says.
            Datum::Null,
            Datum::Null,
        ]);
    }
    Ok(rows)
}

/// `pg_get_indexdef(indexrelid [, column, pretty])`.
///
/// NULL for an oid that names no index — a table's oid included, measured — which is what makes it
/// safe to call over a column of oids nobody has filtered.
#[must_use]
pub fn index_definition(relations: &Relations, oid: Option<i64>, column: Option<i32>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    let Some(relation) = relations.by_oid(oid) else {
        return Datum::Null;
    };
    let Some(table) = relations.table(relation) else {
        return Datum::Null;
    };
    let Some(key) = key_of(relation, table) else {
        return Datum::Null;
    };
    let names: Vec<&str> = key
        .columns
        .iter()
        .filter_map(|at| table.columns.get(*at).map(|column| column.name.as_str()))
        .collect();
    match column {
        // One column of it, by position, and the **empty string** past the last one. Measured:
        // `pg_get_indexdef(ix, 3, true)` over a two-column index is `''`, not NULL.
        Some(at) if at >= 1 => Datum::Text(
            usize::try_from(at - 1)
                .ok()
                .and_then(|at| names.get(at))
                .map(|name| (*name).to_owned())
                .unwrap_or_default(),
        ),
        // Column 0 is the whole definition **unqualified** — the one difference between the two
        // forms, and it is not decoration: `ON ia` rather than `ON public.ia`.
        Some(_) => Datum::Text(definition(
            &relation.name,
            &table.name,
            &names,
            key.unique,
            false,
        )),
        None => Datum::Text(definition(
            &relation.name,
            &table.name,
            &names,
            key.unique,
            true,
        )),
    }
}

/// `CREATE [UNIQUE ]INDEX <name> ON [public.]<table> USING btree (<columns>)`.
///
/// Every index here is a btree, which is not a simplification: `USING` is refused by name in the
/// DDL, so btree is the only access method this node has and naming another would be a claim.
fn definition(name: &str, table: &str, columns: &[&str], unique: bool, qualified: bool) -> String {
    format!(
        "CREATE {}INDEX {name} ON {}{table} USING btree ({})",
        if unique { "UNIQUE " } else { "" },
        if qualified { "public." } else { "" },
        columns.join(", ")
    )
}

/// One index's key, however it is stored.
///
/// A primary key and an index are two records of different shapes describing the same thing, and
/// every column of `pg_index` is a function of this — so they are read into one and the row is
/// built once.
struct Key<'a> {
    columns: &'a [usize],
    unique: bool,
    primary: bool,
    valid: bool,
}

impl Key<'_> {
    /// `indkey`: the attribute numbers, space-separated — an `int2vector`'s own text.
    ///
    /// The numbers are `pg_attribute`'s, not positions in `TableDef::columns`
    /// ([`super::pg_relations::attnum_of`]): a keyless table hides a column in slot 0, and
    /// numbering from there would make `a.attnum = ANY(i.indkey)` name the column after the one
    /// the index is on.
    fn indkey(&self, table: &TableDef) -> String {
        self.columns
            .iter()
            .map(|at| super::pg_relations::attnum_of(table, *at).to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The key a relation is, or `None` for one that is not an index at all.
fn key_of<'a>(relation: &RelationRow, table: &'a TableDef) -> Option<Key<'a>> {
    match relation.kind {
        RelKind::Index => {
            let index = table.indexes.get(relation.index_at?)?;
            Some(Key {
                columns: &index.columns,
                unique: index.unique,
                primary: false,
                // **`indisvalid` is the schema state, read honestly.** An index that is not
                // `Public` is one no reader may use (ADR 0020), and `indisvalid` is exactly the
                // column a client checks before trusting one — `ActiveRecord`'s `indexes()`
                // selects it. Reporting `t` for a half-built index would be the wrong answer in
                // the one place a client asked the right question.
                valid: index.state == SchemaState::Public,
            })
        }
        RelKind::PrimaryKey => Some(Key {
            columns: &table.primary_key,
            unique: true,
            primary: true,
            valid: true,
        }),
        RelKind::Table | RelKind::Sequence => None,
    }
}

/// A table's own oid, out of the same snapshot — never recomputed.
fn oid_of_table(relations: &Relations, table_id: u64) -> i64 {
    relations
        .rows()
        .find(|row| row.kind == RelKind::Table && row.table_id == table_id)
        .map_or(0, |row| row.oid)
}

/// The columns of `pg_index`, in PostgreSQL's own order.
pub const INDEX_COLUMNS: &[(&str, ColumnType)] = &[
    ("indexrelid", ColumnType::Int8),
    ("indrelid", ColumnType::Int8),
    ("indnatts", ColumnType::Int2),
    ("indisunique", ColumnType::Bool),
    ("indnullsnotdistinct", ColumnType::Bool),
    ("indisprimary", ColumnType::Bool),
    ("indisvalid", ColumnType::Bool),
    ("indkey", ColumnType::Text),
    ("indexprs", ColumnType::Text),
    ("indpred", ColumnType::Text),
];
