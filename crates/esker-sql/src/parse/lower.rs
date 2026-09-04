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
    AlterTableOperation, AssignmentTarget, BinaryOperator, CharacterLength, ColumnOption,
    ConstraintReferenceMatchKind, CreateTableOptions, DataType, DeferrableInitial, Distinct,
    DollarQuotedString, ExactNumberInfo, Expr, FromTable, FunctionArg, FunctionArgExpr,
    GeneratedAs, GroupByExpr, Ident, IndexColumn, IndexType, JoinConstraint, JoinOperator,
    LimitClause, NullsDistinctOption, ObjectName, ObjectType, OffsetRows, OrderByKind, Query,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, TableConstraint, TableFactor,
    TableObject, TimezoneInfo, UnaryOperator, Value,
};

use crate::catalog::{self, KeyOrder, fold_identifier};
use crate::catalog::{TypeField, TypeKind};
use crate::error::{Result, SqlError};
use crate::parse::{Parsed, feature_name};
use crate::plan;
use crate::time_machine;
use crate::value::PgDatum;
use crate::value::{self, ColumnType, Datum, NO_TYPMOD, PgType};

impl Parsed {
    /// Lowers this statement into the plan types the executor runs, or names the construct that
    /// stopped it (contract C2).
    pub fn lower(&self) -> Result<plan::Statement> {
        // **Deep once, then deep properly.** `lower_expr` stops at `INLINE_LOWER_DEPTH` on the
        // caller's stack, which is a tokio worker's 2 MiB. A statement past it is not refused —
        // it is lowered again on a thread with room for `MAX_NESTING_DEPTH` levels, exactly as
        // `crate::parse` re-parses a deeply nested statement on one. So the limit a client meets
        // is the parser's, and the worker's stack decides only *where* the work happens.
        match self.lower_inline() {
            Err(SqlError::StatementTooComplex) => self.lower_on_a_deep_stack(),
            other => other,
        }
    }

    /// [`Parsed::lower`] on a thread sized for the full depth, and only for a statement that needs
    /// it. A thread spawn costs tens of microseconds; the statement is about to become a
    /// distributed transaction.
    fn lower_on_a_deep_stack(&self) -> Result<plan::Statement> {
        // **Borrowed, not cloned.** `Parsed` holds `sqlparser`'s tree, and `Clone` on that tree is
        // as recursive as lowering it — cloning a 500-term chain to hand it to the deep thread
        // overflowed the very stack this exists to get off. A scoped thread borrows it instead, so
        // nothing walks the tree on the caller's stack.
        std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("esker-sql-lower".into())
                .stack_size(crate::parse::DEEP_PARSE_STACK_BYTES)
                .spawn_scoped(scope, || {
                    LOWER_LIMIT.with(|cell| cell.set(crate::parse::MAX_PLAN_DEPTH));
                    self.lower_inline()
                })
                .map_err(|error| {
                    SqlError::Internal(format!("could not spawn a lowering thread: {error}"))
                })?;
            // A panic here is a bug in this crate and not something the client did, so it is an
            // internal error rather than a dropped connection — the reading
            // `parse_on_a_deep_stack` takes, and for the same invariant.
            worker
                .join()
                .map_err(|_| SqlError::Internal("the lowering thread panicked".into()))?
        })
    }

    fn lower_inline(&self) -> Result<plan::Statement> {
        // **Built here, not parsed.** `ALTER TABLE … SET { LOGGED | UNLOGGED }` was rewritten to a
        // placeholder because the parser has no `LOGGED` keyword, so the statement is reconstructed
        // from what the class recorded — and then travels the ordinary `ALTER TABLE` path.
        if let crate::parse::StatementClass::SetPersistence { table, persistence } = &self.class {
            return Ok(plan::Statement::AlterTable(plan::AlterTable {
                name: table.clone(),
                if_exists: false,
                actions: vec![plan::AlterTableAction::SetPersistence(*persistence)],
            }));
        }
        let mut lowered = lower_statement(&self.statement)?;
        // The one thing the parser could not carry (`crate::parse::Parsed::concurrently`).
        if let plan::Statement::DropIndex(drop) = &mut lowered {
            drop.concurrently = self.is_concurrently();
        }
        // And the clauses it could not read at all: an `EXCLUDE` constraint is cut out of the
        // source so the statement parses, and re-attached here from its own text.
        if let plan::Statement::CreateTable(create) = &mut lowered {
            if self.is_unlogged() {
                create.persistence = catalog::Persistence::Unlogged;
            }
            for clause in self.exclude_constraints() {
                create.excludes.push(crate::parse::parse_exclude_constraint(
                    clause,
                    &create.name,
                )?);
            }
        }
        if let plan::Statement::CreateDatabase(create) = &mut lowered {
            apply_database_options(create, self.database_options())?;
        }
        Ok(lowered)
    }
}

/// The one encoding this node speaks, which is what every database it has is in.
pub(crate) const ENCODING: &str = "UTF8";

/// The one collation, which sorts by byte value — a collation is a feature this node does not
/// have, so `C` is the honest name for what it does.
pub(crate) const COLLATION: &str = "C";

/// PostgreSQL 19's server encodings, so that a name it has and a name nobody has get the two
/// different errors PostgreSQL gives them: a name off this list is `42704 … is not a valid
/// encoding name`, and one on it that is not [`ENCODING`] is a refusal by name.
const ENCODING_NAMES: &[&str] = &[
    "BIG5",
    "EUC_CN",
    "EUC_JP",
    "EUC_JIS_2004",
    "EUC_KR",
    "EUC_TW",
    "GB18030",
    "GBK",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "JOHAB",
    "KOI8R",
    "KOI8U",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "MULE_INTERNAL",
    "SJIS",
    "SHIFT_JIS_2004",
    "SQL_ASCII",
    "UHC",
    "UTF8",
    "WIN866",
    "WIN874",
    "WIN1250",
    "WIN1251",
    "WIN1252",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1256",
    "WIN1257",
    "WIN1258",
];

/// The spellings PostgreSQL accepts for [`ENCODING`]. `UNICODE` is its documented alias and is
/// what some clients send.
const UTF8_SPELLINGS: &[&str] = &["UTF8", "UTF-8", "UNICODE"];

/// `CREATE DATABASE`'s options, as PostgreSQL answers them on a cluster with one encoding and one
/// collation.
///
/// **Every option PostgreSQL has is read here**, and what separates them is not whether this node
/// implements the option but whether a client could *tell* that it had not:
///
///   * `ENCODING`, `LC_COLLATE`, `LC_CTYPE` and `LOCALE` are **honoured**, which on this cluster
///     means checked against the one encoding and the one collation there are. They are not
///     recorded, because every database here has the same two values and a record of a constant is
///     a place for two answers to disagree.
///   * `TEMPLATE` travels to the executor, which is where the directory is.
///   * `OWNER` and `TABLESPACE` name facilities this node does not have, so every value gets
///     PostgreSQL's own `42704` for a name that is not there — which is true here of every name
///     but `pg_default`, the one that names the only storage there is.
///   * `STRATEGY` names *how* PostgreSQL copies a template and there is nothing to copy, so its
///     two valid values are accepted and have no effect, and a third is PostgreSQL's `22023`.
///   * `CONNECTION LIMIT`, `ALLOW_CONNECTIONS` and `IS_TEMPLATE` are **refused by name** unless
///     they ask for the default, because each of them is a promise a client can check: it would
///     connect past the limit, connect to a database declared closed, or copy from something that
///     is not a template. `CONNECTION LIMIT` in particular waits on the session registry that
///     `DROP DATABASE` of another session's database waits on.
///   * The ICU and locale-provider family is refused by name: this node has one provider and no
///     ICU at all.
///   * An option PostgreSQL does not have is its own `42601 option "x" not recognized`, which is a
///     **syntax** error there and not a feature refusal — measured.
fn apply_database_options(
    create: &mut plan::CreateDatabase,
    options: &[(String, String)],
) -> Result<()> {
    for (name, value) in options {
        match name.as_str() {
            "ENCODING" => {
                let upper = value.to_ascii_uppercase();
                if !UTF8_SPELLINGS.contains(&upper.as_str()) {
                    if !ENCODING_NAMES.contains(&upper.as_str()) {
                        return Err(SqlError::InvalidEncodingName(value.clone()));
                    }
                    return Err(SqlError::unsupported(format!(
                        "the encoding {upper}, on a node whose only encoding is {ENCODING}"
                    )));
                }
            }
            "LC_COLLATE" | "LC_CTYPE" | "LOCALE" => {
                if !value.eq_ignore_ascii_case(COLLATION) {
                    return Err(SqlError::unsupported(format!(
                        "the collation {value}, on a node whose only collation is {COLLATION}"
                    )));
                }
            }
            "TEMPLATE" => create.template = Some(fold_identifier(value, false).0),
            "OWNER" => return Err(SqlError::UndefinedRole(value.clone())),
            "TABLESPACE" => {
                if !value.eq_ignore_ascii_case("pg_default") {
                    return Err(SqlError::UndefinedTablespace(value.clone()));
                }
            }
            "STRATEGY" => {
                if !value.eq_ignore_ascii_case("wal_log")
                    && !value.eq_ignore_ascii_case("file_copy")
                {
                    return Err(SqlError::InvalidCreateDatabaseStrategy(value.clone()));
                }
            }
            // The default is `-1`, which is "no limit" and is the only answer this node can keep.
            "CONNECTION LIMIT" if value == "-1" => {}
            "ALLOW_CONNECTIONS" if is_true(value) => {}
            "IS_TEMPLATE" if !is_true(value) => {}
            "CONNECTION LIMIT" | "ALLOW_CONNECTIONS" | "IS_TEMPLATE" | "OID"
            | "LOCALE_PROVIDER" | "ICU_LOCALE" | "ICU_RULES" | "COLLATION_VERSION"
            | "BUILTIN_LOCALE" => {
                return Err(SqlError::unsupported(format!(
                    "CREATE DATABASE ... {name} = {value}"
                )));
            }
            other => {
                return Err(SqlError::UnrecognizedDatabaseOption(
                    other.to_ascii_lowercase(),
                ));
            }
        }
    }
    Ok(())
}

