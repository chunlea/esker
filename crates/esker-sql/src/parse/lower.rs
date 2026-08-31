//! The parser's tree, lowered into types this crate owns.
//!
//! This is the last place a `sqlparser` type is named (ADR 0014). Everything above it works with
//! `crate::plan`, which is why replacing the dependency is a bounded job: it is this module and
//! its parent, and nothing else.
//!
//! # Reject, do not ignore
//!
//! A lowered statement holds far less than the tree it came from, and that smallness is the risk.
//! An AST field nothing reads is a clause the user *wrote* and the server did not honour, and
//! `CREATE TEMPORARY TABLE t` executed as a permanent table is a worse failure than refusing it —
//! nobody is told, and the next session finds a table it did not expect.
//!
//! So every clause that changes what a statement means is named and refused with contract C2's
//! `0A000`, and `tests/lowering.rs` is written from that side: one case per unhonoured clause,
//! each asserting that the clause's own name comes back.

use sqlparser::ast::{
    AlterTableOperation, AssignmentTarget, BinaryOperator, ColumnOption, CreateTableOptions,
    DataType, DollarQuotedString, ExactNumberInfo, Expr, FromTable, GroupByExpr, Ident,
    IndexColumn, IndexType, JoinConstraint, JoinOperator, LimitClause, NullsDistinctOption,
    ObjectName, ObjectType, OffsetRows, OrderByKind, Query, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableConstraint, TableFactor, TableObject,
    TimezoneInfo, UnaryOperator, Value,
};

use crate::catalog::fold_identifier;
use crate::error::{Result, SqlError};
use crate::parse::{Parsed, feature_name};
use crate::plan;
use crate::time_machine;
use crate::value::{ColumnType, Datum};

impl Parsed {
    /// Lowers this statement into the plan types the executor runs, or names the construct that
    /// stopped it (contract C2).
    pub fn lower(&self) -> Result<plan::Statement> {
        lower_statement(&self.statement)
    }
}

fn lower_statement(statement: &Statement) -> Result<plan::Statement> {
    match statement {
        Statement::CreateTable(create) => {
            Ok(plan::Statement::CreateTable(lower_create_table(create)?))
        }
        // The time machine's verbs are function calls, which is the only spelling PostgreSQL 19
        // parses (`docs/plans/phase-6d.md` §1). Tried before the ordinary query path so that a
        // `SELECT` naming one is the verb rather than a column reference that does not resolve.
        Statement::Query(query) => match lower_verb(query)? {
            Some(verb) => Ok(plan::Statement::TimeMachine(verb)),
            None => Ok(plan::Statement::Select(lower_query(query)?)),
        },
        Statement::Update(update) => Ok(plan::Statement::Update(lower_update(update)?)),
        Statement::Delete(delete) => Ok(plan::Statement::Delete(lower_delete(delete)?)),
        Statement::Insert(insert) => Ok(plan::Statement::Insert(lower_insert(insert)?)),
        Statement::CreateIndex(create) => {
            Ok(plan::Statement::CreateIndex(lower_create_index(create)?))
        }
        Statement::AlterTable(alter) => Ok(plan::Statement::AlterTable(lower_alter_table(alter)?)),
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            restrict,
            purge,
            temporary,
            ..
        } => {
            refuse_if(*cascade, "DROP ... CASCADE")?;
            refuse_if(*restrict, "DROP ... RESTRICT")?;
            refuse_if(*purge, "DROP ... PURGE")?;
            refuse_if(*temporary, "DROP TEMPORARY")?;
            let names = names.iter().map(object_name).collect::<Result<Vec<_>>>()?;
            Ok(match object_type {
                ObjectType::Table => plan::Statement::DropTable(plan::DropTable {
                    names,
                    if_exists: *if_exists,
                }),
                ObjectType::Index => plan::Statement::DropIndex(plan::DropIndex {
                    names,
                    if_exists: *if_exists,
                }),
                other => return Err(SqlError::unsupported(format!("DROP {other}"))),
            })
        }
        Statement::Explain {
            describe_alias,
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
        } => {
            refuse_if(*analyze, "EXPLAIN ANALYZE")?;
            refuse_if(*verbose, "EXPLAIN VERBOSE")?;
            refuse_if(*query_plan, "EXPLAIN QUERY PLAN")?;
            refuse_if(*estimate, "EXPLAIN ESTIMATE")?;
            refuse_if(format.is_some(), "EXPLAIN (FORMAT ...)")?;
            refuse_if(options.is_some(), "EXPLAIN with options")?;
            let _ = describe_alias;
            Ok(plan::Statement::Explain(Box::new(lower_statement(
                statement,
            )?)))
        }
        Statement::Set(set) => lower_set(set),
        Statement::ShowVariable { variable } => lower_show(variable),
        Statement::Reset(reset) => lower_reset(reset),
        other => Err(SqlError::unsupported(feature_name(other))),
    }
}

