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
fn column_default_value(
    table: &TableDef,
    column: &crate::catalog::ColumnDef,
    txn: &dyn Txn,
) -> Result<Datum> {
    let Some(expr) = &column.default_expr else {
        return Ok(column.default.clone().unwrap_or(Datum::Null));
    };
    // **Parsed and evaluated per row**, which is the whole point: two rows of one `INSERT` get two
    // UUIDs and two `random()` draws, and a column defaulted to one can be a primary key. A value
    // folded once at `CREATE TABLE` would give every row the same key and refuse the second
    // insert. The parse is the same one a `CHECK` and a generation expression pay
    // (`fill_generated`), and the same failure is the same `Internal`: text this node wrote and
    // can no longer read is a broken catalog, not a bad statement.
    let parsed = crate::parse::parse_stored_expr(expr).map_err(|error| {
        SqlError::Internal(format!(
            "the stored default of {}.{} no longer parses: {error}",
            table.name, column.name
        ))
    })?;
    let scope = query::Scope::single(table);
    let resolved = query::resolve(&parsed, &scope).map_err(|error| {
        SqlError::Internal(format!(
            "the stored default of {}.{} no longer resolves: {error}",
            table.name, column.name
        ))
    })?;
    // No row: a default cannot read one, which is the first of the three things PostgreSQL forbids
    // in one and is refused where the column is lowered.
    let value = cursor::evaluate_in_txn(&resolved, &[], txn)?;
    // The expression's type is not the column's — `now()` is a `timestamptz` filling a `date`, and
    // `concat` a `text` filling a `varchar` — so the assignment cast every other path through an
    // `INSERT` makes is made here too.
    assign_default(value, column.ty)
}

/// A default's value as the column's type: the assignment cast a real server makes here.
///
/// The expression's type is rarely the column's — `now()` is a `timestamptz` filling a `date`,
/// `concat` a `text` filling a `varchar` — and PostgreSQL coerces the default to the column when
/// the table is created, so a row never sees the difference.
///
/// **A timestamp to a date is not a text round trip.** Both are counts from 2000-01-01, so the
/// conversion is a division; going through text would print a zone offset that `date`'s input
/// function then has to re-parse, and would answer the wrong day for the last hours of one.
#[expect(
    clippy::cast_possible_truncation,
    reason = "`in_range` checks the bound first, which is what makes each cast exact"
)]
fn assign_default(value: Datum, ty: ColumnType) -> Result<Datum> {
    if matches!(value, Datum::Null) || value.column_type() == Some(ty) {
        return Ok(value);
    }
    if let (ColumnType::Date, Datum::Timestamp(micros) | Datum::TimestampTz(micros)) = (ty, &value)
    {
        return Ok(Datum::Date(
            i32::try_from(micros.div_euclid(86_400_000_000)).unwrap_or(i32::MAX),
        ));
    }
    // **A float into an integer rounds, and it rounds half to *even*.** Measured on 19beta1:
    // `0.5` is `0`, `1.5` is `2`, `2.5` is `2`, `3.5` is `4`, and the negatives mirror it. That is
    // `rint`, which is what PostgreSQL's `dtoi4` calls — **not** the away-from-zero rounding a
    // `numeric` gets, where `0.5` is `1` and `2.5` is `3`. The two casts differ and the difference
    // is measurable in one statement, so they are written as two rules rather than one.
    //
    // Going through text instead was a wrong answer rather than a rounding difference: it refused
    // the row outright, which is how `random() * 100` into an `integer` column — statement 738's
    // own default — reported `22P02` where a real server stores a number.
    if let Datum::Double(_) | Datum::Real(_) = value
        && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    {
        let raw = match value {
            Datum::Double(raw) => raw,
            Datum::Real(raw) => f64::from(raw),
            _ => unreachable!("the pattern above admits only the two floats"),
        };
        let rounded = raw.round_ties_even();
        let out_of_range = || SqlError::IntegerLiteralOutOfRange(ty.name());
        // The bound is checked before the conversion, because casting an out-of-range `f64` to an
        // integer saturates in Rust and would store the limit where a real server raises `22003`.
        return match ty {
            ColumnType::Int2 => in_range(rounded, f64::from(i16::MIN), f64::from(i16::MAX))
                .map(|value| Datum::Int2(value as i16))
                .ok_or_else(out_of_range),
            ColumnType::Int4 => in_range(rounded, f64::from(i32::MIN), f64::from(i32::MAX))
                .map(|value| Datum::Int4(value as i32))
                .ok_or_else(out_of_range),
            _ => in_range(
                rounded,
                -9_223_372_036_854_775_808.0,
                9_223_372_036_854_775_807.0,
            )
            .map(|value| Datum::Int8(value as i64))
            .ok_or_else(out_of_range),
        };
    }
    match value.to_text() {
        Some(text) => Datum::from_text(ty, &text),
        None => Ok(Datum::Null),
    }
}