/// A boolean option's value, in the spellings PostgreSQL takes for one.
fn is_true(value: &str) -> bool {
    ["true", "on", "yes", "1", "t", "y"]
        .iter()
        .any(|spelling| value.eq_ignore_ascii_case(spelling))
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per statement kind; splitting it would hide the vocabulary rather than clarify it"
)]
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
            None => Ok(plan::Statement::Select(Box::new(lower_query(query)?))),
        },
        Statement::Update(update) => Ok(plan::Statement::Update(lower_update(update)?)),
        Statement::Delete(delete) => Ok(plan::Statement::Delete(lower_delete(delete)?)),
        Statement::Insert(insert) => Ok(plan::Statement::Insert(lower_insert(insert)?)),
        Statement::CreateIndex(create) => {
            Ok(plan::Statement::CreateIndex(lower_create_index(create)?))
        }
        Statement::CreateFunction(create) => Ok(plan::Statement::CreateFunction(
            lower_create_function(create)?,
        )),
        Statement::CreateTrigger(create) => Ok(plan::Statement::CreateTrigger(
            lower_create_trigger(create)?,
        )),
        Statement::DropTrigger(drop) => Ok(plan::Statement::DropTrigger(plan::DropTrigger {
            name: drop
                .trigger_name
                .0
                .last()
                .and_then(|part| part.as_ident())
                .map(|ident| fold_identifier(&ident.value, ident.quote_style.is_some()).0)
                .ok_or_else(|| SqlError::unsupported("DROP TRIGGER with no name"))?,
            table: relation_name(
                drop.table_name
                    .as_ref()
                    .ok_or_else(|| SqlError::unsupported("DROP TRIGGER with no table"))?,
            )?,
            if_exists: drop.if_exists,
        })),
        Statement::DropFunction(drop) => {
            Ok(plan::Statement::DropFunction(lower_drop_function(drop)?))
        }
        Statement::AlterTable(alter) => Ok(plan::Statement::AlterTable(lower_alter_table(alter)?)),
        Statement::CreateSequence {
            temporary,
            if_not_exists,
            name,
            data_type,
            sequence_options,
            owned_by,
        } => {
            refuse_if(*temporary, "CREATE TEMPORARY SEQUENCE")?;
            // `AS smallint` narrows the counter, which changes when a sequence runs out. This
            // node counts in `i64` whatever it fills; taking the word and counting wider anyway
            // would hand out a number the column cannot hold instead of the `22003` a real server
            // gives at the same point.
            refuse_if(data_type.is_some(), "CREATE SEQUENCE ... AS <type>")?;
            Ok(plan::Statement::CreateSequence(lower_create_sequence(
                name,
                *if_not_exists,
                sequence_options,
                owned_by.as_ref(),
            )?))
        }
        // `CREATE EXTENSION [IF NOT EXISTS] "name"`. The name keeps its case and its hyphens —
        // `ActiveRecord` writes `"uuid-ossp"` — so it is **not** folded the way a relation name
        // is: an extension is looked up by the string a control file is named with, not by a
        // PostgreSQL identifier.
        Statement::CreateExtension(create) => {
            // `SCHEMA` and `VERSION` choose where it goes and which version to install; this node
            // has one schema and offers one version per extension, so honouring the words while
            // ignoring them would answer a question the user did not ask. `CASCADE` installs an
            // extension's own dependencies, of which there are none here.
            refuse_if(create.schema.is_some(), "CREATE EXTENSION ... SCHEMA")?;
            refuse_if(create.version.is_some(), "CREATE EXTENSION ... VERSION")?;
            refuse_if(create.cascade, "CREATE EXTENSION ... CASCADE")?;
            Ok(plan::Statement::CreateExtension(plan::CreateExtension {
                name: create.name.value.clone(),
                if_not_exists: create.if_not_exists,
            }))
        }
        // `CREATE SCHEMA [IF NOT EXISTS] name`. **The suite writes the nested form**
        // (`CREATE SCHEMA s CREATE TABLE t (…)`) which `sqlparser` 0.62.0 cannot read at all — a
        // C1 gap in the plan's register, and the reason `schema_test.rb` is still out of reach.
        Statement::CreateSchema {
            schema_name,
            if_not_exists,
            with,
            options,
            default_collate_spec,
            clone,
        } => {
            use sqlparser::ast::SchemaName;
            refuse_if(with.is_some(), "CREATE SCHEMA ... WITH")?;
            refuse_if(options.is_some(), "CREATE SCHEMA with options")?;
            refuse_if(
                default_collate_spec.is_some(),
                "CREATE SCHEMA ... DEFAULT COLLATE",
            )?;
            refuse_if(clone.is_some(), "CREATE SCHEMA ... CLONE")?;
            let SchemaName::Simple(name) = schema_name else {
                // `AUTHORIZATION` names an owner, and there are no roles here.
                return Err(SqlError::unsupported("CREATE SCHEMA ... AUTHORIZATION"));
            };
            Ok(plan::Statement::CreateSchema(plan::CreateSchema {
                name: object_name(name)?,
                if_not_exists: *if_not_exists,
            }))
        }
        // `CREATE DATABASE [IF NOT EXISTS] name`. **PostgreSQL's options never reach this arm**:
        // `sqlparser` 0.62.0's grammar has `LOCATION`, `MANAGEDLOCATION`, `CLONE` and MySQL's
        // `CHARACTER SET`/`COLLATE` and nothing PostgreSQL spells, so the list is cut out of the
        // source before the parse and re-attached in `lower_inline`. The four this parser *can*
        // read are refused by name here, so that a statement it takes and this node cannot honour
        // is never silently read as the bare form.
        Statement::CreateDatabase {
            db_name,
            if_not_exists,
            location,
            managed_location,
            clone,
            default_charset,
            default_collation,
            ..
        } => {
            refuse_if(location.is_some(), "CREATE DATABASE ... LOCATION")?;
            refuse_if(
                managed_location.is_some(),
                "CREATE DATABASE ... MANAGEDLOCATION",
            )?;
            refuse_if(clone.is_some(), "CREATE DATABASE ... CLONE")?;
            refuse_if(
                default_charset.is_some(),
                "CREATE DATABASE ... CHARACTER SET",
            )?;
            refuse_if(default_collation.is_some(), "CREATE DATABASE ... COLLATE")?;
            Ok(plan::Statement::CreateDatabase(plan::CreateDatabase {
                name: object_name(db_name)?,
                if_not_exists: *if_not_exists,
                // Filled from the text the parse could not read, in `lower_inline`.
                template: None,
            }))
        }
        Statement::AlterSchema(alter) => {
            use sqlparser::ast::AlterSchemaOperation;
            refuse_if(alter.if_exists, "ALTER SCHEMA IF EXISTS")?;
            let [AlterSchemaOperation::Rename { name: to }] = alter.operations.as_slice() else {
                return Err(SqlError::unsupported("ALTER SCHEMA, other than RENAME TO"));
            };
            Ok(plan::Statement::AlterSchemaRename(
                plan::AlterSchemaRename {
                    name: object_name(&alter.name)?,
                    to: object_name(to)?,
                },
            ))
        }
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
            refuse_if(*purge, "DROP ... PURGE")?;
            refuse_if(*temporary, "DROP TEMPORARY")?;
            // `RESTRICT` is the default and needs no flag of its own: written or not, a dependent
            // object is `2BP01`. Measured, both spellings.
            let _ = *restrict;
            let names = names
                .iter()
                .map(relation_name)
                .collect::<Result<Vec<_>>>()?;
            Ok(match object_type {
                ObjectType::Table => plan::Statement::DropTable(plan::DropTable {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                ObjectType::Type => plan::Statement::DropType(plan::DropType {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                ObjectType::Index => plan::Statement::DropIndex(plan::DropIndex {
                    names,
                    // `sqlparser` 0.62.0's `Drop` has no `concurrently` field, so the word is read
                    // from the source. Measured rather than assumed: the statement parses and the
                    // keyword is dropped, which would silently give the *blocking* drop to somebody
                    // who asked for the concurrent one — the failure this crate's lowering exists
                    // to prevent.
                    concurrently: false,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                ObjectType::Sequence => plan::Statement::DropSequence(plan::DropSequence {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                ObjectType::Schema => plan::Statement::DropSchema(plan::DropSchema {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                // **No `CASCADE` and no `RESTRICT`.** PostgreSQL's `DROP DATABASE` takes neither —
                // there is nothing outside a database that can depend on it — so a spelling that
                // carries one is refused rather than ignored.
                ObjectType::Database => {
                    refuse_if(*cascade, "DROP DATABASE ... CASCADE")?;
                    plan::Statement::DropDatabase(plan::DropDatabase {
                        names,
                        if_exists: *if_exists,
                    })
                }
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
            refuse_if(*verbose, "EXPLAIN VERBOSE")?;
            refuse_if(*query_plan, "EXPLAIN QUERY PLAN")?;
            refuse_if(*estimate, "EXPLAIN ESTIMATE")?;
            refuse_if(format.is_some(), "EXPLAIN (FORMAT ...)")?;
            refuse_if(options.is_some(), "EXPLAIN with options")?;
            let _ = describe_alias;
            let inner = lower_statement(statement)?;
            // **`ANALYZE` runs the statement**, which is what the word means on a real server. So
            // it is executed for a `SELECT`, where the point of it is the `ScanStats` a columnar
            // answer carries (ADR 0022 milestone 4), and stays `0A000` for everything else — an
            // `EXPLAIN ANALYZE INSERT` that ran would be an insert.
            refuse_if(
                *analyze && !matches!(inner, plan::Statement::Select(_)),
                "EXPLAIN ANALYZE of a statement that is not a SELECT",
            )?;
            Ok(plan::Statement::Explain(Box::new(inner), *analyze))
        }
        Statement::CreateType {
            name,
            representation,
        } => lower_create_type(name, representation.as_ref()),
        Statement::Comment {
            object_type,
            object_name,
            comment,
            if_exists,
        } => lower_comment(*object_type, object_name, comment.as_deref(), *if_exists),
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
        if called == "esker_columnar_replicas" {
            refuse_if(
                !table.joins.is_empty(),
                "a JOIN on esker_columnar_replicas()",
            )?;
            refuse_if(
                select.selection.is_some(),
                "a WHERE on esker_columnar_replicas()",
            )?;
            refuse_if(
                !matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]),
                "a target list on esker_columnar_replicas() other than *",
            )?;
            return Ok(Some(plan::TimeMachineVerb::ListColumnarReplicas));
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
    scalar_verb(function)
}

/// The scalar half of [`lower_verb`]: `SELECT <verb>(...)` with no `FROM` at all.
///
/// Its own function because the two halves are two shapes — a call in the `FROM` clause and a call
/// in the target list — and together they were more than one screen.
fn scalar_verb(function: &sqlparser::ast::Function) -> Result<Option<plan::TimeMachineVerb>> {
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
        ("esker_flashback", Some([table, to])) => Some(plan::TimeMachineVerb::Flashback {
            table: fold_identifier(table, true).0,
            to: to.clone(),
        }),
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
        // A parameter this node reports. The name is looked up here only to route the statement;
        // the **value** is checked when it runs, because that is when a real server checks it.
        Set::SingleAssignment {
            scope,
            hivevar,
            variable,
            values,
        } if is_guc_assignment(variable) => {
            refuse_if(*hivevar, "SET HIVEVAR")?;
            let name = guc_name(variable).unwrap_or_default();
            // `SET LOCAL` is undone when the transaction ends, whichever way it ends — which needs
            // a per-block undo this node keeps only for `esker.read_as_of`. Refused by name rather
            // than silently promoted to a session-wide `SET`, which would outlive the block.
            refuse_if(
                matches!(scope, Some(ContextModifier::Local)),
                format!("SET LOCAL {name}"),
            )?;
            let [value] = values.as_slice() else {
                // PostgreSQL takes a list for `search_path`, and one is what `ActiveRecord` sends:
                // `SET search_path TO "$user", public`. It arrives as two values and is one path.
                return Ok(plan::Statement::Session(
                    plan::SessionStatement::SetParameter {
                        value: Some(
                            values
                                .iter()
                                .map(guc_list_item)
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                        name,
                    },
                ));
            };
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetParameter {
                    value: guc_value(value),
                    name,
                },
            ))
        }
        // `SET TIME ZONE 'UTC'` is PostgreSQL's own spelling of `SET timezone TO 'UTC'` — the
        // same parameter, and the parser gives it a variant of its own rather than a name.
        Set::SetTimeZone { local, value } => {
            refuse_if(*local, "SET LOCAL timezone")?;
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetParameter {
                    name: "timezone".to_owned(),
                    value: guc_value(value),
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
    if crate::parameter::lookup(&name).is_ok() {
        return Ok(plan::Statement::Session(
            plan::SessionStatement::ShowParameter(name),
        ));
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
        // `RESET x` and `SET x TO DEFAULT` are the same operation on a real server, and both go
        // back to the **boot** value rather than to the last one set. Measured.
        Reset::ConfigurationParameter(name)
            if guc_name(name).is_some_and(|name| crate::parameter::lookup(&name).is_ok()) =>
        {
            Ok(plan::Statement::Session(
                plan::SessionStatement::SetParameter {
                    name: guc_name(name).unwrap_or_default(),
                    value: None,
                },
            ))
        }
        Reset::ALL => Ok(plan::Statement::Session(plan::SessionStatement::ResetAll)),
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

/// One item of a `SET` that takes a list, as PostgreSQL renders it back.
///
/// `SET search_path TO "$user", public` is two items and one path, and `SHOW search_path` answers
/// with the quotes still on the first — measured, so the quoting is kept rather than folded away.
fn guc_list_item(value: &Expr) -> String {
    match value {
        Expr::Identifier(ident) => match ident.quote_style {
            Some(quote) => format!("{quote}{}{quote}", ident.value),
            None => ident.value.clone(),
        },
        Expr::Value(value) => match &value.value {
            Value::DoubleQuotedString(text) => format!("\"{text}\""),
            Value::SingleQuotedString(text) => text.clone(),
            other => other.to_string(),
        },
        other => other.to_string(),
    }
}

/// Whether a `SET` names a run-time parameter at all, known or not.
///
/// **A name this node has never heard of is still a `SET`**, and its answer is `42704` from the
/// executor rather than `0A000` from here: a real server parses the statement and raises
/// "unrecognized configuration parameter" when it runs, so refusing at lowering would both use the
/// wrong SQLSTATE and use it earlier. Contract C2 is about constructs this node cannot *do*, and
/// this is one it does — for a parameter that does not exist.
fn is_guc_assignment(variable: &ObjectName) -> bool {
    guc_name(variable).is_some()
}

/// The feature name for a `SET` this node does not execute.
///
/// Rendered from the statement rather than from the AST variant, so a user is told the construct
/// they wrote. **What reaches it is no longer a named parameter**: every `SET <name> = <value>`
/// now lowers, so what is left here is the spellings that are not an assignment at all —
/// `SET TRANSACTION`, `SET ROLE`, `SET CONSTRAINTS` — plus the assignment whose variable has no
/// name, which the `None` arm below is for.
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
    if key.value.eq_ignore_ascii_case("columnar_replicas") {
        return Ok(plan::AlterTableAction::SetColumnarReplicas {
            replicas: Some(lower_columnar_replicas(value)?),
        });
    }
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

/// `DEFAULT <expression>` on a column: the value it folds to, or the expression it stays.
///
/// **PostgreSQL applies no volatility test here and no constant test.** A default is an arbitrary
/// expression stored as a parse tree and evaluated once per row, and this function used to carry
/// two rules of its own that no server has — a function call was refused as *possibly volatile*
/// and anything else as *not a constant* — which between them refused seven of the ten defaults in
/// one `ActiveRecord` schema table that 19beta1 accepts, including `concat` and `CURRENT_DATE`,
/// both of which are `STABLE`.
///
/// What a real server does refuse is exactly three things, and they are refused below with its own
/// messages: a **column reference**, a **subquery**, and a **set-returning function**. An unknown
/// function is not a fourth rule — it is the ordinary "no such function", raised here because
/// PostgreSQL resolves the expression when the table is created and not when a row is written.
///
/// # The two halves, and why both exist
///
/// The answer is a pair, and at most one side of it is set:
///
/// * a **folded value** ([`catalog::ColumnDef::default`]), for the expressions PostgreSQL's
///   coercion folds to a `Const` — a literal, read as the column's type. This is the common case
///   and it stays a value so that reading it costs nothing;
/// * an **expression** ([`catalog::ColumnDef::default_expr`]), for everything else, stored as the
///   text `pg_get_expr` prints and evaluated per row.
///
/// A literal written with an explicit cast sets **both**: the value is what the column takes and
/// the text is what a real server prints back, and those disagree — `DEFAULT 0::bigint` answers
/// `0` and prints `(0)::bigint`. Storing only the value would print `0`; storing only the text
/// would make every row pay for a parse.
///
/// `DEFAULT NULL` normalises to neither — the same thing as no default, which is what PostgreSQL
/// makes of it too.
pub(super) fn column_default(
    expr: &Expr,
    ty: ColumnType,
) -> Result<(Option<Datum>, Option<String>)> {
    let expr = unwrap_nested(expr);
    refuse_default_shapes(expr)?;
    // A literal, with the sign or the cast a user wrote around it: what PostgreSQL's coercion
    // folds, and nothing more. `1 + 1` is *not* folded by a real server either — it prints back as
    // `(1 + 1)`, measured — so the fold here stops exactly where the server's does.
    let (literal, cast) = match expr {
        Expr::Value(value) => (Some(&value.value), None),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => match unwrap_nested(inner) {
            // A signed number: `DEFAULT -1`. Rendered back and read as the column's type, which is
            // how a negative literal reaches `Datum` everywhere else in this crate.
            Expr::Value(_) => {
                return Ok((Some(Datum::from_text(ty, &expr.to_string())?), None));
            }
            _ => (None, None),
        },
        Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => match unwrap_nested(inner) {
            Expr::Value(value) => (Some(&value.value), Some(data_type)),
            _ => (None, None),
        },
        _ => (None, None),
    };
    if let Some(literal) = literal {
        let text = match literal {
            // The same thing as no default at all, and the value a column with no default takes.
            Value::Null => return Ok((None, None)),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text.clone(),
            Value::Number(digits, _) => digits.clone(),
            Value::Boolean(flag) => (if *flag { "true" } else { "false" }).to_owned(),
            other => other.to_string(),
        };
        // Read as the **column's** type, not the cast's: `a int4 DEFAULT 0::bigint` stores an
        // `int4`, because the cast is one step of a coercion that ends at the column. A literal
        // the type cannot take is that type's own input error, exactly as it would be in a
        // `VALUES` list — `DEFAULT 'not a date'` on a `date` column is `22007` here and there.
        let value = Datum::from_text(ty, &text)?;
        return Ok((
            Some(value),
            cast.map(|to| cast_default_text(&text, literal, to)),
        ));
    }
    // Everything else stays an expression. **Lowering it here is what makes a function this node
    // does not have refused when the table is created** rather than when the first row is written,
    // which is where a real server raises it — and it is also what refuses `random() * 100` by
    // naming the operator, since this node has no arithmetic at all.
    lower_expr(expr)?;
    Ok((None, Some(expr.to_string())))
}

/// How a real server prints a literal that was written with a cast: `(0)::bigint`, `'x'::text`.
///
/// **The parentheses are not decoration and they are not always there.** PostgreSQL prints a
/// constant through `get_const_expr`, which writes a number bare and a string quoted, and then
/// parenthesises only when what it wrote would re-parse wrongly against the `::` that follows. A
/// bare `0::bigint` is that case and `'x'::text` is not.
fn cast_default_text(text: &str, literal: &Value, to: &DataType) -> String {
    let printed = match literal {
        Value::Number(digits, _) => format!("({digits})"),
        Value::Boolean(flag) => (if *flag { "true" } else { "false" }).to_owned(),
        _ => format!("'{}'", text.replace('\'', "''")),
    };
    format!("{printed}::{}", cast_type_name(to))
}

/// The three things PostgreSQL forbids in a `DEFAULT`, with the messages it uses for them.
///
/// Not a volatility rule and not a constant rule — those were this node's own and are gone. These
/// three are refused because a default is evaluated with **no row in scope and one value out**: a
/// column reference has nothing to read, a subquery would need a plan, and a set-returning
/// function would produce a column where a value is wanted.
///
/// The walk is recursive, because a real server refuses `concat(a, 'x')` for the same reason it
/// refuses a bare `a` — the column reference is what it objects to, not where it sits.
fn refuse_default_shapes(expr: &Expr) -> Result<()> {
    match expr {
        // **A bare keyword that is a function is not a column.** `CURRENT_DATE` and
        // `CURRENT_TIMESTAMP` reach the parser without parentheses, and refusing them here as
        // column references is exactly the mistake this function exists to stop making. Quoted,
        // the name is a column again — that is what the quotes mean.
        Expr::Identifier(name)
            if name.quote_style.is_some()
                || plan::CatalogFunc::from_name(&name.value).is_none() =>
        {
            Err(SqlError::DefaultColumnReference)
        }
        Expr::CompoundIdentifier(_) => Err(SqlError::DefaultColumnReference),
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err(SqlError::DefaultSubquery)
        }
        Expr::Function(function) if is_set_returning(function) => {
            Err(SqlError::DefaultSetReturning)
        }
        Expr::Function(function) => function_arguments(function, "")
            .unwrap_or_default()
            .into_iter()
            .try_for_each(|arg| refuse_default_shapes(unwrap_nested(arg))),
        Expr::BinaryOp { left, right, .. } => {
            refuse_default_shapes(unwrap_nested(left))?;
            refuse_default_shapes(unwrap_nested(right))
        }
        Expr::UnaryOp { expr, .. } | Expr::Cast { expr, .. } | Expr::Nested(expr) => {
            refuse_default_shapes(unwrap_nested(expr))
        }
        _ => Ok(()),
    }
}

/// Whether a call is to a **set-returning** function, which a `DEFAULT` cannot hold.
///
/// Named rather than derived: PostgreSQL knows from `prorettype` and `proretset`, and this node
/// has no function catalog to ask. The list is the set-returning functions it could plausibly
/// meet, and one missing from it is refused a step later by `lower_expr` — as a function this node
/// does not have, which is the truth about every name on the list too.
fn is_set_returning(function: &sqlparser::ast::Function) -> bool {
    let Ok(name) = unqualified_function_name(function) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "generate_series"
            | "generate_subscripts"
            | "unnest"
            | "regexp_split_to_table"
            | "json_array_elements"
            | "jsonb_array_elements"
            | "json_each"
            | "jsonb_each"
    )
}

/// `CREATE SEQUENCE [IF NOT EXISTS] s [START n] [INCREMENT BY n] [OWNED BY t.c | NONE]`.
///
/// **`START n` is the first value handed out**, not the one before it — measured, `START 101`
/// answers `101` and then `102`. The counter is stored as that first value, so the sequence needs
/// no "has it started yet" flag beside it.
///
/// `MINVALUE`, `MAXVALUE`, `CYCLE` and `CACHE` are refused by name. Each one changes what happens
/// at an end this node's counter does not have — a bound to stop at, or to wrap at — and a node
/// that read the word and counted on regardless would answer past the limit the user asked for.
fn lower_create_sequence(
    name: &ObjectName,
    if_not_exists: bool,
    options: &[sqlparser::ast::SequenceOptions],
    owned_by: Option<&ObjectName>,
) -> Result<plan::CreateSequence> {
    use sqlparser::ast::SequenceOptions;
    let mut start = 1;
    let mut increment = 1;
    for option in options {
        match option {
            SequenceOptions::StartWith(expr, _) => start = sequence_number(expr, "START")?,
            SequenceOptions::IncrementBy(expr, _) => {
                increment = sequence_number(expr, "INCREMENT BY")?;
                // A sequence that counts down, or does not count, is a different mechanism at
                // every end: this one only goes up, and saying so beats handing out one value
                // for ever.
                refuse_if(
                    increment <= 0,
                    "CREATE SEQUENCE ... INCREMENT BY a non-positive",
                )?;
            }
            SequenceOptions::MinValue(Some(_)) => {
                return Err(SqlError::unsupported("CREATE SEQUENCE ... MINVALUE"));
            }
            SequenceOptions::MaxValue(Some(_)) => {
                return Err(SqlError::unsupported("CREATE SEQUENCE ... MAXVALUE"));
            }
            SequenceOptions::Cycle(false) => {
                return Err(SqlError::unsupported("CREATE SEQUENCE ... CYCLE"));
            }
            SequenceOptions::Cache(_) => {
                return Err(SqlError::unsupported("CREATE SEQUENCE ... CACHE"));
            }
            // `NO MINVALUE`, `NO MAXVALUE` and `NO CYCLE` ask for the behaviour this node already
            // has, so honouring them is honouring nothing. (`Cycle(true)` is the *`NO CYCLE`*
            // spelling in `sqlparser` 0.62.0 — the flag says "no", not "yes".)
            SequenceOptions::MinValue(None)
            | SequenceOptions::MaxValue(None)
            | SequenceOptions::Cycle(true) => {}
        }
    }
    // `OWNED BY NONE` and no clause at all are the same statement — measured, both leave the
    // sequence unowned — so the word `none` is read here rather than carried into the plan.
    let owned_by = match owned_by.map(ObjectName::to_string) {
        Some(owner) if owner.eq_ignore_ascii_case("none") => None,
        Some(_) => {
            let parts: Option<Vec<&str>> = owned_by
                .map(|owner| {
                    owner
                        .0
                        .iter()
                        .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            match parts.as_deref() {
                Some([table, column]) => Some((
                    fold_identifier(table, false).0,
                    fold_identifier(column, false).0,
                )),
                _ => return Err(SqlError::unsupported("CREATE SEQUENCE ... OWNED BY a path")),
            }
        }
        None => None,
    };
    Ok(plan::CreateSequence {
        name: relation_name(name)?,
        if_not_exists,
        start,
        increment,
        owned_by,
    })
}

/// One of `CREATE SEQUENCE`'s numbers: an integer literal, with an optional sign.
fn sequence_number(expr: &Expr, clause: &str) -> Result<i64> {
    let text = unwrap_nested(expr).to_string();
    text.parse::<i64>()
        .map_err(|_| SqlError::unsupported(format!("CREATE SEQUENCE ... {clause} {text}")))
}

/// What `SET DEFAULT` was given: a sequence to draw from, or an ordinary default.
///
/// **`nextval('s')` is matched here rather than lowered as an expression**, because a sequence *is*
/// a column's default in this catalog: pointing a column at one moves which sequence fills it, and
/// that move is what frees the sequence the column used to draw from — the whole point of the
/// statement. Lowering it as an ordinary call would leave the old sequence filling the column and
/// evaluate a second one beside it, so the column would draw twice per row from two counters.
///
/// The column's type is not in reach here, so an ordinary default keeps its expression and is
/// folded where the executor has the column.
fn lower_set_default(value: &Expr) -> Result<plan::ColumnDefault> {
    let expr = unwrap_nested(value);
    if let Expr::Function(function) = expr
        && let Ok(name) = unqualified_function_name(function)
        && name.eq_ignore_ascii_case("nextval")
        && let Ok([argument]) = <[&Expr; 1]>::try_from(function_arguments(function, "nextval")?)
        && let Some(sequence) = sequence_literal_name(argument)
    {
        return Ok(plan::ColumnDefault::Sequence(sequence));
    }
    // Not a sequence: the ordinary `DEFAULT` rules, three refusals and all.
    refuse_default_shapes(expr)?;
    lower_expr(expr)?;
    Ok(plan::ColumnDefault::Value {
        folded: None,
        expr: Some(expr.to_string()),
    })
}

/// The sequence a `nextval` argument names: `'s'` and `'s'::regclass` are the same thing.
fn sequence_literal_name(argument: &Expr) -> Option<String> {
    let inner = match unwrap_nested(argument) {
        Expr::Cast { expr, .. } => unwrap_nested(expr),
        other => other,
    };
    match inner {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) => Some(sequence_reference(text)),
            _ => None,
        },
        _ => None,
    }
}

/// `DROP FUNCTION [IF EXISTS] f [(<types>)] [, …]`.
///
/// The argument types are kept **as the user wrote them**, lower-cased, because matching happens
/// against a signature and a type this node has never heard of is simply one that does not match.
/// Resolving them to `ColumnType` here would refuse `DROP FUNCTION f(hstore)` as an unknown type
/// where a real server answers that no such function exists — a wrong answer rather than a gap.
fn lower_drop_function(drop: &sqlparser::ast::DropFunction) -> Result<plan::DropFunction> {
    let mut functions = Vec::with_capacity(drop.func_desc.len());
    for desc in &drop.func_desc {
        // A schema qualifier is taken and dropped: `pg_catalog.lower(text)` is `lower(text)`, and
        // the message a real server gives does not repeat the qualifier either. An *unknown*
        // schema is absence, not an error, which is what `relation_name` cannot express — so the
        // last part is the name and the rest is discarded.
        let name = desc
            .name
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|ident| fold_identifier(&ident.value, ident.quote_style.is_some()).0)
            .ok_or_else(|| SqlError::unsupported("DROP FUNCTION with no name"))?;
        let args = desc.args.as_ref().map(|args| {
            args.iter()
                .map(|arg| arg.data_type.to_string().to_ascii_lowercase())
                .collect()
        });
        functions.push((name, args));
    }
    Ok(plan::DropFunction {
        functions,
        if_exists: drop.if_exists,
    })
}

/// `UNIQUE … [NOT] DEFERRABLE [INITIALLY IMMEDIATE | DEFERRED]`, and which half of it lands.
///
/// **`DEFERRABLE INITIALLY IMMEDIATE` is not deferred.** Measured: the second of two colliding
/// rows is refused by the statement that writes it, exactly as a plain `UNIQUE` refuses it, and
/// all that differs is `pg_constraint.condeferrable` and what `pg_get_constraintdef` prints. So it
/// is taken, and the flag is stored for those two readers alone.
///
/// **`INITIALLY DEFERRED` really waits**, and now it can: the check is registered against the
/// transaction and run at `COMMIT` (`crate::exec::deferred`). It was refused by name until then,
/// deliberately — accepting the clause while checking at the statement would refuse a transaction
/// PostgreSQL commits, which is a wrong answer rather than a gap.
///
/// Returns `(deferrable, deferred)`. The second is never true without the first: `INITIALLY
/// DEFERRED` implies `DEFERRABLE` in the grammar, and PostgreSQL rejects the pair written the
/// other way round.
fn unique_deferrable(
    characteristics: Option<&sqlparser::ast::ConstraintCharacteristics>,
) -> Result<(bool, bool)> {
    let Some(characteristics) = characteristics else {
        return Ok((false, false));
    };
    refuse_if(
        characteristics.enforced.is_some(),
        "UNIQUE ... ENFORCED, which is MySQL's",
    )?;
    let deferred = characteristics.initially == Some(DeferrableInitial::Deferred);
    Ok((
        characteristics.deferrable.unwrap_or(false) || deferred,
        deferred,
    ))
}

/// `CREATE [OR REPLACE] FUNCTION f() RETURNS TRIGGER AS $$…$$ LANGUAGE plpgsql`.
///
/// **The body is taken verbatim and never parsed.** PostgreSQL validates a plpgsql body when the
/// function is created — a missing semicolon is `42601` from the `CREATE` itself — and this node
/// does not, which is a declared divergence rather than the thing the schema load needs. What the
/// load needs is that the definition survives, semicolons and all.
fn lower_create_function(create: &sqlparser::ast::CreateFunction) -> Result<plan::CreateFunction> {
    use sqlparser::ast::CreateFunctionBody;
    refuse_if(create.temporary, "CREATE TEMPORARY FUNCTION")?;
    // Every function this node stores takes none: the trigger functions the suite defines have no
    // arguments, and a parameter list would be a signature to resolve calls against.
    refuse_if(
        create.args.as_ref().is_some_and(|args| !args.is_empty()),
        "CREATE FUNCTION with arguments",
    )?;
    let language = create
        .language
        .as_ref()
        .map(|ident| fold_identifier(&ident.value, ident.quote_style.is_some()).0)
        .ok_or_else(|| SqlError::unsupported("CREATE FUNCTION with no LANGUAGE"))?;
    let body = match &create.function_body {
        // `AS $$…$$` before the options or after them: PostgreSQL takes both orders and
        // `ActiveRecord` writes the second.
        Some(
            CreateFunctionBody::AsBeforeOptions { body: expr, .. }
            | CreateFunctionBody::AsAfterOptions(expr),
        ) => {
            match expr {
                Expr::Value(value) => match &value.value {
                    // A dollar-quoted body arrives with its delimiters already stripped, which is
                    // exactly what `pg_proc.prosrc` holds.
                    Value::DollarQuotedString(quoted) => quoted.value.clone(),
                    Value::SingleQuotedString(text) => text.clone(),
                    other => other.to_string(),
                },
                other => other.to_string(),
            }
        }
        _ => return Err(SqlError::unsupported("CREATE FUNCTION with no body")),
    };
    Ok(plan::CreateFunction {
        name: relation_name(&create.name)?,
        body,
        language,
        or_replace: create.or_replace,
    })
}

/// `CREATE TRIGGER t BEFORE|AFTER <events> ON tbl FOR EACH ROW EXECUTE FUNCTION|PROCEDURE f()`.
///
/// **`EXECUTE PROCEDURE` and `EXECUTE FUNCTION` are one clause**, and both have to parse: statement
/// 762 writes the first, statement 790 the second, and `pg_get_triggerdef` prints only the second.
fn lower_create_trigger(create: &sqlparser::ast::CreateTrigger) -> Result<plan::CreateTrigger> {
    use sqlparser::ast::{TriggerEvent, TriggerObject, TriggerObjectKind, TriggerPeriod};
    refuse_if(create.is_constraint, "CREATE CONSTRAINT TRIGGER")?;
    refuse_if(create.condition.is_some(), "CREATE TRIGGER ... WHEN")?;
    refuse_if(create.or_replace, "CREATE OR REPLACE TRIGGER")?;
    let before = match create.period {
        Some(TriggerPeriod::Before) => true,
        Some(TriggerPeriod::After) => false,
        other => {
            return Err(SqlError::unsupported(format!(
                "CREATE TRIGGER {}",
                other.map_or_else(|| "with no period".to_owned(), |period| period.to_string())
            )));
        }
    };
    // PostgreSQL's own `tgtype` bits, so the mask is the answer rather than a translation of one.
    let mut events = 0;
    for event in &create.events {
        events |= match event {
            TriggerEvent::Insert => 4,
            TriggerEvent::Delete => 8,
            TriggerEvent::Update(columns) if columns.is_empty() => 16,
            other => {
                return Err(SqlError::unsupported(format!("CREATE TRIGGER ... {other}")));
            }
        };
    }
    let function = create
        .exec_body
        .as_ref()
        .map(|body| relation_name(&body.func_desc.name))
        .transpose()?
        .ok_or_else(|| SqlError::unsupported("CREATE TRIGGER with no EXECUTE clause"))?;
    Ok(plan::CreateTrigger {
        name: create
            .name
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|ident| fold_identifier(&ident.value, ident.quote_style.is_some()).0)
            .ok_or_else(|| SqlError::unsupported("CREATE TRIGGER with no name"))?,
        table: relation_name(&create.table_name)?,
        before,
        events,
        // `FOR EACH ROW` and `FOR ROW` are the same thing; `STATEMENT` is the other object.
        for_each_row: matches!(
            create.trigger_object,
            Some(
                TriggerObjectKind::For(TriggerObject::Row)
                    | TriggerObjectKind::ForEach(TriggerObject::Row)
            )
        ),
        function,
    })
}

/// One expression's text, lowered — for a clause the parser had to be handed in pieces.
///
/// An `EXCLUDE` constraint never reaches `sqlparser` whole, so its key and its `WHERE` come back
/// through here: reading them with the real parser is what makes an expression this node cannot
/// evaluate a refusal at `CREATE TABLE` rather than a surprise at the first insert.
pub(crate) fn parse_expr_text(text: &str) -> Result<plan::Expr> {
    crate::parse::parse_stored_expr(text)
}

/// `PARTITION BY LIST|RANGE (col, …)` — the strategy and the key columns.
///
/// **`HASH` is refused by name.** Nothing captured it — the capture pins `LIST` (the suite's, at
/// `postgresql_specific_schema.rb` statement 781) and `RANGE` — and a strategy whose routing
/// nobody measured is a row this node would put in the wrong partition, which is a wrong answer
/// rather than a gap.
///
/// `sqlparser` gives the whole clause as one expression, so `LIST (city_id)` arrives looking like
/// a function call — the strategy is the "function" and the key columns are its arguments.
fn lower_partition_by(
    partition_by: Option<&Expr>,
) -> Result<Option<(catalog::PartitionStrategy, Vec<String>)>> {
    let Some(expr) = partition_by else {
        return Ok(None);
    };
    let Expr::Function(function) = unwrap_nested(expr) else {
        return Err(SqlError::unsupported(format!(
            "CREATE TABLE ... PARTITION BY {expr}"
        )));
    };
    let name = unqualified_function_name(function)?;
    let strategy = match name.to_ascii_uppercase().as_str() {
        "LIST" => catalog::PartitionStrategy::List,
        "RANGE" => catalog::PartitionStrategy::Range,
        other => {
            return Err(SqlError::unsupported(format!(
                "CREATE TABLE ... PARTITION BY {other}"
            )));
        }
    };
    let columns = function_arguments(function, "PARTITION BY")?
        .into_iter()
        .map(|argument| match unwrap_nested(argument) {
            Expr::Identifier(name) => {
                Ok(fold_identifier(&name.value, name.quote_style.is_some()).0)
            }
            other => Err(SqlError::unsupported(format!(
                "PARTITION BY over the expression {other}"
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((strategy, columns)))
}

/// `PARTITION OF parent FOR VALUES IN (…)`, `… FROM (…) TO (…)` and `… DEFAULT`.
///
/// The values stay literals here: coercing them to the key's types needs the parent, and a plan is
/// lowered without the catalog. `FOR VALUES WITH (MODULUS …)` is `HASH`'s and is refused for the
/// reason that strategy is.
fn lower_partition_of(
    partition_of: Option<&ObjectName>,
    for_values: Option<&sqlparser::ast::ForValues>,
) -> Result<Option<(String, plan::PartitionSpec)>> {
    use sqlparser::ast::ForValues;
    let Some(parent) = partition_of else {
        return Ok(None);
    };
    let spec = match for_values {
        Some(ForValues::Default) => plan::PartitionSpec::Default,
        Some(ForValues::In(values)) => plan::PartitionSpec::Values(
            values
                .iter()
                .map(|value| match unwrap_nested(value) {
                    // Read as text and coerced to the key's type where the parent is in hand,
                    // which is what makes `IN (1)` on a `character varying` key store `'1'`.
                    Expr::Value(literal) => Ok(Datum::Text(literal_text(&literal.value))),
                    other => Err(SqlError::unsupported(format!(
                        "FOR VALUES IN ({other}), which is not a literal"
                    ))),
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        // **Multi-column `RANGE` bounds are refused by name.** One column is what the capture
        // shows, and PostgreSQL's rule past that is not the obvious one — `MINVALUE` in a position
        // makes every column after it unbounded whatever was written there — so a lexicographic
        // guess would route rows a real server routes elsewhere.
        Some(ForValues::From { from, to }) => {
            refuse_if(
                from.len() != 1 || to.len() != 1,
                "FOR VALUES FROM ... TO ... over more than one column",
            )?;
            plan::PartitionSpec::Range {
                from: range_ends(from)?,
                to: range_ends(to)?,
            }
        }
        Some(ForValues::With { .. }) => {
            return Err(SqlError::unsupported("PARTITION OF ... FOR VALUES WITH"));
        }
        None => return Err(SqlError::unsupported("PARTITION OF with no bound")),
    };
    Ok(Some((relation_name(parent)?, spec)))
}

/// `(MINVALUE)`, `(10)` — one end of a `FOR VALUES FROM … TO …`, still untyped.
fn range_ends(ends: &[sqlparser::ast::PartitionBoundValue]) -> Result<Vec<plan::RangeEnd>> {
    use sqlparser::ast::PartitionBoundValue;
    ends.iter()
        .map(|end| match end {
            PartitionBoundValue::MinValue => Ok(plan::RangeEnd::MinValue),
            PartitionBoundValue::MaxValue => Ok(plan::RangeEnd::MaxValue),
            PartitionBoundValue::Expr(expr) => match unwrap_nested(expr) {
                Expr::Value(literal) => Ok(plan::RangeEnd::Value(Datum::Text(literal_text(
                    &literal.value,
                )))),
                other => Err(SqlError::unsupported(format!(
                    "FOR VALUES FROM ... TO ... over {other}, which is not a literal"
                ))),
            },
        })
        .collect()
}

/// A literal's text, for a partition bound: what the value would be written as.
fn literal_text(value: &Value) -> String {
    match value {
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text.clone(),
        Value::Number(digits, _) => digits.clone(),
        Value::Boolean(flag) => (if *flag { "true" } else { "false" }).to_owned(),
        other => other.to_string(),
    }
}

/// A columnar-replica count: a plain non-negative integer, and nothing else.
///
/// No interval grammar, no `'forever'`, no `DEFAULT` — it is a replica count, so the only thing
/// it can be is a number. A `u8` because a table wanting more than 255 columnar copies is a
/// configuration mistake rather than a number worth carrying, and refusing it by name is more
/// use than storing it.
fn lower_columnar_replicas(value: &Expr) -> Result<u8> {
    let text = match value {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, _) => digits.clone(),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text.clone(),
            other => other.to_string(),
        },
        other => other.to_string(),
    };
    text.parse::<u8>()
        .map_err(|_| SqlError::InvalidParameterValue {
            name: "columnar_replicas",
            value: text.clone(),
        })
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
        return Ok(Some(catalog::RETENTION_FOREVER));
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

#[allow(
    clippy::too_many_lines,
    reason = "one arm per column option; splitting the loop would hide the vocabulary rather than clarify it"
)]
fn lower_create_table(create: &sqlparser::ast::CreateTable) -> Result<plan::CreateTable> {
    refuse_create_table_clauses(create)?;

    let name = relation_name(&create.name)?;
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut primary_key = Vec::new();
    let mut primary_key_name = None;
    let mut unique = Vec::new();
    let mut checks: Vec<catalog::CheckDef> = Vec::new();
    let mut foreign_keys: Vec<plan::ForeignKey> = Vec::new();

    for column in &create.columns {
        let column_name = ident(&column.name);
        let (ty, typmod, user_type_name) = lower_column_type(&column.data_type)?;
        let mut not_null = false;
        let mut default = None;
        let mut default_expr: Option<String> = None;
        // `bigserial` is the type saying it; `GENERATED ... AS IDENTITY` is an option saying it.
        // Both end here, because what they produce is the same record.
        let mut sequence = serial_identity(&column.data_type);
        let mut generated: Option<String> = None;
        for option in &column.options {
            match &option.option {
                // A column `CHECK`, named the way PostgreSQL names one when nothing else does:
                // `<table>_<column>_check`. Measured — `q text CHECK (q <> '')` on table `ck` is
                // `ck_q_check`.
                // `constraint.expr`, not the constraint: a `CheckConstraint`'s own `Display`
                // renders `CHECK (…)`, and storing that would make the stored text a call to a
                // function named `CHECK` when it is read back.
                ColumnOption::Check(constraint) => {
                    refuse_if(
                        constraint.enforced.is_some(),
                        "CHECK ... ENFORCED, which is MySQL's",
                    )?;
                    checks.push(catalog::CheckDef {
                        name: constraint
                            .name
                            .as_ref()
                            .map_or_else(|| format!("{name}_{column_name}_check"), ident),
                        expr: unwrap_nested(&constraint.expr).to_string(),
                    });
                }
                ColumnOption::ForeignKey(constraint) => {
                    foreign_keys.push(lower_column_foreign_key(&name, &column_name, constraint)?);
                }
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Null => {}
                // **A default is an arbitrary expression**, and `column_default` decides which of
                // the two halves holds it: the value, for what PostgreSQL's coercion folds, and
                // the text for everything else, evaluated once per row. Folding the rest would
                // give every row the instant `CREATE TABLE` ran, or every row the same UUID and a
                // primary key that refuses the second insert.
                ColumnOption::Default(expr) => (default, default_expr) = column_default(expr, ty)?,
                ColumnOption::Unique(constraint) => {
                    let (deferrable, deferred) =
                        unique_deferrable(constraint.characteristics.as_ref())?;
                    unique.push(plan::UniqueConstraint {
                        name: option.name.as_ref().map(ident),
                        columns: vec![column_name.clone()],
                        nulls_not_distinct: constraint.nulls_distinct
                            == NullsDistinctOption::NotDistinct,
                        deferrable,
                        deferred,
                    });
                }
                ColumnOption::PrimaryKey(_) => {
                    primary_key.push(column_name.clone());
                    primary_key_name = primary_key_name.or_else(|| option.name.as_ref().map(ident));
                }
                // **Two different features under one keyword**, told apart by whether an
                // expression was written: `GENERATED ALWAYS AS IDENTITY` brings a sequence and
                // `GENERATED ALWAYS AS (expr) STORED` brings an expression.
                ColumnOption::Generated {
                    generated_as,
                    sequence_options,
                    generation_expr,
                    generation_expr_mode,
                    ..
                } => match lower_generated(
                    *generated_as,
                    sequence_options.as_deref(),
                    generation_expr.as_ref(),
                    generation_expr_mode.as_ref(),
                )? {
                    Ok(expr) => generated = Some(expr),
                    Err(identity) => sequence = Some(identity),
                },
                other => return Err(SqlError::unsupported(column_option_name(other))),
            }
        }
        // An identity column is `NOT NULL` whether or not it says so, here as there.
        if sequence.is_some() {
            not_null = true;
        }
        columns.push(plan::Column {
            name: column_name,
            ty,
            user_type_name,
            typmod,
            default_expr,
            not_null,
            default,
            sequence,
            generated,
        });
    }

    lower_table_constraints(
        create,
        &name,
        &mut primary_key,
        &mut primary_key_name,
        &mut unique,
        &mut checks,
        &mut foreign_keys,
    )?;

    Ok(plan::CreateTable {
        // Overridden in `Parsed::lower`, which is where the stripped keyword is in reach.
        persistence: catalog::Persistence::Permanent,
        name,
        checks,
        foreign_keys,
        if_not_exists: create.if_not_exists,
        columns,
        primary_key,
        primary_key_name,
        // Filled by `Parsed::lower`, from the clauses the parser was never given.
        excludes: Vec::new(),
        partition_by: lower_partition_by(create.partition_by.as_deref())?,
        partition_of: lower_partition_of(create.partition_of.as_ref(), create.for_values.as_ref())?,
        // Names only: a parent's columns come from the catalog and the catalog is the executor's.
        inherits: create
            .inherits
            .iter()
            .flatten()
            .map(relation_name)
            .collect::<Result<Vec<_>>>()?,
        unique,
    })
}

/// `ALTER TABLE`, of which exactly one action is executed.
///
/// Everything else is named and refused (contract C2). The naming is done from *our* side rather
/// than from the AST's `Display`, because the name is what the user is told to change and
/// `sqlparser` renders an action with the identifiers the user wrote in it — a message that
/// echoes a column name back is a message that cannot be searched for.
#[allow(
    clippy::too_many_lines,
    reason = "one block per ALTER action; splitting it would hide the vocabulary rather than clarify it"
)]
fn lower_alter_table(alter: &sqlparser::ast::AlterTable) -> Result<plan::AlterTable> {
    // `ONLY` is about inheritance, which there is none of here; honouring it silently would be
    // honouring a word we do not implement.
    refuse_if(alter.only, "ALTER TABLE ONLY")?;
    refuse_if(alter.location.is_some(), "ALTER TABLE ... SET LOCATION")?;
    refuse_if(alter.on_cluster.is_some(), "ALTER TABLE ... ON CLUSTER")?;
    refuse_if(alter.table_type.is_some(), "ALTER of a table of that type")?;

    // The table's own name, for deriving a constraint name PostgreSQL would derive.
    let table_name = relation_name(&alter.name)?;
    let mut actions = Vec::with_capacity(alter.operations.len());
    for operation in &alter.operations {
        if let AlterTableOperation::SetOptionsParens { options } = operation {
            actions.push(lower_storage_parameters(options)?);
            continue;
        }
        if let AlterTableOperation::AddConstraint { constraint, .. } = operation {
            actions.push(lower_added_constraint(&table_name, constraint)?);
            continue;
        }
        if let AlterTableOperation::DisableTrigger { name }
        | AlterTableOperation::EnableTrigger { name } = operation
        {
            let disabled = matches!(operation, AlterTableOperation::DisableTrigger { .. });
            actions.push(lower_trigger_state(&table_name, name, disabled)?);
            continue;
        }
        // `ALTER COLUMN c SET DEFAULT <expr>` and `DROP DEFAULT`. The **type is not known here** —
        // a plan is lowered without the catalog — so a literal is not folded until the executor
        // has the column, which is also where `22P02` for one the type will not take comes from.
        if let AlterTableOperation::AlterColumn { column_name, op } = operation {
            use sqlparser::ast::AlterColumnOperation;
            let default = match op {
                AlterColumnOperation::DropDefault => None,
                AlterColumnOperation::SetDefault { value } => Some(lower_set_default(value)?),
                other => {
                    return Err(SqlError::unsupported(format!(
                        "ALTER TABLE ... ALTER COLUMN ... {other}"
                    )));
                }
            };
            actions.push(plan::AlterTableAction::SetDefault {
                column: ident(column_name),
                default,
            });
            continue;
        }
        // **One clause per column, however many the statement has.** `remove_columns` and
        // `remove_timestamps` send `ALTER TABLE "x" DROP COLUMN "a", DROP COLUMN "b"` as one
        // statement (`abstract/schema_statements.rb:700`), and `change_table` can put an
        // `ADD COLUMN` in the same one — which the loop this sits in already allows. `sqlparser`
        // additionally reads `DROP COLUMN a, b` as one action naming two columns, so the names
        // are a list here and each becomes its own action.
        if let AlterTableOperation::DropColumn {
            has_column_keyword: _,
            column_names,
            if_exists,
            drop_behavior,
        } = operation
        {
            // `RESTRICT` is the default and is what happens with neither word, so it needs no
            // flag; `CASCADE` is the one that changes an answer.
            let cascade = matches!(drop_behavior, Some(sqlparser::ast::DropBehavior::Cascade));
            for column_name in column_names {
                actions.push(plan::AlterTableAction::DropColumn {
                    column: ident(column_name),
                    if_exists: *if_exists,
                    cascade,
                });
            }
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
        let (ty, typmod, user_type_name) = lower_column_type(&column_def.data_type)?;
        let mut not_null = false;
        let mut default = None;
        for option in &column_def.options {
            let named = match &option.option {
                // A **folded** default is stored as the column's missing value and the decoder
                // pads with it, so no row is rewritten (ADR 0019's pad rule generalised;
                // `catalog::ColumnDef::missing`).
                //
                // An **expression** default is refused here, and this is the one place where
                // generalising the `DEFAULT` clause did not widen what is accepted. PostgreSQL
                // takes it and **rewrites the table**, so every row already stored gets its own
                // value — measured: `atthasmissing` comes back false for one. This `ALTER` is
                // defined not to rewrite, so honouring the clause would leave those rows NULL
                // where a real server gives them values, which is a wrong answer rather than a
                // gap. `CREATE TABLE` has no rows to rewrite and takes the same expression.
                ColumnOption::Default(expr) => {
                    // **A user-defined type's default is folded by the executor, not here.** The
                    // column's `ty` is a placeholder until the catalog has been read, so folding
                    // `'happy'` against it would read a label as a smallint; `text` keeps the
                    // label as written and `exec::ddl` turns it into the ordinal (ADR 0050).
                    let ty = if user_type_name.is_some() {
                        ColumnType::Text
                    } else {
                        ty
                    };
                    let (folded, unfolded) = column_default(expr, ty)?;
                    if let Some(unfolded) = unfolded {
                        return Err(SqlError::unsupported(format!(
                            "ALTER TABLE ... ADD COLUMN ... DEFAULT {unfolded}, which would \
                             rewrite every row"
                        )));
                    }
                    default = folded;
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
        // `ALTER TABLE ... ADD COLUMN id bigserial` would have to create a sequence *and* fill
        // every row already stored from it, which is the table rewrite this `ALTER` is defined not
        // to do. Refused by name rather than half-done.
        refuse_if(
            serial_identity(&column_def.data_type).is_some(),
            "ALTER TABLE ... ADD COLUMN ... bigserial",
        )?;
        actions.push(plan::AlterTableAction::AddColumn {
            column: plan::Column {
                name: ident(&column_def.name),
                ty,
                user_type_name,
                typmod,
                // Always: an expression default is refused above, because this `ALTER` cannot
                // rewrite the rows a real server would.
                default_expr: None,
                not_null,
                default,
                sequence: None,
                // `ADD COLUMN … GENERATED ALWAYS AS (…) STORED` would have to compute the
                // expression for every row already there, which is a backfill and not a catalog
                // write — refused by name with every other option this action does not take.
                generated: None,
            },
            if_not_exists: *if_not_exists,
        });
    }

    Ok(plan::AlterTable {
        name: relation_name(&alter.name)?,
        if_exists: alter.if_exists,
        actions,
    })
}

/// What to call an `ALTER TABLE` action the executor does not run.
///
/// Every table-level constraint of a `CREATE TABLE`, into the four lists that hold one.
///
/// Split out of [`lower_create_table`] because that function had grown past what one screen
/// holds, not because the constraints are separable — they all write into the same statement.
fn lower_table_constraints(
    create: &sqlparser::ast::CreateTable,
    name: &str,
    primary_key: &mut Vec<String>,
    primary_key_name: &mut Option<String>,
    unique: &mut Vec<plan::UniqueConstraint>,
    checks: &mut Vec<catalog::CheckDef>,
    foreign_keys: &mut Vec<plan::ForeignKey>,
) -> Result<()> {
    for constraint in &create.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => {
                refuse_if(key.index_name.is_some(), "PRIMARY KEY USING INDEX")?;
                primary_key.extend(index_columns(&key.columns)?);
                *primary_key_name = primary_key_name
                    .clone()
                    .or_else(|| key.name.as_ref().map(ident));
            }
            TableConstraint::Unique(key) => {
                let (deferrable, deferred) = unique_deferrable(key.characteristics.as_ref())?;
                unique.push(plan::UniqueConstraint {
                    name: key.name.as_ref().map(ident),
                    columns: index_columns(&key.columns)?,
                    nulls_not_distinct: key.nulls_distinct == NullsDistinctOption::NotDistinct,
                    deferrable,
                    deferred,
                });
            }
            TableConstraint::ForeignKey(constraint) => {
                foreign_keys.push(lower_foreign_key(name, constraint)?);
            }
            // A named table `CHECK`, or an unnamed one, which PostgreSQL names
            // `<table>_check` — the same derivation a column constraint gets without the column.
            TableConstraint::Check(check) => {
                refuse_if(
                    check.enforced.is_some(),
                    "CHECK ... ENFORCED, which is MySQL's",
                )?;
                checks.push(catalog::CheckDef {
                    name: check
                        .name
                        .as_ref()
                        .map_or_else(|| format!("{name}_check"), ident),
                    expr: unwrap_nested(&check.expr).to_string(),
                });
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "the table constraint {other}"
                )));
            }
        }
    }
    Ok(())
}

/// `ENABLE`/`DISABLE TRIGGER [ ALL | USER | <name> ]`.
///
/// `sqlparser` hands all three spellings back as an `Ident`, because `ALL` and `USER` are keywords
/// only in this position. Unquoted is what makes them keywords here, exactly as it does for
/// `DEFAULT` and `current_schema` above: `"ALL"` in quotes is a trigger called `ALL` and is
/// `42704` with the rest.
///
/// * **`ALL`** is the one that does something. It covers the internal foreign-key triggers, so it
///   suspends this table's referential checks until it is enabled again — measured, and the whole
///   reason `ActiveRecord` writes it ([`plan::AlterTableAction::SetTriggersDisabled`]).
/// * **`USER`** covers only triggers a user created, of which this node has none, so it is
///   accepted and records nothing. Measured: an `INSERT` under it is still `23503` on a real
///   server, so accepting it and suspending the checks would be a **wrong answer** rather than a
///   generous one.
/// * **A name** is `42704`, naming the trigger and the table the way PostgreSQL does. There are no
///   triggers here to name, so every name is missing.
fn lower_trigger_state(
    table: &str,
    name: &Ident,
    disabled: bool,
) -> Result<plan::AlterTableAction> {
    let keyword = |word: &str| name.quote_style.is_none() && name.value.eq_ignore_ascii_case(word);
    if keyword("all") {
        return Ok(plan::AlterTableAction::SetTriggersDisabled { disabled });
    }
    if keyword("user") {
        // Accepted, and it records nothing: there are no user triggers to enable or disable.
        return Ok(plan::AlterTableAction::SetTriggersDisabled { disabled: false });
    }
    Err(SqlError::UndefinedTrigger {
        trigger: ident(name),
        table: table.to_owned(),
    })
}

/// Which kind of constraint an `ADD CONSTRAINT` names, for the refusal that follows.
fn constraint_kind(constraint: &TableConstraint) -> &'static str {
    match constraint {
        TableConstraint::ForeignKey(_) => "FOREIGN KEY",
        TableConstraint::Unique(_) => "UNIQUE",
        TableConstraint::PrimaryKey(_) => "PRIMARY KEY",
        TableConstraint::Check(_) => "CHECK",
        _ => "that constraint",
    }
}

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

/// `ALTER TABLE … ADD CONSTRAINT …`: the two kinds this node records, and a name for the rest.
///
/// A `UNIQUE` or `PRIMARY KEY` added after the fact is `0A000` naming itself — recording a
/// constraint that does not constrain would let a schema load and then accept the rows it forbids,
/// which ADR 0031 calls a wrong answer rather than a gap.
fn lower_added_constraint(
    table: &str,
    constraint: &TableConstraint,
) -> Result<plan::AlterTableAction> {
    if let TableConstraint::ForeignKey(foreign_key) = constraint {
        return Ok(plan::AlterTableAction::AddForeignKey(lower_foreign_key(
            table,
            foreign_key,
        )?));
    }
    let TableConstraint::Check(check) = constraint else {
        return Err(SqlError::unsupported(format!(
            "ALTER TABLE ... ADD CONSTRAINT ... {}",
            constraint_kind(constraint)
        )));
    };
    refuse_if(
        check.enforced.is_some(),
        "CHECK ... ENFORCED, which is MySQL's",
    )?;
    Ok(plan::AlterTableAction::AddCheck(catalog::CheckDef {
        name: check
            .name
            .as_ref()
            .map_or_else(|| format!("{table}_check"), ident),
        expr: unwrap_nested(&check.expr).to_string(),
    }))
}

/// `p int8 REFERENCES t` — a column constraint that is the same thing as the table constraint.
///
/// The form `t.references :parrot, foreign_key: true` writes, and the one whose referencing column
/// is not in its own text: it is the column it is written on, and the derived name is the table's
/// plus that column's (`fxe_p_fkey`, measured).
fn lower_column_foreign_key(
    table: &str,
    column: &str,
    constraint: &sqlparser::ast::ForeignKeyConstraint,
) -> Result<plan::ForeignKey> {
    let mut lowered = lower_foreign_key(table, constraint)?;
    if lowered.columns.is_empty() {
        lowered.columns = vec![column.to_owned()];
        if constraint.name.is_none() {
            lowered.name = plan::foreign_key_name(table, &lowered.columns);
        }
    }
    Ok(lowered)
}

/// One `FOREIGN KEY`, as written.
///
/// **`MATCH` is refused unless it is `SIMPLE`**, which is the default and the only one this node
/// implements: `MATCH FULL` refuses a row with *some* of its key NULL where `SIMPLE` admits it,
/// so accepting the word and behaving as `SIMPLE` would admit rows a real server rejects.
fn lower_foreign_key(
    table: &str,
    key: &sqlparser::ast::ForeignKeyConstraint,
) -> Result<plan::ForeignKey> {
    refuse_if(key.index_name.is_some(), "an index name on a FOREIGN KEY")?;
    if let Some(kind) = &key.match_kind {
        refuse_if(
            !matches!(kind, ConstraintReferenceMatchKind::Simple),
            format!("FOREIGN KEY ... MATCH {kind}"),
        )?;
    }
    let deferrable = match &key.characteristics {
        None => false,
        Some(characteristics) => {
            // `INITIALLY DEFERRED` is the one form that would **change an answer**: a transaction
            // that violates the constraint in the middle and repairs it before `COMMIT` succeeds
            // on a real server and would be refused here, because every check in this crate is
            // immediate. Refused by name rather than accepted, which is contract C2's whole rule.
            // `ActiveRecord` writes `DEFERRABLE INITIALLY IMMEDIATE` and never this one.
            refuse_if(
                characteristics.initially == Some(DeferrableInitial::Deferred),
                "FOREIGN KEY ... INITIALLY DEFERRED",
            )?;
            refuse_if(
                characteristics.enforced.is_some(),
                "FOREIGN KEY ... ENFORCED, which is MySQL's",
            )?;
            characteristics.deferrable.unwrap_or(false)
        }
    };
    let columns: Vec<String> = key.columns.iter().map(ident).collect();
    Ok(plan::ForeignKey {
        name: key
            .name
            .as_ref()
            .map_or_else(|| plan::foreign_key_name(table, &columns), ident),
        columns,
        parent: relation_name(&key.foreign_table)?,
        parent_columns: key.referred_columns.iter().map(ident).collect(),
        on_update: referential_action(key.on_update.as_ref())?,
        on_delete: referential_action(key.on_delete.as_ref())?,
        deferrable,
    })
}

/// `ON UPDATE`/`ON DELETE`, defaulting to `NO ACTION` the way a real server does.
///
/// `SET NULL` and `SET DEFAULT` are refused by name: each writes a value into the child's columns
/// rather than refusing or removing, and neither appears in anything `ActiveRecord` emits.
fn referential_action(
    action: Option<&sqlparser::ast::ReferentialAction>,
) -> Result<catalog::ReferentialAction> {
    use sqlparser::ast::ReferentialAction as Written;
    Ok(match action {
        None | Some(Written::NoAction) => catalog::ReferentialAction::NoAction,
        Some(Written::Restrict) => catalog::ReferentialAction::Restrict,
        Some(Written::Cascade) => catalog::ReferentialAction::Cascade,
        Some(other) => {
            return Err(SqlError::unsupported(format!("ON DELETE/UPDATE {other}")));
        }
    })
}

fn lower_create_index(create: &sqlparser::ast::CreateIndex) -> Result<plan::CreateIndex> {
    refuse_if(!create.with.is_empty(), "CREATE INDEX ... WITH")?;

    refuse_if(
        !create.index_options.is_empty(),
        "CREATE INDEX with options",
    )?;
    refuse_if(
        !create.alter_options.is_empty(),
        "CREATE INDEX with table options",
    )?;
    if let Some(using) = &create.using
        && !matches!(using, IndexType::BTree)
    {
        // **PostgreSQL's own sentence comes first when there is an `INCLUDE`**, and it is a
        // different complaint: `amcaninclude` is a property of the access method, checked before
        // anything about the index is built, so `USING hash (…) INCLUDE (…)` is refused for the
        // payload rather than for the method. Measured for `hash` and for `brin`, one sentence
        // with the name substituted.
        if create.include.is_empty() {
            // Every index here is a range of the ordered key space, which is what a btree is.
            // Saying `USING hash` and getting one would be a different index than the user asked
            // for.
            return Err(SqlError::unsupported(format!("an index USING {using}")));
        }
        return Err(SqlError::AccessMethodWithoutInclude(
            using.to_string().to_ascii_lowercase(),
        ));
    }
    Ok(plan::CreateIndex {
        // Kept as text and lowered per row, the same trade a `CHECK` makes — and normalised the
        // way `pg_get_indexdef` prints it, which is one pair of parentheses however it was
        // written (`unwrap_nested`).
        predicate: create
            .predicate
            .as_ref()
            .map(|predicate| unwrap_nested(predicate).to_string()),
        // `NULLS DISTINCT` written out is the **default**, and a real server stores and prints
        // nothing for it — so only the negative form is carried, which is also the only one that
        // changes an answer.
        // `sqlparser` spells this clause as `Option<bool>` on a `CREATE INDEX` and as a
        // three-valued enum on a table constraint — `Some(false)` here is `NULLS NOT DISTINCT`.
        nulls_not_distinct: create.nulls_distinct == Some(false),
        // **Bare identifiers and nothing more.** `sqlparser` 0.62.0 types this clause as a list
        // of them, so `INCLUDE (name DESC)` and `INCLUDE (name varchar_pattern_ops)` -- both of
        // which a real server refuses with `42P17` and its own sentence -- are syntax errors
        // before this is reached. A C1 gap, in the plan's register.
        include: create
            .include
            .iter()
            .map(|name| fold_identifier(&name.value, name.quote_style.is_some()).0)
            .collect(),
        name: create.name.as_ref().map(object_name).transpose()?,
        table: relation_name(&create.table_name)?,
        keys: index_keys(&create.columns)?,
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
    let returning = insert
        .returning
        .as_deref()
        .map(lower_projection)
        .transpose()?;
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
        TableObject::TableName(name) => relation_name(name)?,
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

    // **`INSERT INTO t DEFAULT VALUES` is an `INSERT` of one row of no expressions.**
    //
    // `sqlparser` gives it `columns: []` and `source: None`, and one row of nothing is exactly
    // what it means: `crate::exec::dml` starts every row at each column's own default — the same
    // fill a short `VALUES` tuple and an unnamed column already get — and the sequences run after
    // the values, so a `bigserial` draws its number. So this needs no arm in the executor and
    // gets none; a row of NULLs, which is the tempting reading, would be a different statement.
    //
    // The other `source: None` is MySQL's `INSERT … SET`, refused above by its assignments, so
    // reaching here with no source means the two words were written.
    let Some(source) = insert.source.as_ref() else {
        return Ok(plan::Insert {
            table,
            columns,
            rows: vec![Vec::new()],
            returning,
            on_conflict: insert.on.as_ref().map(lower_on_conflict).transpose()?,
        });
    };
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
        returning,
        on_conflict: insert.on.as_ref().map(lower_on_conflict).transpose()?,
    })
}

/// `ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET …`.
///
/// **The arbiter's `WHERE` has nowhere to go.** PostgreSQL infers a *partial* unique index only
/// when the statement repeats its predicate — `ON CONFLICT ("a") WHERE "b" IS NOT NULL` — and
/// `sqlparser` 0.62.0's `ConflictTarget::Columns` is a bare `Vec<Ident>`, so that spelling does not
/// parse at all. A C1 gap, in the plan's register; the target-less and column-list forms, which are
/// the two `build_insert_sql` writes, both parse.
fn lower_on_conflict(on: &sqlparser::ast::OnInsert) -> Result<plan::OnConflict> {
    use sqlparser::ast::{ConflictTarget, OnConflictAction, OnInsert};
    let OnInsert::OnConflict(conflict) = on else {
        return Err(SqlError::unsupported("INSERT ... ON DUPLICATE KEY UPDATE"));
    };
    let target = match &conflict.conflict_target {
        None => Vec::new(),
        Some(ConflictTarget::Columns(columns)) => columns
            .iter()
            .map(|name| fold_identifier(&name.value, name.quote_style.is_some()).0)
            .collect(),
        // A constraint by name is a different inference: it names the constraint rather than
        // asking PostgreSQL to find one, and nothing captured it.
        Some(ConflictTarget::OnConstraint(_)) => {
            return Err(SqlError::unsupported("ON CONFLICT ON CONSTRAINT"));
        }
    };
    let action = match &conflict.action {
        OnConflictAction::DoNothing => plan::ConflictAction::DoNothing,
        OnConflictAction::DoUpdate(update) => {
            refuse_if(
                update.selection.is_some(),
                "ON CONFLICT ... DO UPDATE ... WHERE",
            )?;
            // **A target-less `DO UPDATE` is refused by a real server too**, which has nothing to
            // infer from; this node names the clause instead of guessing an index.
            refuse_if(
                target.is_empty(),
                "ON CONFLICT DO UPDATE with no conflict target",
            )?;
            plan::ConflictAction::DoUpdate(
                update
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
                    .collect::<Result<Vec<_>>>()?,
            )
        }
    };
    Ok(plan::OnConflict { target, action })
}

/// An expression, as far as phase 6a's `VALUES` needs one.
///
/// A negative number arrives as a unary minus over a positive literal, which is folded here so
/// that `-1` is one literal rather than an operator this crate would otherwise have to run.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per expression shape; splitting it would hide the vocabulary rather than clarify it"
)]
/// How deep [`lower_expr`] may descend on the caller's own stack.
///
/// **Measured, not chosen**: an `OR` chain overflows a 2 MiB stack at 59 terms in a debug build,
/// which is about 35 KiB of frame per level — `lower_expr` is one large match and every arm's
/// locals get a slot. Half of that measurement, so the guard fires with the stack half used.
///
/// A statement past it is not refused: it is lowered again on a thread with room to spare, which
/// is what `crate::parse` already does for a deeply nested *parse*. Only a statement past
/// [`crate::parse::MAX_PLAN_DEPTH`] is `54001` — and that limit is set by what the **executor**
/// can walk on a worker's stack, because a plan this crate builds and then cannot execute would
/// only move the crash a layer along. It did, once: guarding lowering alone left a 500-term chain
/// lowering happily and overflowing in the resolver.
const INLINE_LOWER_DEPTH: usize = if cfg!(debug_assertions) { 24 } else { 128 };

thread_local! {
    /// How many [`lower_expr`] frames this thread is inside.
    static LOWER_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The ceiling this thread is working to: the inline budget, or the full limit on a thread
    /// spawned with a stack for it.
    static LOWER_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(INLINE_LOWER_DEPTH) };
}

/// Counts one level of [`lower_expr`] and gives it back on the way out.
///
/// A guard object rather than a depth parameter, because `lower_expr` is reached from a dozen
/// sibling walkers — the query lowerer, the `CASE` arms, the function arguments — and a parameter
/// would have to be threaded through every one of them, where any missed call site silently
/// resets the count to zero.
struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<Self> {
        let limit = LOWER_LIMIT.with(std::cell::Cell::get);
        let depth = LOWER_DEPTH.with(std::cell::Cell::get);
        if depth >= limit {
            return Err(SqlError::StatementTooComplex);
        }
        LOWER_DEPTH.with(|cell| cell.set(depth + 1));
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        LOWER_DEPTH.with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per `sqlparser` expression node, in the parser's own order; splitting it \
              would put half the tree's shapes in a function named after nothing"
)]
fn lower_expr(expr: &Expr) -> Result<plan::Expr> {
    // **Invariant 9, at the one place that was missing it.** The parser has been guarded since
    // phase 6a, and its guard counts *brackets* — which is why it never saw this: `a OR b OR c`
    // has one bracket and builds an N-deep tree, so run 46's node parsed a boolean chain happily
    // and then overflowed a tokio worker's stack lowering it. Counted here rather than inferred
    // from the source, because the tree's depth is what this function descends.
    let _depth = DepthGuard::enter()?;
    let expr_ref = expr;
    match expr {
        Expr::Value(value) => lower_value(&value.value, false),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => lower_value(&value.value, true),
            // **`-2 ^ 2` is 4, not -4.** Unary minus binds *tighter* than `^` on a real server and
            // looser in this parser, so the minus is pushed into the base here. Measured, and the
            // opposite of the mathematical convention — which is why it is worth a line rather
            // than an assumption.
            Expr::BinaryOp { op, left, right }
                if arithmetic_op(op) == Some(plan::ArithOp::Power) =>
            {
                Ok(plan::Expr::Arithmetic {
                    op: plan::ArithOp::Power,
                    left: Box::new(lower_expr(&Expr::UnaryOp {
                        op: UnaryOperator::Minus,
                        expr: left.clone(),
                    })?),
                    right: Box::new(lower_expr(right)?),
                    ty: None,
                })
            }
            other => Ok(plan::Expr::Negate(Box::new(lower_expr(other)?))),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => lower_expr(expr),
        Expr::Nested(inner) => lower_expr(inner),
        // `~`, `~*`, `!~`, `!~*` — POSIX matching, in `LIKE`'s shape and for `LIKE`'s reason: a
        // subject and a *pattern*, with modifiers a binary op has nowhere to put.
        Expr::BinaryOp {
            left,
            op:
                op @ (BinaryOperator::PGRegexMatch
                | BinaryOperator::PGRegexIMatch
                | BinaryOperator::PGRegexNotMatch
                | BinaryOperator::PGRegexNotIMatch),
            right,
        } => Ok(plan::Expr::RegexMatch {
            operand: Box::new(lower_expr(left)?),
            pattern: Box::new(lower_expr(right)?),
            negated: matches!(
                op,
                BinaryOperator::PGRegexNotMatch | BinaryOperator::PGRegexNotIMatch
            ),
            case_insensitive: matches!(
                op,
                BinaryOperator::PGRegexIMatch | BinaryOperator::PGRegexNotIMatch
            ),
        }),
        // `x [NOT] LIKE p [ESCAPE c]` and `ILIKE`, which is the same matcher with both sides
        // folded. `ANY` is Snowflake's and is refused by name.
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        }
        | Expr::ILike {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => {
            refuse_if(*any, "LIKE ANY, which is Snowflake's")?;
            Ok(plan::Expr::Like {
                operand: Box::new(lower_expr(expr)?),
                pattern: Box::new(lower_expr(pattern)?),
                negated: *negated,
                case_insensitive: matches!(expr_ref, Expr::ILike { .. }),
                // **`ESCAPE` replaces the backslash rather than adding to it**: with one written a
                // lone `\` is an ordinary character. One character only, which is what
                // PostgreSQL takes.
                escape: match escape_char {
                    None => None,
                    Some(value) => match &value.value {
                        Value::SingleQuotedString(text) => {
                            let mut chars = text.chars();
                            match (chars.next(), chars.next()) {
                                (Some(one), None) => Some(one),
                                _ => {
                                    return Err(SqlError::unsupported(
                                        "ESCAPE with more than one character",
                                    ));
                                }
                            }
                        }
                        other => {
                            return Err(SqlError::unsupported(format!("ESCAPE {other}")));
                        }
                    },
                },
            })
        }
        // `ARRAY[…]` as a value. `= ANY(ARRAY[…])` never reaches here — that path reads the
        // elements as a list before the expression is lowered (`lower_array`) — so this is the
        // constructor standing on its own, in a projection or beside a comparison.
        Expr::Array(array) => lower_array_constructor(&array.elem),
        // `a[i]`. Only a **single** subscript: a slice (`a[1:2]`) answers an array rather than an
        // element and a second dimension is a shape the catalog's arrays do not have, so both are
        // named rather than approximated by the one this node has.
        Expr::CompoundFieldAccess { root, access_chain } => {
            use sqlparser::ast::{AccessExpr, Subscript};
            // **A qualified column puts its qualifier in the chain**: `d.indkey[0]` is the root
            // `d` with `indkey` and then the subscript after it, where `('{a,b}')[1]` is the array
            // with only the subscript. Both are one element of one array; a longer chain is a
            // field access this node does not have.
            // **A function call cannot be subscripted directly**, and that is PostgreSQL's
            // grammar rather than a limitation here: `array_agg(i)[1]` is
            // `42601 syntax error at or near "["` on a real server and `(array_agg(i))[1]` is the
            // spelling that works. `sqlparser` accepts both, so the parenthesised one is told
            // apart by the `Nested` it keeps — without this, this node answered a value where a
            // real server refuses the statement.
            if let [AccessExpr::Subscript(_)] = access_chain.as_slice()
                && matches!(root.as_ref(), Expr::Function(_))
            {
                return Err(SqlError::SyntaxAtOrNear("[".to_owned()));
            }
            let (operand, subscript) = match access_chain.as_slice() {
                [AccessExpr::Subscript(subscript)] => (lower_expr(root)?, subscript),
                [
                    AccessExpr::Dot(Expr::Identifier(column)),
                    AccessExpr::Subscript(subscript),
                ] => (
                    plan::Expr::Column {
                        table: Some(qualifier(root)?),
                        name: ident(column),
                    },
                    subscript,
                ),
                _ => return Err(SqlError::unsupported(format!("the access chain on {root}"))),
            };
            // A slice answers an **array** rather than an element, which is a different feature
            // and not a wider subscript; named rather than approximated by the one this node has.
            let Subscript::Index { index } = subscript else {
                return Err(SqlError::unsupported("an array slice"));
            };
            Ok(plan::Expr::Subscript {
                operand: Box::new(operand),
                index: Box::new(lower_expr(index)?),
                // `text` until a comparison gives it one, which is where an element's type comes
                // from (`plan::Expr::Subscript::element`).
                element: ColumnType::Text,
            })
        }
        // `DEFAULT` is a keyword `sqlparser` hands back as a bare identifier. Quoted, it is a
        // column called `DEFAULT` and stays one; unquoted, it is the clause.
        Expr::Identifier(name)
            if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("default") =>
        {
            Ok(plan::Expr::Default)
        }
        // **`current_schema` with no parentheses is the function**, not a column. SQL's niladic
        // functions may be written bare, and a real server answers `public` where a bare name it
        // does not know is `42703`. Unquoted only: `"current_schema"` is a column called that and
        // stays one, exactly as `DEFAULT` above.
        //
        // This is the shape r1's capture was failing on while `current_schema()` had been right
        // since the rung-4 unit — one name, two spellings, and only one of them was implemented.
        Expr::Identifier(name)
            if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("current_schema") =>
        {
            Ok(plan::Expr::CurrentSchema { all: None })
        }
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
        // **Not a `NOT` around an `=`.** The two are one operator each, because the negation of
        // unknown is unknown and `NOT (NULL = NULL)` is therefore NULL where
        // `NULL IS DISTINCT FROM NULL` is `false`.
        Expr::IsDistinctFrom(left, right) => Ok(plan::Expr::Binary {
            op: plan::BinaryOp::Distinct,
            left: Box::new(lower_expr(left)?),
            right: Box::new(lower_expr(right)?),
        }),
        Expr::IsNotDistinctFrom(left, right) => Ok(plan::Expr::Binary {
            op: plan::BinaryOp::NotDistinct,
            left: Box::new(lower_expr(left)?),
            right: Box::new(lower_expr(right)?),
        }),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(plan::Expr::Not(Box::new(lower_expr(expr)?))),
        // A comparison over `json` or `jsonb` is refused; [`refuse_json_comparison`] says why.
        Expr::BinaryOp { left, op, right } if is_comparison(op) && either_is_json(left, right) => {
            Err(refuse_json_comparison(op))
        }
        // `date + time` and `time + date`, the one arithmetic in this type that answers a type
        // this node has. Folded here, over **constants only**, which is the same boundary
        // `lower_cast` draws: a per-row `+` needs a `BinaryOp::Plus` that produces a value rather
        // than a boolean, and every one of the seventy matches on that enum assumes a comparison.
        // The per-row form stays `0A000` naming the operator, and
        // `tests/time.rs::a_column_plus_a_column_is_still_named` pins that it does — so this is a
        // constant folded, not arithmetic landed.
        Expr::BinaryOp {
            op: BinaryOperator::Plus,
            left,
            right,
        } if date_plus_time(left, right)?.is_some() => {
            let micros = date_plus_time(left, right)?.unwrap_or_default();
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Timestamp(micros),
            ))))
        }
        // The six that yield a **value**. They are lowered before the comparisons because they are
        // a different node: `crate::plan::ArithOp` says why the two are not one enum.
        Expr::BinaryOp { op, left, right } if arithmetic_op(op).is_some() => {
            Ok(plan::Expr::Arithmetic {
                op: arithmetic_op(op).unwrap_or(plan::ArithOp::Add),
                left: Box::new(lower_expr(left)?),
                right: Box::new(lower_expr(right)?),
                ty: None,
            })
        }
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
                // **`&&` is carried as a call, not as a comparison.** Two ranges are not ordered
                // — `pg_cmp` says nothing about them — so it cannot be a `BinaryOp`, and every
                // walker already descends into a call's arguments
                // (`crate::plan::expr::CatalogFunc::RangeOverlaps`).
                // **The hstore operators are calls, not comparisons**, for the reason `&&` is:
                // none of them is an ordering, and every walker already descends into a call's
                // arguments. Which type they are *for* is decided when they are evaluated, so
                // `||` over anything but two hstores still gets the refusal it has today rather
                // than a wrong answer.
                BinaryOperator::Arrow => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreFetch,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::Question => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreHasKey,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::AtArrow => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreContains,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::StringConcat => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreConcat,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::PGOverlap => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::RangeOverlaps,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
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
        Expr::InList {
            expr,
            list,
            negated,
        } => Ok(plan::Expr::InList {
            operand: Box::new(lower_expr(expr)?),
            list: list.iter().map(lower_expr).collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        // `INTERVAL '1 day'` and `INTERVAL '1' DAY`: SQL's typed-literal spelling for this one
        // type, which `sqlparser` gives its own node rather than a `TypedString`. A **leading
        // field** names the unit the bare number is in — `INTERVAL '1' DAY` is one day — and a
        // trailing one bounds the range, which is the typmod this node drops.
        Expr::Interval(interval) => {
            let text = cast_literal_text(&interval.value)?
                .ok_or_else(|| SqlError::unsupported("an INTERVAL over a non-literal"))?;
            let spelled = match &interval.leading_field {
                // Already carries its own units, so the field adds nothing.
                _ if text.contains(|c: char| c.is_ascii_alphabetic()) => text.clone(),
                Some(field) => format!("{text} {field}"),
                None => text.clone(),
            };
            let value = value::interval::from_text(&spelled)?;
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Interval {
                    months: value.months,
                    days: value.days,
                    micros: value.micros,
                },
            ))))
        }
        // **`ARRAY( SELECT … )` is a sixth spelling**, and `sqlparser` gives it as a *function*
        // named `ARRAY` whose arguments are a subquery — which is why it refused as "the function
        // ARRAY is not supported" while `ARRAY[…]`, a different production entirely, answered.
        // Its argument is any query, so `ARRAY(VALUES (1),(2))` is one too.
        Expr::Function(function)
            // Written without `relation_name`, which *refuses* a qualified name: reading it here
            // turned `public.obj_description(…)`'s `42883` into this arm's `0A000` — a guard has
            // to be a question, not a decision.
            if function.name.to_string().eq_ignore_ascii_case("array")
                && matches!(
                    function.args,
                    sqlparser::ast::FunctionArguments::Subquery(_)
                ) =>
        {
            let sqlparser::ast::FunctionArguments::Subquery(query) = &function.args else {
                return Err(SqlError::unsupported("the function ARRAY"));
            };
            Ok(plan::Expr::Subquery(Box::new(plan::SubqueryExpr::bare(
                plan::SubqueryKind::Array,
                Box::new(lower_query(query)?),
            ))))
        }
        Expr::Function(function) => lower_function(function),
        Expr::Cast {
            expr, data_type, ..
        } => lower_cast(expr, data_type),
        // `DATE '2020-01-01'` and `TIMESTAMP '…'`: SQL's typed-literal spelling, which is the
        // **same thing** as the cast written the other way round — a real server records no
        // difference between `DATE 'x'` and `'x'::date`, so neither does this.
        Expr::TypedString(typed) => lower_cast(
            &Expr::Value(
                Value::SingleQuotedString(typed.value.clone().into_string().unwrap_or_default())
                    .into(),
            ),
            &typed.data_type,
        ),
        // The five spellings of a subquery in an expression. Each carries the sub-select lowered
        // the same way the outer one is -- `lower_query` refuses inside a subquery exactly what it
        // refuses outside one, so a `WITH` or a second `JOIN` in there is the same `0A000` by the
        // same name (`docs/plans/phase-12-subquery.md` §3).
        Expr::Subquery(query) => Ok(plan::Expr::Subquery(Box::new(plan::SubqueryExpr::bare(
            plan::SubqueryKind::Scalar,
            Box::new(lower_query(query)?),
        )))),
        Expr::Exists { subquery, negated } => {
            Ok(plan::Expr::Subquery(Box::new(plan::SubqueryExpr::bare(
                plan::SubqueryKind::Exists { negated: *negated },
                Box::new(lower_query(subquery)?),
            ))))
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Ok(plan::Expr::Subquery(Box::new(
            plan::SubqueryExpr::compared(
                plan::SubqueryKind::In { negated: *negated },
                lower_expr(expr)?,
                Box::new(lower_query(subquery)?),
            ),
        ))),
        // `<op> ANY (SELECT …)` -- a **subquery** on the right, which is this phase's and takes
        // every one of the six operators. It is matched before the array arm below because the two
        // share a grammar and nothing else: `= ANY (SELECT …)` is a nested loop over a plan and
        // `= ANY ('{1,2}')` is a list of values, and only the right-hand side tells them apart.
        // `SOME` is not carried at all: it is a **spelling** of `ANY` rather than a second
        // operator, and a real server answers the two identically (measured,
        // `SELECT 1 = SOME (SELECT …)`).
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } if matches!(strip_nesting(right), Expr::Subquery(_)) => {
            lower_quantified(left, compare_op, right, false)
        }
        // **A list when the lowering can see one, and a value when it cannot.** `ARRAY[1,2]`,
        // `'{a,b}'` and `current_schemas(false)` are all known here, and expanding them into an
        // `IN` list is what lets an index seek use them. A **column** — `a.attnum =
        // ANY(i.indkey)` — is not: its value differs per row, so it stays an array and is read
        // when the row is (`plan::Expr::AnyArray`).
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } if *compare_op == BinaryOperator::Eq => {
            let operand = Box::new(lower_expr(left)?);
            match lower_array(right)? {
                Some(list) => Ok(plan::Expr::InList {
                    operand,
                    list,
                    negated: false,
                }),
                None => Ok(plan::Expr::AnyArray {
                    operand,
                    array: Box::new(lower_expr(strip_nesting(right))?),
                }),
            }
        }
        // Any other operator against an array — `> ANY`, `<> ANY` — is a different quantifier and
        // is named rather than approximated by the one this node has.
        Expr::AnyOp { compare_op, .. } => Err(SqlError::unsupported(format!(
            "the quantifier {compare_op} ANY"
        ))),
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => lower_quantified(left, compare_op, right, true),
        // `CASE WHEN … THEN … [ELSE …] END`. The **simple** form carries an operand after `CASE`
        // and is refused by name: a real server prints it back as `CASE x WHEN 1 THEN …`, so
        // desugaring it into `WHEN x = 1` would store a definition that is not the one written and
        // `pg_get_indexdef` would answer with something `ActiveRecord` never wrote. Nothing in
        // `schema.rb` uses it.
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            refuse_if(
                operand.is_some(),
                "CASE <expression> WHEN ..., the simple form",
            )?;
            Ok(plan::Expr::Case {
                branches: conditions
                    .iter()
                    .map(|branch| {
                        Ok(plan::CaseBranch {
                            when: lower_expr(&branch.condition)?,
                            then: lower_expr(&branch.result)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                otherwise: else_result
                    .as_deref()
                    .map(lower_expr)
                    .transpose()?
                    .map(Box::new),
            })
        }
        other => Err(SqlError::unsupported(format!("the expression {other}"))),
    }
}

/// `x <op> ANY (SELECT …)` and `x <op> ALL (SELECT …)`.
///
/// The right-hand side has to be a **subquery**. `= ANY (array)` is the same grammar over a value
/// of an array type, which this node has no types for yet, and it is refused by that name rather
/// than by "the expression", so a reader searching for what is missing finds the array and not the
/// quantifier (`docs/plans/phase-12-subquery.md` §4).
fn lower_quantified(
    left: &Expr,
    compare_op: &BinaryOperator,
    right: &Expr,
    all: bool,
) -> Result<plan::Expr> {
    let op = match compare_op {
        BinaryOperator::Eq => plan::BinaryOp::Eq,
        BinaryOperator::NotEq => plan::BinaryOp::NotEq,
        BinaryOperator::Lt => plan::BinaryOp::Lt,
        BinaryOperator::LtEq => plan::BinaryOp::LtEq,
        BinaryOperator::Gt => plan::BinaryOp::Gt,
        BinaryOperator::GtEq => plan::BinaryOp::GtEq,
        other => {
            return Err(SqlError::unsupported(format!(
                "the operator {other} with ANY/ALL"
            )));
        }
    };
    let quantifier = if all { "ALL" } else { "ANY" };
    let Expr::Subquery(query) = strip_nesting(right) else {
        return Err(SqlError::unsupported(format!("{quantifier} over an array")));
    };
    Ok(plan::Expr::Subquery(Box::new(
        plan::SubqueryExpr::compared(
            plan::SubqueryKind::Quantified { op, all },
            lower_expr(left)?,
            Box::new(lower_query(query)?),
        ),
    )))
}

/// Past any number of `(…)` wrappers, which is how `= ANY ((SELECT …))` parses.
fn strip_nesting(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => strip_nesting(inner),
        other => other,
    }
}