/// The time machine's verbs, each recognised as the function call it is spelled as.
///
/// `None` when the query is an ordinary one, which is every query that does not name one of the
/// four functions below. The recognition is deliberately narrow: a *bare* call in the target list
/// with nothing else in the query, or a table function as the only `FROM` item. A verb buried in
/// an expression — `SELECT pg_export_snapshot() || 'x'` — is not recognised and falls through to
/// the ordinary path, where it is `0A000` naming the expression, because honouring half of it
/// would export a snapshot and then fail to use it.
fn lower_verb(query: &Query) -> Result<Option<plan::TimeMachineVerb>> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };

    // `SELECT * FROM esker_checkpoints()` — a table function, and the only one.
    if let [table] = select.from.as_slice()
        && let TableFactor::Table {
            name,
            args: Some(_),
            ..
        } = &table.relation
    {
        let called = name.to_string().to_ascii_lowercase();
        if called == "esker_diff" {
            refuse_if(!table.joins.is_empty(), "a JOIN on esker_diff()")?;
            refuse_if(select.selection.is_some(), "a WHERE on esker_diff()")?;
            refuse_if(
                !matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]),
                "a target list on esker_diff() other than *",
            )?;
            let TableFactor::Table {
                args: Some(args), ..
            } = &table.relation
            else {
                unreachable!("matched with args above")
            };
            let arguments = table_function_arguments(args).ok_or_else(|| {
                SqlError::unsupported("esker_diff() with arguments that are not string literals")
            })?;
            return Ok(Some(match arguments.as_slice() {
                [table, from] => plan::TimeMachineVerb::Diff {
                    table: fold_identifier(table, true).0,
                    from: from.clone(),
                    to: None,
                },
                [table, from, to] => plan::TimeMachineVerb::Diff {
                    table: fold_identifier(table, true).0,
                    from: from.clone(),
                    to: Some(to.clone()),
                },
                _ => {
                    return Err(SqlError::unsupported(
                        "esker_diff() with a number of arguments other than two or three",
                    ));
                }
            }));
        }
        if called == "esker_schema_jobs" {
            refuse_if(!table.joins.is_empty(), "a JOIN on esker_schema_jobs()")?;
            refuse_if(select.selection.is_some(), "a WHERE on esker_schema_jobs()")?;
            refuse_if(
                !matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]),
                "a target list on esker_schema_jobs() other than *",
            )?;
            return Ok(Some(plan::TimeMachineVerb::ListSchemaJobs));
        }
        if called == "esker_checkpoints" {
            // Nothing else may be attached: this returns what it returns, and a `WHERE` silently
            // ignored would answer a different question than the one asked.
            refuse_if(!table.joins.is_empty(), "a JOIN on esker_checkpoints()")?;
            refuse_if(select.selection.is_some(), "a WHERE on esker_checkpoints()")?;
            refuse_if(
                !matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]),
                "a target list on esker_checkpoints() other than *",
            )?;
            return Ok(Some(plan::TimeMachineVerb::ListCheckpoints));
        }
        return Ok(None);
    }

    // `SELECT pg_export_snapshot()` and friends — a scalar call, and no `FROM` at all.
    if !select.from.is_empty() {
        return Ok(None);
    }
    let [SelectItem::UnnamedExpr(Expr::Function(function))] = select.projection.as_slice() else {
        return Ok(None);
    };
    let called = function.name.to_string().to_ascii_lowercase();
    let arguments = verb_arguments(function);
    Ok(match (called.as_str(), arguments.as_deref()) {
        ("pg_export_snapshot", Some([])) => {
            Some(plan::TimeMachineVerb::ExportSnapshot { name: None })
        }
        ("esker_checkpoint", Some([name])) => Some(plan::TimeMachineVerb::ExportSnapshot {
            name: Some(name.clone()),
        }),
        ("esker_drop_checkpoint", Some([name])) => {
            Some(plan::TimeMachineVerb::DropCheckpoint { name: name.clone() })
        }
        ("esker_schema_step", Some([index])) => Some(plan::TimeMachineVerb::SchemaStep {
            // An index *name*, folded the way every relation name is — unlike a checkpoint's,
            // which is a string literal PostgreSQL would not fold.
            index: fold_identifier(index, false).0,
        }),
        // A verb called with the wrong arguments is PostgreSQL's `42883`, not a silent fallthrough
        // to "that column does not exist".
        (
            "pg_export_snapshot"
            | "esker_checkpoint"
            | "esker_drop_checkpoint"
            | "esker_schema_step",
            _,
        ) => {
            return Err(SqlError::unsupported(format!(
                "{called} with these arguments"
            )));
        }
        _ => None,
    })
}

/// A table function's arguments as strings, or `None` when any is not a plain string literal.
///
/// Same rule as [`verb_arguments`] and a different AST shape: a table function's arguments hang
/// off the `FROM` item rather than off a projected expression.
fn table_function_arguments(args: &sqlparser::ast::TableFunctionArgs) -> Option<Vec<String>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};

    if args.settings.is_some() {
        return None;
    }
    args.args
        .iter()
        .map(|argument| match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) => match &value.value {
                Value::SingleQuotedString(text) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A verb's arguments as strings, or `None` when any of them is not a plain string literal.
///
/// Every argument these verbs take is a name, and a name is a literal. An expression would have to
/// be evaluated, and a verb whose argument depended on a row is not a verb.
fn verb_arguments(function: &sqlparser::ast::Function) -> Option<Vec<String>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};

    let FunctionArguments::List(list) = &function.args else {
        return Some(Vec::new());
    };
    if !list.clauses.is_empty() {
        return None;
    }
    list.args
        .iter()
        .map(|argument| match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) => match &value.value {
                Value::SingleQuotedString(text) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `SET`, of which this node executes two spellings and refuses the rest by name.
///
/// The two are PostgreSQL's own (`docs/plans/phase-6d.md` §1): a namespaced custom GUC, which a
/// real server accepts and stores, and `SET TRANSACTION SNAPSHOT`, which a real server *acts* on
/// and whose every precondition is one this feature wants anyway.
fn lower_set(set: &sqlparser::ast::Set) -> Result<plan::Statement> {
    use sqlparser::ast::{ContextModifier, Set};

    match set {
        Set::SingleAssignment {
            scope,
            hivevar,
            variable,
            values,
        } if guc_name(variable).as_deref() == Some(time_machine::READ_AS_OF) => {
            refuse_if(*hivevar, "SET HIVEVAR")?;
            let [value] = values.as_slice() else {
                // PostgreSQL takes a list for some parameters. Not for this one, and a list
                // silently taking its first element would honour something nobody wrote.
                return Err(SqlError::unsupported(
                    "a list of values for esker.read_as_of",
                ));
            };
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetReadAsOf {
                    value: guc_value(value),
                    local: matches!(scope, Some(ContextModifier::Local)),
                },
            ))
        }
        Set::SetTransaction {
            modes,
            snapshot: Some(snapshot),
            session,
        } => {
            refuse_if(*session, "SET SESSION CHARACTERISTICS ... SNAPSHOT")?;
            // PostgreSQL's grammar admits both in one statement; nothing here reads the modes,
            // and a mode silently dropped is a mode the user asked for and did not get.
            refuse_if(!modes.is_empty(), "SET TRANSACTION SNAPSHOT with modes")?;
            let Value::SingleQuotedString(id) = &snapshot.value else {
                return Err(SqlError::InvalidSnapshotIdentifier(snapshot.to_string()));
            };
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetSnapshot(id.clone()),
            ))
        }
        other => Err(SqlError::unsupported(set_feature_name(other))),
    }
}

