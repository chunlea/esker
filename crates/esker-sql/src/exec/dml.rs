//! `INSERT`: a row's worth of values into a row's worth of keys.
//!
//! One row becomes one key/value pair plus one entry per index, all written into the caller's
//! transaction. Nothing here can fail at the write — `Txn::put` buffers (`crate::backend`) — so the
//! checks that *can* fail all happen before it: the column list resolves, every value assigns to
//! its column's type, no `NOT NULL` column is left NULL, and every unique index key is read and
//! required absent.
//!
//! # The two ways a duplicate arrives, and where each is caught
//!
//! `docs/plans/phase-6a.md` §5 rules that uniqueness needs no new storage primitive. Here is the
//! half of that ruling this file implements:
//!
//! 1. **Already committed.** The read below is at the transaction's snapshot, so an entry another
//!    transaction committed is visible, and `23505` is raised before anything is written.
//! 2. **Concurrent.** Two transactions both read the key as absent and both write it. Neither can
//!    see the other, and no read here could have caught it. Percolator's write-write conflict
//!    detection lets exactly one commit, and the loser is told `40001` — which `crate::exec` turns
//!    back into `23505`, because it recorded that the key was a unique index entry.
//!
//! The primary key is the same rule with no index behind it: two rows with one key *are* one key,
//! so the row key itself is read and required absent, and the constraint named is the table's
//! `_pkey`.

use crate::backend::Txn;
use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::exec::{Executor, Unique, Written};
use crate::exec::{cursor, query};
use crate::pgwire::message::FieldDescription;
use crate::pgwire::session::Outcome;
use crate::plan::{Delete, Insert, Returning, Update};
use crate::row;
use crate::value::ColumnType;
use crate::value::Datum;
use crate::value::{PgDatum, PgType};

/// The rows a statement's `RETURNING` will answer with, gathered as the statement writes them.
///
/// `None` from [`Returned::open`] is the ordinary case — no `RETURNING` — and then nothing here
/// runs and the statement answers with its tag alone.
///
/// **The rows are gathered as they are written, not read back afterwards.** Reading them back
/// would be a second pass over keys the transaction has already touched, and for a `DELETE` there
/// would be nothing left to read; more importantly it would answer with what a *later* statement
/// could see rather than with what this one did, which is not what `RETURNING` means.
struct Returned {
    columns: Vec<(String, ColumnType, i32)>,
    exprs: Vec<crate::plan::Expr>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

impl Returned {
    /// Resolves the target list against the table, or `None` when the statement has no
    /// `RETURNING`. Resolution happens **before** the first row is written, so `RETURNING nope`
    /// is `42703` with nothing written rather than after half the statement has run.
    fn open(returning: Option<&Returning>, table: &TableDef) -> Result<Option<Self>> {
        let Some(items) = returning else {
            return Ok(None);
        };
        let (columns, exprs) = query::returning_columns(items, table)?;
        Ok(Some(Returned {
            columns,
            exprs,
            rows: Vec::new(),
        }))
    }