/// A function call: one of the five aggregates, or `0A000` naming it.
///
/// Everything a real server would answer `42883` for is *also* refused here, so the distinction
/// this function does not make -- a function PostgreSQL has and we do not, against one neither of
/// us has -- is one no caller can act on anyway. What it must not do is execute a name it does not
/// know, which is why the fall-through is a refusal rather than a lookup that returns NULL.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per function family; splitting it would hide the vocabulary rather than clarify it"
)]
fn lower_function(function: &sqlparser::ast::Function) -> Result<plan::Expr> {
    use sqlparser::ast::{
        DuplicateTreatment, FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments,
    };

    let name = unqualified_function_name(function)?;
    // `current_schema()` is the scalar half of `current_schemas()`: one name rather than a list,
    // and `public` on this node because `public` is the only schema it has. Measured; the two are
    // together here so a reader finds both at once.
    //
    // **The arity is checked**, because PostgreSQL resolves a function by name *and* argument
    // types: `current_schema(false)` is `42883 function current_schema(boolean) does not exist`,
    // not a `current_schema()` that shrugged at an argument. A node that ignored the argument
    // would answer `public` where a real server refuses, which is a wrong answer rather than a
    // gap — and it did, until r1's capture replay caught it.
    if name.eq_ignore_ascii_case("current_schema") {
        refuse_wrong_arity(function, "current_schema", 0)?;
        return Ok(plan::Expr::CurrentSchema { all: None });
    }
    // **The database this session is connected to.** Left unresolved here exactly as
    // `current_schema()` is, and for the same reason: the answer is a property of the session and
    // a lowering has no session. `ActiveRecord`'s adapter runs it four times while connecting —
    // once alone and three times joined to `pg_database` for the encoding, the collation and the
    // ctype.
    if name.eq_ignore_ascii_case("current_database") {
        refuse_wrong_arity(function, "current_database", 0)?;
        return Ok(plan::Expr::CurrentDatabase);
    }
    // **The array, unresolved.** It was folded here into a literal `{public}` while `public` was
    // the only schema; now the value is the session's `search_path` and a lowering has no session,
    // so it becomes an expression `crate::exec::Executor::bound` fills in. The `= ANY (…)`
    // expansion into an `IN` list moved with it, for the same reason: the list is not known here.
    if let Some(implicit) = schema_function(function)? {
        return Ok(plan::Expr::CurrentSchema {
            all: Some(implicit),
        });
    }
    // `current_setting(name)` and `current_setting(name, missing_ok)`. The **name is not checked
    // here**: a real server parses `current_setting('nosuch')` and raises `42704` when it runs, so
    // refusing at lowering would answer earlier than PostgreSQL does — the reading `SET` already
    // takes for the same reason.
    if name.eq_ignore_ascii_case("current_setting") {
        let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args else {
            return Err(SqlError::UndefinedFunction("current_setting()".to_owned()));
        };
        let text = |arg: &FunctionArg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) => match &value.value {
                Value::SingleQuotedString(text) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        };
        let flag = |arg: &FunctionArg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) => match value.value {
                Value::Boolean(flag) => Some(flag),
                _ => None,
            },
            _ => None,
        };
        let setting = match args.as_slice() {
            [one] => text(one).map(|name| (name, false)),
            [one, two] => text(one).zip(flag(two)),
            _ => {
                refuse_wrong_arity(function, "current_setting", 1)?;
                None
            }
        };
        let Some((name, missing_ok)) = setting else {
            // A wrong **type** is `42883` naming the signature, the reading `current_schemas`
            // takes: PostgreSQL resolves by name *and* argument types.
            return Err(SqlError::UndefinedFunctionTypes(
                "current_setting(...)".to_owned(),
            ));
        };
        return Ok(plan::Expr::CurrentSetting { name, missing_ok });
    }
    // `lower` and `upper`, the two scalar functions this node has. Both take exactly one
    // argument and a wrong count is `42883` naming the signature, not a badly-called function —
    // `lower()` and `lower('a','b')` are each their own message, measured.
    if let Some(func) = plan::ScalarFunc::from_name(&name) {
        refuse_wrong_arity(function, func.name(), 1)?;
        let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args else {
            return Err(SqlError::UndefinedFunction(format!("{}()", func.name())));
        };
        let [FunctionArg::Unnamed(FunctionArgExpr::Expr(operand))] = args.as_slice() else {
            return Err(SqlError::UndefinedFunction(format!(
                "{}(unknown)",
                func.name()
            )));
        };
        return Ok(plan::Expr::Scalar {
            func,
            operand: Box::new(lower_expr(operand)?),
        });
    }
    // The two UUID functions. Zero arguments, and a wrong count is `42883` naming the signature
    // the way every other function's is — `gen_random_uuid(1)` does not exist either.
    if let Some(func) = plan::UuidFunc::from_name(&name) {
        refuse_wrong_arity(function, func.name(), 0)?;
        return Ok(plan::Expr::Uuid(func));
    }
    // **`COALESCE` is a construct, not a function**, so its failures are the grammar's: no
    // arguments at all is `42601 syntax error at or near ")"` where a function would be `42883`.
    // `sqlparser` parses it as an ordinary call, which is why the distinction has to be made here.
    if name.eq_ignore_ascii_case("coalesce") {
        let args = function_arguments(function, "coalesce")?;
        if args.is_empty() {
            return Err(SqlError::SyntaxAtOrNear(")".to_owned()));
        }
        return Ok(plan::Expr::Coalesce(
            args.into_iter()
                .map(lower_expr)
                .collect::<Result<Vec<_>>>()?,
        ));
    }
    if let Some(func) = plan::SequenceFunc::from_name(&name) {
        return lower_sequence_function(func, function);
    }
    if let Some(func) = plan::CatalogFunc::from_name(&name) {
        return lower_catalog_function(func, function);
    }
    // **A set-returning function in the target list.** The same call `FROM` takes, in the one
    // other place PostgreSQL allows it; what it does there is make rows, which the cursor's
    // projection does rather than this.
    if is_set_returning(function) {
        return Ok(plan::Expr::SetFunc(Box::new(plan::TableFunction {
            name: name.to_ascii_lowercase(),
            args: function_arguments(function, &name)?
                .into_iter()
                .map(lower_expr)
                .collect::<Result<Vec<_>>>()?,
            def: None,
        })));
    }
    let Some(func) = plan::AggregateFunc::from_name(&name) else {
        return Err(SqlError::unsupported(format!("the function {name}")));
    };

    // Each of these turns an aggregate into a different aggregate, so honouring the call and
    // dropping the clause would answer a question the user did not ask.
    refuse_if(function.over.is_some(), "a window function")?;
    refuse_if(function.filter.is_some(), "an aggregate FILTER clause")?;
    refuse_if(function.null_treatment.is_some(), "IGNORE/RESPECT NULLS")?;
    refuse_if(!function.within_group.is_empty(), "WITHIN GROUP")?;

    let FunctionArguments::List(FunctionArgumentList {
        duplicate_treatment,
        args,
        clauses,
    }) = &function.args
    else {
        // `count` with no parentheses at all cannot be an aggregate; PostgreSQL parses `count` as
        // a column reference and answers `42703`, which is what a bare identifier already gets.
        return Err(SqlError::unsupported(format!("the function {name}")));
    };
    let order_by = lower_aggregate_clauses(clauses)?;
    let distinct = matches!(duplicate_treatment, Some(DuplicateTreatment::Distinct));

    // A `*` mixed with anything else is not a call PostgreSQL's grammar has, and it says so with
    // a syntax error rather than a missing function -- naming the comma when the `*` came first
    // and the `*` when it did not. Both spellings were measured; `sqlparser` reads them both.
    let star_at = args.iter().position(|arg| {
        matches!(
            arg,
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_))
        )
    });
    if let Some(at) = star_at
        && args.len() > 1
    {
        return Err(SqlError::SyntaxAtOrNear(
            if at == 0 { "," } else { "*" }.to_owned(),
        ));
    }

    let star = star_at.is_some();
    if star && func != plan::AggregateFunc::Count {
        // `sum(*)` and friends: PostgreSQL has no aggregate with a `*` form but `count`.
        return Err(SqlError::unsupported(format!("{name}(*)")));
    }
    // `count()` is `42809` on a real server and says which spelling to use instead. Every other
    // arity is `42883` naming the argument **types**, so it waits for the planner.
    if args.is_empty() && func == plan::AggregateFunc::Count {
        return Err(SqlError::ParameterlessAggregate);
    }

    let args = if star {
        Vec::new()
    } else {
        args.iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => lower_expr(expr),
                other => Err(SqlError::unsupported(format!(
                    "the aggregate argument {other}"
                ))),
            })
            .collect::<Result<Vec<_>>>()?
    };

    Ok(plan::Expr::Aggregate(Box::new(plan::AggregateCall {
        func,
        args,
        star,
        distinct,
        order_by,
    })))
}