/// `SHOW <parameter>`. Only the one this node has; every other name is PostgreSQL's `42704`.
///
/// A `SHOW` of an unknown parameter is *not* contract C2's `0A000`: the statement is one this node
/// executes, and what is missing is the parameter, which is the condition PostgreSQL reports.
fn lower_show(variable: &[Ident]) -> Result<plan::Statement> {
    // `SHOW esker.read_as_of` arrives as two idents, because the parser splits on the dot.
    let name = variable
        .iter()
        .map(|ident| ident.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    if name.eq_ignore_ascii_case(time_machine::READ_AS_OF) {
        return Ok(plan::Statement::Session(
            plan::SessionStatement::ShowReadAsOf,
        ));
    }
    if name.is_empty() || name.eq_ignore_ascii_case("ALL") {
        return Err(SqlError::unsupported("SHOW ALL"));
    }
    Err(SqlError::UnrecognizedParameter(name))
}

/// `RESET <parameter>` — `SET <parameter> = DEFAULT` by another name, and PostgreSQL treats them
/// as the same operation.
fn lower_reset(reset: &sqlparser::ast::ResetStatement) -> Result<plan::Statement> {
    use sqlparser::ast::Reset;

    match &reset.reset {
        Reset::ConfigurationParameter(name)
            if guc_name(name).as_deref() == Some(time_machine::READ_AS_OF) =>
        {
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetReadAsOf {
                    value: None,
                    local: false,
                },
            ))
        }
        Reset::ALL => Err(SqlError::unsupported("RESET ALL")),
        // Named with `guc_name` rather than `object_name`: a parameter is not a relation, so a
        // qualified one must not be refused as "a qualified name". `esker.no_such_thing` is a
        // parameter this node does not have, which is `42704` and not a feature gap.
        Reset::ConfigurationParameter(name) => Err(SqlError::UnrecognizedParameter(
            guc_name(name).unwrap_or_else(|| name.to_string()),
        )),
    }
}

/// A parameter name as PostgreSQL spells it, lower-cased: GUC names are case-insensitive, and a
/// namespaced one arrives as two parts.
fn guc_name(name: &ObjectName) -> Option<String> {
    let joined = name
        .0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
        .collect::<Option<Vec<_>>>()?
        .join(".");
    Some(joined.to_ascii_lowercase())
}

/// The text a `SET` assigns, or `None` for `DEFAULT`.
///
/// A bare identifier that is not `DEFAULT` is a value PostgreSQL would take unquoted; taking it
/// here keeps `SET esker.read_as_of TO now` from being a syntax-shaped surprise, and the value
/// grammar refuses it with `22023` a moment later, which is the right condition for it.
fn guc_value(value: &Expr) -> Option<String> {
    match value {
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("DEFAULT") => None,
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Some(text.clone()),
            other => Some(other.to_string()),
        },
        other => Some(other.to_string()),
    }
}

/// The feature name for a `SET` this node does not execute.
///
/// Rendered from the statement rather than from the AST variant, so a user is told the construct
/// they wrote — and a *named* parameter is named, because "SET is not supported" tells somebody
/// who set `search_path` nothing about which of their statements to remove.
fn set_feature_name(set: &sqlparser::ast::Set) -> String {
    use sqlparser::ast::Set;

    match set {
        Set::SingleAssignment { variable, .. } => match guc_name(variable) {
            Some(name) => format!("SET {name}"),
            None => "SET".to_owned(),
        },
        Set::SetTransaction { .. } => "SET TRANSACTION".to_owned(),
        other => feature_name(&Statement::Set(other.clone())),
    }
}

/// `ALTER TABLE t SET (<parameter> = <value>)`, of which this node has exactly one.
///
/// PostgreSQL's storage-parameter syntax, which is where a per-table knob belongs and which needs
/// no grammar of our own. Every parameter but `retention` is `0A000` **naming the parameter**: a
/// user who wrote `fillfactor` is told about `fillfactor`, not about `ALTER TABLE`.
fn lower_storage_parameters(
    options: &[sqlparser::ast::SqlOption],
) -> Result<plan::AlterTableAction> {
    use sqlparser::ast::SqlOption;

    let [SqlOption::KeyValue { key, value }] = options else {
        // More than one at a time would have to be applied atomically or not at all, and there is
        // only one parameter to combine it with.
        return Err(SqlError::unsupported(
            "ALTER TABLE ... SET with more than one storage parameter",
        ));
    };
    if !key.value.eq_ignore_ascii_case("retention") {
        return Err(SqlError::unsupported(format!(
            "the storage parameter {}",
            key.value
        )));
    }
    Ok(plan::AlterTableAction::SetRetention {
        retention_ms: lower_retention(value)?,
    })
}

/// `DEFAULT <expression>` on a column, as a constant of that column's type.
///
/// **Constants only, and the refusals are the interesting part.** PostgreSQL evaluates a default at
/// insert time and, for `ADD COLUMN`, decides between storing a *missing value* and rewriting every
/// row by asking one question: is the expression volatile? Measured on 19beta1 — `DEFAULT 'old'`
/// and `DEFAULT (1+1)` set `atthasmissing` and rewrite nothing; `DEFAULT random()` clears it and
/// rewrites the table.
///
/// This node stores a value and never rewrites, so it takes the half PostgreSQL does not rewrite
/// for and refuses the other half **by name**:
///
/// * a literal is read as the column's type, which is the same conversion an `INSERT` does, so
///   `DEFAULT 'x'` in an `int8` column is the same `22P02` it would be in a value list;
/// * a function call is refused as *volatile* even when it is not (`length('x')` is immutable),
///   because deciding otherwise needs a volatility catalog and guessing would store one row's
///   answer for every row;
/// * `(1+1)` is refused as an unfolded expression, which PostgreSQL folds. Naming it is the honest
///   answer: a folder is a feature, not an oversight to paper over.
///
/// `DEFAULT NULL` normalises to `None` — the same thing as no default, which is what PostgreSQL
/// makes of it too.
fn column_default(expr: &Expr, ty: ColumnType) -> Result<Option<Datum>> {
    let literal = match expr {
        Expr::Value(value) => &value.value,
        Expr::UnaryOp { .. } => {
            // A signed number: `DEFAULT -1`. Rendered back and read as the column's type, which is
            // how a negative literal reaches `Datum` everywhere else in this crate.
            return Datum::from_text(ty, &expr.to_string())
                .map(Some)
                .map_err(|_| default_not_constant(expr));
        }
        Expr::Function(_) => {
            return Err(SqlError::unsupported(format!(
                "DEFAULT {expr}, which may be volatile"
            )));
        }
        _ => return Err(default_not_constant(expr)),
    };
    match literal {
        Value::Null => Ok(None),
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
            Datum::from_text(ty, text).map(Some)
        }
        Value::Number(digits, _) => Datum::from_text(ty, digits).map(Some),
        Value::Boolean(flag) => {
            Datum::from_text(ty, if *flag { "true" } else { "false" }).map(Some)
        }
        other => Err(default_not_constant_text(&other.to_string())),
    }
}

fn default_not_constant(expr: &Expr) -> SqlError {
    default_not_constant_text(&expr.to_string())
}

fn default_not_constant_text(rendered: &str) -> SqlError {
    SqlError::unsupported(format!("DEFAULT {rendered}, which is not a constant"))
}

