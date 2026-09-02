//! One row's place in one index — **the** answer, computed once for every writer.
//!
//! Four places build index entries: an insert, a delete, the backfill inside `CREATE INDEX`, and
//! the staged job that backfills a `CONCURRENTLY` one. They have to agree on every byte, because
//! an entry written by one and deleted by another that disagreed would be a key nothing ever
//! removes; and they have to agree on *whether there is an entry at all*, because a partial index
//! holds one only for the rows its predicate admits.
//!
//! They did not. The predicate was checked in the two `crate::exec::dml` paths and in neither
//! backfill, so `CREATE UNIQUE INDEX … WHERE …` over a table that already had rows indexed the
//! rows the predicate excludes — and refused the statement with `23505` for a duplicate among
//! them. That is why this module exists rather than a fifth copy: the rule is written once and
//! the four callers cannot drift from it.

use crate::backend::Txn;
use crate::catalog::{IndexDef, IndexKey, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::{cursor, query};
use crate::row;
use crate::value::{Datum, PgDatum as _};

/// One row's entry in one index.
pub(super) struct Entry {
    /// The encoded key.
    pub key: Vec<u8>,
    /// The key's values, in key order — what a `23505` `DETAIL` prints.
    pub values: Vec<Datum>,
    /// Whether the key alone identifies the entry, which is what makes a duplicate a collision on
    /// one key. False for a non-unique index and for a unique one with a NULL in its key, both of
    /// which carry the primary key as a suffix (`crate::row`).
    pub by_value: bool,
}

/// The entry this row has in this index, or `None` when the index excludes it.
///
/// Excluded means one thing only: a **partial** index whose predicate the row does not make true.
/// The schema state is the caller's to check, because the three callers check different states —
/// a writer inserts one state later than a deleter removes (ADR 0020).
pub(super) fn entry(
    tenant: u64,
    table: &TableDef,
    index: &IndexDef,
    row: &[Datum],
    primary_key: &[Datum],
) -> Result<Option<Entry>> {
    if !admits(table, index, row)? {
        return Ok(None);
    }
    let values = key_values(table, index, row)?;
    let by_value = index.unique && row::unique_index_key_is_unique_by_value(&values);
    let suffix = if by_value { None } else { Some(primary_key) };
    let key = row::index_key(tenant, table.id, index.id, &values, suffix)?;
    Ok(Some(Entry {
        key,
        values,
        by_value,
    }))
}

/// Whether a partial index holds an entry for this row.
///
/// True for an index with no predicate, and for one whose predicate is **true** of the row. A
/// NULL keeps the row out: `WHERE published_on IS NOT NULL` admits only rows where the predicate
/// is true, and unknown is not true. That is the mirror of a `CHECK`, which admits everything the
/// predicate does not make *false* — the two rules look alike and point opposite ways, which is
/// why each says so where it is written.
pub(super) fn admits(table: &TableDef, index: &IndexDef, row: &[Datum]) -> Result<bool> {
    let Some(predicate) = &index.predicate else {
        return Ok(true);
    };
    Ok(matches!(
        evaluate_stored(table, index, predicate, row)?,
        Datum::Bool(true)
    ))
}

/// The key's values for this row: a column's value, or an expression's.
///
/// The expression is re-lowered from its stored text per row, which is the trade
/// `crate::exec::dml::check_constraints` already makes and for the same reason: the catalog
/// caches a `TableDef`, and a lowered expression cached beside it would have to be invalidated
/// with it. Re-lowering a short expression per row is the cheaper mistake, and the only one that
/// cannot go stale.
fn key_values(table: &TableDef, index: &IndexDef, row: &[Datum]) -> Result<Vec<Datum>> {
    index
        .keys
        .iter()
        .map(|key| match key {
            IndexKey::Column(at) => Ok(row[*at].clone()),
            IndexKey::Expression { expr, .. } => evaluate_stored(table, index, expr, row),
        })
        .collect()
}

/// One stored expression of an index, lowered against the table and evaluated over the row.
///
/// A failure here is **internal**, not the user's: the text came out of the catalog, and this
/// crate put it there after resolving it against the same table
/// (`crate::exec::ddl::create_index`). Saying so names the index rather than reporting a `42703`
/// at a write, which would blame the statement for a record that no longer parses.
fn evaluate_stored(table: &TableDef, index: &IndexDef, expr: &str, row: &[Datum]) -> Result<Datum> {
    let internal = |what: &str, error: SqlError| {
        SqlError::Internal(format!(
            "the stored {what} of index {} no longer resolves: {error}",
            index.name
        ))
    };
    let parsed =
        crate::parse::parse_stored_expr(expr).map_err(|error| internal("expression", error))?;
    let scope = query::Scope::single(table);
    let resolved =
        query::resolve(&parsed, &scope).map_err(|error| internal("expression", error))?;
    cursor::evaluate(&resolved, row)
}

/// `Key (a, lower(b))=(1, x)`, PostgreSQL's `DETAIL` for a uniqueness failure.
///
/// An expression key part prints **as the expression**, which is what a real server does:
/// `Key (lower(b))=(alpha) already exists.` Measured — and the form it prints is the one
/// `pg_get_expr` gives, not the one that goes in the key list, so the parentheses here are one
/// pair fewer than in `pg_get_indexdef` ([`crate::catalog::pg_index`]).
pub(super) fn render_key(table: &TableDef, keys: &[IndexKey], values: &[Datum]) -> String {
    let names: Vec<String> = keys
        .iter()
        .map(|key| match key {
            IndexKey::Column(at) => table.columns[*at].name.clone(),
            IndexKey::Expression { expr, shape, .. } => shape.printed(expr),
        })
        .collect();
    format!("Key ({})=({})", names.join(", "), render_values(values))
}

/// Values joined with `, `, a NULL written `null`, nothing quoted.
///
/// Nothing is quoted or escaped, which is PostgreSQL's own behaviour and not a shortcut: a text
/// value containing `, y)` really does come back as `Key (a, b)=(1, x, y))`. Copied exactly.
pub(super) fn render_values(values: &[Datum]) -> String {
    values
        .iter()
        .map(|value| value.to_text().unwrap_or_else(|| "null".to_owned()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Removes this row's entry from every index that holds one and is being maintained.
///
/// The state check is the caller's above; here it is one state — `maintained` — because every
/// caller that removes checks the same one.
pub(super) fn remove_entries(
    tenant: u64,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
    primary_key: &[Datum],
) -> Result<()> {
    for index in &table.indexes {
        // **Delete-only removes, and that is one state earlier than write-only inserts.** The
        // asymmetry is the whole reason there are four states rather than three: every node has
        // to be removing entries before any node starts creating them, or a node still at
        // `Absent` deletes a row and leaves behind an entry that a scan will later return as a
        // row the table does not contain (ADR 0020, "skip delete-only").
        //
        // Deleting an entry that is not there costs one tombstone and is correct, which is what
        // makes "remove first, ask later" affordable — **for a whole index**. For a *partial* one
        // it is not: two rows may share an index key when only one of them is in the index, and
        // deleting on behalf of the one that is out would delete the entry belonging to the one
        // that is in. That is why `entry` answers `None` rather than a key here, and it is the
        // only place in this crate where "delete blindly" is wrong.
        if !index.state.maintained() {
            continue;
        }
        if let Some(entry) = entry(tenant, table, index, row, primary_key)? {
            txn.delete(&entry.key);
        }
    }
    Ok(())
}