/// A function's name with its `pg_catalog.` qualifier removed — and any **other** qualifier
/// refused.
///
/// **Checked rather than stripped.** These functions live in `pg_catalog` and in no other schema,
/// so `public.obj_description(…)` is `42883 function public.obj_description(regclass) does not
/// exist` on a real server; a lowering that dropped whatever schema it was handed would answer
/// where a real server raises, which is a wrong answer and not a gap. Measured, and the refusal
/// keeps the spelling the user wrote.
///
/// Matched the way a schema name is: unquoted it folds case, and quoted it still matches, because
/// the schema really is `pg_catalog` in lower case. The same rule [`relation_name`] has had for
/// relations since the rung-4 unit — `pg_catalog.pg_class` is `pg_class` — applied to the other
/// kind of name a query can qualify.
///
/// The strip happens **before** the name is resolved, so a function this node does not have is
/// refused under its bare name: `pg_catalog.length('abc')` names `length`, which is what a reader
/// can search for.
fn unqualified_function_name(function: &sqlparser::ast::Function) -> Result<String> {
    let parts: Option<Vec<&Ident>> = function.name.0.iter().map(|part| part.as_ident()).collect();
    let Some([schema, name]) = parts.as_deref() else {
        return Ok(function.name.to_string());
    };
    if schema.value.eq_ignore_ascii_case("pg_catalog") {
        return Ok(ident(name));
    }
    Err(SqlError::UndefinedQualifiedFunction(format!(
        "{}.{}({})",
        ident(schema),
        ident(name),
        function_argument_types(function)
    )))
}