/// A retention value: an interval, `'forever'`, or `DEFAULT`.
///
/// The same interval grammar the read timestamp uses, because they are the same kind of quantity
/// and a user who learned `'-1h'` for one should not have to learn a second spelling for the
/// other. A retention is a *distance* and so is written without a sign; `'forever'` is the
/// sentinel and `DEFAULT` deletes the override, which is not the same as storing a zero.
fn lower_retention(value: &Expr) -> Result<Option<u64>> {
    let text = match value {
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("DEFAULT") => return Ok(None),
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text.clone(),
            // A bare number is milliseconds, which is the unit the record stores. Read here
            // rather than handed to the interval grammar, which requires a unit on purpose:
            // `'7'` with no unit is ambiguous and `7` alone is not.
            Value::Number(digits, _) => {
                return digits.parse::<u64>().map(Some).map_err(|_| {
                    SqlError::InvalidParameterValue {
                        name: "retention",
                        value: digits.clone(),
                    }
                });
            }
            other => other.to_string(),
        },
        other => other.to_string(),
    };
    if text.eq_ignore_ascii_case("forever") {
        return Ok(Some(crate::catalog::RETENTION_FOREVER));
    }
    time_machine::retention_ms(&text).map(Some)
}

/// The clauses of `CREATE TABLE` that change what the statement means, each refused by name.
///
/// Split from [`lower_create_table`] so that the sweep and the column walk are two things a reader
/// can take one at a time; together they are more than one screen, which is the point at which a
/// function stops being read and starts being skimmed.
fn refuse_create_table_clauses(create: &sqlparser::ast::CreateTable) -> Result<()> {
    refuse_if(create.or_replace, "CREATE OR REPLACE TABLE")?;
    refuse_if(create.temporary, "CREATE TEMPORARY TABLE")?;
    refuse_if(create.external, "CREATE EXTERNAL TABLE")?;
    refuse_if(create.global.is_some(), "CREATE GLOBAL/LOCAL TABLE")?;
    refuse_if(create.transient, "CREATE TRANSIENT TABLE")?;
    refuse_if(create.volatile, "CREATE VOLATILE TABLE")?;
    refuse_if(create.iceberg, "CREATE ICEBERG TABLE")?;
    refuse_if(create.query.is_some(), "CREATE TABLE ... AS")?;
    refuse_if(create.like.is_some(), "CREATE TABLE ... LIKE")?;
    refuse_if(create.clone.is_some(), "CREATE TABLE ... CLONE")?;
    refuse_if(create.inherits.is_some(), "CREATE TABLE ... INHERITS")?;
    refuse_if(
        create.partition_of.is_some(),
        "CREATE TABLE ... PARTITION OF",
    )?;
    refuse_if(
        create.partition_by.is_some(),
        "CREATE TABLE ... PARTITION BY",
    )?;
    refuse_if(create.on_commit.is_some(), "CREATE TABLE ... ON COMMIT")?;
    refuse_if(create.without_rowid, "CREATE TABLE ... WITHOUT ROWID")?;
    refuse_if(create.strict, "CREATE TABLE ... STRICT")?;
    refuse_if(create.comment.is_some(), "CREATE TABLE ... COMMENT")?;
    refuse_if(create.order_by.is_some(), "CREATE TABLE ... ORDER BY")?;
    refuse_if(create.cluster_by.is_some(), "CREATE TABLE ... CLUSTER BY")?;
    refuse_if(
        !matches!(create.table_options, CreateTableOptions::None),
        "CREATE TABLE ... WITH",
    )?;

    Ok(())
}

fn lower_create_table(create: &sqlparser::ast::CreateTable) -> Result<plan::CreateTable> {
    refuse_create_table_clauses(create)?;

    let name = object_name(&create.name)?;
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut primary_key = Vec::new();
    let mut primary_key_name = None;
    let mut unique = Vec::new();

    for column in &create.columns {
        let column_name = ident(&column.name);
        let ty = lower_type(&column.data_type)?;
        let mut not_null = false;
        let mut default = None;
        for option in &column.options {
            match &option.option {
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Null => {}
                ColumnOption::Default(expr) => default = column_default(expr, ty)?,
                ColumnOption::Unique(constraint) => {
                    refuse_if(
                        constraint.nulls_distinct != NullsDistinctOption::None,
                        "UNIQUE NULLS [NOT] DISTINCT",
                    )?;
                    unique.push(plan::UniqueConstraint {
                        name: option.name.as_ref().map(ident),
                        columns: vec![column_name.clone()],
                    });
                }
                ColumnOption::PrimaryKey(_) => {
                    primary_key.push(column_name.clone());
                    primary_key_name = primary_key_name.or_else(|| option.name.as_ref().map(ident));
                }
                other => return Err(SqlError::unsupported(column_option_name(other))),
            }
        }
        columns.push(plan::Column {
            name: column_name,
            ty,
            not_null,
            default,
        });
    }

    for constraint in &create.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => {
                refuse_if(key.index_name.is_some(), "PRIMARY KEY USING INDEX")?;
                primary_key.extend(index_columns(&key.columns)?);
                primary_key_name = primary_key_name.or_else(|| key.name.as_ref().map(ident));
            }
            TableConstraint::Unique(key) => {
                refuse_if(
                    key.nulls_distinct != NullsDistinctOption::None,
                    "UNIQUE NULLS [NOT] DISTINCT",
                )?;
                unique.push(plan::UniqueConstraint {
                    name: key.name.as_ref().map(ident),
                    columns: index_columns(&key.columns)?,
                });
            }
            TableConstraint::ForeignKey(_) => {
                return Err(SqlError::unsupported("FOREIGN KEY"));
            }
            TableConstraint::Check(_) => return Err(SqlError::unsupported("CHECK")),
            other => {
                return Err(SqlError::unsupported(format!(
                    "the table constraint {other}"
                )));
            }
        }
    }

    Ok(plan::CreateTable {
        name,
        if_not_exists: create.if_not_exists,
        columns,
        primary_key,
        primary_key_name,
        unique,
    })
}