    /// One row, as the statement leaves it.
    fn push(&mut self, row: &[Datum]) -> Result<()> {
        let values = self
            .exprs
            .iter()
            .map(|expr| {
                cursor::evaluate(expr, row).map(|value| value.to_text().map(String::into_bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        self.rows.push(values);
        Ok(())
    }
}

/// The statement's answer: its rows and its tag, or its tag alone.
///
/// The tag is **the same either way**. `INSERT 0 2` is what a real server sends whether or not the
/// statement had a `RETURNING`, because the tag counts rows written and the result set is a second
/// thing the statement produced rather than a different thing it did.
fn finish(returned: Option<Returned>, tag: String) -> Outcome {
    match returned {
        None => Outcome::done(tag),
        Some(returned) => Outcome::Rows {
            fields: returned
                .columns
                .iter()
                .map(|(name, ty, typmod)| FieldDescription::of(name.clone(), *ty, *typmod))
                .collect(),
            rows: returned.rows,
            tag,
        },
    }
}

/// A sequence's next value, as the column that takes it.
///
/// A sequence counts in `i64` whatever width it fills, so an `integer` identity column narrows
/// here — and a sequence that has run past 2^31 answers the same `22003` a constant that far out
/// would, which is what a real server does when a `serial` runs out rather than wrapping.
/// Every value of a row as its column's typmod requires it: `varchar(n)` refused, `character(n)`
/// padded, `timestamp(p)` rounded.
///
/// Applied to the **whole row** just before it is written, rather than where each value is
/// produced, and that is deliberate: a row reaches this point from four directions — a literal in
/// a `VALUES` list, an expression in a `SET`, a column's `DEFAULT` from the catalog, and a
/// sequence — and only one of them passes through anything that knows the column's type. Padding
/// three of the four and forgetting the fourth would store a `char(3)` holding `x` beside one
/// holding `x  `, which compare equal to PostgreSQL and not to a byte comparison, and the row key
/// built from them would be two different keys for one value.
fn fit_typmods(table: &TableDef, row: &mut [Datum]) -> Result<()> {
    for (value, column) in row.iter_mut().zip(&table.columns) {
        if column.typmod == crate::value::NO_TYPMOD {
            continue;
        }
        let taken = std::mem::replace(value, Datum::Null);
        *value = crate::value::fit_to_typmod(taken, column.ty, column.typmod)?;
    }
    Ok(())
}

/// What a column writes when an `INSERT` omits it, or a `SET c = DEFAULT` names it.
///
/// `now` is the **transaction's** timestamp, not the statement's and not a wall-clock reading: it
/// is what `CURRENT_TIMESTAMP` means on a real server — constant within a transaction, equal to
/// `now()` — and it is the only clock this node is allowed (`CLAUDE.md` invariant 6). Two columns
/// defaulting to `CURRENT_TIMESTAMP` in one `INSERT` therefore hold the same instant, which is a
/// thing the capture checks (`a = b` is `t`).
///
/// A `timestamptz` column takes it as it is and a `timestamp` column takes the same number: this
/// node stores both as microseconds from 2000-01-01 UTC, and the assignment cast a real server
/// applies here is a zone conversion that is the identity at UTC.
fn column_default_value(column: &crate::catalog::ColumnDef, now: i64) -> Datum {
    if column.default_now {
        return match column.ty {
            ColumnType::TimestampTz => Datum::TimestampTz(now),
            _ => Datum::Timestamp(now),
        };
    }
    column.default.clone().unwrap_or(Datum::Null)
}

fn sequence_datum(ty: ColumnType, value: i64) -> Result<Datum> {
    Ok(match ty {
        ColumnType::Int4 => Datum::Int4(
            i32::try_from(value).map_err(|_| SqlError::IntegerLiteralOutOfRange(ty.name()))?,
        ),
        ColumnType::Int2 => Datum::Int2(
            i16::try_from(value).map_err(|_| SqlError::IntegerLiteralOutOfRange(ty.name()))?,
        ),
        _ => Datum::Int8(value),
    })
}

pub(super) fn insert(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    insert: &Insert,
    written: &mut Written,
) -> Result<Outcome> {
    crate::catalog::pg_catalog::refuse_write(&insert.table)?;
    let table = executor.require_table(txn, &insert.table)?;
    let targets = target_columns(&table, insert)?;
    let mut returned = Returned::open(insert.returning.as_ref(), &table)?;

    for values in &insert.rows {
        if values.len() > targets.len() {
            // PostgreSQL calls this a syntax error, oddly enough, and says so before it looks at
            // any of the values.
            return Err(SqlError::InsertTooManyExpressions);
        }

        // Every column starts at its **default**, which is NULL unless one was declared:
        // `INSERT INTO t (a) VALUES (1)` leaves the rest at theirs, and so does a `VALUES` tuple
        // shorter than the column list.
        //
        // The default and the *missing* value are different fields and this is the one that reads
        // the default (`crate::catalog::ColumnDef`). A row written now is written at full width,
        // so nothing about it is missing; the other field answers for rows that predate the column.
        let now = crate::time_machine::micros_of_ts(txn.start_ts());
        let mut row: Vec<Datum> = table
            .columns
            .iter()
            .map(|column| column_default_value(column, now))
            .collect();
        for (target, expr) in targets.iter().zip(values) {
            let column = &table.columns[*target];
            // `DEFAULT` written for a column is the column keeping its own default, which is what
            // the row already holds — including, below, its sequence. It is *not* an explicit
            // value, so a `GENERATED ALWAYS` column takes it: measured, `VALUES (DEFAULT, …)` into
            // one is accepted where `VALUES (7, …)` is `428C9`.
            if matches!(expr, crate::plan::Expr::Default) {
                continue;
            }
            // `GENERATED ALWAYS` refuses a value the user wrote, and names the clause that
            // overrides it — the whole of the difference between the three identity kinds
            // (`crate::catalog::Identity`), measured on all three.
            if let Some(sequence) = table.sequence_for(*target)
                && sequence.identity.refuses_explicit()
            {
                return Err(SqlError::GeneratedAlways {
                    column: column.name.clone(),
                });
            }
            row[*target] = expr.evaluate(column.ty, &column.name)?;
        }
        // A sequence fills its column when the statement did not name it, or named it and wrote
        // `DEFAULT`. It runs **after** the values, so a `bigserial` the user did write keeps their
        // number and does not consume one — which is what a real server does, and the reason the
        // next insert can collide with it.
        for sequence in &table.sequences {
            if targets
                .iter()
                .take(values.len())
                .position(|at| *at == sequence.column)
                .is_some_and(|at| !matches!(values[at], crate::plan::Expr::Default))
            {
                continue;
            }
            // Narrowed to the column's own width. A sequence counts in `i64` whatever it fills,
            // so an `integer` identity column has to be told — and running past 2^31 is the same
            // `22003` a constant that far out gets, which is what a real server answers when a
            // `serial` runs out.
            row[sequence.column] = sequence_datum(
                table.columns[sequence.column].ty,
                executor.next_sequence_value(sequence.id)?,
            )?;
        }
        // A table with no declared key carries an internal row id the user cannot write, so the
        // executor fills it (`crate::catalog::TableDef::row_id`).
        if let Some(at) = table.row_id() {
            row[at] = Datum::Int8(executor.next_row_id(table.id)?);
        }

        fit_typmods(&table, &mut row)?;
        check_not_null(&table, &row)?;
        check_constraints(&table, &row)?;
        write_row(executor, txn, &table, &row, written)?;
        // The row **as stored**, so a column filled from its `DEFAULT` comes back with that value
        // rather than with the NULL the user did not write.
        if let Some(returned) = &mut returned {
            returned.push(&row)?;
        }
    }

    // The leading zero is the OID of the inserted row, which PostgreSQL stopped assigning in 8.1
    // and still reports as 0. A client that parses the tag expects three fields.
    Ok(finish(returned, format!("INSERT 0 {}", insert.rows.len())))
}

/// Which column each value in a `VALUES` tuple is for.
fn target_columns(table: &TableDef, insert: &Insert) -> Result<Vec<usize>> {
    let Some(names) = &insert.columns else {
        // The user's columns, in order. An internal row id is not one of them: `INSERT INTO t
        // VALUES (1, 2)` fills the two columns the user declared.
        return Ok(table.user_columns().map(|(at, _)| at).collect());
    };
    names
        .iter()
        .map(|name| {
            table
                .column(name)
                .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                    column: name.clone(),
                    relation: table.name.clone(),
                })
        })
        .collect()
}

/// Writes one row and its index entries, checking every uniqueness constraint on the way.
pub(super) fn write_row(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
    written: &mut Written,
) -> Result<()> {
    let tenant = executor.tenant;
    let primary_key: Vec<Datum> = table
        .primary_key
        .iter()
        .map(|&ordinal| row[ordinal].clone())
        .collect();
    let key = row::row_key(tenant, table.id, &primary_key)?;

    // The primary key is a unique index whose entry is the row itself.
    let detail = render_key(table, &table.primary_key, &primary_key);
    if txn.get(&key)?.is_some() {
        // An internal row id is handed out once and never reused, so a taken key here is not a
        // user's duplicate -- it is this crate having lost track of the sequence, and reporting it
        // as a `23505` would name a constraint that does not exist and blame the wrong party.
        if table.row_id().is_some() {
            return Err(SqlError::Internal(format!(
                "internal row id {} of table \"{}\" is already taken",
                super::index::render_values(&primary_key),
                table.name
            )));
        }
        return Err(SqlError::UniqueViolation {
            constraint: table.primary_key_name.clone(),
            key: Some(detail.clone()),
        });
    }
    // Only a *declared* key is a constraint a lost race can be reported against; an internal row
    // id cannot collide, so recording it would only add a key for `explain_conflict` to probe.
    if table.row_id().is_none() {
        written.unique_keys.push(Unique {
            key: key.clone(),
            constraint: table.primary_key_name.clone(),
            detail,
        });
    }

    for index in &table.indexes {
        // **Write-only and public write an entry; delete-only and absent do not.** One state later
        // than [`SchemaState::maintained`], and the asymmetry is the design: removal has to lead
        // creation, or a node that does not yet know about the index deletes a row and leaves an
        // entry pointing at nothing (ADR 0020, "skip delete-only").
        if !index.state.written() {
            continue;
        }
        // A **partial** index holds entries only for the rows its predicate admits, and an
        // expression index holds the expression's value rather than a column's. Both are
        // `super::index`'s to decide, so that a writer, a deleter and the two backfills cannot
        // decide them differently.
        let Some(entry) = super::index::entry(tenant, table, index, row, &primary_key)? else {
            continue;
        };
        // A unique index leaves the primary key off, which is what makes a duplicate a collision
        // on one key -- unless a column is NULL, because PostgreSQL admits any number of NULLs in
        // a `UNIQUE` column and those entries need the suffix to stay apart (`crate::row`).
        if entry.by_value {
            let detail = super::index::render_key(table, &index.keys, &entry.values);
            if txn.get(&entry.key)?.is_some() {
                return Err(SqlError::UniqueViolation {
                    constraint: index.name.clone(),
                    key: Some(detail),
                });
            }
            written.unique_keys.push(Unique {
                key: entry.key.clone(),
                constraint: index.name.clone(),
                detail,
            });
        }
        // The value is the primary key, which is what an index lookup follows back to the row.
        txn.put(
            &entry.key,
            &row::encode_row(&table.primary_key_types(), &primary_key)?,
        );
    }

    txn.put(&key, &row::encode_row(&table.column_types(), row)?);
    Ok(())
}

/// `Key (a, b)=(1, x)` for the **primary key**, whose parts are always columns.
///
/// An index's is [`super::index::render_key`], which has an expression key part to print too.
fn render_key(table: &TableDef, ordinals: &[usize], values: &[Datum]) -> String {
    let keys: Vec<crate::catalog::IndexKey> = ordinals
        .iter()
        .map(|&ordinal| crate::catalog::IndexKey::column(ordinal))
        .collect();
    super::index::render_key(table, &keys, values)
}

/// `UPDATE`: read the rows that match, rewrite them, and keep every index in step.
///
/// # Why the rows are read before any of them is written
///
/// The scan and the writes go through the same transaction, and the buffer is merged into a scan
/// (`crate::backend`), so a row whose primary key this statement *moves* could be met again
/// further along the scan and updated twice. That is the Halloween problem, and materialising the
/// matching rows first is the cheap way out of it: what the statement writes can no longer change
/// what it is about to read.
pub(super) fn update(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    update: &Update,
    written: &mut Written,
) -> Result<Outcome> {
    crate::catalog::pg_catalog::refuse_write(&update.table)?;
    let table = executor.require_table(txn, &update.table)?;
    let mut returned = Returned::open(update.returning.as_ref(), &table)?;

    // Resolve the target of every assignment first, so `SET nope = 1` fails before anything is
    // read rather than after some rows have been rewritten.
    let assignments = update
        .assignments
        .iter()
        .map(|(name, value)| {
            let ordinal =
                table
                    .column(name)
                    .ok_or_else(|| SqlError::UndefinedColumnInRelation {
                        column: name.clone(),
                        relation: table.name.clone(),
                    })?;
            Ok((ordinal, value.clone()))
        })
        .collect::<Result<Vec<_>>>()?;

    let rows = collect(executor, txn, update.filter.as_ref(), &table)?;
    let mut count = 0;
    for old in rows {
        let mut new = old.clone();
        for (ordinal, value) in &assignments {
            let column = &table.columns[*ordinal];
            // Evaluated against the row as it was, so `SET a = b, b = a` swaps them.
            let evaluated = match value {
                // `SET a = DEFAULT` is the column's own default, which for a sequence column is
                // the next value and for every other one is the constant the catalog holds.
                crate::plan::Expr::Default => match table.sequence_for(*ordinal) {
                    Some(sequence) => sequence_datum(
                        table.columns[sequence.column].ty,
                        executor.next_sequence_value(sequence.id)?,
                    )?,
                    None => column_default_value(
                        column,
                        crate::time_machine::micros_of_ts(txn.start_ts()),
                    ),
                },
                crate::plan::Expr::Literal(literal) => literal.assign(column.ty, &column.name)?,
                other => {
                    let resolved = query::resolve_against(other, &table)?;
                    cursor::evaluate(&resolved, &old)?
                }
            };
            if !evaluated.fits(column.ty) {
                return Err(SqlError::DatatypeMismatchInColumn {
                    column: column.name.clone(),
                    column_type: column.ty.name(),
                    expression_type: "the expression's",
                });
            }
            new[*ordinal] = evaluated;
        }
        fit_typmods(&table, &mut new)?;
        check_not_null(&table, &new)?;
        check_constraints(&table, &new)?;
        remove_row(executor, txn, &table, &old)?;
        write_row(executor, txn, &table, &new, written)?;
        // The row **after** the assignments: `UPDATE t SET n = n + 1 RETURNING n` answers with
        // the new value, which is the whole reason a client writes it.
        if let Some(returned) = &mut returned {
            returned.push(&new)?;
        }
        count += 1;
    }
    Ok(finish(returned, format!("UPDATE {count}")))
}

/// `DELETE`: the row and every index entry that points at it.
pub(super) fn delete(
    executor: &mut Executor,
    txn: &mut dyn Txn,
    delete: &Delete,
) -> Result<Outcome> {
    crate::catalog::pg_catalog::refuse_write(&delete.table)?;
    let table = executor.require_table(txn, &delete.table)?;
    let mut returned = Returned::open(delete.returning.as_ref(), &table)?;
    let rows = collect(executor, txn, delete.filter.as_ref(), &table)?;
    let count = rows.len();
    for row in rows {
        // The row as it was, gathered before it goes: after `remove_row` there is nothing to read.
        if let Some(returned) = &mut returned {
            returned.push(&row)?;
        }
        remove_row(executor, txn, &table, &row)?;
    }
    Ok(finish(returned, format!("DELETE {count}")))
}

/// Every row a predicate matches, read before anything is written. See [`update`] for why.
fn collect(
    executor: &Executor,
    txn: &dyn Txn,
    filter: Option<&crate::plan::Expr>,
    table: &TableDef,
) -> Result<Vec<Vec<Datum>>> {
    let node = query::matching_rows(filter, executor.tenant, table)?;
    let mut cursor = cursor::Cursor::open(txn, executor.tenant, &node)?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next()? {
        rows.push(row);
    }
    Ok(rows)
}

/// Every `CHECK` on the table, against the row about to be written.
///
/// **A NULL passes.** A `CHECK` fails only when its predicate is *false*, and SQL's three-valued
/// logic makes `NULL > 0` unknown rather than false — so a row with a NULL in the checked column
/// is admitted. Measured: `INSERT INTO ck VALUES (4, NULL, 'x')` succeeds under `CHECK (p > 0)`.
/// That is the rule most likely to be got wrong by evaluating the predicate as a boolean and
/// treating "not true" as a violation.
///
/// The predicate is re-lowered from its stored text each time it is checked. It could be lowered
/// once when the table is loaded; it is not, because the catalog caches a `TableDef` and a lowered
/// expression would have to be invalidated with it. Re-lowering a short predicate per row is the
/// cheaper mistake to make, and the only one that cannot go stale.
fn check_constraints(table: &TableDef, row: &[Datum]) -> Result<()> {
    for check in &table.checks {
        let parsed = crate::parse::parse_stored_expr(&check.expr).map_err(|error| {
            SqlError::Internal(format!(
                "the stored CHECK {} of {} no longer parses: {error}",
                check.name, table.name
            ))
        })?;
        let scope = query::Scope::single(table);
        let resolved = query::resolve(&parsed, &scope)?;
        // Only `false` violates. NULL is unknown and passes, which is PostgreSQL's rule.
        if matches!(cursor::evaluate(&resolved, row)?, Datum::Bool(false)) {
            return Err(SqlError::CheckViolation {
                constraint: check.name.clone(),
                relation: table.name.clone(),
                row: super::index::render_values(row),
            });
        }
    }
    Ok(())
}

fn check_not_null(table: &TableDef, row: &[Datum]) -> Result<()> {
    for (ordinal, column) in table.columns.iter().enumerate() {
        if column.not_null && matches!(row[ordinal], Datum::Null) {
            return Err(SqlError::NotNullViolationInRelation {
                column: column.name.clone(),
                relation: table.name.clone(),
                row: Some(super::index::render_values(row)),
            });
        }
    }
    Ok(())
}

/// Deletes a row and every index entry built from it.
pub(super) fn remove_row(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    row: &[Datum],
) -> Result<()> {
    let tenant = executor.tenant;
    let primary_key: Vec<Datum> = table
        .primary_key
        .iter()
        .map(|&ordinal| row[ordinal].clone())
        .collect();

    super::index::remove_entries(tenant, txn, table, row, &primary_key)?;
    txn.delete(&row::row_key(tenant, table.id, &primary_key)?);
    Ok(())
}