/// The argument types a `42883` names, in the order they were written.
fn function_argument_types(function: &sqlparser::ast::Function) -> String {
    let sqlparser::ast::FunctionArguments::List(list) = &function.args else {
        return String::new();
    };
    list.args
        .iter()
        .map(argument_type_name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a cast's operand is a **quoted string**, which is what tells the two `regclass`
/// directions apart.
///
/// `'rc'::regclass` is a name being resolved to an oid; `2147483647::regclass` and
/// `c.oid::regclass` are oids being resolved to a name. The test cannot be "is it a literal" — a
/// number is one too, and reading `2147483647` as a relation *name* is how the first version of
/// this got it wrong.
fn is_string_literal(expr: &Expr) -> bool {
    match unwrap_nested(expr) {
        Expr::Value(value) => matches!(
            value.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ),
        _ => false,
    }
}

/// The table qualifier at the root of an access chain — the `d` in `d.indkey[0]`.
fn qualifier(root: &Expr) -> Result<String> {
    match root {
        Expr::Identifier(name) => Ok(ident(name)),
        other => Err(SqlError::unsupported(format!(
            "the access chain on {other}"
        ))),
    }
}

/// `GENERATED …` on a column: an **expression** or an **identity**, told apart by whether an
/// expression was written.
///
/// `Ok(expr)` is a `GENERATED ALWAYS AS (expr) STORED` column and `Err(identity)` is one of the
/// three identity kinds — a `Result` used as a two-way answer rather than as a failure, because
/// both outcomes are success and the caller stores them in different fields.
fn lower_generated(
    generated_as: GeneratedAs,
    sequence_options: Option<&[sqlparser::ast::SequenceOptions]>,
    generation_expr: Option<&Expr>,
    mode: Option<&sqlparser::ast::GeneratedExpressionMode>,
) -> Result<std::result::Result<String, plan::Identity>> {
    let Some(expr) = generation_expr else {
        return Ok(Err(identity_kind(generated_as, sequence_options, None)?));
    };
    // `VIRTUAL` computes on read where `STORED` computes on write, so a node that took the word
    // and stored anyway would answer the same value after the source changed under it. Named
    // rather than approximated.
    //
    // **Currently unreachable, and kept anyway**: `sqlparser` 0.62.0 expects `STORED` and makes
    // `VIRTUAL` a syntax error, which is a contract C1 gap in the plan's register — PostgreSQL 19
    // takes the word and reports `attgenerated` `v`. This is what the day the parser learns it
    // needs.
    refuse_if(
        matches!(mode, Some(sqlparser::ast::GeneratedExpressionMode::Virtual)),
        "GENERATED ALWAYS AS (expression) VIRTUAL",
    )?;
    refuse_if(
        generated_as == GeneratedAs::ByDefault,
        "GENERATED BY DEFAULT AS (expression)",
    )?;
    // The **normalised** text, the way a `CHECK` and an index predicate are stored: `pg_get_expr`
    // prints this back, so the parentheses a user wrote must not survive into the catalog.
    Ok(Ok(unwrap_nested(expr).to_string()))
}

/// The clauses inside an aggregate's parentheses.
///
/// **`ORDER BY` is honoured and every other clause is named.** It orders the values within one
/// group — `array_agg(x ORDER BY y DESC)` — and a real server takes it on every aggregate, so it
/// is lowered for every aggregate rather than for the one that can show it. Accepting it and
/// dropping it would put an `array_agg` in an order the caller did not ask for, which is a wrong
/// answer and not a gap.
fn lower_aggregate_clauses(
    clauses: &[sqlparser::ast::FunctionArgumentClause],
) -> Result<Vec<plan::OrderItem>> {
    let mut order_by = Vec::new();
    for clause in clauses {
        let sqlparser::ast::FunctionArgumentClause::OrderBy(items) = clause else {
            return Err(SqlError::unsupported(format!(
                "an aggregate {clause} clause"
            )));
        };
        for item in items {
            refuse_if(item.with_fill.is_some(), "WITH FILL")?;
            order_by.push(plan::OrderItem {
                expr: lower_expr(&item.expr)?,
                descending: item.options.asc == Some(false),
                nulls_first: item.options.nulls_first,
            });
        }
    }
    Ok(order_by)
}

/// A `pg_catalog` function that prints a definition — `format_type(oid, typmod)`.
///
/// **The arity is checked and nothing else is**, which is the shape PostgreSQL resolves a function
/// by: `format_type(23)` is `42883 function format_type(integer) does not exist` with
/// `DETAIL: No function of that name accepts the given number of arguments.`, naming the *number*
/// rather than the types. Measured (`esker-rails-harness/captures/pg19_format_type.txt`).
///
/// The arguments are ordinary expressions and are evaluated per row, because that is how
/// `ActiveRecord` writes it: `format_type(a.atttypid, a.atttypmod)` over every row of
/// `pg_attribute`.
fn lower_catalog_function(
    func: plan::CatalogFunc,
    function: &sqlparser::ast::Function,
) -> Result<plan::Expr> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments};

    // Each of these would turn the call into a different call, so honouring it and dropping the
    // clause would answer a question nobody asked.
    refuse_if(function.over.is_some(), "a window function")?;
    refuse_if(function.filter.is_some(), "an aggregate FILTER clause")?;
    refuse_if(!function.within_group.is_empty(), "WITHIN GROUP")?;

    refuse_wrong_arities(function, func.name(), func.arities())?;
    let empty = Vec::new();
    let args = match &function.args {
        FunctionArguments::List(FunctionArgumentList { args, .. }) => args,
        // **No argument list at all**, which is how `CURRENT_TIMESTAMP` and `CURRENT_DATE` are
        // written: a keyword, no parentheses. For a function that takes none that is a call with
        // zero arguments and not a missing one — `refuse_wrong_arities` above has already said so.
        _ if func.arities().contains(&0) => &empty,
        _ => return Err(SqlError::UndefinedFunction(format!("{}()", func.name()))),
    };
    let args = args
        .iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => lower_expr(expr),
            other => Err(SqlError::unsupported(format!(
                "{}({other}) with an argument that is not an expression",
                func.name()
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
        func,
        args,
    })))
}

/// `nextval('s')`, `currval('s')`, `setval('s', 10 [, false])`, `lastval()`.
///
/// The argument is a **name**, not a string, and that is the thing a reader would get wrong. Its
/// text goes through the same folding an unquoted identifier does — measured, `nextval('Q1_ID_SEQ')`
/// finds `q1_id_seq` and `nextval('"q1_id_seq"')` finds it too — so a sequence created for a column
/// named `Id` is reachable under either spelling, exactly as the column is.
fn lower_sequence_function(
    func: plan::SequenceFunc,
    function: &sqlparser::ast::Function,
) -> Result<plan::Expr> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments};

    refuse_if(function.over.is_some(), "a window function")?;
    refuse_if(function.filter.is_some(), "an aggregate FILTER clause")?;

    let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args else {
        return Err(SqlError::unsupported(format!(
            "the function {} with no argument list",
            func.name()
        )));
    };
    let plain: Vec<&Expr> = args
        .iter()
        .filter_map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
            _ => None,
        })
        .collect();
    if plain.len() != args.len() {
        return Err(SqlError::unsupported(format!(
            "a named argument to {}",
            func.name()
        )));
    }

    let named = |expr: &Expr| -> Result<String> {
        match expr {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text)
                | Value::DollarQuotedString(DollarQuotedString { value: text, .. }) => {
                    Ok(sequence_reference(text))
                }
                other => Err(SqlError::unsupported(format!(
                    "the sequence name {other}, which is not a string literal"
                ))),
            },
            other => Err(SqlError::unsupported(format!(
                "the sequence name {other}, which is not a string literal"
            ))),
        }
    };

    let call = match (func, plain.as_slice()) {
        (plan::SequenceFunc::LastVal, []) => plan::SequenceCall {
            func,
            name: None,
            value: None,
            is_called: true,
        },
        (plan::SequenceFunc::NextVal | plan::SequenceFunc::CurrVal, [name]) => plan::SequenceCall {
            func,
            name: Some(named(name)?),
            value: None,
            is_called: true,
        },
        (plan::SequenceFunc::SetVal, [name, value] | [name, value, _]) => {
            let is_called = match plain.as_slice() {
                [_, _, flag] => match flag {
                    Expr::Value(value) => match &value.value {
                        Value::Boolean(flag) => *flag,
                        other => {
                            return Err(SqlError::unsupported(format!(
                                "setval's is_called argument {other}, which is not a boolean \
                                 literal"
                            )));
                        }
                    },
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "setval's is_called argument {other}, which is not a boolean literal"
                        )));
                    }
                },
                // Two arguments: `is_called` defaults to true, so the next `nextval` answers one
                // past the value rather than the value itself.
                _ => true,
            };
            let value = match lower_expr(value)? {
                plan::Expr::Literal(plan::Literal::Integer(value)) => value,
                other => {
                    return Err(SqlError::unsupported(format!(
                        "setval's value {other:?}, which is not an integer literal"
                    )));
                }
            };
            plan::SequenceCall {
                func,
                name: Some(named(name)?),
                value: Some(value),
                is_called,
            }
        }
        // Every other arity. PostgreSQL answers `42883` naming the argument types; this crate has
        // no overload table to name them from, so it names the call instead.
        (_, other) => {
            return Err(SqlError::unsupported(format!(
                "{} with {} arguments",
                func.name(),
                other.len()
            )));
        }
    };
    Ok(plan::Expr::Sequence(Box::new(call)))
}

/// A sequence's name, as it is written inside `nextval`'s string argument — **carried, not
/// parsed**.
///
/// It used to be read here: a literal `public.` prefix stripped, one pair of surrounding quotes
/// taken off the whole string, and the rest folded. That is a name parser, and it was the third in
/// this crate — `catalog::parse_qualified` is the one `::regclass` uses and it handles the same
/// input. Two parsers of one grammar disagree eventually, and this pair disagreed on the spelling
/// `ActiveRecord` sends more than any other: `"public"."accounts_id_seq"` has no `public.` prefix
/// to strip and is not one quoted identifier, so it came out as the nonsense `public"."accounts_id_seq`
/// and every fixture load in run 50 failed on it — 4,873 tests in 93 files.
///
/// So the text travels whole and `crate::exec::Executor::require_sequence` resolves it, which is
/// also the only place that *can* resolve it: a schema qualifier now names a real schema, and
/// picking the right one needs the catalog and the `search_path` that a lowering does not have.
fn sequence_reference(text: &str) -> String {
    text.to_owned()
}

/// `ARRAY[…]` as a **value**, folded where every element is a constant.
///
/// The constructor builds an array from expressions where a literal builds one from text, and the
/// element type is the elements' — PostgreSQL's `select_common_type`, narrowed to the four array
/// types this node has. A constructor over anything but constants is refused by name: building one
/// per row is a node of its own, and none of the statements this node is measured against has one.
///
/// **`ARRAY[]` is an error and `'{}'::int[]` is not.** An empty constructor has no elements to take
/// a type from, so PostgreSQL answers `42P18` with a hint; an empty *literal* has its type from the
/// cast and is a perfectly good empty array. Measured, both.
fn lower_array_constructor(elements: &[Expr]) -> Result<plan::Expr> {
    let mut texts: Vec<Option<String>> = Vec::with_capacity(elements.len());
    let mut element = None;
    for expr in elements {
        // **An `'…'::hstore` element is a constant too.** `hstore_test.rb` writes
        // `t.hstore "payload", array: true` and then `ARRAY['"AA"=>"BB"'::hstore, …]`, which is a
        // cast of a string constant and folds here exactly as the string would — narrowed to
        // hstore rather than to every type, because a cast to any other one is a declared
        // divergence and widening it would move answers this unit did not measure.
        if let Expr::Cast {
            expr: inner,
            data_type,
            ..
        } = strip_nesting(expr)
            && matches!(lower_type(data_type), Ok((ColumnType::Hstore, _)))
            && let Expr::Value(value) = strip_nesting(inner)
            && let Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) = &value.value
        {
            element = Some(ColumnType::Hstore);
            texts.push(Some(text.clone()));
            continue;
        }
        let Expr::Value(value) = strip_nesting(expr) else {
            return Err(SqlError::unsupported(
                "an ARRAY constructor over anything but constants",
            ));
        };
        // The widest element type wins, in PostgreSQL's own order: a string makes the whole array
        // `text`, a decimal makes it `numeric`, and integers alone leave it an integer array.
        let (text, wanted) = match &value.value {
            Value::Null => (None, None),
            Value::Number(digits, _) if digits.contains('.') => {
                (Some(digits.clone()), Some(ColumnType::Numeric))
            }
            Value::Number(digits, _) => (Some(digits.clone()), Some(ColumnType::Int8)),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                (Some(text.clone()), Some(ColumnType::Text))
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "an ARRAY constructor holding {other}"
                )));
            }
        };
        element = match (element, wanted) {
            (Some(ColumnType::Text), _) | (_, Some(ColumnType::Text)) => Some(ColumnType::Text),
            (Some(ColumnType::Numeric), _) | (_, Some(ColumnType::Numeric)) => {
                Some(ColumnType::Numeric)
            }
            (known, None) => known,
            (None, wanted) => wanted,
            (known, _) => known,
        };
        texts.push(text);
    }
    let Some(element) = element else {
        return Err(SqlError::EmptyArrayType);
    };
    let mut values = Vec::with_capacity(texts.len());
    for text in texts {
        values.push(match text {
            None => None,
            Some(text) => Some(Datum::from_text(element, &text)?),
        });
    }
    Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
        Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
            element, 1, values,
        )),
    ))))
}

/// The elements of an array expression, for the right-hand side of `= ANY(…)`.
///
/// Three spellings, all of which `ActiveRecord` or its `pg` driver may send:
///
/// * `ARRAY['a', 'b']` — the constructor, which `sqlparser` gives as `Expr::Array`;
/// * `'{a,b}'` — an array **literal** as text, which PostgreSQL's array input function reads;
/// * `current_schemas(false)` — a function that returns one.
///
/// Everything stays at the level of an **expression**: this node has no array *value* and no array
/// column, and nothing here creates one. `= ANY` is the only place an array appears, it becomes an
/// `IN` list before the planner sees it, and a `Datum` is never an array. That is the split ADR
/// 0033's roadmap describes — expression-level arrays now, stored arrays with the tier-2 unit that
/// needs a column of them.
fn lower_array(expr: &Expr) -> Result<Option<Vec<plan::Expr>>> {
    Ok(Some(match expr {
        Expr::Array(array) => array
            .elem
            .iter()
            .map(lower_expr)
            .collect::<Result<Vec<_>>>()?,
        Expr::Nested(inner) => return lower_array(inner),
        // **`current_schemas(…)` is no longer a list this lowering can see.** Its value is the
        // session's `search_path`, which arrives at `crate::exec::Executor::bound`, so it stays an
        // expression and the row evaluator reads it — the same road a column takes below. The
        // `IN`-list expansion it used to get was an index-seek optimisation, and every statement
        // that writes it reads a catalog view, which has no index to seek.
        Expr::Function(function) => {
            let _ = schema_function(function)?;
            return Ok(None);
        }
        // `'{a,b}'::text[]` and a bare `'{a,b}'`: the cast is a no-op here, because what the array
        // holds is decided by what it is compared against, exactly as an `IN` list's elements are.
        Expr::Cast { expr, .. } => return lower_array(expr),
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) => parse_array_literal(text)?
                .into_iter()
                .map(|item| {
                    plan::Expr::Literal(match item {
                        Some(text) => plan::Literal::String(text),
                        None => plan::Literal::Null,
                    })
                })
                .collect(),
            _ => return Ok(None),
        },
        // A **column** — `a.attnum = ANY(i.indkey)`, which is how `ActiveRecord`'s
        // `primary_keys()` reads a key. Not a list this lowering can see, so it stays an
        // expression and the row evaluator reads its value.
        _ => return Ok(None),
    }))
}

/// `'{a,b,"c d"}'` as its elements, with `None` for a SQL NULL.
///
/// PostgreSQL's array input syntax, narrowed to what an `= ANY` operand needs: braces, commas, and
/// double quotes around an element containing a comma, a brace or a space.
///
/// **An unquoted `NULL`, in any case, is a SQL NULL; a quoted `"NULL"` is the four characters.**
/// That distinction is the whole reason this returns `Option<String>` rather than `String`, and it
/// is not decoration: `'{NULL,a}'` and `'{"NULL",a}'` answer differently for the same probe —
/// `'NULL' = ANY(…)` is unknown against the first and true against the second, measured. Getting
/// it wrong handed the element input function the *word* `NULL`, which `text` accepted as a string
/// and `int4` refused with `22P02`, so `1 = ANY('{NULL,1}'::int[])` was an error where a real
/// server says `t`.
///
/// An earlier version of this function noted that simplification and said it "cannot be reached
/// from anything `ActiveRecord` sends". It can: `where(id: [1, nil])` emits exactly it. A claim
/// about what a client sends belongs in a capture, not in a comment.
fn parse_array_literal(text: &str) -> Result<Vec<Option<String>>> {
    let inner = text
        .trim()
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| SqlError::InvalidTextRepresentation {
            ty: "array",
            value: text.to_owned(),
        })?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut items: Vec<Option<String>> = Vec::new();
    let mut current = String::new();
    // Whether *any* part of the element was quoted. `"NULL"` is a string and so is `"NU"LL`,
    // which is why this is a flag on the element rather than a test of its first character.
    let mut was_quoted = false;
    let mut quoted = false;
    let mut chars = inner.chars();
    let finish = |current: &mut String, was_quoted: &mut bool, items: &mut Vec<_>| {
        let text = std::mem::take(current);
        items.push(
            if !*was_quoted && text.trim().eq_ignore_ascii_case("null") {
                None
            } else {
                Some(text)
            },
        );
        *was_quoted = false;
    };
    while let Some(character) = chars.next() {
        match character {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    was_quoted = true;
                    current.push(escaped);
                }
            }
            ',' if !quoted => finish(&mut current, &mut was_quoted, &mut items),
            _ => current.push(character),
        }
    }
    finish(&mut current, &mut was_quoted, &mut items);
    Ok(items)
}

/// `current_schemas(bool)` as the schemas it returns, or `None` for a function that is not it.
///
/// Measured on 19beta1: `current_schemas(false)` is `{public}` and `current_schemas(true)` is
/// `{pg_catalog,public}` — the `true` form includes the implicitly-searched catalog schema. This
/// node has exactly one schema and no `search_path` to vary it, so both answers are constants;
/// what would make them not constants is schema support, which is a unit of its own.
fn schema_function(function: &sqlparser::ast::Function) -> Result<Option<bool>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments};
    if !function
        .name
        .to_string()
        .eq_ignore_ascii_case("current_schemas")
    {
        return Ok(None);
    }
    refuse_wrong_arity(function, "current_schemas", 1)?;
    let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args else {
        return Err(SqlError::UndefinedFunction("current_schemas()".to_owned()));
    };
    let include_implicit = match args.as_slice() {
        [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value)))]
            if matches!(value.value, Value::Boolean(_)) =>
        {
            matches!(value.value, Value::Boolean(true))
        }
        // A wrong **type** is `42883` naming the signature, not `0A000` naming the call:
        // PostgreSQL resolves a function by name *and* argument types, so `current_schemas(1)` is
        // a function that does not exist rather than one this node has not built. Measured — and
        // its DETAIL differs from the wrong-*arity* one `refuse_wrong_arity` raises above.
        [arg] => {
            return Err(SqlError::UndefinedFunctionTypes(format!(
                "current_schemas({})",
                argument_type_name(arg)
            )));
        }
        _ => return Err(SqlError::unsupported("current_schemas with that argument")),
    };
    Ok(Some(include_implicit))
}

/// `42883` when a function is called with the wrong number of arguments.
///
/// PostgreSQL resolves a function by name **and** argument types, so the wrong arity is not a
/// badly-called function — it is a function that does not exist, and the message says so with the
/// types spelled out: `function current_schema(boolean) does not exist`. Only the shapes this node
/// can produce are named; a type it has no name for is written as it was parsed.
fn refuse_wrong_arity(
    function: &sqlparser::ast::Function,
    name: &'static str,
    wanted: usize,
) -> Result<()> {
    refuse_wrong_arities(function, name, std::slice::from_ref(&wanted))
}

/// The same, for a function with more than one form — `pg_get_expr` has a two- and a
/// three-argument one and a real server takes both.
fn refuse_wrong_arities(
    function: &sqlparser::ast::Function,
    name: &str,
    wanted: &[usize],
) -> Result<()> {
    use sqlparser::ast::{FunctionArgumentList, FunctionArguments};
    let given: Vec<String> = match &function.args {
        FunctionArguments::List(FunctionArgumentList { args, .. }) => {
            args.iter().map(argument_type_name).collect()
        }
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => vec!["record".to_owned()],
    };
    if wanted.contains(&given.len()) {
        return Ok(());
    }
    Err(SqlError::UndefinedFunction(format!(
        "{name}({})",
        given.join(", ")
    )))
}

/// A call's positional arguments, or `42883` for a call written in a shape that has none.
fn function_arguments<'a>(
    function: &'a sqlparser::ast::Function,
    name: &str,
) -> Result<Vec<&'a Expr>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments};
    let args = match &function.args {
        FunctionArguments::None => return Ok(Vec::new()),
        FunctionArguments::List(FunctionArgumentList { args, .. }) => args,
        FunctionArguments::Subquery(_) => {
            return Err(SqlError::UndefinedFunction(format!("{name}(record)")));
        }
    };
    args.iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
            _ => Err(SqlError::UndefinedFunction(format!("{name}(unknown)"))),
        })
        .collect()
}

/// The element type inside an `ArrayElemTypeDef`, or `None` for the two spellings that carry no
/// type — `int[]` written as a bare `ARRAY` has nothing to be an array *of*.
fn array_element(inner: &sqlparser::ast::ArrayElemTypeDef) -> Option<&DataType> {
    use sqlparser::ast::ArrayElemTypeDef;
    match inner {
        ArrayElemTypeDef::AngleBracket(ty)
        | ArrayElemTypeDef::SquareBracket(ty, _)
        | ArrayElemTypeDef::Parenthesis(ty) => Some(ty),
        ArrayElemTypeDef::None => None,
    }
}

/// The type name PostgreSQL would print for one argument in a `42883`.
fn argument_type_name(arg: &FunctionArg) -> String {
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
        return "unknown".to_owned();
    };
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Boolean(_) => "boolean",
            Value::Number(..) => "integer",
            // Everything else is `unknown`, which is what a real server's resolver calls an
            // unadorned string literal: `current_schema('x')` names `unknown`, not `text`.
            _ => "unknown",
        }
        .to_owned(),
        // **A cast names the type it casts to.** `'cf'::regclass` is a `regclass` argument, not an
        // `unknown` one, and `col_description('cf'::regclass)` says so on a real server. Without
        // this the message named the *literal underneath* the cast, which is the one thing the
        // user did not write.
        Expr::Cast { data_type, .. } => cast_type_name(data_type),
        Expr::Nested(inner) => argument_type_name(&FunctionArg::Unnamed(FunctionArgExpr::Expr(
            (**inner).clone(),
        ))),
        _ => "unknown".to_owned(),
    }
}

/// What a cast's target is called in a `42883`.
///
/// The stored types answer with their own PostgreSQL names; `regclass` and the other catalog
/// pseudo-types are not stored types here and are spelled from the syntax, which is what they are
/// on a real server too — `'x'::regclass` is a cast to `regclass` whatever the catalog holds.
fn cast_type_name(data_type: &DataType) -> String {
    if let Ok((ty, _)) = lower_type(data_type) {
        return ty.name().to_owned();
    }
    data_type.to_string().to_ascii_lowercase()
}

/// The one schema this node has. `public`, which is what `current_schema()` answers.
const PUBLIC_SCHEMA: &str = "public";

/// The one database this node has, which is what `current_database()` answers.
///
/// A constant for the reason [`PUBLIC_SCHEMA`] is one: there is exactly one, and every statement
/// that names a database names this one. A client may connect under another name — the startup
/// packet's `database` is not read — and gets this back, which is the declared divergence; what
/// matters to the adapter is that `pg_database.datname` carries the **same** name, so
/// `WHERE datname = current_database()` matches by construction rather than by coincidence.
pub(crate) const DATABASE_NAME: &str = "esker";

/// A cast to one of the four array types, or `None` when this is not one.
///
/// Split out of [`lower_cast`] because it is a whole rule rather than a case of one: what an
/// array cast does is read its operand with `array_in` at the target's element type, and the
/// two shapes it accepts — a literal's text and an already-built array's — are the same door.
fn lower_array_cast(expr: &Expr, data_type: &DataType) -> Result<Option<plan::Expr>> {
    // **A cast to an array type reads the literal**, and used to be the identity on its text.
    // It was the identity because there was no array *column* type to cast to and the array
    // operators read their arrays out of text anyway; the cost was that nothing ever checked the
    // literal, so `'{a,,b}'::text[]` answered `{a,,b}` where a real server raises `22P02`, and
    // `'{a,b}'::text[]` reported its type as `text`. Now that an array is a type, this is an
    // ordinary cast to a stored type: `Datum::from_text` is `array_in`, and it is the element
    // type that answers for a bad element.
    if let DataType::Array(inner) = data_type
        && let Some(element) = array_element(inner)
        && let Ok((element, NO_TYPMOD)) = lower_type(element)
        && let Some(array) = esker_keys::array::ArrayValue::array_of(element)
    {
        // **`ARRAY[]::int[]` is the empty array and `ARRAY[]` is an error**, and the difference
        // is exactly this cast: the constructor has no element to take a type from, and the cast
        // is what supplies one. So it is answered here rather than by lowering the constructor,
        // which would raise `42P18` before the type arrived.
        if matches!(strip_nesting(expr), Expr::Array(array) if array.elem.is_empty()) {
            return Ok(Some(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Array(esker_keys::array::ArrayValue::empty(element)),
            )))));
        }
        // A literal's text, or an already-built array's — `ARRAY[1,2,3]::int8[]` is the
        // constructor folded and then read again at the target's element type. Going through the
        // text is `array_in` doing the element conversion, so a value that does not fit the new
        // element type fails with that type's own message.
        let text = match cast_literal_text(expr)? {
            Some(text) => Some(text),
            None => match lower_expr(expr) {
                Ok(plan::Expr::Literal(plan::Literal::Typed(value))) => value.to_text(),
                _ => None,
            },
        };
        if let Some(text) = text {
            return Ok(Some(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::from_text(array, &text)?,
            )))));
        }
    }
    Ok(None)
}

