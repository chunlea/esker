//! What a `$1` is, and what to read its bytes as.
//!
//! A `Bind` carries values and format codes and no types at all. The type is inferred from *where*
//! the parameter appears — the column it is being inserted into, the column it is compared
//! against, the column it is assigned to — which is why this lives beside the executor, where the
//! catalog is in reach, and not in the protocol layer.
//!
//! # The fallback is `text`, and that was measured
//!
//! A parameter no context types is `text`. `PREPARE p AS SELECT $1` on a real PostgreSQL 19
//! reports `{text}`, not an error and not "unknown" — so a client that binds anything to a bare
//! `SELECT $1` gets a string back, and so does one talking to this node.
//!
//! A type the client *declared* in `Parse` wins over any inference. It said what it is sending; the
//! server's job is to read it that way, not to argue.

use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::pgwire::session::Params;
use crate::plan::{BinaryOp, Expr, Literal, Statement};
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// Every parameter's type, indexed from zero for `$1`.
pub(super) fn infer(
    statement: &Statement,
    tables: &[std::sync::Arc<TableDef>],
    declared: &[u32],
) -> Vec<ColumnType> {
    // Sized by the highest `$n` the statement mentions anywhere, not only where a type comes from:
    // `SELECT $1` names a parameter that nothing types, and it still has to be reported.
    let mut count = declared.len();
    for_each_expr(statement, &mut |expr| {
        if let Expr::Parameter(number) = expr {
            count = count.max(*number as usize);
        }
    });

    let mut found: Vec<Option<ColumnType>> = vec![None; count];
    walk(statement, tables, &mut |number, ty| {
        let at = (number as usize).saturating_sub(1);
        if found.len() <= at {
            found.resize(at + 1, None);
        }
        found[at].get_or_insert(ty);
    });

    found
        .into_iter()
        .enumerate()
        .map(|(at, inferred)| {
            // What the client declared wins: it is the one that knows what bytes it is sending.
            match declared.get(at).copied().unwrap_or(0) {
                0 => inferred.unwrap_or(ColumnType::Text),
                oid => from_oid(oid).unwrap_or_else(|| inferred.unwrap_or(ColumnType::Text)),
            }
        })
        .collect()
}