/// `ALTER TABLE`, of which exactly one action is executed.
///
/// Everything else is named and refused (contract C2). The naming is done from *our* side rather
/// than from the AST's `Display`, because the name is what the user is told to change and
/// `sqlparser` renders an action with the identifiers the user wrote in it — a message that
/// echoes a column name back is a message that cannot be searched for.
fn lower_alter_table(alter: &sqlparser::ast::AlterTable) -> Result<plan::AlterTable> {
    // `ONLY` is about inheritance, which there is none of here; honouring it silently would be
    // honouring a word we do not implement.
    refuse_if(alter.only, "ALTER TABLE ONLY")?;
    refuse_if(alter.location.is_some(), "ALTER TABLE ... SET LOCATION")?;
    refuse_if(alter.on_cluster.is_some(), "ALTER TABLE ... ON CLUSTER")?;
    refuse_if(alter.table_type.is_some(), "ALTER of a table of that type")?;

    let mut actions = Vec::with_capacity(alter.operations.len());
    for operation in &alter.operations {
        if let AlterTableOperation::SetOptionsParens { options } = operation {
            actions.push(lower_storage_parameters(options)?);
            continue;
        }
        let AlterTableOperation::AddColumn {
            column_keyword: _,
            if_not_exists,
            column_def,
            column_position,
        } = operation
        else {
            return Err(SqlError::unsupported(alter_action_name(operation)));
        };
        // MySQL's `FIRST`/`AFTER c`. A column added anywhere but the end is a column the row
        // format cannot place, since a row is decoded by position.
        refuse_if(
            column_position.is_some(),
            "ALTER TABLE ... ADD COLUMN at a position",
        )?;
        let ty = lower_type(&column_def.data_type)?;
        let mut not_null = false;
        let mut default = None;
        for option in &column_def.options {
            let named = match &option.option {
                // A **constant** default is admitted: it is stored as the column's missing value
                // and the decoder pads with it, so no row is rewritten (ADR 0019's pad rule
                // generalised; `crate::catalog::ColumnDef::missing`). Volatility and unfolded
                // expressions are refused inside `column_default`, by name.
                ColumnOption::Default(expr) => {
                    default = column_default(expr, ty)?;
                    continue;
                }
                ColumnOption::NotNull => {
                    not_null = true;
                    continue;
                }
                ColumnOption::PrimaryKey(_) => "ALTER TABLE ... ADD COLUMN ... PRIMARY KEY",
                ColumnOption::Unique(_) => "ALTER TABLE ... ADD COLUMN ... UNIQUE",
                // `NULL` is the default and says nothing; honouring it is honouring nothing.
                ColumnOption::Null => continue,
                other => return Err(SqlError::unsupported(column_option_name(other))),
            };
            return Err(SqlError::unsupported(named));
        }
        // `NOT NULL` needs a value for every row already stored, and a constant default is exactly
        // that value. Without one there is nothing to pad with, and the alternative is a rewrite
        // this `ALTER` is defined not to do — so it stays refused, and the refusal names the pair
        // rather than the keyword, because `NOT NULL DEFAULT 7` *is* accepted.
        if not_null && default.is_none() {
            return Err(SqlError::unsupported(
                "ALTER TABLE ... ADD COLUMN ... NOT NULL without a DEFAULT",
            ));
        }
        actions.push(plan::AlterTableAction::AddColumn {
            column: plan::Column {
                name: ident(&column_def.name),
                ty,
                not_null,
                default,
            },
            if_not_exists: *if_not_exists,
        });
    }

    Ok(plan::AlterTable {
        name: object_name(&alter.name)?,
        if_exists: alter.if_exists,
        actions,
    })
}