/// `'<name>'::regtype`, `'<name>'::regtype::oid` and `'<digits>'::oid`.
///
/// The three shapes `ActiveRecord` needs and no others. The middle one is what stopped the ladder
/// at rung 2 for three scoreboard runs: `Quoting#lookup_cast_type` sends
/// `SELECT 'integer'::regtype::oid` once per column type, and `pg_type` here already held the
/// answer — what was missing was only the cast that asks it.
///
/// # Why the nested shape is matched rather than composed
///
/// A `regtype` is a real type on a real server, four bytes holding an OID that *print* as the
/// type's name; `::oid` from one is then a free coercion. This node has no `regtype`, so
/// `'x'::regtype` lowers to the **name**, as text — which makes `SELECT 'int4'::regtype` answer
/// `integer`, exactly right, and leaves only `RowDescription`'s OID differing (`text` where a real
/// server says `regtype`). It also means a composed `::oid` would be a text-to-oid cast, which a
/// real server refuses: `'integer'::oid` is `22P02`, and this node answers that too.
///
/// So the pair is recognised together. That is not a shortcut around a missing type — it is the
/// one place where composing the two steps would have to allow a cast PostgreSQL forbids.
fn lower_cast(expr: &Expr, data_type: &DataType) -> Result<plan::Expr> {
    // **`NULL::bigint` is a NULL that knows it is a `bigint`.** The value is nothing either way;
    // what the cast carries is the type, and everything downstream resolves against it — a
    // subquery's column type, an operator's two sides, a `COALESCE`'s unification. Dropping it
    // made `IN (SELECT NULL::bigint)` the `42883 bigint = text` an *untyped* NULL deserves, which
    // is a refusal where a real server matches nothing.
    //
    // A type this node does not have keeps the old answer: an untyped NULL is still a NULL, and
    // refusing `NULL::money` would refuse a statement whose value is not in question.
    if matches!(expr, Expr::Value(value) if matches!(value.value, Value::Null)) {
        return Ok(plan::Expr::Literal(match lower_type(data_type) {
            Ok((ty, _)) => plan::Literal::TypedNull(ty),
            Err(_) => plan::Literal::Null,
        }));
    }
    if let Some(array) = lower_array_cast(expr, data_type)? {
        return Ok(array);
    }
    let Some(target) = cast_target(data_type) else {
        // A cast to a **stored type**, which is a different thing from `regtype` and `oid`: it
        // reads the operand with that type's input function, exactly as assigning it to a column
        // of that type would. Only a literal, and only a chain of them — `'{"a":1}'::json::jsonb`
        // is two of these — because a cast of a *column* has to happen per row and this node has
        // no expression-level cast to do it with.
        // **A cast that does not exist is `42846`, before any value is read.** A `date` and a
        // number have none in either direction — the day count it holds is an implementation
        // detail — and neither does a `date` and a `json`. Without this the cast would go through
        // text and answer `22P02` about the digits, which blames the value for a pair that has no
        // cast at all.
        if let Some(refusal) = refused_cast(expr, data_type)? {
            return Err(refusal);
        }
        // **A `numeric` cast to an integer rounds**, half away from zero — `1.5::int` is `2`,
        // `2.5::int` is `3` and `0.5::int` is `1`. One rule for the cast, the assignment and the
        // `round` function, and it is not the parser's: reading `1.5` with `int4in` is
        // `22P02 invalid input syntax`, which is what this arm exists to not do.
        // **`uuid::bytea` is the sixteen bytes, not the text's bytes.** Through the ordinary
        // text path this became the hex of `a0eebc99-…`'s ASCII, which is a different value of a
        // different length. The pair has a real conversion and it is the identity on the bytes.
        if source_type(expr)? == Some(ColumnType::Uuid)
            && lower_type(data_type).ok().map(|(ty, _)| ty) == Some(ColumnType::Bytea)
            && let Some(text) = cast_literal_text(expr)?
        {
            let bytes = value::uuid::from_text(&text)?;
            return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Bytea(bytes.to_vec()),
            ))));
        }
        if source_type(expr)? == Some(ColumnType::Numeric)
            && let Some(to) = lower_type(data_type).ok().map(|(ty, _)| ty)
            && matches!(to, ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2)
            && let Some(text) = cast_literal_text(expr)?
        {
            return numeric_to_integer(&text, to)
                .map(|value| plan::Expr::Literal(plan::Literal::Typed(Box::new(value))));
        }
        return match cast_literal_text(expr)? {
            Some(text) => {
                // **The typmod applies**, which is the whole difference between `::timestamp` and
                // `::timestamp(3)`: the second rounds. Dropping it here read the text and then
                // ignored the number beside it, so `'…123456'::timestamp(3)` kept its microseconds
                // where a real server rounds to `.123`. Same function the write path uses, so a
                // cast and an `INSERT` cannot disagree about what `(3)` means.
                let (ty, typmod) = lower_type(data_type)?;
                let value = value::fit_to_typmod(Datum::from_text(ty, &text)?, ty, typmod)?;
                Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(value))))
            }
            // Not a literal, so the cast happens **per row**. Only `text` is a target: a cast to
            // `text` is the operand's own output function and needs nothing of the operand but
            // that it have one, where a cast *to* another type has to read the text back and can
            // fail per row — a different feature with its own errors.
            None if matches!(lower_type(data_type), Ok((ColumnType::Text, _))) => {
                Ok(plan::Expr::ToText {
                    operand: Box::new(lower_expr(expr)?),
                    // Both set at resolution, where the operand's type is known.
                    strip_blanks: false,
                    enum_labels: None,
                })
            }
            None => Err(SqlError::unsupported(format!("a cast to {data_type}"))),
        };
    };
    match (target, expr) {
        // `'integer'::regtype::oid` — the inner cast is matched here rather than lowered first.
        (
            CastTarget::Oid,
            Expr::Cast {
                expr: inner,
                data_type: inner_type,
                ..
            },
        ) if cast_target(inner_type) == Some(CastTarget::RegType) => {
            let name = cast_operand(inner, data_type)?;
            let named = value::named_type(&name)?
                .ok_or_else(|| SqlError::UndefinedType(name.trim().to_owned()))?;
            Ok(plan::Expr::Literal(plan::Literal::Integer(i64::from(
                named.oid(),
            ))))
        }
        // `'cb'::regclass::oid` — the same value, since a `regclass` here already *is* the oid.
        (
            CastTarget::Oid,
            Expr::Cast {
                expr: inner,
                data_type: inner_type,
                ..
            },
        ) if cast_target(inner_type) == Some(CastTarget::RegClass) => {
            lower_regclass(inner, data_type)
        }
        // `'cb'::regclass`. The text is a **name**, read the way `nextval`'s argument is — so
        // `'"companies"'::regclass`, which is what `ActiveRecord` writes, keeps its case and
        // `'CB'::regclass` folds. Measured: `'"CB"'::regclass` is `42P01 relation "CB" does not
        // exist`, quoted spelling and all.
        // **Both directions of `regclass`, told apart by what is being cast.** A *name* — a string
        // literal — is the forward form, resolved once per statement before the plan is built. An
        // **oid**, which in practice is a column, is the inverse: the relation's name, read per
        // row. `ActiveRecord`'s `foreign_keys()` writes `t2.oid::regclass::text`, and the `::text`
        // after it is the identity on what this already answers.
        (CastTarget::RegClass, _) if is_string_literal(expr) => lower_regclass(expr, data_type),
        (CastTarget::RegClass, _) => Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
            func: plan::CatalogFunc::RegClassName,
            args: vec![lower_expr(expr)?],
        }))),
        // `'integer'::regtype` on its own, which answers the name PostgreSQL prints it by.
        (CastTarget::RegType, _) => {
            let name = cast_operand(expr, data_type)?;
            let named = value::named_type(&name)?
                .ok_or_else(|| SqlError::UndefinedType(name.trim().to_owned()))?;
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Text(named.printed()),
            ))))
        }
        // `'23'::oid`. **This is a cast to a real type now**, not a special form that happens to
        // read digits: `oid` is `ColumnType::Oid` since its own unit, so the reading is
        // `value::oid::from_text` and a negative one wraps instead of being refused. The two
        // arms above still come first, because `'x'::regtype::oid` is asking a different
        // question — what OID does this *name* have — and answers before any value is read.
        (CastTarget::Oid, _) => {
            let text = cast_operand(expr, data_type)?;
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Oid(value::oid::from_text(&text)?),
            ))))
        }
    }
}

/// `'name'::regclass` as the call that resolves it.
///
/// The name is folded here, where the quoting is still visible, and the *lookup* happens in the
/// executor — a cast that reads the catalog is a function of the catalog, and one resolved per row
/// would read it once per row filtered.
fn lower_regclass(expr: &Expr, data_type: &DataType) -> Result<plan::Expr> {
    let name = sequence_reference(&cast_operand(expr, data_type)?);
    Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
        func: plan::CatalogFunc::RegClass,
        args: vec![plan::Expr::Literal(plan::Literal::String(name))],
    })))
}

/// `0A000` for a comparison over `json` or `jsonb`.
///
/// Refused **in the lowering**, where the operand still says which type it is. Once a cast has
/// been lowered a `jsonb` is a `Datum::Text` and the type is gone: `Datum` has no json variant,
/// because these two share `text`'s representation. That sharing is exactly what `varchar` and
/// `bpchar` do and it is safe for them, because their comparison *is* text comparison. `jsonb`'s
/// is not — `'1.0'::jsonb = '1.00'::jsonb` is `t` on a real server and byte comparison says `f`,
/// and `ORDER BY` sorts by kind before value. Answering either from the bytes would be a wrong
/// answer, so both are `0A000` until `jsonb` has a `Datum` of its own.
///
/// That is the `real` unit's lesson one layer up: **a type may share another's representation only
/// if it shares its comparison.** `json` has no comparison operators at all on a real server, so
/// refusing there is closer still. [ADR 0042](../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md).
fn refuse_json_comparison(op: &BinaryOperator) -> SqlError {
    SqlError::unsupported(format!("the operator {op} over json or jsonb"))
}

/// Whether either operand of a comparison is written as a `json` or `jsonb` value.
///
/// Syntactic, and it has to be: after lowering, a `jsonb` is a `Datum::Text` like any other.
fn either_is_json(left: &Expr, right: &Expr) -> bool {
    is_json_expr(left) || is_json_expr(right)
}

fn is_json_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_json_expr(inner),
        Expr::Cast {
            expr, data_type, ..
        } => matches!(data_type, DataType::JSON | DataType::JSONB) || is_json_expr(expr),
        _ => false,
    }
}

/// The arithmetic operator a token is, or `None` for one that compares or combines.
///
/// `^` is here and `#`, `&`, `|`, `<<` and `>>` are not: PostgreSQL's bit operators are a separate
/// surface with their own types, and naming them is better than approximating them.
fn arithmetic_op(op: &BinaryOperator) -> Option<plan::ArithOp> {
    Some(match op {
        BinaryOperator::Plus => plan::ArithOp::Add,
        BinaryOperator::Minus => plan::ArithOp::Subtract,
        BinaryOperator::Multiply => plan::ArithOp::Multiply,
        BinaryOperator::Divide => plan::ArithOp::Divide,
        BinaryOperator::Modulo => plan::ArithOp::Modulo,
        // `^` under the PostgreSQL dialect is exponentiation, not a bitwise XOR — that is `#`
        // there — so both spellings the parser can produce for the token mean the same operator.
        BinaryOperator::PGExp | BinaryOperator::BitwiseXor => plan::ArithOp::Power,
        _ => return None,
    })
}

/// Whether an operator compares, as against combines.
fn is_comparison(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

/// A `numeric` as an integer of `to`'s width, rounded half away from zero.
///
/// The width check is the type's own and gives `22003` naming it, which is the same message a
/// constant that far out gets — a `numeric` past `int4` is `integer out of range`, not a
/// `numeric` error.
fn numeric_to_integer(text: &str, to: ColumnType) -> Result<Datum> {
    let value = value::numeric::from_text(text)?;
    // Scale zero is what "an integer" means here, and `fit_to_typmod` is where the rounding rule
    // lives — so the cast and an assignment into a `numeric(p,0)` round identically.
    let rounded = value::numeric::fit_to_typmod(value, value::numeric::typmod_of(1000, 0))?;
    let digits = value::numeric::to_text(&rounded);
    let wide: i64 = digits
        .parse()
        .map_err(|_| SqlError::IntegerLiteralOutOfRange(to.name()))?;
    Ok(match to {
        ColumnType::Int8 => Datum::Int8(wide),
        ColumnType::Int4 => Datum::Int4(
            i32::try_from(wide).map_err(|_| SqlError::IntegerLiteralOutOfRange(to.name()))?,
        ),
        _ => Datum::Int2(
            i16::try_from(wide).map_err(|_| SqlError::IntegerLiteralOutOfRange(to.name()))?,
        ),
    })
}

/// The `42846` a pair of types with no cast between them gets, or `None` for a pair that has one.
///
/// Only the pairs a `date` is one half of, because it is the only type here that PostgreSQL
/// refuses to cast to a number: every other pair in this crate either has a cast or fails on the
/// value. Both directions, measured — `'2020-01-01'::date::int` and `1::date`.
fn refused_cast(expr: &Expr, data_type: &DataType) -> Result<Option<SqlError>> {
    let target = lower_type(data_type).ok().map(|(ty, _)| ty);
    let source = source_type(expr)?;
    let numeric = |ty: ColumnType| {
        matches!(
            ty,
            ColumnType::Int8
                | ColumnType::Int4
                | ColumnType::Int2
                | ColumnType::Double
                | ColumnType::Real
                | ColumnType::Json
                | ColumnType::Jsonb
        )
    };
    // A `numeric` whose value has no integer at all: `0A000 cannot convert NaN to integer`, which
    // is a *different* refusal from the `22003` a merely-too-large value gets. Decided from the
    // literal's text, which is the only place the value is known at lowering.
    if source == Some(ColumnType::Numeric)
        && let Some(to) = target
        && matches!(to, ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2)
        && let Some(text) = cast_literal_text(expr)?
    {
        let special = match text.trim() {
            "NaN" => Some("NaN"),
            "Infinity" | "-Infinity" => Some("infinity"),
            _ => None,
        };
        if let Some(value) = special {
            return Ok(Some(SqlError::CannotConvert {
                value,
                target: to.name(),
            }));
        }
    }
    // **A `time` casts to a string and to nothing else.** Measured one target at a time against
    // 19beta1: `text`, `varchar` and `character(n)` are the whole of it, and every other type
    // this node has — both integers and floats, `numeric`, `bool`, `bytea`, `json`, `jsonb`,
    // `date` and both timestamps — is `42846 cannot cast type time without time zone to …`.
    // Without this the cast goes through the rendered text and blames the *value*, which is a
    // `22P02` about digits for a pair that has no cast at all.
    let stringy = |ty: ColumnType| {
        matches!(
            ty,
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar
        )
    };
    Ok(match (source, target) {
        (Some(ColumnType::Date), Some(to)) if numeric(to) => Some(SqlError::CannotCast {
            from: ColumnType::Date.name(),
            to: to.name(),
        }),
        (Some(from), Some(ColumnType::Date)) if numeric(from) => Some(SqlError::CannotCast {
            from: from.name(),
            to: ColumnType::Date.name(),
        }),
        // **A uuid casts to a string and to `bytea`, and to nothing else.** Measured: `::int` and
        // `::json` are `42846 cannot cast type uuid to …`, where `::bytea` is the sixteen raw
        // bytes. The value is bytes and never a number.
        // An interval casts to a string and to `time`; `::int` and `::json` are `42846`.
        (Some(ColumnType::Interval), Some(to))
            if !stringy(to) && !matches!(to, ColumnType::Time | ColumnType::Interval) =>
        {
            Some(SqlError::CannotCast {
                from: ColumnType::Interval.name(),
                to: to.name(),
            })
        }
        (Some(ColumnType::Uuid), Some(to))
            if !stringy(to) && to != ColumnType::Bytea && to != ColumnType::Uuid =>
        {
            Some(SqlError::CannotCast {
                from: ColumnType::Uuid.name(),
                to: to.name(),
            })
        }
        (Some(ColumnType::Time), Some(to)) if !stringy(to) && to != ColumnType::Time => {
            Some(SqlError::CannotCast {
                from: ColumnType::Time.name(),
                to: to.name(),
            })
        }
        // The other direction is narrower: a `timestamp` **does** cast to a `time` — it is the
        // clock of the instant — so only the types with no path at all are refused here.
        (Some(from), Some(ColumnType::Time))
            if !stringy(from)
                && !matches!(
                    from,
                    // An interval casts to a time too — it is the clock part of it — which the
                    // time unit could not know when it wrote this list.
                    ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Interval
                )
                && from != ColumnType::Time =>
        {
            Some(SqlError::CannotCast {
                from: from.name(),
                to: ColumnType::Time.name(),
            })
        }
        _ => None,
    })
}

/// `date + time` as an instant, when both sides are constants and one of each.
///
/// PostgreSQL's `+` over this pair is a `timestamp` — the day at that time of day — and it is
/// commutative: `'12:34:56'::time + '2020-01-01'::date` and the reverse are the same value. Both
/// spellings are in `tests/corpus/pg19_time.txt`.
///
/// `Ok(None)` means "not this pair", which is what lets the caller fall through to the ordinary
/// operator lowering and refuse `+` by name.
fn date_plus_time(left: &Expr, right: &Expr) -> Result<Option<i64>> {
    let constant = |expr: &Expr| -> Result<Option<Datum>> {
        let Some(ty) = source_type(expr)? else {
            return Ok(None);
        };
        if !matches!(ty, ColumnType::Date | ColumnType::Time) {
            return Ok(None);
        }
        match cast_literal_text(expr)? {
            Some(text) => Datum::from_text(ty, &text).map(Some),
            None => Ok(None),
        }
    };
    let (Some(left), Some(right)) = (constant(left)?, constant(right)?) else {
        return Ok(None);
    };
    // `date + date` and `time + time` are not this pair: the first has no `+` at all on a real
    // server and the second is `42725 operator is not unique`.
    let ((Datum::Date(day), Datum::Time(micros)) | (Datum::Time(micros), Datum::Date(day))) =
        (left, right)
    else {
        return Ok(None);
    };
    // The day's midnight plus the time of day. `date::as_micros` is where the day count becomes
    // an instant, including the two infinities, so this adds to the same epoch a `timestamp` has.
    Ok(Some(value::date::as_micros(day).saturating_add(micros)))
}

/// The type a cast's operand already has, for the pairs [`refused_cast`] decides between.
///
/// A bare number is `integer` — PostgreSQL's type for an unadorned constant, and the one it names
/// in `cannot cast type integer to date`. A string literal is `unknown` and has no type to refuse
/// a cast from, which is why `'2020-01-01'::date` is a value and `1::date` is not.
fn source_type(expr: &Expr) -> Result<Option<ColumnType>> {
    Ok(match expr {
        // A sign is transparent to the question: `-1` is an `integer` exactly as `1` is, and
        // `(1)` is too.
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => source_type(inner)?,
        Expr::Value(value) => match &value.value {
            // A **bare integer** constant is `integer`, which is what `cannot cast type integer
            // to date` names. A decimal one is a `numeric` on a real server — and this crate
            // still types it `double` everywhere else, which is the declared divergence
            // `tests/unknown_literal.rs` holds and the next unit's to close.
            Value::Number(digits, _) if digits.contains('.') => Some(ColumnType::Numeric),
            Value::Number(..) => Some(ColumnType::Int4),
            _ => None,
        },
        Expr::Cast { data_type, .. } => lower_type(data_type).ok().map(|(ty, _)| ty),
        // A bare decimal constant is a `numeric` on a real server, which is what makes
        // `1.5::int` and `'NaN'::numeric::int` two different questions.
        Expr::TypedString(typed) => lower_type(&typed.data_type).ok().map(|(ty, _)| ty),
        _ => None,
    })
}

/// The text a literal — or a chain of casts over one — carries, for a cast to read.
///
/// **Each cast in a chain is applied in turn**, not skipped to the innermost literal, because the
/// steps are not interchangeable: `'{"b":1,"a":2}'::jsonb::json` is `{"a": 2, "b": 1}` on a real
/// server — the `jsonb` canonicalised it and the `json` stored *that* — while reading the original
/// text as `json` directly would answer `{"b":1,"a":2}`. One reordering, two different answers.
fn cast_literal_text(expr: &Expr) -> Result<Option<String>> {
    match expr {
        // A `+` is transparent here the way parentheses are; a `-` is not, and keeps its own arm
        // below because it has to put the sign back on the text.
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr: inner,
        } => cast_literal_text(inner),
        Expr::Value(value) => Ok(match &value.value {
            Value::SingleQuotedString(text) => Some(text.clone()),
            // **A number is a literal too.** `1.5::numeric` reads the digits with `numeric`'s
            // input function, exactly as `'1.5'::numeric` does — and it is the spelling the
            // corpus uses everywhere, because it is the one a person writes.
            Value::Number(digits, _) => Some(digits.clone()),
            _ => None,
        }),
        // `(-1.5)::numeric`: a signed number is a unary minus over a literal, and the sign is
        // part of the value being cast rather than an operator applied to the result.
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => Ok(cast_literal_text(inner)?.map(|text| format!("-{text}"))),
        // The inner cast, run: its *result* is what the outer one reads.
        Expr::Cast { .. } => match lower_expr(expr)? {
            plan::Expr::Literal(plan::Literal::Typed(value)) => Ok(match value.as_ref() {
                Datum::Text(text) => Some(text.clone()),
                other => other.to_text(),
            }),
            plan::Expr::Literal(plan::Literal::String(text)) => Ok(Some(text)),
            plan::Expr::Literal(plan::Literal::Integer(value)) => Ok(Some(value.to_string())),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// The string a cast is applied to, or `0A000` naming the cast.
///
/// Only a literal: `column::regtype` would need the cast at run time, and answering it here from
/// the text of an expression would be a wrong answer rather than a missing feature.
fn cast_operand(expr: &Expr, data_type: &DataType) -> Result<String> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) => Ok(text.clone()),
            _ => Err(SqlError::unsupported(format!(
                "the cast {expr}::{data_type}"
            ))),
        },
        _ => Err(SqlError::unsupported(format!(
            "the cast {expr}::{data_type}"
        ))),
    }
}

/// The two cast targets this node answers, or `None` for every other one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CastTarget {
    /// PostgreSQL's `regclass`: a **relation**, named. Unlike `regtype` this cannot be answered
    /// from the text — it is a name to look up in the catalog — so it lowers to a
    /// [`plan::CatalogFunc::RegClass`] the executor resolves before it plans.
    RegClass,
    /// PostgreSQL's `regtype`: a type, named.
    RegType,
    /// PostgreSQL's `oid`.
    Oid,
}