/// Replaces every `$n` with the value bound to it, already read as the type inferred for it.
///
/// After this the statement holds no parameters at all, so everything downstream — the planner,
/// the filter, the row builder — sees the same shapes it would have seen from a literal.
pub(super) fn substitute(
    statement: &mut Statement,
    params: &Params<'_>,
    types: &[ColumnType],
) -> Result<()> {
    let mut failure = None;
    walk_mut(statement, &mut |expr| {
        let Expr::Parameter(number) = expr else {
            return;
        };
        let number = *number;
        let at = (number as usize).saturating_sub(1);
        let Some(slot) = params.values.get(at) else {
            // Nothing was bound. In the simple query protocol nothing ever is, and PostgreSQL says
            // exactly this.
            failure.get_or_insert(SqlError::UndefinedParameter(number));
            return;
        };
        let ty = types.get(at).copied().unwrap_or(ColumnType::Text);
        let value = match slot {
            None => Ok(Datum::Null),
            Some(bytes) => match params.format(at) {
                0 => std::str::from_utf8(bytes)
                    .map_err(|_| SqlError::InvalidByteSequence(bytes.first().copied().unwrap_or(0)))
                    .and_then(|text| Datum::from_text(ty, text)),
                1 => Datum::from_binary(ty, bytes),
                other => Err(SqlError::ProtocolViolation(format!(
                    "parameter format {other} is neither text nor binary"
                ))),
            },
        };
        match value {
            Ok(value) => *expr = Expr::Literal(Literal::Typed(Box::new(value))),
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    });
    failure.map_or(Ok(()), Err)
}

/// The type a client's declared OID names, or `None` for one this crate does not have.
fn from_oid(oid: u32) -> Option<ColumnType> {
    ColumnType::ALL.into_iter().find(|ty| ty.oid() == oid)
}

/// Visits every parameter with the type its context gives it.
fn walk(
    statement: &Statement,
    tables: &[std::sync::Arc<TableDef>],
    seen: &mut impl FnMut(u32, ColumnType),
) {
    match statement {
        Statement::Insert(insert) => {
            let Some(table) = tables.first() else { return };
            let targets: Vec<usize> = match &insert.columns {
                Some(names) => names.iter().filter_map(|name| table.column(name)).collect(),
                // The user's columns only, matching what `exec::dml` fills: a `$1` in the first
                // position is the first column the user declared, not an internal row id.
                None => table.user_columns().map(|(at, _)| at).collect(),
            };
            for row in &insert.rows {
                for (target, expr) in targets.iter().zip(row) {
                    if let Expr::Parameter(number) = expr {
                        seen(*number, table.columns[*target].ty);
                    }
                }
            }
        }
        Statement::Select(select) => {
            if let Some(filter) = &select.filter {
                walk_predicate(filter, tables, seen);
            }
            // `LIMIT $1` is a count, whatever else is going on.
            for clause in [select.limit.as_ref(), select.offset.as_ref()]
                .into_iter()
                .flatten()
            {
                if let Expr::Parameter(number) = clause {
                    seen(*number, ColumnType::Int8);
                }
            }
        }
        Statement::Update(update) => {
            if let Some(table) = tables.first() {
                for (name, value) in &update.assignments {
                    if let (Some(at), Expr::Parameter(number)) = (table.column(name), value) {
                        seen(*number, table.columns[at].ty);
                    }
                }
            }
            if let Some(filter) = &update.filter {
                walk_predicate(filter, tables, seen);
            }
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &delete.filter {
                walk_predicate(filter, tables, seen);
            }
        }
        Statement::Explain(inner, _) => walk(inner, tables, seen),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::AlterSchemaRename(_)
        | Statement::DropSequence(_)
        | Statement::DropFunction(_)
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::CreateSequence(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Comment(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        | Statement::TimeMachine(_) => {}
    }
}

/// A parameter compared against a column takes that column's type. That is the whole of the
/// inference in a `WHERE`, and it is what `WHERE id = $1` needs.
fn walk_predicate(
    expr: &Expr,
    tables: &[std::sync::Arc<TableDef>],
    seen: &mut impl FnMut(u32, ColumnType),
) {
    match expr {
        Expr::Binary { op, left, right }
            if op.is_comparison() || *op == BinaryOp::And || *op == BinaryOp::Or =>
        {
            if op.is_comparison() {
                let pair = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column { table, name }, Expr::Parameter(number))
                    | (Expr::Parameter(number), Expr::Column { table, name }) => {
                        Some((table.as_deref(), name, *number))
                    }
                    _ => None,
                };
                // Across a join, the column may belong to either table, and a qualifier says
                // which. An ambiguous bare name types nothing rather than the first match: the
                // planner will refuse the statement anyway, and guessing a type here would put a
                // `ParameterDescription` on the wire for a query that is about to fail.
                if let Some((qualifier, name, number)) = pair {
                    let mut found = None;
                    for candidate in tables {
                        if qualifier.is_some_and(|qualifier| qualifier != candidate.name) {
                            continue;
                        }
                        if let Some(at) = candidate.column(name) {
                            if found.is_some() {
                                found = None;
                                break;
                            }
                            found = Some(candidate.columns[at].ty);
                        }
                    }
                    if let Some(ty) = found {
                        seen(number, ty);
                    }
                }
            }
            walk_predicate(left, tables, seen);
            walk_predicate(right, tables, seen);
        }
        Expr::Binary { left, right, .. } => {
            walk_predicate(left, tables, seen);
            walk_predicate(right, tables, seen);
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => walk_predicate(operand, tables, seen),
        Expr::InList { operand, list, .. } => {
            walk_predicate(operand, tables, seen);
            for item in list {
                walk_predicate(item, tables, seen);
            }
        }
        _ => {}
    }
}

/// Visits every expression in a statement, for substitution.
pub(super) fn walk_mut(statement: &mut Statement, visit: &mut impl FnMut(&mut Expr)) {
    match statement {
        Statement::Insert(insert) => {
            for row in &mut insert.rows {
                for expr in row {
                    walk_expr_mut(expr, visit);
                }
            }
        }
        Statement::Select(select) => {
            for item in &mut select.projection {
                if let crate::plan::SelectItem::Expr { expr, .. } = item {
                    walk_expr_mut(expr, visit);
                }
            }
            for expr in select
                .filter
                .iter_mut()
                .chain(select.limit.iter_mut())
                .chain(select.offset.iter_mut())
            {
                walk_expr_mut(expr, visit);
            }
            for item in &mut select.order_by {
                walk_expr_mut(&mut item.expr, visit);
            }
        }
        Statement::Update(update) => {
            for (_, value) in &mut update.assignments {
                walk_expr_mut(value, visit);
            }
            if let Some(filter) = &mut update.filter {
                walk_expr_mut(filter, visit);
            }
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &mut delete.filter {
                walk_expr_mut(filter, visit);
            }
        }
        Statement::Explain(inner, _) => walk_mut(inner, visit),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::AlterSchemaRename(_)
        | Statement::DropSequence(_)
        | Statement::DropFunction(_)
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::CreateSequence(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Comment(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        | Statement::TimeMachine(_) => {}
    }
}

fn walk_expr_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    visit(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            walk_expr_mut(left, visit);
            walk_expr_mut(right, visit);
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => walk_expr_mut(operand, visit),
        Expr::InList { operand, list, .. } => {
            walk_expr_mut(operand, visit);
            for item in list {
                walk_expr_mut(item, visit);
            }
        }
        // `format_type($1, $2)` is a statement a client may prepare, so its arguments are walked
        // like any other operand — without this the parameter would never be substituted and the
        // statement would answer `42P02` for a parameter that was bound.
        Expr::CatalogFunc(call) => {
            for arg in &mut call.args {
                walk_expr_mut(arg, visit);
            }
        }
        // **Every other node that holds an expression**, and the reason the list has to be
        // complete: whatever is not here is never visited, so a `$1` inside it is never bound and
        // a `'x'::regclass` inside it is never resolved. Measured — `'rc'::regclass::text` reached
        // the row evaluator with the cast unresolved, because a cast to text was not on this list.
        Expr::ToText { operand, .. } | Expr::Scalar { operand, .. } => {
            walk_expr_mut(operand, visit);
        }
        Expr::AnyArray { operand, array } => {
            walk_expr_mut(operand, visit);
            walk_expr_mut(array, visit);
        }
        Expr::Subscript { operand, index, .. } => {
            walk_expr_mut(operand, visit);
            walk_expr_mut(index, visit);
        }
        Expr::Case {
            branches,
            otherwise,
        } => {
            for branch in branches {
                walk_expr_mut(&mut branch.when, visit);
                walk_expr_mut(&mut branch.then, visit);
            }
            if let Some(otherwise) = otherwise {
                walk_expr_mut(otherwise, visit);
            }
        }
        Expr::Aggregate(call) => {
            for arg in &mut call.args {
                walk_expr_mut(arg, visit);
            }
        }
        _ => {}
    }
}

/// The table a statement is about, by name, so the inference has column types to work from.
pub(super) fn table_names(statement: &Statement) -> Vec<&str> {
    match statement {
        Statement::Insert(insert) => vec![insert.table.as_str()],
        // A join's two tables, outer first, which is the order their columns appear in a row.
        Statement::Select(select) => select
            .from
            .iter()
            .map(|table| table.name.as_str())
            .chain(select.joins.iter().map(|join| join.table.name.as_str()))
            .collect(),
        Statement::Update(update) => vec![update.table.as_str()],
        Statement::Delete(delete) => vec![delete.table.as_str()],
        Statement::Explain(inner, _) => table_names(inner),
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::AlterSchemaRename(_)
        | Statement::DropSequence(_)
        | Statement::DropFunction(_)
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::CreateSequence(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Comment(_)
        // DDL over a table, but nothing here needs its column types: a parameter cannot appear
        // in an `ALTER TABLE`, so there is nothing to infer against.
        | Statement::AlterTable(_)
        // Neither a session statement nor a time-machine verb is about a table this crate has
        // to resolve names against: the checkpoint verbs take a name that is their own, and a
        // `DIFF`'s table is resolved where it is scanned, in its own snapshot.
        | Statement::Session(_)
        | Statement::TimeMachine(_) => Vec::new(),
    }
}

/// Whether a statement mentions a parameter at all, so the common case costs no walk of its own.
pub(super) fn any(statement: &Statement, wanted: impl Fn(&Expr) -> bool) -> bool {
    let mut found = false;
    for_each_expr(statement, &mut |expr| {
        found = found || wanted(expr);
    });
    found
}

pub(super) fn has_parameters(statement: &Statement) -> bool {
    let mut found = false;
    for_each_expr(statement, &mut |expr| {
        found = found || matches!(expr, Expr::Parameter(_));
    });
    found
}

/// Every expression in a statement, read-only.
pub(super) fn for_each_expr(statement: &Statement, visit: &mut impl FnMut(&Expr)) {
    let mut each = |expr: &Expr| descend(expr, visit);
    match statement {
        Statement::Insert(insert) => {
            for row in &insert.rows {
                row.iter().for_each(&mut each);
            }
        }
        Statement::Select(select) => {
            for item in &select.projection {
                if let crate::plan::SelectItem::Expr { expr, .. } = item {
                    each(expr);
                }
            }
            select
                .filter
                .iter()
                .chain(select.limit.iter())
                .chain(select.offset.iter())
                .for_each(&mut each);
            for item in &select.order_by {
                each(&item.expr);
            }
        }
        Statement::Update(update) => {
            for (_, value) in &update.assignments {
                each(value);
            }
            update.filter.iter().for_each(&mut each);
        }
        Statement::Delete(delete) => delete.filter.iter().for_each(&mut each),
        Statement::Explain(inner, _) => for_each_expr(inner, visit),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::AlterSchemaRename(_)
        | Statement::DropSequence(_)
        | Statement::DropFunction(_)
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::CreateSequence(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Comment(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        | Statement::TimeMachine(_) => {}
    }
}

fn descend(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            descend(left, visit);
            descend(right, visit);
        }
        Expr::Not(operand)
        | Expr::IsNull { operand, .. }
        | Expr::ToText { operand, .. }
        | Expr::Scalar { operand, .. } => descend(operand, visit),
        Expr::InList { operand, list, .. } => {
            descend(operand, visit);
            for item in list {
                descend(item, visit);
            }
        }
        Expr::CatalogFunc(call) => {
            for arg in &call.args {
                descend(arg, visit);
            }
        }
        Expr::AnyArray { operand, array } => {
            descend(operand, visit);
            descend(array, visit);
        }
        Expr::Subscript { operand, index, .. } => {
            descend(operand, visit);
            descend(index, visit);
        }
        Expr::Case {
            branches,
            otherwise,
        } => {
            for branch in branches {
                descend(&branch.when, visit);
                descend(&branch.then, visit);
            }
            if let Some(otherwise) = otherwise {
                descend(otherwise, visit);
            }
        }
        Expr::Aggregate(call) => {
            for arg in &call.args {
                descend(arg, visit);
            }
        }
        _ => {}
    }
}

/// Replaces every parameter with a *typed placeholder*, for `Describe`.
///
/// Describing a statement needs to know the shape of its output, and a `SELECT $1` cannot be
/// planned while `$1` has no type. Nothing is run, so the value does not matter — only that it has
/// the type the inference gave the parameter.
pub(super) fn substitute_placeholders(statement: &mut Statement, types: &[ColumnType]) {
    walk_mut(statement, &mut |expr| {
        if let Expr::Parameter(number) = expr {
            let at = (*number as usize).saturating_sub(1);
            let ty = types.get(at).copied().unwrap_or(ColumnType::Text);
            *expr = Expr::Literal(Literal::Typed(Box::new(placeholder(ty))));
        }
    });
}

fn placeholder(ty: ColumnType) -> Datum {
    match ty {
        // An empty array of the right element type: the shape a parameter takes before its value
        // arrives, and one that answers `column_type` correctly while it stands in.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::NumericArray
        | ColumnType::TextArray => Datum::Array(esker_keys::array::ArrayValue::empty(
            esker_keys::array::ArrayValue::element_of(ty).unwrap_or(ColumnType::Text),
        )),
        ColumnType::Int8 => Datum::Int8(0),
        ColumnType::Time => Datum::Time(0),
        ColumnType::Uuid => Datum::Uuid([0; 16]),
        ColumnType::Oid => Datum::Oid(0),
        ColumnType::Interval => Datum::Interval {
            months: 0,
            days: 0,
            micros: 0,
        },
        ColumnType::Int4 => Datum::Int4(0),
        ColumnType::Int2 => Datum::Int2(0),
        ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => Datum::Text(String::new()),
        // The empty string is not a document, so a `json` placeholder is the smallest one that
        // is. It only ever stands in for a type while a `Describe` is answered.
        ColumnType::Json | ColumnType::Jsonb => Datum::Text("null".to_owned()),
        ColumnType::Bool => Datum::Bool(false),
        ColumnType::Bytea => Datum::Bytea(Vec::new()),
        ColumnType::TimestampTz => Datum::TimestampTz(0),
        ColumnType::Timestamp => Datum::Timestamp(0),
        ColumnType::Double => Datum::Double(0.0),
        ColumnType::Real => Datum::Real(0.0),
        ColumnType::Date => Datum::Date(0),
        ColumnType::Numeric => Datum::Numeric(esker_keys::numeric::Numeric::Finite(
            esker_keys::numeric::Decimal::zero(),
        )),
    }
}