/// What to call an `ALTER TABLE` action the executor does not run.
///
/// The three column actions are named the way PostgreSQL's own documentation names them, because
/// they are the ones a user of this subset actually reaches. The rest fall back to the action's
/// leading keywords, which is the same rule [`crate::parse::feature_name`] uses for a statement.
fn alter_action_name(operation: &AlterTableOperation) -> String {
    match operation {
        AlterTableOperation::DropColumn { .. } => "ALTER TABLE ... DROP COLUMN".into(),
        AlterTableOperation::RenameColumn { .. } => "ALTER TABLE ... RENAME COLUMN".into(),
        AlterTableOperation::AlterColumn { .. } => "ALTER TABLE ... ALTER COLUMN".into(),
        AlterTableOperation::RenameTable { .. } => "ALTER TABLE ... RENAME TO".into(),
        AlterTableOperation::AddConstraint { .. } => "ALTER TABLE ... ADD CONSTRAINT".into(),
        AlterTableOperation::DropConstraint { .. } => "ALTER TABLE ... DROP CONSTRAINT".into(),
        other => {
            let rendered = other.to_string();
            let words = rendered
                .split_whitespace()
                .take_while(|word| word.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            if words.is_empty() {
                "this ALTER TABLE action".to_owned()
            } else {
                format!("ALTER TABLE ... {words}")
            }
        }
    }
}

fn lower_create_index(create: &sqlparser::ast::CreateIndex) -> Result<plan::CreateIndex> {
    refuse_if(!create.include.is_empty(), "CREATE INDEX ... INCLUDE")?;
    refuse_if(
        create.nulls_distinct.is_some(),
        "CREATE INDEX ... NULLS [NOT] DISTINCT",
    )?;
    refuse_if(!create.with.is_empty(), "CREATE INDEX ... WITH")?;
    refuse_if(create.predicate.is_some(), "a partial index")?;
    refuse_if(
        !create.index_options.is_empty(),
        "CREATE INDEX with options",
    )?;
    refuse_if(
        !create.alter_options.is_empty(),
        "CREATE INDEX with table options",
    )?;
    if let Some(using) = &create.using {
        // Every index here is a range of the ordered key space, which is what a btree is. Saying
        // `USING hash` and getting one would be a different index than the user asked for.
        refuse_if(
            !matches!(using, IndexType::BTree),
            format!("an index USING {using}"),
        )?;
    }
    Ok(plan::CreateIndex {
        name: create.name.as_ref().map(object_name).transpose()?,
        table: object_name(&create.table_name)?,
        columns: index_columns(&create.columns)?,
        unique: create.unique,
        if_not_exists: create.if_not_exists,
        concurrently: create.concurrently,
    })
}

fn lower_insert(insert: &sqlparser::ast::Insert) -> Result<plan::Insert> {
    refuse_if(insert.or.is_some(), "INSERT OR")?;
    refuse_if(insert.ignore, "INSERT IGNORE")?;
    refuse_if(insert.overwrite, "INSERT OVERWRITE")?;
    refuse_if(insert.replace_into, "REPLACE INTO")?;
    refuse_if(insert.on.is_some(), "INSERT ... ON CONFLICT")?;
    refuse_if(insert.returning.is_some(), "INSERT ... RETURNING")?;
    refuse_if(insert.table_alias.is_some(), "INSERT ... AS")?;
    refuse_if(insert.partitioned.is_some(), "INSERT ... PARTITION")?;
    refuse_if(!insert.after_columns.is_empty(), "INSERT ... AFTER")?;
    refuse_if(!insert.assignments.is_empty(), "INSERT ... SET")?;
    refuse_if(insert.priority.is_some(), "an INSERT priority")?;
    refuse_if(insert.insert_alias.is_some(), "INSERT ... AS")?;
    refuse_if(insert.settings.is_some(), "INSERT ... SETTINGS")?;
    refuse_if(insert.format_clause.is_some(), "INSERT ... FORMAT")?;
    refuse_if(insert.output.is_some(), "INSERT ... OUTPUT")?;
    refuse_if(
        insert.multi_table_insert_type.is_some() || !insert.multi_table_into_clauses.is_empty(),
        "a multi-table INSERT",
    )?;
    refuse_if(!insert.optimizer_hints.is_empty(), "an optimizer hint")?;

    let table = match &insert.table {
        TableObject::TableName(name) => object_name(name)?,
        other => return Err(SqlError::unsupported(format!("INSERT INTO {other}"))),
    };
    let columns = if insert.columns.is_empty() {
        None
    } else {
        Some(
            insert
                .columns
                .iter()
                .map(object_name)
                .collect::<Result<Vec<_>>>()?,
        )
    };

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| SqlError::unsupported("INSERT with no source"))?;
    refuse_if(source.with.is_some(), "INSERT ... WITH")?;
    refuse_if(source.order_by.is_some(), "INSERT ... ORDER BY")?;
    refuse_if(source.limit_clause.is_some(), "INSERT ... LIMIT")?;
    refuse_if(source.fetch.is_some(), "INSERT ... FETCH")?;
    refuse_if(!source.locks.is_empty(), "INSERT with a locking clause")?;

    let SetExpr::Values(values) = source.body.as_ref() else {
        // `INSERT INTO t SELECT ...` needs the query executor, which unit 6c brings.
        return Err(SqlError::unsupported("INSERT ... SELECT"));
    };
    refuse_if(values.explicit_row, "INSERT ... VALUES ROW(...)")?;

    let rows = values
        .rows
        .iter()
        .map(|row| row.iter().map(lower_expr).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    Ok(plan::Insert {
        table,
        columns,
        rows,
    })
}

/// An expression, as far as phase 6a's `VALUES` needs one.
///
/// A negative number arrives as a unary minus over a positive literal, which is folded here so
/// that `-1` is one literal rather than an operator this crate would otherwise have to run.
fn lower_expr(expr: &Expr) -> Result<plan::Expr> {
    match expr {
        Expr::Value(value) => lower_value(&value.value, false),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => lower_value(&value.value, true),
            other => Err(SqlError::unsupported(format!("the expression -{other}"))),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => lower_expr(expr),
        Expr::Nested(inner) => lower_expr(inner),
        Expr::Identifier(name) => Ok(plan::Expr::Column {
            table: None,
            name: ident(name),
        }),
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            // `t.a`. The qualifier is carried, not dropped: the planner checks it against the
            // query's tables, which is the only way `SELECT wrong.a FROM t` can be the `42P01` a
            // real server gives.
            [table, column] => Ok(plan::Expr::Column {
                table: Some(ident(table)),
                name: ident(column),
            }),
            _ => Err(SqlError::unsupported(format!(
                "the qualified column {}",
                parts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(".")
            ))),
        },
        Expr::IsNull(operand) => Ok(plan::Expr::IsNull {
            operand: Box::new(lower_expr(operand)?),
            negated: false,
        }),
        Expr::IsNotNull(operand) => Ok(plan::Expr::IsNull {
            operand: Box::new(lower_expr(operand)?),
            negated: true,
        }),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(plan::Expr::Not(Box::new(lower_expr(expr)?))),
        Expr::BinaryOp { op, left, right } => {
            let op = match op {
                BinaryOperator::Eq => plan::BinaryOp::Eq,
                BinaryOperator::NotEq => plan::BinaryOp::NotEq,
                BinaryOperator::Lt => plan::BinaryOp::Lt,
                BinaryOperator::LtEq => plan::BinaryOp::LtEq,
                BinaryOperator::Gt => plan::BinaryOp::Gt,
                BinaryOperator::GtEq => plan::BinaryOp::GtEq,
                BinaryOperator::And => plan::BinaryOp::And,
                BinaryOperator::Or => plan::BinaryOp::Or,
                other => {
                    return Err(SqlError::unsupported(format!("the operator {other}")));
                }
            };
            Ok(plan::Expr::Binary {
                op,
                left: Box::new(lower_expr(left)?),
                right: Box::new(lower_expr(right)?),
            })
        }
        other => Err(SqlError::unsupported(format!("the expression {other}"))),
    }
}

fn lower_value(value: &Value, negated: bool) -> Result<plan::Expr> {
    let sign = if negated { "-" } else { "" };
    let literal = match value {
        Value::Null => plan::Literal::Null,
        Value::Boolean(value) => plan::Literal::Bool(*value),
        Value::Number(digits, _) => {
            let text = format!("{sign}{digits}");
            if digits.contains(['.', 'e', 'E']) {
                plan::Literal::Decimal(text)
            } else {
                // `bigint out of range`, not the input function's longer message: a literal
                // too large is caught on a different path in PostgreSQL and says so differently.
                plan::Literal::Integer(
                    text.parse()
                        .map_err(|_| SqlError::IntegerLiteralOutOfRange)?,
                )
            }
        }
        Value::SingleQuotedString(text)
        | Value::DollarQuotedString(DollarQuotedString { value: text, .. }) => {
            refuse_if(negated, "a negated string literal")?;
            plan::Literal::String(text.clone())
        }
        Value::Placeholder(name) => {
            refuse_if(negated, "a negated parameter")?;
            let number = name
                .strip_prefix('$')
                .and_then(|digits| digits.parse().ok())
                .ok_or_else(|| SqlError::unsupported(format!("the placeholder {name}")))?;
            return Ok(plan::Expr::Parameter(number));
        }
        other => return Err(SqlError::unsupported(format!("the literal {other}"))),
    };
    Ok(plan::Expr::Literal(literal))
}