/// `sqlparser` files both as custom type names, since neither is in its `DataType`.
fn cast_target(data_type: &DataType) -> Option<CastTarget> {
    // `regclass` is the one of the three `sqlparser` has a variant for — it parses it because
    // `serial` expands to `nextval('s'::regclass)` — so it never reaches the custom-name path.
    if matches!(data_type, DataType::Regclass) {
        return Some(CastTarget::RegClass);
    }
    let DataType::Custom(name, modifiers) = data_type else {
        return None;
    };
    if !modifiers.is_empty() {
        return None;
    }
    match name.to_string().to_ascii_lowercase().as_str() {
        "regtype" => Some(CastTarget::RegType),
        "regclass" => Some(CastTarget::RegClass),
        "oid" => Some(CastTarget::Oid),
        _ => None,
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
                        .map_err(|_| SqlError::IntegerLiteralOutOfRange("bigint"))?,
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

/// A target list, lowered — the one a `SELECT` projects and the one a `RETURNING` returns.
///
/// One function because they are one grammar: `*`, `t.*`, an expression, an expression with an
/// alias, and the five `SELECT * EXCLUDE`-style modifiers that are each `0A000` naming themselves.
/// Two copies of this is two places for `RETURNING *` to stop meaning what `SELECT *` means.
fn lower_projection(items: &[SelectItem]) -> Result<Vec<plan::SelectItem>> {
    items
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
        .collect()
}

/// A `VALUES` list as a relation: its rows and the names its columns take.
///
/// **The length check is here and is a syntax error.** PostgreSQL calls a ragged list
/// `42601 VALUES lists must all be the same length` — not a type error and not a padded row — so
/// it is raised where the statement is read rather than carried into a plan that cannot hold it.
///
/// **The types are not decided here**: a column's type is its first row's, and reading an
/// expression's type needs a scope this pass does not have. `crate::exec::values` decides it and
/// reads every other row as it, which is where `VALUES (1),('a')` becomes its `22P02`.
///
/// The names are `column1`, `column2`, … unless an alias list renames them — and a list **longer**
/// than the columns is `42P10` naming both counts, which is `Derived`'s message and the same
/// mistake, so it is worded the same way.
fn lower_values(
    values: &sqlparser::ast::Values,
    columns: &[String],
    name: &str,
) -> Result<plan::ValuesList> {
    let width = values.rows.first().map_or(0, |row| row.len());
    if values.rows.iter().any(|row| row.len() != width) {
        return Err(SqlError::ValuesRowLength);
    }
    if columns.len() > width {
        return Err(SqlError::InvalidColumnReference(format!(
            "table \"{name}\" has {width} columns available but {} columns specified",
            columns.len()
        )));
    }
    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        rows.push(row.iter().map(lower_expr).collect::<Result<Vec<_>>>()?);
    }
    let names = (0..width)
        .map(|at| {
            columns
                .get(at)
                .cloned()
                .unwrap_or_else(|| format!("column{}", at + 1))
        })
        .collect();
    Ok(plan::ValuesList {
        rows,
        columns: names,
    })
}

/// A query's `ORDER BY`, which belongs to the query and not to the select under it.
///
/// Its own function because a `VALUES` list is a query with no select, and the clause it carries
/// is the same clause — lowering it twice would be two chances for the two to disagree.
fn lower_order_by(query: &Query) -> Result<Vec<plan::OrderItem>> {
    let Some(order_by) = &query.order_by else {
        return Ok(Vec::new());
    };
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
        .collect()
}

/// A query's `LIMIT` and `OFFSET`, for the same reason.
fn lower_limit_offset(query: &Query) -> Result<(Option<plan::Expr>, Option<plan::Expr>)> {
    match &query.limit_clause {
        None => Ok((None, None)),
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
            Ok((limit.as_ref().map(lower_expr).transpose()?, offset))
        }
        Some(other) => Err(SqlError::unsupported(format!("the limit clause {other}"))),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "most of it is the refusal list, which is the point: one line per clause not honoured"
)]
fn lower_query(query: &Query) -> Result<plan::Select> {
    refuse_if(!query.locks.is_empty(), "a row-level locking clause")?;
    refuse_if(query.fetch.is_some(), "FETCH FIRST")?;
    refuse_if(query.for_clause.is_some(), "FOR XML/JSON")?;
    refuse_if(query.settings.is_some(), "SETTINGS")?;
    refuse_if(query.format_clause.is_some(), "FORMAT")?;
    refuse_if(!query.pipe_operators.is_empty(), "a pipe operator")?;

    let SetExpr::Select(select) = query.body.as_ref() else {
        // **`VALUES …` on its own is a query**, so it takes the clauses a query takes: its rows
        // are a relation with no name, and the `ORDER BY`, `LIMIT` and `OFFSET` above it are the
        // ordinary ones over the columns it names itself.
        if let SetExpr::Values(values) = query.body.as_ref() {
            let (limit, offset) = lower_limit_offset(query)?;
            return Ok(plan::Select {
                projection: vec![plan::SelectItem::Wildcard],
                from: Some(plan::TableRef {
                    values: Some(Box::new(lower_values(values, &[], "")?)),
                    name: String::new(),
                    alias: None,
                    derived: None,
                    function: None,
                    hidden_cte: false,
                }),
                joins: Vec::new(),
                filter: None,
                group_by: Vec::new(),
                having: None,
                order_by: lower_order_by(query)?,
                limit,
                offset,
                distinct: false,
                ctes: Vec::new(),
            });
        }
        // `UNION`, `TABLE t` after the rewrite -- each is its own feature.
        return Err(SqlError::unsupported(match query.body.as_ref() {
            SetExpr::SetOperation { op, .. } => format!("{op}"),
            other => format!("the query body {other}"),
        }));
    };

    // `DISTINCT` this node runs; `DISTINCT ON` is a different clause with a different answer --
    // it keeps the first row per key rather than deduplicating the target list -- and is named
    // rather than approximated by the one that is here.
    let distinct = match &select.distinct {
        // `SELECT ALL` is the default spelled out; PostgreSQL takes it and it changes nothing.
        None | Some(Distinct::All) => false,
        Some(Distinct::Distinct) => true,
        Some(Distinct::On(_)) => return Err(SqlError::unsupported("SELECT DISTINCT ON")),
    };
    refuse_if(select.top.is_some(), "SELECT TOP")?;
    refuse_if(select.into.is_some(), "SELECT INTO")?;
    refuse_if(!select.lateral_views.is_empty(), "a LATERAL VIEW")?;
    refuse_if(select.prewhere.is_some(), "PREWHERE")?;
    // `GROUP BY` proper is executed; its four modifiers are not, and each is named. `GROUP BY
    // ALL` is a different grammar again -- it is not PostgreSQL's, so it can only arrive from a
    // dialect this crate does not offer.
    let group_by = match &select.group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            if let Some(modifier) = modifiers.first() {
                return Err(SqlError::unsupported(format!("GROUP BY ... {modifier}")));
            }
            let mut keys = Vec::with_capacity(exprs.len());
            for expr in exprs {
                match expr {
                    // `GROUP BY ()` is the **empty grouping set**: one group over everything,
                    // which is the same answer as no `GROUP BY` at all -- measured, including
                    // over an empty table, where it still returns one row of zero. Contributing
                    // no key is exactly that, because the empty-input rule keys off whether any
                    // key survives rather than off whether the clause was written.
                    Expr::Tuple(items) if items.is_empty() => {}
                    // `sqlparser` files these three as *expressions* rather than as modifiers, so
                    // without this they would be refused as "the expression ROLLUP (a)" -- true,
                    // and not the name a user would search the documentation for.
                    Expr::Rollup(_) => return Err(SqlError::unsupported("GROUP BY ROLLUP")),
                    Expr::Cube(_) => return Err(SqlError::unsupported("GROUP BY CUBE")),
                    Expr::GroupingSets(_) => {
                        return Err(SqlError::unsupported("GROUP BY GROUPING SETS"));
                    }
                    other => keys.push(lower_expr(other)?),
                }
            }
            keys
        }
        GroupByExpr::All(_) => return Err(SqlError::unsupported("GROUP BY ALL")),
    };
    refuse_if(!select.cluster_by.is_empty(), "CLUSTER BY")?;
    refuse_if(!select.distribute_by.is_empty(), "DISTRIBUTE BY")?;
    refuse_if(!select.sort_by.is_empty(), "SORT BY")?;
    let having = select.having.as_ref().map(lower_expr).transpose()?;
    refuse_if(!select.named_window.is_empty(), "WINDOW")?;
    refuse_if(select.qualify.is_some(), "QUALIFY")?;
    refuse_if(!select.connect_by.is_empty(), "CONNECT BY")?;
    refuse_if(select.value_table_mode.is_some(), "a value table")?;
    refuse_if(select.window_before_qualify, "WINDOW before QUALIFY")?;
    refuse_if(select.exclude.is_some(), "EXCLUDE")?;
    refuse_if(!select.optimizer_hints.is_empty(), "an optimizer hint")?;

    // **`SELECT srf(…)` with no `FROM` is `SELECT * FROM srf(…)`.** A set-returning function in
    // the target list multiplies the rows of the query it is written in; where there is no `FROM`
    // there is one input row, so the two spellings are the same query and this is the rewrite
    // rather than an approximation of one. It is deliberately narrow — one item, that item the
    // whole projection, no `FROM` — because a set-returning function *beside* other items is the
    // mechanism this does not have: two of them run in lockstep and pad the shorter with NULL,
    // which is measured in `tests/corpus/pg19_generate_subscripts.txt` and is not this.
    if select.from.is_empty()
        && select.selection.is_none()
        && let [SelectItem::UnnamedExpr(Expr::Function(function))] = select.projection.as_slice()
        && is_set_returning(function)
        && let Ok(name) = unqualified_function_name(function)
        && matches!(
            name.to_ascii_lowercase().as_str(),
            "generate_series" | "generate_subscripts"
        )
        && let sqlparser::ast::FunctionArguments::List(list) = &function.args
    {
        // The same query with the call moved into the `FROM` and the projection made a wildcard,
        // lowered by the ordinary path — so its `ORDER BY`, `LIMIT` and `OFFSET` are handled once,
        // here as everywhere.
        let mut rewritten = select.clone();
        rewritten.projection = vec![SelectItem::Wildcard(
            sqlparser::ast::WildcardAdditionalOptions::default(),
        )];
        rewritten.from = vec![sqlparser::ast::TableWithJoins {
            relation: TableFactor::Table {
                name: function.name.clone(),
                alias: None,
                args: Some(sqlparser::ast::TableFunctionArgs {
                    args: list.args.clone(),
                    settings: None,
                }),
                with_hints: Vec::new(),
                version: None,
                with_ordinality: false,
                partitions: Vec::new(),
                json_path: None,
                sample: None,
                index_hints: Vec::new(),
            },
            joins: Vec::new(),
        }];
        let mut inner = query.clone();
        *inner.body = SetExpr::Select(rewritten);
        return lower_query(&inner);
    }

    let (from, joins) = match select.from.as_slice() {
        [] => (None, Vec::new()),
        [table] => {
            let left = table_reference(&table.relation)?;
            let joins = table
                .joins
                .iter()
                .map(lower_join)
                .collect::<Result<Vec<_>>>()?;
            // `USING` in a **chain** is refused by name, and only in a chain. It does a second
            // thing an `ON` does not — it *merges* the named column — and the merging compounds:
            // `SELECT *` returns the column once rather than once per table, and a later `ON`
            // join that brings a third column of the same name makes a bare reference `42702`
            // rather than resolving to the merged one. Both measured
            // (`tests/corpus/pg19_join_chain.txt`). Approximating either would be a wrong answer,
            // and `ActiveRecord` sends no `USING` at all — zero in 5396 captured statements — so
            // this is a gap nothing is waiting on.
            if joins.len() > 1 && joins.iter().any(|join| !join.using.is_empty()) {
                return Err(SqlError::unsupported(
                    "USING in a chain of more than one JOIN",
                ));
            }
            (Some(left), joins)
        }
        // **`FROM a, b` is a cross join**, and it is lowered to one rather than refused. It was
        // refused on the argument that the comma form usually means a `WHERE` was meant to join
        // the tables and saying so is more useful than running the cartesian product — which is
        // true of a person's typo and false of generated SQL. `ActiveRecord`'s
        // `pk_and_sequence_for` is **five tables in one comma list** with the join conditions in
        // the `WHERE`, and it is what `reset_pk_sequence!` calls before it can fix a sequence:
        // run 46's largest row, 199 tests over 29 files, all stopped here.
        //
        // The `WHERE` does the joining either way — a cross join with an equality above it is what
        // the comma form *means* — so nothing is approximated by writing it as one.
        [first, rest @ ..] => {
            let left = table_reference(&first.relation)?;
            let mut joins = first
                .joins
                .iter()
                .map(lower_join)
                .collect::<Result<Vec<_>>>()?;
            for entry in rest {
                joins.push(plan::Join {
                    table: table_reference(&entry.relation)?,
                    kind: plan::JoinKind::Inner,
                    on: None,
                    using: Vec::new(),
                });
                for join in &entry.joins {
                    joins.push(lower_join(join)?);
                }
            }
            (Some(left), joins)
        }
    };

    let projection = lower_projection(&select.projection)?;

    let filter = select.selection.as_ref().map(lower_expr).transpose()?;

    let order_by = lower_order_by(query)?;
    let (limit, offset) = lower_limit_offset(query)?;

    let mut lowered = plan::Select {
        from,
        ctes: Vec::new(),
        joins,
        projection,
        filter,
        distinct,
        group_by,
        having,
        order_by,
        limit,
        offset,
    };
    lower_with(query.with.as_ref(), &mut lowered)?;
    Ok(lowered)
}

/// The `WITH` list, inlined into the statement it belongs to.
///
/// It runs **after** the statement is lowered, because what it does is a rewrite of the lowered
/// form: a `FROM a` that names a CTE becomes the derived table `FROM (…) AS a`, and unit 2's
/// machinery does everything from there (`crate::plan::cte`). It runs **before** the statement
/// holding this one, which is what makes an inner `WITH` shadow an outer one — by the time the
/// outer list looks for its own names, the inner one has already taken the ones it defined.
///
/// Each item is lowered against the ones before it, in the order written, so a forward reference
/// is simply a name nothing substituted — and the relation lookup that follows it is the `42P01`
/// with the `DETAIL` and `HINT` PostgreSQL sends. That ordering is also what makes a
/// self-reference an error rather than a loop.
fn lower_with(with: Option<&sqlparser::ast::With>, into: &mut plan::Select) -> Result<()> {
    let Some(with) = with else { return Ok(()) };
    // A second evaluation model -- a working table iterated to a fixed point, with its own
    // termination and its own memory bound. It is a phase, not a unit
    // (`docs/plans/phase-12-subquery.md` §4).
    refuse_if(with.recursive, "WITH RECURSIVE")?;

    // Every name up front, because deciding whether a reference is a *forward* one needs the list
    // the body being lowered is not yet part of.
    let all_names: Vec<String> = with
        .cte_tables
        .iter()
        .map(|cte| ident(&cte.alias.name))
        .collect();
    let mut named: Vec<String> = Vec::new();
    let mut bodies: Vec<(String, plan::Select, Vec<String>)> = Vec::new();
    for cte in &with.cte_tables {
        // `AS MATERIALIZED` and `AS NOT MATERIALIZED` are **accepted and change nothing**, which
        // is not the usual "reject rather than ignore": both spellings return the same rows on a
        // real server (measured), because what they choose is a plan and not an answer.
        refuse_if(cte.from.is_some(), "a WITH item with a FROM identifier")?;
        let name = ident(&cte.alias.name);
        plan::cte::refuse_duplicate(&named, &name)?;
        // Data-modifying CTEs are the read path's write half and are a unit of their own.
        let body = match cte.query.body.as_ref() {
            SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) => {
                return Err(SqlError::unsupported("a data-modifying WITH item"));
            }
            _ => lower_query(&cte.query)?,
        };
        let mut body = body;
        // Each item sees the ones before it and **not itself**: `WITH t AS (SELECT id FROM t)` is
        // the same `42P01` a forward reference gets, measured.
        for (earlier, earlier_body, earlier_columns) in &bodies {
            plan::cte::inline(&mut body, earlier, earlier_body, earlier_columns);
        }
        // What is left of the list is what this body may not reference -- itself included -- and a
        // reference to one of those is only an error if the catalog has no such relation.
        plan::cte::mark_hidden(&mut body, &all_names[bodies.len()..]);
        let columns: Vec<String> = cte
            .alias
            .columns
            .iter()
            .map(|column| ident(&column.name))
            .collect();
        named.push(name.clone());
        bodies.push((name, body, columns));
    }

    for (name, body, columns) in &bodies {
        plan::cte::inline(into, name, body, columns);
        // Carried whether anything referenced it or not: **an unreferenced CTE is still
        // analysed**, measured, and inlining alone would never look at one.
        into.ctes.push(plan::TableRef {
            values: None,
            name: name.clone(),
            alias: None,
            derived: Some(Box::new(plan::Derived::from_cte(
                Box::new(body.clone()),
                columns.clone(),
            ))),
            function: None,
            hidden_cte: false,
        });
    }
    Ok(())
}

/// One `JOIN`, lowered. Every join this crate does not run is named rather than approximated.
fn lower_join(join: &sqlparser::ast::Join) -> Result<plan::Join> {
    let table = table_reference(&join.relation)?;
    refuse_if(join.global, "a GLOBAL JOIN")?;
    let (kind, constraint) = match &join.join_operator {
        JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => {
            (plan::JoinKind::Inner, Some(constraint))
        }
        JoinOperator::CrossJoin(constraint) => (plan::JoinKind::Inner, Some(constraint)),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            (plan::JoinKind::Left, Some(constraint))
        }
        // Each of these keeps rows an inner join drops, so running one as an inner join would
        // silently return fewer rows than the user asked for -- the worst thing a join can do.
        other => {
            return Err(SqlError::unsupported(match other {
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
    let (on, using) = match constraint {
        Some(constraint) => join_constraint(constraint)?,
        None => (None, Vec::new()),
    };
    Ok(plan::Join {
        table,
        kind,
        on,
        using,
    })
}

/// A join's condition: an `ON` expression, the columns of a `USING`, or neither for a cross join.
///
/// `USING (a)` is carried as *columns* and not lowered here into `ON l.a = r.a`, because it does a
/// second thing an equality cannot: it **merges** the two columns, so `SELECT *` returns one `a`
/// and at the front. The planner builds the equality from the columns and the scope carries the
/// merge, which keeps both halves of the clause in one place.
fn join_constraint(constraint: &JoinConstraint) -> Result<(Option<plan::Expr>, Vec<String>)> {
    match constraint {
        JoinConstraint::On(expr) => Ok((Some(lower_expr(expr)?), Vec::new())),
        JoinConstraint::Using(columns) => {
            let columns = columns
                .iter()
                .map(object_name)
                .collect::<Result<Vec<_>>>()?;
            Ok((None, columns))
        }
        JoinConstraint::Natural => Err(SqlError::unsupported("a NATURAL JOIN")),
        JoinConstraint::None => Ok((None, Vec::new())),
    }
}

/// One `FROM` entry, with the alias it carries.
///
/// The alias is folded as an identifier like every other name here, so `AS "T"` and `AS T` are two
/// different names — measured: `SELECT T.id FROM alias_l AS "T"` is `42P01` on a real server.
///
/// A **column** alias list (`AS t (c, d)`) renames the columns as well, which is a second feature
/// and not a spelling of this one: after it the table's own column names are gone (`t.id` becomes
/// `42703`, measured). It is refused by name rather than silently ignored, because ignoring it
/// would answer a query about `c` with a column called `id`.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per `FROM` shape, and most of each is the refusal list"
)]
fn table_reference(factor: &TableFactor) -> Result<plan::TableRef> {
    match factor {
        // **`sqlparser` gives `UNNEST` a `TableFactor` of its own**, where every other table
        // function is a `Table` with arguments — so the same call that lowers one way in the
        // target list arrives here in another shape, and both have to end at the same
        // `plan::TableFunction`. `WITH OFFSET` and `WITH ORDINALITY` add a second column, which is
        // a different relation from the one this node builds.
        TableFactor::UNNEST {
            alias,
            array_exprs,
            with_offset,
            with_offset_alias,
            with_ordinality,
        } => {
            refuse_if(
                *with_offset || with_offset_alias.is_some(),
                "UNNEST WITH OFFSET",
            )?;
            refuse_if(*with_ordinality, "UNNEST WITH ORDINALITY")?;
            let alias = match alias {
                None => None,
                Some(alias) => {
                    refuse_if(!alias.columns.is_empty(), "a column alias list")?;
                    Some(ident(&alias.name))
                }
            };
            let args = array_exprs
                .iter()
                .map(lower_expr)
                .collect::<Result<Vec<_>>>()?;
            Ok(plan::TableRef {
                values: None,
                name: "unnest".to_owned(),
                alias,
                derived: None,
                function: Some(Box::new(plan::TableFunction {
                    name: "unnest".to_owned(),
                    args,
                    def: None,
                })),
                hidden_cte: false,
            })
        }
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            partitions,
            ..
        } => {
            // **A set-returning function standing where a relation does.** Two of them:
            // `generate_subscripts`, which is what the schema dump reads every constraint's and
            // every index's column list through, and `generate_series`. Anything else is still
            // named rather than approximated.
            if let Some(args) = args {
                let folded = relation_name(name)?;
                refuse_if(
                    !matches!(
                        folded.as_str(),
                        "generate_subscripts" | "generate_series" | "unnest"
                    ),
                    format!("the table function {folded}"),
                )?;
                let alias = match alias {
                    None => None,
                    Some(alias) => {
                        refuse_if(!alias.columns.is_empty(), "a column alias list")?;
                        Some(ident(&alias.name))
                    }
                };
                let mut lowered = Vec::new();
                for arg in &args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                            lowered.push(lower_expr(expr)?);
                        }
                        other => {
                            return Err(SqlError::unsupported(format!(
                                "the table function argument {other}"
                            )));
                        }
                    }
                }
                return Ok(plan::TableRef {
                    values: None,
                    name: folded.clone(),
                    alias,
                    derived: None,
                    function: Some(Box::new(plan::TableFunction {
                        name: folded,
                        args: lowered,
                        def: None,
                    })),
                    hidden_cte: false,
                });
            }
            refuse_if(args.is_some(), "a table function")?;
            refuse_if(!with_hints.is_empty(), "a table hint")?;
            refuse_if(version.is_some(), "a table version")?;
            refuse_if(!partitions.is_empty(), "a partition list")?;
            let alias = match alias {
                None => None,
                Some(alias) => {
                    refuse_if(!alias.columns.is_empty(), "a column alias list")?;
                    Some(ident(&alias.name))
                }
            };
            Ok(plan::TableRef {
                values: None,
                name: relation_name(name)?,
                alias,
                derived: None,
                function: None,
                hidden_cte: false,
            })
        }
        // `FROM (SELECT …) AS t` — a **derived table**. The alias is optional on PostgreSQL 19
        // (measured; it was mandatory before 16), and without one there is simply no name to
        // qualify the relation with, which an empty `TableRef::name` says exactly.
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample,
        } => {
            // `LATERAL` is unit 4's correlation applied to a `FROM` item, and it is deliberately
            // not folded into either (`docs/plans/phase-12-subquery.md` §4).
            refuse_if(*lateral, "LATERAL")?;
            refuse_if(sample.is_some(), "TABLESAMPLE")?;
            let (name, columns) = match alias {
                None => (String::new(), Vec::new()),
                Some(alias) => (
                    ident(&alias.name),
                    alias
                        .columns
                        .iter()
                        .map(|column| ident(&column.name))
                        .collect(),
                ),
            };
            // **A `VALUES` list is not a derived table**, even though it is written like one: a
            // derived table's rows come from a sub-select, and no select without a `FROM` produces
            // more than one row. So it is its own kind of entry, with the alias list applied to
            // the names it gives itself.
            if let SetExpr::Values(values) = subquery.body.as_ref() {
                return Ok(plan::TableRef {
                    values: Some(Box::new(lower_values(values, &columns, &name)?)),
                    name,
                    alias: None,
                    derived: None,
                    function: None,
                    hidden_cte: false,
                });
            }
            Ok(plan::TableRef {
                values: None,
                name,
                alias: None,
                derived: Some(Box::new(plan::Derived::new(
                    Box::new(lower_query(subquery)?),
                    columns,
                ))),
                function: None,
                hidden_cte: false,
            })
        }
        other => Err(SqlError::unsupported(format!("the FROM item {other}"))),
    }
}

/// The same entry where an alias is not executed: `UPDATE` and `DELETE`, whose one table is
/// resolved against itself and has no second name to tell apart. A real server takes one
/// (measured), so this is a refusal by name and not a claim about the grammar.
fn table_factor(factor: &TableFactor) -> Result<String> {
    let table = table_reference(factor)?;
    refuse_if(table.alias.is_some(), "a table alias")?;
    Ok(table.name)
}

fn lower_update(update: &sqlparser::ast::Update) -> Result<plan::Update> {
    let returning = update
        .returning
        .as_deref()
        .map(lower_projection)
        .transpose()?;
    refuse_if(update.output.is_some(), "UPDATE ... OUTPUT")?;
    refuse_if(update.or.is_some(), "UPDATE OR")?;
    refuse_if(!update.order_by.is_empty(), "UPDATE ... ORDER BY")?;
    refuse_if(update.limit.is_some(), "UPDATE ... LIMIT")?;
    refuse_if(!update.optimizer_hints.is_empty(), "an optimizer hint")?;
    // **A join written on the target itself is not the `FROM` clause.** `UPDATE a JOIN b ON …` is
    // not PostgreSQL's grammar at all; `UPDATE a SET … FROM b …` is, and it is the one below.
    refuse_if(!update.table.joins.is_empty(), "a JOIN in UPDATE")?;
    // `FROM` **before** `SET` is Snowflake's spelling and not PostgreSQL's, so it is refused by
    // name rather than accepted as the same clause in another place.
    let from_entries = match &update.from {
        None => &[][..],
        Some(sqlparser::ast::UpdateTableFromKind::AfterSet(entries)) => entries.as_slice(),
        Some(sqlparser::ast::UpdateTableFromKind::BeforeSet(_)) => {
            return Err(SqlError::unsupported("UPDATE ... FROM before SET"));
        }
    };
    let (from, joins) = lower_from_entries(from_entries)?;

    // **The target carries its alias.** Where there is no `FROM` an alias is only a second name
    // for the only relation there is; with one it is the whole mechanism, because an alias
    // *replaces* the name and frees it for the `FROM` entry that shares it. One rule for both,
    // because two would be a rule about a clause the alias is not part of.
    let target = table_reference(&update.table.relation)?;
    // A relation, and only a relation: `UPDATE (SELECT …)` is not a statement, and neither is an
    // `UPDATE` of a `VALUES` list or of a set-returning function.
    refuse_if(
        target.derived.is_some() || target.values.is_some() || target.function.is_some(),
        "an UPDATE of something that is not a table",
    )?;
    let alias = target.alias.clone();

    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let name = match &assignment.target {
                // **A `SET` target is never qualified**, not even with the target's own alias,
                // and PostgreSQL's error is about a *column*: `a.body` is read as the column `a`
                // and a field of it, so `a` is what it reports missing. The relation it names is
                // the table's own name and not the alias — measured, `HINT` included.
                AssignmentTarget::ColumnName(name) => match name.0.as_slice() {
                    [_] => object_name(name)?,
                    [first, ..] => {
                        return Err(SqlError::QualifiedSetTarget {
                            column: first.as_ident().map_or_else(|| first.to_string(), ident),
                            relation: target.name.clone(),
                        });
                    }
                    [] => return Err(SqlError::unsupported("an empty assignment target")),
                },
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
        table: target.name,
        alias,
        from,
        joins,
        assignments,
        filter: update.selection.as_ref().map(lower_expr).transpose()?,
        returning,
    })
}

/// A comma list of `FROM` entries with their joins, as a left-most relation and a join chain.
///
/// The same reading a `SELECT`'s `FROM` gets, and deliberately the same code path: a comma is a
/// cross join with the condition in the `WHERE`, which is what the comma form *means*, so nothing
/// is approximated by writing it as one.
fn lower_from_entries(
    entries: &[sqlparser::ast::TableWithJoins],
) -> Result<(Option<plan::TableRef>, Vec<plan::Join>)> {
    let [first, rest @ ..] = entries else {
        return Ok((None, Vec::new()));
    };
    let left = table_reference(&first.relation)?;
    let mut joins = first
        .joins
        .iter()
        .map(lower_join)
        .collect::<Result<Vec<_>>>()?;
    for entry in rest {
        joins.push(plan::Join {
            table: table_reference(&entry.relation)?,
            kind: plan::JoinKind::Inner,
            on: None,
            using: Vec::new(),
        });
        for join in &entry.joins {
            joins.push(lower_join(join)?);
        }
    }
    Ok((Some(left), joins))
}

fn lower_delete(delete: &sqlparser::ast::Delete) -> Result<plan::Delete> {
    refuse_if(!delete.tables.is_empty(), "a multi-table DELETE")?;
    refuse_if(delete.using.is_some(), "DELETE ... USING")?;
    let returning = delete
        .returning
        .as_deref()
        .map(lower_projection)
        .transpose()?;
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
        returning,
    })
}

/// Every stored type, under every spelling PostgreSQL accepts for it — and the serial spellings,
/// which are not types at all — with the **typmod** the declaration carries.
///
/// A serial is its integer plus a sequence: `bigserial` lowers to [`ColumnType::Int8`] and
/// `serial` to [`ColumnType::Int4`], with the caller reading [`serial_identity`] to find out that
/// a sequence goes with it. `serial` was `0A000` until [ADR
/// 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md), and only because `int4` was
/// missing — accepting it as an `int8` would have taken every value between 2^31 and 2^63 that a
/// real server answers `22003` for. With `int4` there is nothing left of that argument.
///
/// # The number after the type
///
/// Three types take one and each does something different with it, which is the whole of the
/// typmod unit: `varchar(n)` **refuses** a longer value, `character(n)` **pads** a shorter one,
/// and `timestamp(p)` **rounds**. The number is returned rather than folded into the type because
/// that is where PostgreSQL keeps it — one `varchar` row in `pg_type` and an `atttypmod` per
/// column — and `crate::value::NO_TYPMOD` is what a declaration without one carries.
///
/// **A bare `character` is `character(1)`**, not "unlimited": measured on 19beta1, where
/// `format_type` says `character(1)` and a second character is `22001`. That is the opposite of
/// `character varying`, whose bare spelling means no limit at all.
/// A **column's** declared type: one of this node's own, or the name of a user-defined one.
///
/// The two are told apart by the catalog and not here, which is the whole reason the third
/// element exists. `sqlparser` gives every name it has no variant for as a `Custom`, so `mood`,
/// `money` and `nosuchtype` arrive identically — the first is an enum a `CREATE TYPE` made, the
/// second is a PostgreSQL type this node does not have, and the third is a typo. Lowering cannot
/// separate them without reading the catalog, so it hands the name on and the executor answers:
/// a type it finds becomes [`crate::catalog::ColumnDef::user_type`], and one it does not is the
/// same `0A000 the type <name> is not supported` this function used to raise here.
///
/// **A modifier or a qualifier disqualifies it.** `mood(3)` is not a user type — no user type
/// takes a typmod here — and `test_schema.mood` is a type in a schema, which is
/// [ADR 0050](../../../docs/adr/0050-a-user-defined-type-is-a-value.md)'s explicit non-goal and
/// the namespace lane's. Both keep the refusal `lower_type` gives them.
fn lower_column_type(data_type: &DataType) -> Result<(ColumnType, i32, Option<String>)> {
    match lower_type(data_type) {
        Ok((ty, typmod)) => Ok((ty, typmod, None)),
        Err(error) => match data_type {
            DataType::Custom(name, modifiers)
                if modifiers.is_empty() && name.0.len() == 1 && !is_serial_spelling(data_type) =>
            {
                let Some(part) = name.0.first().and_then(|part| part.as_ident()) else {
                    return Err(error);
                };
                // The type is unknown here and the placeholder says so: `Int2` is what an enum's
                // ordinal is, and the executor replaces it for any other kind. Nothing reads it
                // before then — a `CREATE TABLE` plan is executed, never evaluated.
                Ok((ColumnType::Int2, NO_TYPMOD, Some(ident(part))))
            }
            _ => Err(error),
        },
    }
}

/// Whether a custom type name is one of the `serial` spellings, which are integers plus a sequence
/// and never a user-defined type — a table with a column called `serial` would otherwise resolve
/// against the catalog and get a worse error than the one [`lower_type`] already gives it.
fn is_serial_spelling(data_type: &DataType) -> bool {
    serial_identity(data_type).is_some()
}

