//! The one write to a system catalog this node takes: `pg_constraint.convalidated` —
//! [ADR 0113](../../../../docs/adr/0113-plpgsql-is-the-subset-the-suite-sends.md), ruled (b) by the
//! user on 2026-09-13.
//!
//! **Every other write to a catalog is `42501`** (`catalog::pg_catalog::refuse_write`), because this
//! node's catalog is computed from records and has no rows to change. This one is taken because
//! `check_all_foreign_keys_valid!` cannot work without it — `ALTER TABLE … VALIDATE CONSTRAINT` does
//! nothing to a constraint already validated, measured — and because what it writes is a state the
//! records already hold: a foreign key or a `CHECK` that is `NOT VALID`.
//!
//! So the statement is read for exactly that shape. Another column, a value that is not a boolean
//! constant, a `FROM`, a `RETURNING`, or a selected row whose constraint stores no flag — a primary
//! key, a `UNIQUE`, a `NOT NULL`, an exclusion — answers the refusal it always had.

use super::Executor;
use crate::backend::Txn;
use crate::catalog::pg_catalog::{self, CatalogView};
use crate::catalog::pg_constraint::{ValidatedFlag, validated_flag_of};
use crate::error::{Result, SqlError};
use crate::pgwire::session::Outcome;
use crate::plan::{Expr, Literal, Statement, Update};
use crate::value::Datum;

/// `UPDATE pg_catalog.pg_constraint SET convalidated = <true | false> WHERE …`, run; or `None` for
/// any other statement, which the caller refuses as it refuses every write to a catalog.
///
/// **The rows are chosen by the planner a `SELECT` of the view uses**, with the statement's own
/// predicate, so the `WHERE` means here what it means everywhere else. Each selected constraint's
/// flag is written into its table's record with the schema version moved, as `VALIDATE CONSTRAINT`
/// writes it, and the tag counts the constraints written.
pub(super) fn update_convalidated(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    update: &Update,
) -> Result<Option<Outcome>> {
    if pg_catalog::view(&update.table) != Some(CatalogView::PgConstraint) {
        return Ok(None);
    }
    let [(column, Expr::Literal(Literal::Bool(validated)))] = update.assignments.as_slice() else {
        return Ok(None);
    };
    if column != "convalidated" || update.from.is_some() || !update.joins.is_empty() {
        return Ok(None);
    }
    if update.returning.is_some() {
        return Ok(None);
    }
    let parsed = crate::parse::parse_statements("SELECT oid FROM pg_catalog.pg_constraint")?;
    let Some(Statement::Select(mut select)) = parsed
        .first()
        .map(crate::parse::Parsed::lower)
        .transpose()?
    else {
        return Err(SqlError::Internal(
            "the pg_constraint query did not lower to a SELECT".to_owned(),
        ));
    };
    if let Some(from) = &mut select.from {
        from.alias.clone_from(&update.alias);
    }
    select.filter.clone_from(&update.filter);
    let (_, rows) = executor.planned_rows(txn, &select)?;
    let mut flags = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(Datum::Int8(oid)) = row.first() else {
            return Err(SqlError::Internal(
                "a pg_constraint row with no oid".to_owned(),
            ));
        };
        match validated_flag_of(*oid) {
            Some(flag) => flags.push(flag),
            // A constraint that stores no flag has nothing this write could change.
            None => return pg_catalog::refuse_write(&update.table).map(|()| None),
        }
    }
    // **Before the first write**, as `run_recording` marks any DDL: from here on this transaction
    // reads its own catalog and publishes none of it.
    executor.catalog_written = true;
    executor.forget_the_catalog_version();
    for flag in &flags {
        let (ValidatedFlag::ForeignKey { table, .. } | ValidatedFlag::Check { table, .. }) = *flag;
        let Some(previous) = executor.catalog_view(&*txn)?.table_by_id(table)? else {
            return Err(SqlError::Internal(format!(
                "pg_constraint named table {table}, which is not there"
            )));
        };
        let mut updated = (*previous).clone();
        let slot = match *flag {
            ValidatedFlag::ForeignKey { at, .. } => updated
                .foreign_keys
                .get_mut(at)
                .map(|key| &mut key.validated),
            ValidatedFlag::Check { at, .. } => {
                updated.checks.get_mut(at).map(|check| &mut check.validated)
            }
        };
        let Some(slot) = slot else {
            return Err(SqlError::Internal(format!(
                "pg_constraint named a constraint table {table} does not have"
            )));
        };
        *slot = *validated;
        updated.schema_version += 1;
        crate::catalog::replace_table(txn, executor.tenant, &previous, &updated)?;
    }
    Ok(Some(Outcome::done(format!("UPDATE {}", flags.len()))))
}