#[allow(
    clippy::too_many_lines,
    reason = "most of it is the refusal list, which is the point: one line per clause not honoured"
)]
fn lower_query(query: &Query) -> Result<plan::Select> {
    refuse_if(query.with.is_some(), "WITH")?;
    refuse_if(!query.locks.is_empty(), "a row-level locking clause")?;
    refuse_if(query.fetch.is_some(), "FETCH FIRST")?;
    refuse_if(query.for_clause.is_some(), "FOR XML/JSON")?;
    refuse_if(query.settings.is_some(), "SETTINGS")?;
    refuse_if(query.format_clause.is_some(), "FORMAT")?;
    refuse_if(!query.pipe_operators.is_empty(), "a pipe operator")?;

    let SetExpr::Select(select) = query.body.as_ref() else {
        // `VALUES (...)`, `UNION`, `TABLE t` after the rewrite -- each is its own feature.
        return Err(SqlError::unsupported(match query.body.as_ref() {
            SetExpr::Values(_) => "a bare VALUES list".to_owned(),
            SetExpr::SetOperation { op, .. } => format!("{op}"),
            other => format!("the query body {other}"),
        }));
    };

    refuse_if(select.distinct.is_some(), "SELECT DISTINCT")?;
    refuse_if(select.top.is_some(), "SELECT TOP")?;
    refuse_if(select.into.is_some(), "SELECT INTO")?;
    refuse_if(!select.lateral_views.is_empty(), "a LATERAL VIEW")?;
    refuse_if(select.prewhere.is_some(), "PREWHERE")?;
    refuse_if(
        !matches!(select.group_by, GroupByExpr::Expressions(ref e, ref m) if e.is_empty() && m.is_empty()),
        "GROUP BY",
    )?;
    refuse_if(!select.cluster_by.is_empty(), "CLUSTER BY")?;
    refuse_if(!select.distribute_by.is_empty(), "DISTRIBUTE BY")?;
    refuse_if(!select.sort_by.is_empty(), "SORT BY")?;
    refuse_if(select.having.is_some(), "HAVING")?;
    refuse_if(!select.named_window.is_empty(), "WINDOW")?;
    refuse_if(select.qualify.is_some(), "QUALIFY")?;
    refuse_if(!select.connect_by.is_empty(), "CONNECT BY")?;
    refuse_if(select.value_table_mode.is_some(), "a value table")?;
    refuse_if(select.window_before_qualify, "WINDOW before QUALIFY")?;
    refuse_if(select.exclude.is_some(), "EXCLUDE")?;
    refuse_if(!select.optimizer_hints.is_empty(), "an optimizer hint")?;

    let (from, join) = match select.from.as_slice() {
        [] => (None, None),
        [table] => {
            let left = table_factor(&table.relation)?;
            let join = match table.joins.as_slice() {
                [] => None,
                [one] => Some(lower_join(one)?),
                _ => return Err(SqlError::unsupported("more than one JOIN")),
            };
            (Some(left), join)
        }
        // `FROM a, b` is a cross join in PostgreSQL, and writing it that way is how a user asks
        // for one. Refused by name rather than lowered to a cross join, because the comma form
        // usually means a `WHERE` was meant to join them and saying so is more useful than
        // running the cartesian product.
        _ => return Err(SqlError::unsupported("a comma-separated FROM list")),
    };

    let projection = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) => Ok(plan::SelectItem::Expr {
                expr: lower_expr(expr)?,
                alias: None,
            }),
            SelectItem::ExprWithAlias { expr, alias } => Ok(plan::SelectItem::Expr {
                expr: lower_expr(expr)?,
                alias: Some(ident(alias)),
            }),
            SelectItem::Wildcard(options) => {
                refuse_if(options.opt_exclude.is_some(), "SELECT * EXCLUDE")?;
                refuse_if(options.opt_except.is_some(), "SELECT * EXCEPT")?;
                refuse_if(options.opt_replace.is_some(), "SELECT * REPLACE")?;
                refuse_if(options.opt_rename.is_some(), "SELECT * RENAME")?;
                Ok(plan::SelectItem::Wildcard)
            }
            SelectItem::QualifiedWildcard(kind, options) => {
                refuse_if(options.opt_ilike.is_some(), "SELECT * ILIKE")?;
                refuse_if(options.opt_exclude.is_some(), "SELECT * EXCLUDE")?;
                refuse_if(options.opt_except.is_some(), "SELECT * EXCEPT")?;
                refuse_if(options.opt_replace.is_some(), "SELECT * REPLACE")?;
                refuse_if(options.opt_rename.is_some(), "SELECT * RENAME")?;
                match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        Ok(plan::SelectItem::QualifiedWildcard(object_name(name)?))
                    }
                    // `STRUCT('x').*` and friends: an expression, not a table.
                    SelectItemQualifiedWildcardKind::Expr(_) => {
                        Err(SqlError::unsupported("a SELECT * over an expression"))
                    }
                }
            }
            SelectItem::ExprWithAliases { .. } => {
                Err(SqlError::unsupported("a multi-column alias"))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let filter = select.selection.as_ref().map(lower_expr).transpose()?;

    let order_by = match &query.order_by {
        None => Vec::new(),
        Some(order_by) => {
            let OrderByKind::Expressions(exprs) = &order_by.kind else {
                return Err(SqlError::unsupported("ORDER BY ALL"));
            };
            refuse_if(order_by.interpolate.is_some(), "INTERPOLATE")?;
            exprs
                .iter()
                .map(|item| {
                    refuse_if(item.with_fill.is_some(), "WITH FILL")?;
                    Ok(plan::OrderItem {
                        expr: lower_expr(&item.expr)?,
                        descending: item.options.asc == Some(false),
                        nulls_first: item.options.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
    };

    let (limit, offset) = match &query.limit_clause {
        None => (None, None),
        Some(LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            refuse_if(!limit_by.is_empty(), "LIMIT BY")?;
            let offset = match offset {
                None => None,
                Some(offset) => {
                    refuse_if(
                        !matches!(offset.rows, OffsetRows::None | OffsetRows::Rows),
                        "OFFSET ... ROW",
                    )?;
                    Some(lower_expr(&offset.value)?)
                }
            };
            (limit.as_ref().map(lower_expr).transpose()?, offset)
        }
        Some(other) => return Err(SqlError::unsupported(format!("the limit clause {other}"))),
    };

    Ok(plan::Select {
        from,
        join,
        projection,
        filter,
        order_by,
        limit,
        offset,
    })
}

/// One `JOIN`, lowered. Every join this crate does not run is named rather than approximated.
fn lower_join(join: &sqlparser::ast::Join) -> Result<plan::Join> {
    let table = table_factor(&join.relation)?;
    refuse_if(join.global, "a GLOBAL JOIN")?;
    let on = match &join.join_operator {
        JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
            join_constraint(constraint)?
        }
        JoinOperator::CrossJoin(constraint) => join_constraint(constraint)?,
        // Each of these keeps rows an inner join drops, so running one as an inner join would
        // silently return fewer rows than the user asked for -- the worst thing a join can do.
        other => {
            return Err(SqlError::unsupported(match other {
                JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => "a LEFT JOIN",
                JoinOperator::Right(_) | JoinOperator::RightOuter(_) => "a RIGHT JOIN",
                JoinOperator::FullOuter(_) => "a FULL JOIN",
                JoinOperator::LeftSemi(_) | JoinOperator::Semi(_) => "a SEMI JOIN",
                JoinOperator::LeftAnti(_) | JoinOperator::Anti(_) => "an ANTI JOIN",
                JoinOperator::CrossApply | JoinOperator::OuterApply => "APPLY",
                JoinOperator::AsOf { .. } => "an ASOF JOIN",
                JoinOperator::StraightJoin(_) => "a STRAIGHT_JOIN",
                _ => "this join type",
            }));
        }
    };
    Ok(plan::Join { table, on })
}

fn join_constraint(constraint: &JoinConstraint) -> Result<Option<plan::Expr>> {
    match constraint {
        JoinConstraint::On(expr) => Ok(Some(lower_expr(expr)?)),
        // `USING (a)` also *merges* the two columns into one in the output, which is a projection
        // rule and not only a condition; running it as `ON a.x = b.x` would give the wrong number
        // of columns for `SELECT *`.
        JoinConstraint::Using(_) => Err(SqlError::unsupported("a JOIN ... USING clause")),
        JoinConstraint::Natural => Err(SqlError::unsupported("a NATURAL JOIN")),
        JoinConstraint::None => Ok(None),
    }
}

fn table_factor(factor: &TableFactor) -> Result<String> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            partitions,
            ..
        } => {
            refuse_if(alias.is_some(), "a table alias")?;
            refuse_if(args.is_some(), "a table function")?;
            refuse_if(!with_hints.is_empty(), "a table hint")?;
            refuse_if(version.is_some(), "a table version")?;
            refuse_if(!partitions.is_empty(), "a partition list")?;
            object_name(name)
        }
        other => Err(SqlError::unsupported(format!("the FROM item {other}"))),
    }
}

fn lower_update(update: &sqlparser::ast::Update) -> Result<plan::Update> {
    refuse_if(update.from.is_some(), "UPDATE ... FROM")?;
    refuse_if(update.returning.is_some(), "UPDATE ... RETURNING")?;
    refuse_if(update.output.is_some(), "UPDATE ... OUTPUT")?;
    refuse_if(update.or.is_some(), "UPDATE OR")?;
    refuse_if(!update.order_by.is_empty(), "UPDATE ... ORDER BY")?;
    refuse_if(update.limit.is_some(), "UPDATE ... LIMIT")?;
    refuse_if(!update.optimizer_hints.is_empty(), "an optimizer hint")?;
    refuse_if(!update.table.joins.is_empty(), "a JOIN in UPDATE")?;

    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let name = match &assignment.target {
                AssignmentTarget::ColumnName(name) => object_name(name)?,
                other @ AssignmentTarget::Tuple(_) => {
                    return Err(SqlError::unsupported(format!(
                        "the assignment target {other}"
                    )));
                }
            };
            Ok((name, lower_expr(&assignment.value)?))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(plan::Update {
        table: table_factor(&update.table.relation)?,
        assignments,
        filter: update.selection.as_ref().map(lower_expr).transpose()?,
    })
}

fn lower_delete(delete: &sqlparser::ast::Delete) -> Result<plan::Delete> {
    refuse_if(!delete.tables.is_empty(), "a multi-table DELETE")?;
    refuse_if(delete.using.is_some(), "DELETE ... USING")?;
    refuse_if(delete.returning.is_some(), "DELETE ... RETURNING")?;
    refuse_if(!delete.order_by.is_empty(), "DELETE ... ORDER BY")?;
    refuse_if(delete.limit.is_some(), "DELETE ... LIMIT")?;
    refuse_if(delete.output.is_some(), "DELETE ... OUTPUT")?;
    refuse_if(!delete.optimizer_hints.is_empty(), "an optimizer hint")?;

    let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) = &delete.from;
    let [table] = tables.as_slice() else {
        return Err(SqlError::unsupported("a multi-table DELETE"));
    };
    refuse_if(!table.joins.is_empty(), "a JOIN in DELETE")?;

    Ok(plan::Delete {
        table: table_factor(&table.relation)?,
        filter: delete.selection.as_ref().map(lower_expr).transpose()?,
    })
}