fn lower_type(data_type: &DataType) -> Result<(ColumnType, i32)> {
    let plain = |ty| Ok((ty, NO_TYPMOD));
    match data_type {
        // **`int8[]` is a column type**, over one of the four element types this node has an
        // array of. The element's own declaration is read first, so `numeric(10,2)[]` is refused
        // by naming the typmod rather than by silently dropping it — an array takes none here.
        DataType::Array(inner) => {
            let Some(element) = array_element(inner) else {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            };
            let (element, typmod) = lower_type(element)?;
            if typmod != NO_TYPMOD {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            }
            let Some(array) = esker_keys::array::ArrayValue::array_of(element) else {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            };
            plain(array)
        }
        // The three that take a number. Each is checked against PostgreSQL's own limit, because a
        // length this node accepted and a real server refused would be a table that exists here
        // and not there.
        DataType::Varchar(Some(length)) | DataType::CharacterVarying(Some(length)) => {
            Ok((ColumnType::Varchar, string_typmod(length, "varchar")?))
        }
        // `numeric` and `decimal` are one type under two spellings, which is PostgreSQL's own
        // model: `'decimal(3,2)'::regtype` is `numeric(3,2)` there. A **bare precision means
        // scale zero**, not "no scale" — `numeric(10)` is `numeric(10,0)` and rounds — which is
        // the reading that would silently keep a fraction if it were got wrong.
        DataType::Numeric(info) | DataType::Decimal(info) | DataType::Dec(info) => {
            // A number too large for an `i32` cannot be in range either, so it is clamped rather
            // than a second error path: `declared_typmod` names the bound it broke.
            let (precision, scale) = match info {
                ExactNumberInfo::None => (None, None),
                ExactNumberInfo::Precision(precision) => {
                    (Some(i32::try_from(*precision).unwrap_or(i32::MAX)), None)
                }
                // The scale is **signed** on a real server — `numeric(10,-2)` is a real type —
                // which is why this one is an `i64` where the precision is a `u64`.
                ExactNumberInfo::PrecisionAndScale(precision, scale) => (
                    Some(i32::try_from(*precision).unwrap_or(i32::MAX)),
                    Some(i32::try_from(*scale).unwrap_or(i32::MAX)),
                ),
            };
            Ok((
                ColumnType::Numeric,
                value::numeric::declared_typmod(precision, scale)?,
            ))
        }
        DataType::Char(length) | DataType::Character(length) => match length {
            Some(length) => Ok((ColumnType::Bpchar, string_typmod(length, "char")?)),
            // `character` with no number is `character(1)`.
            None => Ok((ColumnType::Bpchar, value::typmod_of_length(1))),
        },
        DataType::Timestamp(
            Some(precision),
            TimezoneInfo::None | TimezoneInfo::WithoutTimeZone,
        ) if *precision <= 6 => Ok((
            ColumnType::Timestamp,
            value::typmod_of_precision(u32::try_from(*precision).unwrap_or(6)),
        )),
        // **`time(7)` is `time(6)`, not an error.** A precision past the maximum is reduced to it
        // — a `WARNING` on a real server and no complaint at all in the answer — where a
        // `varchar` length past *its* bound is `22023`. The asymmetry is PostgreSQL's, measured:
        // `'time(9)'::regtype` is 1083 and `'12:34:56'::time(7)` declares `time(6)`.
        DataType::Time(Some(precision), TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            Ok((
                ColumnType::Time,
                value::typmod_of_precision(u32::try_from(*precision).unwrap_or(6).min(6)),
            ))
        }
        other => lower_plain_type(other).and_then(&plain),
    }
}

/// PostgreSQL's ceiling on a declared string length: [`value::MAX_TYPE_LENGTH`], which a
/// `regtype` name is held to as well so that the two cannot disagree.
use crate::value::MAX_TYPE_LENGTH as MAX_STRING_LENGTH;

/// The declared length of a `varchar(n)` or `character(n)`, as a typmod.
///
/// `spelled` is the **short** name — `varchar`, `char` — because that is what these two messages
/// use, where `22001 value too long for type character varying(5)` uses the long one. One type,
/// two vocabularies, and `tests/corpus/pg19_typmod.txt` carries both rather than either being
/// inferred from the other.
fn string_typmod(length: &CharacterLength, spelled: &'static str) -> Result<i32> {
    let length = match length {
        CharacterLength::IntegerLength { length, unit } => {
            // `varchar(5 OCTETS)` and `varchar(5 CHARACTERS)` are the standard's spellings, which
            // PostgreSQL does not take. Named rather than ignored: a unit this node dropped would
            // silently change what the column holds for a multi-byte value.
            refuse_if(unit.is_some(), format!("a length unit on {spelled}"))?;
            *length
        }
        // `varchar(MAX)` is SQL Server's.
        CharacterLength::Max => return Err(SqlError::unsupported(format!("{spelled}(MAX)"))),
    };
    if length < 1 {
        return Err(SqlError::TypeLengthTooSmall(spelled));
    }
    if length > u64::from(MAX_STRING_LENGTH) {
        return Err(SqlError::TypeLengthTooLarge(spelled, MAX_STRING_LENGTH));
    }
    Ok(value::typmod_of_length(
        u32::try_from(length).unwrap_or(MAX_STRING_LENGTH),
    ))
}

/// Every type that takes no number.
fn lower_plain_type(data_type: &DataType) -> Result<ColumnType> {
    Ok(match data_type {
        DataType::Int8(None) | DataType::BigInt(None) => ColumnType::Int8,
        // `int`, `int4` and `integer` are one type under three spellings, and `sqlparser` gives
        // each its own variant. A display width — `int(11)` — is MySQL's and is refused below
        // with the type as the user wrote it.
        DataType::Int4(None) | DataType::Int(None) | DataType::Integer(None) => ColumnType::Int4,
        DataType::Int2(None) | DataType::SmallInt(None) => ColumnType::Int2,
        DataType::Float4 | DataType::Real => ColumnType::Real,
        DataType::Text => ColumnType::Text,
        // Neither takes a typmod, and the refusal is the *parser's*: `json(10)` is a syntax error
        // before it reaches here, like `integer(4)`. ADR 0042.
        DataType::JSON => ColumnType::Json,
        DataType::JSONB => ColumnType::Jsonb,
        // `character varying` and `varchar` with **no length**: unlimited, which is what the bare
        // spelling means. The lengths are handled by the caller, which is where the typmod is.
        DataType::Varchar(None) | DataType::CharacterVarying(None) => ColumnType::Varchar,
        DataType::Bool | DataType::Boolean => ColumnType::Bool,
        DataType::Bytea => ColumnType::Bytea,
        DataType::Float8 | DataType::DoublePrecision | DataType::Double(ExactNumberInfo::None) => {
            ColumnType::Double
        }
        // **`FLOAT` is a spelling, not a type.** Bare it is `double precision`; with a precision
        // in bits it is whichever IEEE width holds that many — `float(1)` through `float(24)` are
        // `real` and `float(25)` through `float(53)` are `double precision`. Measured, including
        // both ends of the range, which have their own message: `precision for type float must be
        // at least 1 bit`, not the `length for type …` a string gets.
        DataType::Float(precision) => match precision {
            ExactNumberInfo::None => ColumnType::Double,
            ExactNumberInfo::Precision(0) => return Err(SqlError::FloatPrecisionTooSmall),
            ExactNumberInfo::Precision(bits) if *bits <= 24 => ColumnType::Real,
            ExactNumberInfo::Precision(bits) if *bits <= 53 => ColumnType::Double,
            ExactNumberInfo::Precision(_) => return Err(SqlError::FloatPrecisionTooLarge),
            // `float(10, 2)` is a syntax a real server does not take for this type.
            ExactNumberInfo::PrecisionAndScale(..) => {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            }
        },
        DataType::Timestamp(None, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::TimestampTz
        }
        // `timestamp` with no precision: six digits, PostgreSQL's default and its maximum.
        // `timestamp(6)` is handled by the caller and carries a typmod, which is the only
        // difference between the two — `format_type` prints one as `timestamp(6) without time
        // zone` and the other as `timestamp without time zone`, and they hold the same values.
        DataType::Timestamp(None, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            ColumnType::Timestamp
        }
        // `date` takes no typmod at all — `format_type(1082, 3)` prints `date(3)` on a real
        // server and `CREATE TABLE t (d date(3))` is a syntax error there, so the number has
        // nowhere to come from and nothing here produces one.
        DataType::Date => ColumnType::Date,
        DataType::Uuid => ColumnType::Uuid,
        // The fields and the precision are the **typmod**, which this node does not carry for
        // this type: `interval day to hour` is a bitmask on a real server (`0x408ffff`), not a
        // number, and it restricts what the value keeps. Accepted and dropped here, which is
        // declared in `tests/interval.rs` — the column stores every field either way.
        DataType::Interval { .. } => ColumnType::Interval,
        // `time` with no precision: six digits, the default and the maximum, as `timestamp` has
        // it. `time(p)` is the caller's, and carries a typmod.
        DataType::Time(None, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            ColumnType::Time
        }
        // `oid` is not one of `sqlparser`'s data types, so a declared `o oid` column arrives as
        // a custom name — the same road `serial` takes below.
        DataType::Custom(name, modifiers)
            if modifiers.is_empty() && name.to_string().eq_ignore_ascii_case("oid") =>
        {
            ColumnType::Oid
        }
        // **`hstore` is a real type here and a `CREATE EXTENSION` type there**, and it reaches
        // `sqlparser` as a custom name for the same reason `oid` does. Resolving it before the
        // catalog is asked is what makes `'a=>b'::hstore` an ordinary constant cast rather than
        // the cast-to-a-named-type this crate does not have yet.
        DataType::Custom(name, modifiers)
            if modifiers.is_empty() && name.to_string().eq_ignore_ascii_case("hstore") =>
        {
            ColumnType::Hstore
        }
        // `bigserial` and `serial` are `bigint`/`integer` plus a sequence, and `sqlparser` 0.62
        // has no variant for either -- both arrive as a custom type name. `smallserial` arrives
        // the same way and falls through to the refusal below until `int2` lands, which names
        // what the user wrote.
        other if serial_identity(other).is_some() => match serial_width(other) {
            Some(ty) => ty,
            None => return Err(SqlError::unsupported(format!("the type {other}"))),
        },
        other => return Err(SqlError::unsupported(format!("the type {other}"))),
    })
}

/// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`, and the two things that share its variant.
///
/// Only one of them is a sequence. `GENERATED ALWAYS AS (<expr>) STORED` is a **computed column**,
/// a different feature entirely, and is refused by name rather than read as an identity that would
/// then hand out numbers where the user asked for an expression. The *sequence options* after an
/// identity are gap G07 and are refused too, for the same reason: a `START WITH` this node ignored
/// would hand out numbers nobody asked for.
fn identity_kind(
    generated_as: GeneratedAs,
    sequence_options: Option<&[sqlparser::ast::SequenceOptions]>,
    generation_expr: Option<&Expr>,
) -> Result<plan::Identity> {
    refuse_if(
        generation_expr.is_some(),
        "GENERATED ALWAYS AS (expression) STORED",
    )?;
    refuse_if(
        sequence_options.is_some_and(|options| !options.is_empty()),
        "a sequence option on an identity column",
    )?;
    match generated_as {
        GeneratedAs::Always => Ok(plan::Identity::Always),
        GeneratedAs::ByDefault => Ok(plan::Identity::ByDefault),
        GeneratedAs::ExpStored => Err(SqlError::unsupported(
            "GENERATED ALWAYS AS (expression) STORED",
        )),
    }
}

/// Whether a declared type is one of the serial spellings, and therefore brings a sequence.
///
/// `smallserial` and `serial` are refused by [`lower_type`] before this is reached, so the only
/// one that answers `Some` is `bigserial`.
fn serial_identity(data_type: &DataType) -> Option<plan::Identity> {
    serial_width(data_type).map(|_| plan::Identity::Default)
}

/// The integer a serial spelling stands for, or `None` if it is not one.
///
/// Measured rather than assumed: a real server reports a `serial` column as `integer`, `NOT NULL`,
/// `DEFAULT nextval('t_a_seq'::regclass)` — a serial is not a type, it is three things this node
/// already has. `smallserial` waits for `int2` and is refused by name until then, which is the
/// same shape `serial` itself was in before this ADR.
fn serial_width(data_type: &DataType) -> Option<ColumnType> {
    let DataType::Custom(name, modifiers) = data_type else {
        return None;
    };
    if !modifiers.is_empty() {
        return None;
    }
    let name = name.to_string();
    if name.eq_ignore_ascii_case("bigserial") || name.eq_ignore_ascii_case("serial8") {
        return Some(ColumnType::Int8);
    }
    if name.eq_ignore_ascii_case("serial") || name.eq_ignore_ascii_case("serial4") {
        return Some(ColumnType::Int4);
    }
    if name.eq_ignore_ascii_case("smallserial") || name.eq_ignore_ascii_case("serial2") {
        return Some(ColumnType::Int2);
    }
    None
}

/// A **constraint's** columns, which must be plain names.
///
/// `UNIQUE ((lower(b)))` is a syntax error on a real server — measured — so an expression here
/// would be inventing a feature rather than implementing one. `CREATE INDEX` is the form that
/// takes expressions, and it uses [`index_keys`].
fn index_columns(columns: &[IndexColumn]) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|column| {
            index_key_options(column)?;
            // `UNIQUE (a DESC)` and `PRIMARY KEY (a NULLS FIRST)` are **syntax errors** on a real
            // server — measured, `42601 syntax error at or near "DESC"` — because a constraint's
            // grammar has no direction in it at all. Refused by name rather than accepted and
            // ignored, and the SQLSTATE is the one divergence
            // (`tests/desc_index.rs`'s `DIVERGENCES`).
            refuse_if(
                column.column.options.asc.is_some(),
                "a direction on a constraint's columns",
            )?;
            refuse_if(
                column.column.options.nulls_first.is_some(),
                "NULLS FIRST/LAST on a constraint's columns",
            )?;
            match &column.column.expr {
                Expr::Identifier(name) => Ok(ident(name)),
                other => Err(SqlError::unsupported(format!(
                    "the index expression {other}"
                ))),
            }
        })
        .collect()
}

/// The options a key part may not carry, whichever kind of key it is in.
fn index_key_options(column: &IndexColumn) -> Result<()> {
    refuse_if(column.operator_class.is_some(), "an index operator class")?;
    refuse_if(column.column.with_fill.is_some(), "WITH FILL")?;
    Ok(())
}

/// The order a `CREATE INDEX` key part is written in.
///
/// `ASC` is the default and `DESC` is not, and **each direction has its own default null
/// placement**: ascending sorts NULLs last, descending sorts them first. So an unwritten
/// `NULLS …` is not "last", it is "whatever this direction means", which is what
/// [`catalog::KeyOrder::of`] says once rather than at each call.
fn index_key_order(column: &IndexColumn) -> KeyOrder {
    let descending = column.column.options.asc == Some(false);
    KeyOrder {
        descending,
        nulls_first: column
            .column
            .options
            .nulls_first
            .unwrap_or(KeyOrder::of(descending).nulls_first),
    }
}

/// A `CREATE INDEX`'s key parts: a column by name, or an expression.
///
/// The parentheses around an index expression are the **column list's**, not the expression's, so
/// `((lower(b)))` reaches here as `(lower(b))` and `(lower(b))` as `lower(b)` — a real server
/// takes both spellings and prints one. [`unwrap_nested`] is what makes them the same thing.
///
/// A bare identifier after that unwrapping is a **column**, not an expression, which is also
/// PostgreSQL's answer: `CREATE INDEX ON t ((b))` has `indkey = 3` and no `indexprs` at all.
fn index_keys(columns: &[IndexColumn]) -> Result<Vec<plan::IndexKeyPart>> {
    columns
        .iter()
        .map(|column| {
            index_key_options(column)?;
            let written = &column.column.expr;
            let part = match unwrap_nested(written) {
                Expr::Identifier(name) => plan::KeyPartName::Column(ident(name)),
                expr => {
                    // **The doubled parenthesis is grammar, not style.** PostgreSQL's `index_elem`
                    // is `ColId | func_expr_windowless | '(' a_expr ')'`, so `ON t ((lower(b)))`
                    // and the bare call `ON t (lower(b))` are both accepted and everything else
                    // needs a pair of its own: `ON t (a + 1)` is `42601`, and so is
                    // `ON t (CASE … END)`. `sqlparser` parses all of them, so this is where the
                    // narrower grammar is enforced — without it this node builds an index a real
                    // server refuses to create, which is a wrong answer and not a gap.
                    if !matches!(written, Expr::Nested(_) | Expr::Function(_)) {
                        return Err(SqlError::SyntaxAtOrNear(index_elem_token(written)));
                    }
                    plan::KeyPartName::Expression {
                        expr: expr.to_string(),
                        shape: expr_shape(expr),
                    }
                }
            };
            Ok(plan::IndexKeyPart {
                part,
                order: index_key_order(column),
            })
        })
        .collect()
}

/// The token PostgreSQL names in the `42601` an unparenthesised index expression gets.
///
/// Not the first token of the expression in every case, and that is the whole of what this
/// function is: PostgreSQL's parser consumes what the grammar allows and then names where it
/// stopped. `ON t (1)` stops at the **first** token because nothing may begin an index element
/// with a constant; `ON t (a + 1)` stops at the **second**, because `a` is a perfectly good
/// `ColId` and the `+` after it is not. Measured, all six spellings
/// (`tests/corpus/pg19_case_expression.txt`).
fn index_elem_token(expr: &Expr) -> String {
    /// Whether this could have begun an index element, which decides whether the offending token
    /// is the first one or the one after it.
    fn begins_an_element(expr: &Expr) -> bool {
        matches!(
            expr,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Function(_)
        )
    }
    match expr {
        Expr::BinaryOp { left, op, .. } if begins_an_element(left) => op.to_string(),
        Expr::Cast { expr: inner, .. } if begins_an_element(inner) => "::".to_owned(),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) if begins_an_element(inner) => "IS".to_owned(),
        Expr::Case { .. } => "CASE".to_owned(),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        } => "NOT".to_owned(),
        Expr::Value(value) => value.value.to_string(),
        // Every other shape stops at its own first token, which is the first word of what was
        // written — the same fallback [`alter_action_name`] uses for an action it has no name for.
        other => other
            .to_string()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// An expression with its redundant outer parentheses removed.
///
/// PostgreSQL stores a *parsed* expression and re-prints it, so the parentheses that come back
/// are the ones its own deparser adds and not the ones that were written: `WHERE (a > 10)` comes
/// back `WHERE (a > 10)` and `WHERE a > 10` comes back the same, one pair either way. This crate
/// stores text, so the normalising has to happen where the text is taken — here — or the two
/// spellings would print differently for the rest of the record's life.
fn unwrap_nested(expr: &Expr) -> &Expr {
    let mut expr = expr;
    while let Expr::Nested(inner) = expr {
        expr = inner;
    }
    expr
}

/// Which of PostgreSQL's three deparse shapes an index expression is
/// ([`catalog::ExprShape`]).
///
/// The shape is a property of the **node**, and this is the one place that still knows which node
/// it was: by the time the expression is text, `'x)'::text` and `f(x)` are the same characters at
/// the ends.
fn expr_shape(expr: &Expr) -> catalog::ExprShape {
    use catalog::ExprShape;
    match expr {
        Expr::Function(_) => ExprShape::Call,
        // **A `CASE` is parenthesised exactly like a value**, which is measured rather than
        // assumed: `pg_get_expr` gives it bare, the key list gives it one pair, and the per-column
        // form gives it one pair — the `Value` row of [`catalog::ExprShape`]'s table, and not the
        // `Operator` row its `WHEN` conditions might suggest. What is different about it is the
        // text, not the parentheses (`crate::exec::ddl::index_expression`).
        Expr::Value(_) | Expr::Cast { .. } | Expr::TypedString { .. } | Expr::Case { .. } => {
            ExprShape::Value
        }
        _ => ExprShape::Operator,
    }
}

/// A relation **anywhere a relation is named** — a `FROM` clause, a `DROP`, an `INSERT INTO`, a
/// `CREATE INDEX ... ON` — which is where a schema may be written.
///
/// Two schemas and no others, and they behave differently on purpose:
///
/// * **`pg_catalog.x` is `x`.** `pg_class` is a relation on its own here, and `pg_catalog` is the
///   schema it is in, so the qualifier names the thing that is already there. Measured:
///   `SELECT relname FROM pg_catalog.pg_class` and `FROM pg_class` are the same query on a real
///   server, and `ActiveRecord` writes the qualified form in three of its boot statements.
/// * **`information_schema.tables` keeps its qualifier**, because a bare `tables` is **not** a
///   relation on a real server — `42P01` — and answering it here would invent one. So the schema
///   is part of the view's name (`catalog::information_schema`).
///
/// Everything else stays refused by name. `public.t` is the interesting one: a real server takes
/// it, this node has one schema, and answering it would be right *when the qualifier is `public`*
/// and a wrong answer when it is not — so it waits for a unit that has schemas rather than being
/// guessed at here.
///
/// **This is what a write reaches too**, and it has to be: `DROP TABLE pg_catalog.pg_class` is
/// `42501 permission denied` on a real server, and a lowering that refused the *qualifier* first
/// would answer `0A000` and skip the guard that stops a client dropping a catalog relation
/// (`catalog::pg_catalog::refuse_write`). Measured, and `CREATE TABLE pg_catalog.pg_type`
/// is `42P07` there for the same reason.
fn relation_name(name: &ObjectName) -> Result<String> {
    let parts: Option<Vec<&str>> = name
        .0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
        .collect();
    if let Some([schema, relation]) = parts.as_deref() {
        if schema.eq_ignore_ascii_case("pg_catalog") {
            return Ok(fold_identifier(relation, false).0);
        }
        // **`public` is this node's only schema**, which is what `current_schema()` answers two
        // screens up — so `public.t` and `t` are the same relation and the qualifier is dropped
        // rather than refused. A qualifier naming any *other* schema still is: `s.t` and `t` would
        // be different tables on a real server and answering about the second would be wrong.
        if schema.eq_ignore_ascii_case(PUBLIC_SCHEMA) {
            return Ok(fold_identifier(relation, false).0);
        }
        if schema.eq_ignore_ascii_case("information_schema") {
            return Ok(format!(
                "information_schema.{}",
                fold_identifier(relation, false).0
            ));
        }
    }
    // **A user schema is part of the stored name**, separated by a NUL rather than a dot — see
    // `catalog::SCHEMA_SEPARATOR` for why a dot cannot do it. Each half is folded on its own,
    // because each was quoted or not on its own: `test_schema."Things"` is `Things` in
    // `test_schema`.
    if let [schema, relation] = name.0.as_slice()
        && let (Some(schema), Some(relation)) = (schema.as_ident(), relation.as_ident())
    {
        return Ok(catalog::qualify(&ident(schema), &ident(relation)));
    }
    object_name(name)
}

/// `CREATE TYPE <name> AS RANGE (…) | AS (…) | AS ENUM (…)`.
///
/// **Three shapes, and the row that asked for them wants ranges nine times out of ten**:
/// `adapters/postgresql/range_test.rb` is 46 of the 51 tests, `composite_test.rb` 4 and
/// `timestamp_test.rb` 1. All three are read here; what a type can then be *used* for is a
/// separate question and a narrower one.
fn lower_create_type(
    name: &ObjectName,
    representation: Option<&sqlparser::ast::UserDefinedTypeRepresentation>,
) -> Result<plan::Statement> {
    use sqlparser::ast::{
        UserDefinedTypeRangeOption as RangeOption, UserDefinedTypeRepresentation,
    };
    let name = relation_name(name)?;
    let kind = match representation {
        Some(UserDefinedTypeRepresentation::Range { options }) => {
            let mut subtype = None;
            let mut subtype_diff = None;
            for option in options {
                match option {
                    RangeOption::Subtype(data_type) => subtype = Some(lower_type(data_type)?.0),
                    // Carried verbatim: `pg_range.rngsubdiff` prints the name back and nothing
                    // here calls it, which is what the capture measured — `float8mi` is a real
                    // server's own function and this node has no function to point at.
                    RangeOption::SubtypeDiff(function) => {
                        subtype_diff = Some(function.to_string());
                    }
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "CREATE TYPE ... AS RANGE ( {other} )"
                        )));
                    }
                }
            }
            // `42704 type "nosuchtype" does not exist` comes out of `lower_type` above; a range
            // with no `subtype` at all is PostgreSQL's own `42P16`, which this node has not
            // measured, so it is named rather than guessed.
            let subtype = subtype.ok_or_else(|| {
                SqlError::unsupported("CREATE TYPE ... AS RANGE without a subtype")
            })?;
            TypeKind::Range {
                subtype,
                subtype_diff,
            }
        }
        Some(UserDefinedTypeRepresentation::Composite { attributes }) => {
            let mut fields = Vec::with_capacity(attributes.len());
            for attribute in attributes {
                refuse_if(
                    attribute.collation.is_some(),
                    "CREATE TYPE ... AS ( ... COLLATE )",
                )?;
                let (ty, typmod) = lower_type(&attribute.data_type)?;
                fields.push(TypeField {
                    name: ident(&attribute.name),
                    ty,
                    typmod,
                });
            }
            TypeKind::Composite { fields }
        }
        // **An empty label list is legal**, measured: `CREATE TYPE emptyenum AS ENUM ()`
        // succeeds and its `typtype` is `e`.
        Some(UserDefinedTypeRepresentation::Enum { labels }) => TypeKind::Enum {
            labels: labels.iter().map(|label| label.value.clone()).collect(),
        },
        Some(UserDefinedTypeRepresentation::SqlDefinition { .. }) => {
            return Err(SqlError::unsupported("CREATE TYPE with a SQL definition"));
        }
        // `CREATE TYPE name` with no body is a **shell type**, which exists to be filled in by a
        // C function. There is nothing this node could put in one.
        None => return Err(SqlError::unsupported("CREATE TYPE with no definition")),
    };
    Ok(plan::Statement::CreateType(plan::CreateType { name, kind }))
}

/// `COMMENT ON TABLE | COLUMN | INDEX <name> IS '…' | NULL`.
///
/// **The three kinds this node has, and no others.** PostgreSQL takes a comment on a schema, a
/// type, a role and eleven more; each of those is an object this node does not have, so a comment
/// on one has nowhere to live and is `0A000` naming the kind rather than a write that goes
/// nowhere.
///
/// `IF EXISTS` is not PostgreSQL's — it is Snowflake's, which `sqlparser` parses for every
/// dialect — so it is refused rather than honoured: accepting a spelling a real server rejects
/// would make this node's grammar wider than the one it is copying.
fn lower_comment(
    object_type: sqlparser::ast::CommentObject,
    name: &ObjectName,
    comment: Option<&str>,
    if_exists: bool,
) -> Result<plan::Statement> {
    use sqlparser::ast::CommentObject as Object;
    refuse_if(if_exists, "COMMENT ON ... IF EXISTS")?;
    let object = match object_type {
        Object::Table => plan::CommentObject::Table,
        Object::Column => plan::CommentObject::Column,
        Object::Index => plan::CommentObject::Index,
        // Taken so that the *kind* can be reported: a real server resolves the name first, so
        // `COMMENT ON SEQUENCE <a table>` is `42809` there and not a refusal of the statement.
        Object::Sequence => plan::CommentObject::Sequence,
        Object::View => plan::CommentObject::View,
        other => return Err(SqlError::unsupported(format!("COMMENT ON {other}"))),
    };
    // A column's name is the table's plus one more part, so the last part is split off before the
    // rest is read as a relation name — which is what lets `public.t.a` work wherever `public.t`
    // does, and keeps one place deciding what a schema qualifier means.
    let (name, column) = if object == plan::CommentObject::Column {
        let mut parts = name.0.clone();
        let last = parts.pop().ok_or_else(|| {
            SqlError::unsupported("COMMENT ON COLUMN without a column".to_owned())
        })?;
        let column = last
            .as_ident()
            .map(ident)
            .ok_or_else(|| SqlError::unsupported(format!("the name {name}")))?;
        (relation_name(&ObjectName(parts))?, Some(column))
    } else {
        (relation_name(name)?, None)
    };
    Ok(plan::Statement::Comment(plan::Comment {
        object,
        name,
        column,
        comment: comment.map(str::to_owned),
    }))
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