/// A rounded float, if it is inside an integer type's range — NaN and the infinities are not.
fn in_range(value: f64, low: f64, high: f64) -> Option<f64> {
    (value >= low && value <= high).then_some(value)
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
        let mut row: Vec<Datum> = table
            .columns
            .iter()
            .map(|column| column_default_value(&table, column, &*txn))
            .collect::<Result<_>>()?;
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
            // A `GENERATED ALWAYS AS (…) STORED` column refuses one too, with its **own** sentence
            // — `cannot insert a non-DEFAULT value into column "x"`, where an `UPDATE` says
            // `column "x" can only be updated to DEFAULT`. One SQLSTATE, two messages, measured.
            if column.generated.is_some() {
                return Err(SqlError::GeneratedColumnInsert {
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
            // **Only a sequence that fills a column writes one.** A table may own a sequence that
            // fills nothing — `CREATE SEQUENCE s OWNED BY t.c` makes one — and it has no column
            // to put a value in.
            let Some(fills) = sequence.column else {
                continue;
            };
            if targets
                .iter()
                .take(values.len())
                .position(|at| *at == fills)
                .is_some_and(|at| !matches!(values[at], crate::plan::Expr::Default))
            {
                continue;
            }
            // Narrowed to the column's own width. A sequence counts in `i64` whatever it fills,
            // so an `integer` identity column has to be told — and running past 2^31 is the same
            // `22003` a constant that far out gets, which is what a real server answers when a
            // `serial` runs out.
            row[fills] = sequence_datum(
                table.columns[fills].ty,
                executor.next_sequence_value(sequence.id)?,
            )?;
        }
        // A table with no declared key carries an internal row id the user cannot write, so the
        // executor fills it (`crate::catalog::TableDef::row_id`).
        if let Some(at) = table.row_id() {
            row[at] = Datum::Int8(executor.next_row_id(table.id)?);
        }

        fit_typmods(&table, &mut row)?;
        fill_generated(&table, &mut row)?;
        check_not_null(&table, &row)?;
        check_constraints(&table, &row)?;
        // **A partitioned table stores nothing itself**: the row goes to the partition its key
        // selects, and lands there under that table's own row id, key and indexes. A row no
        // partition takes is `23514` naming the *parent* — there is no partition to name, which
        // is the whole condition.
        let routed = route_to_partition(executor, txn, &table, &row)?;
        // Not routed is either an ordinary table or a row written **straight into a partition**,
        // where the bound is a constraint rather than a route.
        if routed.is_none() {
            check_partition_bound(executor, txn, &table, &row)?;
        }
        write_row(
            executor,
            txn,
            routed.as_ref().unwrap_or(&table),
            &row,
            written,
        )?;
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

    // The row it points at has to exist, and this is where a `CHECK` is enforced too — before
    // anything is stored, so a violation leaves nothing behind.
    super::foreign_key::check_references(executor, txn, table, row)?;

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
    let named = executor.require_table(txn, &update.table)?;
    let mut returned = Returned::open(update.returning.as_ref(), &named)?;
    let targets = inheritance_targets(executor, txn, &named)?;
    let mut count = 0;

    for (table, project) in targets {
        // Resolve the target of every assignment first, so `SET nope = 1` fails before anything is
        // read rather than after some rows have been rewritten. Per relation, because a child's
        // ordinals are its own — the same name is a different position the moment it has a row id
        // the parent has not.
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
        for old in rows {
            let mut new = old.clone();
            for (ordinal, value) in &assignments {
                let column = &table.columns[*ordinal];
                // A `GENERATED ALWAYS AS (…) STORED` column takes `DEFAULT` and nothing else, and
                // an `UPDATE`'s sentence for that is **not** the `INSERT`'s: `column "x" can only be
                // updated to DEFAULT`. Measured, both.
                if column.generated.is_some() && !matches!(value, crate::plan::Expr::Default) {
                    return Err(SqlError::GeneratedColumnUpdate {
                        column: column.name.clone(),
                    });
                }
                // Evaluated against the row as it was, so `SET a = b, b = a` swaps them.
                let evaluated = match value {
                    // `SET a = DEFAULT` is the column's own default, which for a sequence column is
                    // the next value and for every other one is the constant the catalog holds.
                    // `sequence_for` answers only for a sequence that *fills* this column, so an
                    // owned-but-unused one cannot capture a `SET c = DEFAULT`.
                    crate::plan::Expr::Default => match table.sequence_for(*ordinal) {
                        Some(sequence) => sequence_datum(
                            table.columns[*ordinal].ty,
                            executor.next_sequence_value(sequence.id)?,
                        )?,
                        None => column_default_value(&table, column, &*txn)?,
                    },
                    crate::plan::Expr::Literal(literal) => {
                        literal.assign(column.ty, &column.name)?
                    }
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
            // The column is a function of the row, so an `UPDATE` that moved its source moves it too
            // — and a `SET generated = DEFAULT` recomputes rather than storing NULL.
            fill_generated(&table, &mut new)?;
            check_not_null(&table, &new)?;
            check_constraints(&table, &new)?;
            // Anything pointing at the row's **old** key. Refusing comes first, so a `RESTRICT` leaves
            // the table as it was; the cascade comes *after* the row has moved, because a child whose
            // key follows the parent's re-checks that key and it has to be there already.
            super::foreign_key::refuse_if_referenced(executor, txn, &table, &old, &new)?;
            // **An `UPDATE` that changes the partition key moves the row.** Row movement is the
            // default on a real server, not an opt-in: the row leaves the partition it was in and
            // arrives in the one the new key selects, with no error raised. Routed from the
            // *parent*, because a partition's own bound is one list and the destination may be
            // any sibling.
            let destination = match parent_of_partition(executor, txn, &table)? {
                Some(parent) => route_to_partition(executor, txn, &parent, &new)?,
                None => None,
            }
            .filter(|target| target.id != table.id);
            remove_row(executor, txn, &table, &old)?;
            match &destination {
                Some(target) => write_row(executor, txn, target, &new, written)?,
                None => write_row(executor, txn, &table, &new, written)?,
            }
            super::foreign_key::cascade_update(executor, txn, &table, &old, &new, written)?;
            // The row **after** the assignments: `UPDATE t SET n = n + 1 RETURNING n` answers with
            // the new value, which is the whole reason a client writes it.
            if let Some(returned) = &mut returned {
                returned.push(&projected(&new, project.as_ref()))?;
            }
            count += 1;
        }
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
    let named = executor.require_table(txn, &delete.table)?;
    let mut returned = Returned::open(delete.returning.as_ref(), &named)?;
    let mut count = 0;
    // Itself and everything that inherits from it: `DELETE FROM parent` removes a child's rows,
    // measured, and each row has to go through its own table's keys and indexes.
    for (table, project) in inheritance_targets(executor, txn, &named)? {
        let rows = collect(executor, txn, delete.filter.as_ref(), &table)?;
        count += rows.len();
        for row in rows {
            // The row as it was, gathered before it goes: after `remove_row` there is nothing to
            // read.
            if let Some(returned) = &mut returned {
                returned.push(&projected(&row, project.as_ref()))?;
            }
            // Anything pointing at this row: refused, or cascaded into first. Before the row goes,
            // so a refusal leaves the table as it was.
            super::foreign_key::on_parent_removed(executor, txn, &table, &row)?;
            remove_row(executor, txn, &table, &row)?;
        }
    }
    Ok(finish(returned, format!("DELETE {count}")))
}

/// One relation an `UPDATE` or a `DELETE` acts on, and how its rows line up with the one named.
///
/// The projection is `None` for the named table itself, whose rows already are its own shape.
type Target = (std::sync::Arc<TableDef>, Option<Vec<usize>>);

/// The partitioned table this one is a partition of, if it is one.
///
/// A partition has exactly one parent — the edge it shares with `INHERITS` — and only a
/// *partitioned* parent can re-route a row, which is what separates this from an inheriting child.
fn parent_of_partition(
    executor: &Executor,
    txn: &dyn Txn,
    table: &TableDef,
) -> Result<Option<std::sync::Arc<TableDef>>> {
    if table.partition_bound.is_none() {
        return Ok(None);
    }
    let Some(&parent_id) = table.parents.first() else {
        return Ok(None);
    };
    let parent = executor.table_by_id(txn, parent_id)?;
    Ok(parent.partition_by.is_some().then_some(parent))
}

/// The partition a row belongs in, or `None` for a table that is not partitioned.
///
/// **`DEFAULT` is the fallback and is tried last**, whatever order the partitions were declared
/// in: it takes what no value list does, so a list that matches must win even when the default
/// partition was created first.
fn route_to_partition(
    executor: &Executor,
    txn: &dyn Txn,
    table: &TableDef,
    row: &[Datum],
) -> Result<Option<std::sync::Arc<TableDef>>> {
    let Some(key) = &table.partition_by else {
        return Ok(None);
    };
    let values: Vec<&Datum> = key.columns.iter().map(|&at| &row[at]).collect();
    let mut fallback = None;
    for &child_id in &table.children {
        let child = executor.table_by_id(txn, child_id)?;
        match &child.partition_bound {
            Some(bound) if bound_admits(bound, &values) => return Ok(Some(child)),
            Some(crate::catalog::PartitionBound::Default) => fallback = Some(child),
            Some(_) | None => {}
        }
    }
    fallback.map_or_else(
        || Err(SqlError::NoPartitionForRow(table.name.clone())),
        |child| Ok(Some(child)),
    )
}

/// Whether one partition's bound admits this key.
///
/// **`DEFAULT` admits nothing here** — it is the fallback the caller reaches after every other
/// partition has said no, which is what makes a value list win over a default declared before it.
///
/// A `RANGE` bound is **half-open**: `FROM (MINVALUE) TO (10)` takes `9` and `FROM (10) TO
/// (MAXVALUE)` takes `10`, so the lower end is compared with `<=` and the upper with `<`.
fn bound_admits(bound: &crate::catalog::PartitionBound, values: &[&Datum]) -> bool {
    use crate::catalog::PartitionBound::{Default, Range, Values};
    match bound {
        Default => false,
        Values(listed) => {
            listed.len() == values.len()
                && listed.iter().zip(values).all(|(one, other)| one == *other)
        }
        Range { from, to } => {
            from.len() == values.len()
                && to.len() == values.len()
                && from
                    .iter()
                    .zip(values)
                    .all(|(end, value)| end.cmp_value(value).is_le())
                && to
                    .iter()
                    .zip(values)
                    .all(|(end, value)| end.cmp_value(value).is_gt())
        }
    }
}

/// A row written **straight into a partition**, against that partition's own bound.
///
/// Routing cannot raise this: a routed row is sent to the partition whose bound admits it. This is
/// the other way in — `INSERT INTO measurements_toronto … VALUES ('2', …)` — and PostgreSQL calls
/// it a violated *constraint* rather than a missing partition, naming the partition.
///
/// A `DEFAULT` partition admits anything by this test, and correctly: what excludes a row from it
/// is another partition's list claiming that row, which routing has already settled.
fn check_partition_bound(
    executor: &Executor,
    txn: &dyn Txn,
    table: &TableDef,
    row: &[Datum],
) -> Result<()> {
    let Some(bound) = &table.partition_bound else {
        return Ok(());
    };
    // A `DEFAULT` partition admits anything by this test, and correctly.
    if matches!(bound, crate::catalog::PartitionBound::Default) {
        return Ok(());
    }
    let Some(parent) = parent_of_partition(executor, txn, table)? else {
        return Ok(());
    };
    let Some(key) = &parent.partition_by else {
        return Ok(());
    };
    // **The key columns are found by name**, because the ordinals in it are the *parent's*: a
    // partition carries the parent's columns and may carry an internal row id the parent has not,
    // which moves every one of them by a position. Matching positionally is how a bound on a key
    // that is not the table's first column reads the wrong column.
    let mut values = Vec::with_capacity(key.columns.len());
    for &at in &key.columns {
        let Some(name) = parent.columns.get(at).map(|column| &column.name) else {
            return Ok(());
        };
        let Some(mine) = table.column(name) else {
            return Ok(());
        };
        values.push(row.get(mine).unwrap_or(&Datum::Null));
    }
    if bound_admits(bound, &values) {
        return Ok(());
    }
    Err(SqlError::PartitionConstraintViolation(table.name.clone()))
}

/// The relations an `UPDATE` or a `DELETE` on this one acts on: itself and everything that
/// inherits from it, each seen **alone**.
///
/// **Inheritance is a write rule too.** `UPDATE parent SET …` changes the child's rows and
/// `DELETE FROM parent` removes them — measured, both. It cannot be done by one scan the way a
/// `SELECT` is, because each row has to go back where it came from: a child's row key, row layout
/// and indexes are its own, and writing one through the parent's would put it in the wrong table.
///
/// So each relation is acted on separately, with `child_scans` cleared so its own scan returns
/// only its own rows. The projection beside it is how a child's row is reported back through a
/// `RETURNING` clause the parent's columns were named in.
fn inheritance_targets(
    executor: &Executor,
    txn: &dyn Txn,
    table: &std::sync::Arc<TableDef>,
) -> Result<Vec<Target>> {
    let alone = |table: &std::sync::Arc<TableDef>| {
        if table.child_scans.is_empty() {
            return std::sync::Arc::clone(table);
        }
        let mut alone = (**table).clone();
        alone.child_scans.clear();
        std::sync::Arc::new(alone)
    };
    let mut targets = vec![(alone(table), None)];
    // Breadth first from the named table, so a grandchild is reached through its own parent's
    // list rather than being missed for not being named directly.
    let mut queue: std::collections::VecDeque<(u64, Vec<usize>)> = table
        .child_scans
        .iter()
        .map(|child| (child.table_id, child.project.clone()))
        .collect();
    while let Some((child_id, project)) = queue.pop_front() {
        let child = executor.table_by_id(txn, child_id)?;
        for grandchild in &child.child_scans {
            // Composed, so a grandchild's row lands in the *named* table's columns and not in its
            // own parent's — the two differ as soon as either adds a column.
            let composed = project
                .iter()
                .map(|&at| grandchild.project.get(at).copied().unwrap_or(at))
                .collect();
            queue.push_back((grandchild.table_id, composed));
        }
        targets.push((alone(&child), Some(project)));
    }
    Ok(targets)
}

/// A row of `table` as the relation an `UPDATE` or `DELETE` named would report it.
fn projected(row: &[Datum], project: Option<&Vec<usize>>) -> Vec<Datum> {
    match project {
        Some(project) => project
            .iter()
            .map(|&at| row.get(at).cloned().unwrap_or(Datum::Null))
            .collect(),
        None => row.to_vec(),
    }
}

/// One row replaced by another, running every check a written row runs.
///
/// What `ON UPDATE CASCADE` does to a child: its key follows the parent's, and because it goes
/// through [`write_row`] it re-checks its own foreign keys, its `CHECK`s and its unique indexes —
/// so a three-level chain stays consistent without a second rule.
pub(super) fn rewrite_row(
    executor: &Executor,
    txn: &mut dyn Txn,
    table: &TableDef,
    old: &[Datum],
    new: &[Datum],
    written: &mut Written,
) -> Result<()> {
    check_not_null(table, new)?;
    check_constraints(table, new)?;
    remove_row(executor, txn, table, old)?;
    // The statement's own `Written`, not a fresh one: a unique key a cascade took is a key this
    // statement wrote, and a lost race on it has to be reported as the `23505` it is.
    write_row(executor, txn, table, new, written)
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
/// Computes every `GENERATED ALWAYS AS (…) STORED` column of a row.
///
/// **After the values and before the checks**, and both halves of that matter: after, because the
/// expression reads the columns the statement just wrote; before, because a `CHECK` or a `NOT
/// NULL` on a generated column is about the value the expression produced.
///
/// Run on **every write**, not only on insert — the column is a function of the row, so an
/// `UPDATE` that moves its source moves it too. Measured: `UPDATE gen SET name = 'zoe'` carries
/// `upper_name` with it.
///
/// The expression is re-lowered from its stored text per row, the trade
/// [`check_constraints`] already makes and for the same reason.
fn fill_generated(table: &TableDef, row: &mut [Datum]) -> Result<()> {
    for (at, column) in table.columns.iter().enumerate() {
        let Some(expr) = &column.generated else {
            continue;
        };
        let parsed = crate::parse::parse_stored_expr(expr).map_err(|error| {
            SqlError::Internal(format!(
                "the stored generation expression of {}.{} no longer parses: {error}",
                table.name, column.name
            ))
        })?;
        let scope = query::Scope::single(table);
        let resolved = query::resolve(&parsed, &scope).map_err(|error| {
            SqlError::Internal(format!(
                "the stored generation expression of {}.{} no longer resolves: {error}",
                table.name, column.name
            ))
        })?;
        row[at] = cursor::evaluate(&resolved, row)?;
    }
    Ok(())
}

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