/// The six types, under every spelling PostgreSQL accepts for them.
fn lower_type(data_type: &DataType) -> Result<ColumnType> {
    Ok(match data_type {
        DataType::Int8(None) | DataType::BigInt(None) => ColumnType::Int8,
        DataType::Text => ColumnType::Text,
        DataType::Bool | DataType::Boolean => ColumnType::Bool,
        DataType::Bytea => ColumnType::Bytea,
        DataType::Float8 | DataType::DoublePrecision | DataType::Double(ExactNumberInfo::None) => {
            ColumnType::Double
        }
        DataType::Timestamp(None, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::TimestampTz
        }
        other => return Err(SqlError::unsupported(format!("the type {other}"))),
    })
}

/// An index's columns, which must be plain names: an expression index is a different feature.
fn index_columns(columns: &[IndexColumn]) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|column| {
            refuse_if(column.operator_class.is_some(), "an index operator class")?;
            refuse_if(
                column.column.options.asc == Some(false),
                "a DESC index column",
            )?;
            refuse_if(
                column.column.options.nulls_first.is_some(),
                "NULLS FIRST/LAST on an index",
            )?;
            refuse_if(column.column.with_fill.is_some(), "WITH FILL")?;
            match &column.column.expr {
                Expr::Identifier(name) => Ok(ident(name)),
                other => Err(SqlError::unsupported(format!(
                    "the index expression {other}"
                ))),
            }
        })
        .collect()
}

/// A name, folded and truncated the way PostgreSQL stores it. Schema qualification is refused
/// rather than ignored: `other.t` and `t` are different tables and answering about the second
/// would be a wrong answer, not a missing feature.
fn object_name(name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(ident)
            .ok_or_else(|| SqlError::unsupported(format!("the name {name}"))),
        _ => Err(SqlError::unsupported(format!("the qualified name {name}"))),
    }
}

/// An identifier, folded unless it was quoted -- which is the only thing `quote_style` is for.
fn ident(ident: &Ident) -> String {
    fold_identifier(&ident.value, ident.quote_style.is_some()).0
}

fn refuse_if(condition: bool, feature: impl Into<String>) -> Result<()> {
    if condition {
        return Err(SqlError::unsupported(feature));
    }
    Ok(())
}

fn column_option_name(option: &ColumnOption) -> String {
    match option {
        ColumnOption::Default(_) => "DEFAULT".into(),
        ColumnOption::ForeignKey(_) => "REFERENCES".into(),
        ColumnOption::Check(_) => "CHECK".into(),
        ColumnOption::Generated { .. } => "GENERATED".into(),
        ColumnOption::Identity(_) => "IDENTITY".into(),
        ColumnOption::Collation(name) => format!("COLLATE {name}"),
        ColumnOption::Comment(_) => "COMMENT".into(),
        other => format!("the column option {other}"),
    }
}
