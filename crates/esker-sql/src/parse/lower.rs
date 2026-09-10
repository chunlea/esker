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
    TableObject, TimezoneInfo, TrimWhereField, UnaryOperator, UtilityOption, Value,
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
    /// The options an `EXPLAIN` was written with, **without lowering the statement inside it**.
    ///
    /// `EXPLAIN … EXECUTE p1(1)` explains a statement that lives in the session's store, so the
    /// statement and the options reach the executor separately and only the options are in this
    /// tree. `None` when this is not an `EXPLAIN` at all.
    ///
    /// # Errors
    ///
    /// The option list's own refusals — an unrecognised option or value, `FORMAT` written outside
    /// the parentheses — raised here exactly as they are when the whole statement is lowered,
    /// because it is the same reader — the private `explain_settings` below, which is the one
    /// function in this module that reads an option list.
    pub fn explain_options(&self) -> Result<Option<(bool, plan::ExplainFormat)>> {
        Ok(explain_settings(&self.statement)?.map(|settings| (settings.analyze, settings.format)))
    }

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

    /// Puts back what `sqlparser` could not hold in an `ON CONFLICT` clause: the index predicate,
    /// and the expression each target placeholder stands for
    /// (`crate::parse::strip_on_conflict_target`).
    fn apply_conflict_shim(&self, lowered: &mut plan::Statement) {
        let plan::Statement::Insert(insert) = lowered else {
            return;
        };
        let Some(on_conflict) = insert.on_conflict.as_mut() else {
            return;
        };
        if let Some(predicate) = self.conflict_predicate() {
            on_conflict.predicate = Some(predicate.to_owned());
        }
        for key in &mut on_conflict.target {
            if let plan::ConflictKey::Column(name) = key
                && let Some(expression) = self.conflict_expression(name)
            {
                *key = plan::ConflictKey::Expression(expression.to_owned());
            }
        }
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
        // The same arrangement: `crate::parse::read_cursor` read the statement and the tree is a
        // placeholder, because `sqlparser` cannot read three of the four.
        if let Some(cursor) = &self.cursor {
            return lower_cursor(cursor);
        }
        // A `RAISE` block's tree is a placeholder (`crate::parse::strip_do_raise`): there is no
        // statement it is a disguised form of, so the whole lowering is this.
        if let Some((message, severity)) = self.raised() {
            // **`RAISE EXCEPTION` is a failure, not a message about one.** It leaves here as an
            // error so that the statement fails, the transaction aborts inside a block, and the
            // client reads `P0001` — none of which a notice does.
            if *severity == crate::error::Severity::Error {
                return Err(SqlError::RaisedException(message.clone()));
            }
            return Ok(plan::Statement::Raise {
                message: message.clone(),
                severity: *severity,
            });
        }
        // `REFRESH MATERIALIZED VIEW`'s tree is a placeholder too (`crate::parse::read_refresh`):
        // `sqlparser` has no `REFRESH` statement, so the source was read directly and this is the
        // whole lowering.
        if let Some(refresh) = self.refresh() {
            return Ok(plan::Statement::RefreshMaterializedView(
                plan::RefreshMaterializedView {
                    name: fold_identifier(&refresh.name, refresh.quoted).0,
                    concurrently: refresh.concurrently,
                    with_data: refresh.with_data,
                },
            ));
        }
        if let Some(reset) = self.alter_table_reset() {
            return Ok(plan::Statement::AlterTable(lower_alter_table_reset(reset)?));
        }
        if let Some(clause) = self.alter_exclude() {
            // The parser was handed `CHECK (true)` in the clause's place, so the `ALTER` itself —
            // its table, its `IF EXISTS`, its `ONLY` — is lowered normally and only the one action
            // is swapped. There is exactly one placeholder, because the rewrite makes exactly one.
            let mut lowered = lower_statement(&self.statement, self.parameter_namespace())?;
            let plan::Statement::AlterTable(alter) = &mut lowered else {
                return Err(SqlError::unsupported("an EXCLUDE constraint"));
            };
            let exclude = crate::parse::parse_exclude_constraint(clause, &alter.name)?;
            let placeholder = alter
                .actions
                .iter()
                .position(|action| matches!(action, plan::AlterTableAction::AddCheck(_)))
                .ok_or_else(|| SqlError::unsupported("an EXCLUDE constraint"))?;
            alter.actions[placeholder] = plan::AlterTableAction::AddExclude(exclude);
            return Ok(lowered);
        }
        let mut lowered = lower_statement(&self.statement, self.parameter_namespace())?;
        // `WITH [NO] DATA` was cut off the source so the statement would parse.
        if let plan::Statement::CreateMaterializedView(create) = &mut lowered
            && let Some(with_data) = self.with_data()
        {
            create.with_data = with_data;
        }
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
        // Which generated columns were `VIRTUAL`: the rewritten tree says `STORED` for all of
        // them, so the flags are applied here, in the order the clauses appeared in the source.
        // The lowering walks a table's columns in that same order, which is what makes an index
        // into the flag list line up with a column.
        let mut virtual_flags = self.virtual_generated().iter().copied();
        let mut mark = |columns: &mut Vec<plan::Column>| {
            for column in columns {
                if column.generated.is_some() {
                    column.generated_virtual = virtual_flags.next().unwrap_or(false);
                }
            }
        };
        match &mut lowered {
            plan::Statement::CreateTable(create) => mark(&mut create.columns),
            plan::Statement::AlterTable(alter) => {
                for action in &mut alter.actions {
                    if let plan::AlterTableAction::AddColumn { column, .. } = action
                        && column.generated.is_some()
                    {
                        column.generated_virtual = virtual_flags.next().unwrap_or(false);
                    }
                }
            }
            _ => {}
        }
        // `create_enum`'s `DO` block is a guard around a `CREATE TYPE`, and the guard is the one
        // thing the rewritten source cannot carry (`crate::parse::strip_do_create_enum`).
        if let plan::Statement::CreateType(create) = &mut lowered {
            create.if_not_exists = self.is_do_guarded();
            // `NOT NULL` on a `CREATE DOMAIN` was cut out of the source so the statement would
            // parse (`crate::parse::strip_domain_not_null`).
            if let TypeKind::Domain { not_null, .. } = &mut create.kind {
                *not_null = self.domain_not_null();
            }
        }
        self.apply_conflict_shim(&mut lowered);
        if let plan::Statement::CreateDatabase(create) = &mut lowered {
            apply_database_options(create, self.database_options())?;
        }
        if let Some(written) = self.alter_column_collation() {
            apply_alter_collation(&mut lowered, written)?;
        }
        Ok(lowered)
    }
}

/// The `COLLATE` an `ALTER COLUMN … TYPE` named, which came off the source so the statement would
/// parse (`crate::parse::strip_alter_column_collation`).
///
/// Decided here and not in the strip, so that a name this node does not have is the same `42704` a
/// `CREATE TABLE` gives it and a type with no ordering the same `42804` — one rule for the clause,
/// wherever it is written.
fn apply_alter_collation(lowered: &mut plan::Statement, written: &str) -> Result<()> {
    let plan::Statement::AlterTable(alter) = lowered else {
        return Ok(());
    };
    for action in &mut alter.actions {
        if let plan::AlterTableAction::SetColumnType { ty, collation, .. } = action {
            *collation = Some(collation_text(written, *ty)?);
        }
    }
    Ok(())
}

/// `ROW(a, b, …)` as the record literal it becomes.
///
/// **Constants only, and the refusal says so.** A field whose value is not known until there is a
/// row would make this a runtime constructor — the shape `ARRAY[…]` needed — and nothing in the
/// suite writes one: `composite_test.rb` sends string literals and its custom type sends the text
/// form directly. Refused by name rather than half-built.
fn lower_row_constructor(args: &[FunctionArg]) -> Result<plan::Expr> {
    let mut fields = Vec::with_capacity(args.len());
    for arg in args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
            return Err(SqlError::unsupported("ROW with a named argument"));
        };
        // Each field by its own output function, which for a constant is the text it was written
        // as — the same rule the composite's renderer applies to every value.
        fields.push(match lower_expr(expr)? {
            plan::Expr::Literal(plan::Literal::Null) => None,
            plan::Expr::Literal(plan::Literal::String(text)) => Some(text),
            plan::Expr::Literal(plan::Literal::Integer(value)) => Some(value.to_string()),
            plan::Expr::Literal(plan::Literal::Decimal(digits)) => Some(digits),
            plan::Expr::Literal(plan::Literal::Bool(value)) => {
                Some(if value { "t" } else { "f" }.to_owned())
            }
            plan::Expr::Literal(plan::Literal::Typed(value)) => value.to_text(),
            _ => return Err(SqlError::unsupported("ROW over anything but constants")),
        });
    }
    Ok(plan::Expr::Literal(plan::Literal::String(
        value::composite::render(&fields),
    )))
}

/// The collation a `COLLATE` names, if this node has it.
///
/// `C` and `POSIX` are the same ordering under two names — byte order, which a memcomparable key
/// already gives — and they are the only two
/// ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)). Anything
/// else is `42704` with PostgreSQL's own sentence, because accepting the name and sorting by bytes
/// anyway would answer a question the client did not ask.
fn collation_name(name: &ObjectName) -> Result<String> {
    let written = name.0.last().map_or_else(String::new, ToString::to_string);
    collation(written.trim_matches('"'))
}

/// [`collation_name`] for a name that arrived as text rather than as a parsed one — the clause
/// [`crate::parse::strip_alter_column_collation`] cut out. **One rule, asked twice**: the two
/// spellings of `COLLATE` in this grammar must not be able to disagree about which names exist.
fn collation(written: &str) -> Result<String> {
    if written.eq_ignore_ascii_case("C") || written.eq_ignore_ascii_case("POSIX") {
        return Ok(written.to_uppercase());
    }
    Err(SqlError::UndefinedCollation(written.to_owned()))
}

/// [`collation`] and then the type, which is [`column_collation`] for the same text spelling.
fn collation_text(written: &str, ty: ColumnType) -> Result<String> {
    let name = collation(written)?;
    if catalog::pg_attribute::collatable(ty) {
        Ok(name)
    } else {
        Err(SqlError::CollationNotSupported(ty.name()))
    }
}

/// [`collation_name`], and then the type it was written on.
///
/// **Two different sqlstates, and the order between them is measured.** A name this node does not
/// have is `42704` whatever it sits on; a name it does have on a type with no ordering to override
/// is `42804`. PostgreSQL checks the name first — `a int COLLATE "nope"` is `42704`, not `42804` —
/// so this does too, by asking `collation_name` before it asks the type.
fn column_collation(name: &ObjectName, ty: ColumnType) -> Result<String> {
    let collation = collation_name(name)?;
    if catalog::pg_attribute::collatable(ty) {
        Ok(collation)
    } else {
        Err(SqlError::CollationNotSupported(ty.name()))
    }
}

/// The type a literal has on its own, for the questions that can be asked before a scope exists.
///
/// `None` is PostgreSQL's `unknown`: a quoted string takes the type of whatever it is used with,
/// and `NULL` has none at all — so neither can be told it is not collatable, and PostgreSQL agrees
/// (`SELECT 'x' COLLATE "C"` is accepted, measured).
fn literal_type(literal: &plan::Literal) -> Option<ColumnType> {
    match literal {
        plan::Literal::Integer(value) if i32::try_from(*value).is_ok() => Some(ColumnType::Int4),
        plan::Literal::Integer(_) => Some(ColumnType::Int8),
        plan::Literal::Decimal(_) => Some(ColumnType::Numeric),
        plan::Literal::Bool(_) => Some(ColumnType::Bool),
        plan::Literal::TypedNull(ty) => Some(*ty),
        plan::Literal::Typed(value) => value.column_type(),
        plan::Literal::Null | plan::Literal::String(_) => None,
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
            // `C` and `POSIX` are one ordering under two names (ADR 0076), so a database asking
            // for either is asking for what this node already is.
            "LC_COLLATE" | "LC_CTYPE" | "LOCALE" => {
                if !value.eq_ignore_ascii_case(COLLATION) && !value.eq_ignore_ascii_case("POSIX") {
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
fn lower_statement(
    statement: &Statement,
    parameter_namespace: Option<&str>,
) -> Result<plan::Statement> {
    match statement {
        Statement::CreateTable(create) => {
            // **`CREATE TABLE … AS <query>` is a different statement, not a clause.** It has no
            // column declarations to lower — its shape is the query's — so it branches here
            // rather than inside `lower_create_table`, which is entirely about declarations.
            if let Some(query) = &create.query {
                refuse_create_table_clauses(create)?;
                refuse_if(create.like.is_some(), "CREATE TABLE ... AS with LIKE")?;
                return Ok(plan::Statement::CreateTableAs(plan::CreateTableAs {
                    name: relation_name(&create.name)?,
                    columns: create.columns.iter().map(|c| ident(&c.name)).collect(),
                    // Rendered back through the parser, for the reason the matview's is
                    // (`lower_create_view`): text this node re-reads must be text it can parse.
                    definition: query.to_string(),
                    if_not_exists: create.if_not_exists,
                }));
            }
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
        Statement::AlterTable(alter) => Ok(plan::Statement::AlterTable(lower_alter_table(
            alter,
            parameter_namespace,
        )?)),
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
            // **`SCHEMA` is carried now**; the comment here used to say "this node has one schema"
            // and that stopped being true when schemas landed. `pg_extension.extnamespace` reads
            // it back, which is the whole of what `ActiveRecord#extensions` needs.
            //
            // `VERSION` still is not: this build offers exactly one version per extension
            // (`catalog::pg_catalog::AVAILABLE_EXTENSIONS`), so honouring the word while ignoring
            // the number would answer a question the user did not ask. `CASCADE` installs an
            // extension's own dependencies, of which there are none here.
            refuse_if(create.version.is_some(), "CREATE EXTENSION ... VERSION")?;
            refuse_if(create.cascade, "CREATE EXTENSION ... CASCADE")?;
            Ok(plan::Statement::CreateExtension(plan::CreateExtension {
                name: create.name.value.clone(),
                if_not_exists: create.if_not_exists,
                // A schema name is an identifier and its case is its own, the way every other
                // qualifier in this lowering is taken.
                schema: create.schema.as_ref().map(|name| name.value.clone()),
            }))
        }
        // `ALTER INDEX <name> RENAME TO <name>`, the one form `ActiveRecord` sends. `sqlparser`
        // reads no other `ALTER INDEX` operation, so the rest reach `parse`'s refusal table.
        Statement::AlterIndex { name, operation } => {
            let sqlparser::ast::AlterIndexOperation::RenameIndex { index_name } = operation;
            Ok(plan::Statement::AlterIndexRename(plan::AlterIndexRename {
                name: relation_name(name)?,
                to: relation_name(index_name)?,
                // `sqlparser` 0.62.0 has nowhere to put `IF EXISTS` on this statement, so a client
                // that writes it gets a syntax error before this — a C1 gap recorded in the
                // corpus rather than a flag that is always false.
                if_exists: false,
            }))
        }
        // `DROP EXTENSION [IF EXISTS] name [CASCADE|RESTRICT]` — the suite's teardown, always in
        // the `IF EXISTS` form (`postgresql_adapter.rb:503`).
        //
        // **One name per statement.** `sqlparser` takes a list because PostgreSQL's grammar does,
        // and a list is refused rather than half-run: dropping the second of two would leave the
        // first gone and the statement failed, which is the shape a client cannot undo.
        Statement::DropExtension(drop) => {
            let [name] = drop.names.as_slice() else {
                return Err(SqlError::unsupported(
                    "DROP EXTENSION of more than one extension",
                ));
            };
            Ok(plan::Statement::DropExtension(plan::DropExtension {
                name: name.value.clone(),
                if_exists: drop.if_exists,
                cascade: matches!(
                    drop.cascade_or_restrict,
                    Some(sqlparser::ast::ReferentialAction::Cascade)
                ),
            }))
        }
        // `CREATE SCHEMA [IF NOT EXISTS] name`. **The suite writes the nested form**
        // (`CREATE SCHEMA s CREATE TABLE t (…)`) which `sqlparser` 0.62.0 cannot read at all — a
        // C1 gap in the plan's register, and the reason `schema_test.rb` is still out of reach.
        Statement::CreateRole(create) => {
            let [name] = create.names.as_slice() else {
                // PostgreSQL takes one name; `sqlparser` models a list.
                return Err(SqlError::unsupported("CREATE ROLE with more than one name"));
            };
            Ok(plan::Statement::CreateRole(plan::CreateRole {
                name: object_name(name)?,
                // `CREATE USER` was rewritten to `CREATE ROLE … LOGIN` before the parser saw it,
                // so `Some(true)` here is that rewrite arriving — and a bare `CREATE ROLE` is
                // `None`, which is `NOLOGIN`, which is what a real server does.
                flags: catalog::RoleFlags {
                    // `CREATE USER` was rewritten to `CREATE ROLE … LOGIN` before the parser saw
                    // it, so `Some(true)` here is that rewrite arriving; a bare `CREATE ROLE` is
                    // `None`, which is `NOLOGIN`, which is what a real server does.
                    login: create.login.unwrap_or(false),
                    superuser: create.superuser.unwrap_or(false),
                    create_db: create.create_db.unwrap_or(false),
                    create_role: create.create_role.unwrap_or(false),
                },
                if_not_exists: create.if_not_exists,
            }))
        }
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
            // **`AUTHORIZATION u` with no schema name creates a schema *named* `u`.** The name is
            // derived rather than optional, measured against PG19 — and it is the shape
            // `schema_authorization_test.rb` sends, which is why this arm exists at all. Its
            // refusal used to read "there are no roles here"; there are now.
            let (name, owner) = match schema_name {
                SchemaName::Simple(name) => (object_name(name)?, None),
                SchemaName::UnnamedAuthorization(owner) => (ident(owner), Some(ident(owner))),
                SchemaName::NamedAuthorization(name, owner) => {
                    (object_name(name)?, Some(ident(owner)))
                }
            };
            Ok(plan::Statement::CreateSchema(plan::CreateSchema {
                name,
                owner,
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
        // `CREATE [OR REPLACE] VIEW name [(cols)] AS SELECT …`.
        Statement::CreateView(create) => {
            refuse_if(create.temporary, "CREATE TEMPORARY VIEW")?;
            refuse_if(create.or_alter, "CREATE OR ALTER VIEW")?;
            refuse_if(create.secure, "CREATE SECURE VIEW")?;
            refuse_if(
                create.if_not_exists && !create.materialized,
                "CREATE VIEW IF NOT EXISTS",
            )?;
            refuse_if(create.with_no_schema_binding, "WITH NO SCHEMA BINDING")?;
            refuse_if(create.to.is_some(), "CREATE VIEW ... TO")?;
            refuse_if(create.params.is_some(), "CREATE VIEW with view parameters")?;
            refuse_if(!create.cluster_by.is_empty(), "CREATE VIEW ... CLUSTER BY")?;
            // A column list on the view names its columns; a *type* on one is not PostgreSQL's
            // grammar, and an option list on a column is nobody's.
            for column in &create.columns {
                refuse_if(column.data_type.is_some(), "a type on a view column")?;
                refuse_if(column.options.is_some(), "an option on a view column")?;
            }
            // **Rendered back rather than kept verbatim**, because the parser is what this node
            // re-reads it with: a definition that round-trips through `sqlparser`'s own rendering
            // is one it can certainly parse again, where the user's text may carry comments and
            // line breaks that no catalog needs.
            let definition = create.query.to_string();
            let columns: Vec<String> = create.columns.iter().map(|c| ident(&c.name)).collect();
            if create.materialized {
                // `OR REPLACE` has no meaning for one: PostgreSQL's grammar does not have it,
                // because replacing a relation that holds rows is not a rename of a definition.
                refuse_if(create.or_replace, "CREATE OR REPLACE MATERIALIZED VIEW")?;
                return Ok(plan::Statement::CreateMaterializedView(
                    plan::CreateMaterializedView {
                        name: object_name(&create.name)?,
                        columns,
                        definition,
                        // Filled from `Parsed::with_data` where the clause was cut out of the
                        // source; with no clause at all PostgreSQL's default is `WITH DATA`.
                        with_data: true,
                        if_not_exists: create.if_not_exists,
                    },
                ));
            }
            Ok(plan::Statement::CreateView(plan::CreateView {
                name: object_name(&create.name)?,
                columns,
                definition,
                or_replace: create.or_replace,
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
                // `CASCADE` was refused here rather than ignored, on the argument that "nothing
                // depends on a view yet, but a clause that silently did nothing would be a promise
                // broken the day something does". A view can now depend on a view, so the day
                // came and the clause does what it says.
                ObjectType::View => plan::Statement::DropView(plan::DropView {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                ObjectType::MaterializedView => {
                    plan::Statement::DropMaterializedView(plan::DropMaterializedView {
                        names,
                        if_exists: *if_exists,
                        cascade: *cascade,
                    })
                }
                ObjectType::Schema => plan::Statement::DropSchema(plan::DropSchema {
                    names,
                    if_exists: *if_exists,
                    cascade: *cascade,
                }),
                // **Neither `CASCADE` nor `RESTRICT` on a role**, as on a real server: what would
                // depend on one is the objects it owns, and this node records no ownership to
                // cascade through. `DROP USER` arrives here as `DROP ROLE`
                // (`crate::parse::rewrite_user_as_role`).
                ObjectType::Role => {
                    refuse_if(*cascade, "DROP ROLE ... CASCADE")?;
                    plan::Statement::DropRole(plan::DropRole {
                        names,
                        if_exists: *if_exists,
                    })
                }
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
            statement: explained,
            ..
        } => {
            let settings = explain_settings(statement)?.unwrap_or_default();
            // A nested statement carries no storage parameter of its own.
            let inner = lower_statement(explained, None)?;
            // **`ANALYZE` runs the statement**, which is what the word means on a real server. So
            // it is executed for a `SELECT`, where the point of it is the `ScanStats` a columnar
            // answer carries (ADR 0022 milestone 4), and stays `0A000` for everything else — an
            // `EXPLAIN ANALYZE INSERT` that ran would be an insert.
            refuse_if(
                settings.analyze && !matches!(inner, plan::Statement::Select(_)),
                EXPLAIN_ANALYZE_NOT_A_SELECT,
            )?;
            Ok(plan::Statement::Explain(Box::new(plan::Explain {
                statement: Box::new(inner),
                analyze: settings.analyze,
                format: settings.format,
            })))
        }
        // **A domain lowers into a `CREATE TYPE`**, because that is what it is: a fourth
        // `TypeKind` beside the range, the composite and the enum
        // ([ADR 0065](../../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)).
        // Everything downstream — the record, `pg_type`, the drop, the dependency check — is the
        // one path all four already take.
        // `ALTER TYPE` — the three shapes `rename_enum`, `add_enum_value` and
        // `rename_enum_value` send, and no others.
        Statement::AlterType(alter) => {
            use sqlparser::ast::{AlterTypeAddValuePosition, AlterTypeOperation};
            let action = match &alter.operation {
                AlterTypeOperation::Rename(rename) => {
                    plan::AlterTypeAction::RenameTo(ident(&rename.new_name))
                }
                AlterTypeOperation::AddValue(add) => plan::AlterTypeAction::AddValue {
                    // **A label, not an identifier**: it is written in single quotes and its case
                    // is its own, so it is taken verbatim the way an enum's labels are at
                    // `CREATE TYPE`.
                    label: add.value.value.clone(),
                    if_not_exists: add.if_not_exists,
                    position: match &add.position {
                        Some(AlterTypeAddValuePosition::Before(other)) => {
                            Some(plan::AddValuePosition::Before(other.value.clone()))
                        }
                        Some(AlterTypeAddValuePosition::After(other)) => {
                            Some(plan::AddValuePosition::After(other.value.clone()))
                        }
                        None => None,
                    },
                },
                AlterTypeOperation::RenameValue(rename) => plan::AlterTypeAction::RenameValue {
                    from: rename.from.value.clone(),
                    to: rename.to.value.clone(),
                },
            };
            Ok(plan::Statement::AlterType(plan::AlterType {
                name: relation_name(&alter.name)?,
                action,
            }))
        }
        Statement::CreateDomain(create) => {
            refuse_if(create.collation.is_some(), "CREATE DOMAIN ... COLLATE")?;
            let (base, typmod) = lower_type(&create.data_type)?;
            let mut check = None;
            for constraint in &create.constraints {
                match constraint {
                    TableConstraint::Check(clause) => {
                        // Kept as **text**, the same trade a table's `CHECK` and a view's body
                        // make: it is re-read where it is evaluated, and a tree here would put a
                        // `sqlparser` type in `plan`.
                        check = Some(unwrap_nested(&clause.expr).to_string());
                    }
                    other => {
                        return Err(SqlError::unsupported(format!(
                            "CREATE DOMAIN with the constraint {other}"
                        )));
                    }
                }
            }
            Ok(plan::Statement::CreateType(plan::CreateType {
                // **A domain takes a schema**, which is the shape `schema_test.rb` needs: it
                // creates `schema_1.text`, a domain whose bare name is a built-in type's, and
                // resolves it through the `search_path`. So the name is stored qualified the way
                // a relation's is, and `relation_name` is the one reader of that grammar.
                name: relation_name(&create.name)?,
                kind: TypeKind::Domain {
                    base,
                    typmod,
                    // `NOT NULL` is cut out of the source, because `sqlparser` 0.62.0 stops at the
                    // keyword — the fact travels on `Parsed` and is applied in `lower_inline`.
                    not_null: false,
                    default: create.default.as_ref().map(ToString::to_string),
                    check,
                },
                if_not_exists: false,
            }))
        }
        Statement::DropDomain(drop) => Ok(plan::Statement::DropType(plan::DropType {
            names: vec![relation_name(&drop.name)?],
            if_exists: drop.if_exists,
            cascade: matches!(
                drop.drop_behavior,
                Some(sqlparser::ast::DropBehavior::Cascade)
            ),
        })),
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
        // `DISCARD ALL` and its three narrower spellings. **Not refused inside a transaction
        // here** — the session is what knows whether one is open, and it raises `25001` there,
        // beside the same check `CREATE DATABASE` gets.
        Statement::Discard { object_type } => {
            use sqlparser::ast::DiscardObject;

            let target = match object_type {
                DiscardObject::ALL => plan::DiscardTarget::All,
                DiscardObject::PLANS => plan::DiscardTarget::Plans,
                DiscardObject::SEQUENCES => plan::DiscardTarget::Sequences,
                DiscardObject::TEMP => plan::DiscardTarget::Temp,
            };
            Ok(plan::Statement::Session(plan::SessionStatement::Discard(
                target,
            )))
        }
        Statement::ShowVariable { variable } => lower_show(variable),
        Statement::Reset(reset) => lower_reset(reset),
        // `TRUNCATE [TABLE] t [, …]`. `ONLY`, a partition list and `ON CLUSTER` are refused by
        // name: each narrows *what* is emptied, and emptying more than was asked is the wrong
        // answer in the one direction that cannot be undone.
        Statement::Truncate(truncate) => {
            refuse_if(truncate.partitions.is_some(), "TRUNCATE ... PARTITION")?;
            refuse_if(truncate.on_cluster.is_some(), "TRUNCATE ... ON CLUSTER")?;
            refuse_if(truncate.if_exists, "TRUNCATE ... IF EXISTS")?;
            for target in &truncate.table_names {
                refuse_if(target.only, "TRUNCATE ONLY")?;
            }
            Ok(plan::Statement::Truncate(plan::Truncate {
                names: truncate
                    .table_names
                    .iter()
                    .map(|target| relation_name(&target.name))
                    .collect::<Result<Vec<_>>>()?,
                restart_identity: matches!(
                    truncate.identity,
                    Some(sqlparser::ast::TruncateIdentityOption::Restart)
                ),
                cascade: matches!(
                    truncate.cascade,
                    Some(sqlparser::ast::CascadeOption::Cascade)
                ),
            }))
        }
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

/// The isolation level a `SET TRANSACTION` names, or `None` for one that names none.
fn named_isolation(modes: &[sqlparser::ast::TransactionMode]) -> Option<&'static str> {
    use sqlparser::ast::{TransactionIsolationLevel, TransactionMode};

    modes.iter().find_map(|mode| match mode {
        TransactionMode::IsolationLevel(TransactionIsolationLevel::RepeatableRead) => {
            Some("repeatable read")
        }
        TransactionMode::IsolationLevel(TransactionIsolationLevel::Serializable) => {
            Some("serializable")
        }
        // `READ UNCOMMITTED` is `READ COMMITTED` on a real server: there is no weaker level.
        TransactionMode::IsolationLevel(_) => Some("read committed"),
        TransactionMode::AccessMode(_) => None,
    })
}

/// `SET TRANSACTION ISOLATION LEVEL x` and `SET SESSION CHARACTERISTICS AS TRANSACTION …`.
///
/// **One value under four spellings** (ADR 0057), so both become a `SET` of the parameter that
/// holds it — `SHOW transaction_isolation` then answers without a rule of its own, and the
/// transaction-scoped one goes back to the session's default when the block ends. The `SESSION`
/// form is the one that changes the default itself.
fn lower_set_isolation(
    modes: &[sqlparser::ast::TransactionMode],
    session: bool,
) -> plan::Statement {
    let value = named_isolation(modes).unwrap_or("read committed");
    let name = if session {
        crate::parameter::default_transaction_isolation().name
    } else {
        crate::parameter::transaction_isolation().name
    };
    plan::Statement::Session(plan::SessionStatement::SetParameter {
        name: name.to_owned(),
        value: Some(value.to_owned()),
    })
}

/// `SET`, of which this node executes two spellings and refuses the rest by name.
///
/// The two are PostgreSQL's own (`docs/plans/phase-6d.md` §1): a namespaced custom GUC, which a
/// real server accepts and stores, and `SET TRANSACTION SNAPSHOT`, which a real server *acts* on
/// and whose every precondition is one this feature wants anyway.
fn lower_set(set: &sqlparser::ast::Set) -> Result<plan::Statement> {
    use sqlparser::ast::{ContextModifier, Set, SetSessionAuthorizationParamKind};

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
            // Every item, list or not: `$user` is a syntax error wherever it appears unquoted.
            refuse_placeholders(values)?;
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
        // **`SET TRANSACTION ISOLATION LEVEL x`, and its `SESSION CHARACTERISTICS` form.**
        Set::SetTransaction {
            modes,
            snapshot: None,
            session,
        } if named_isolation(modes).is_some() => Ok(lower_set_isolation(modes, *session)),
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
        // **`SET SESSION AUTHORIZATION` on a node with no roles**, which is the 12 refusals behind
        // run 57's aborted-transaction row — every one of them in `schema_authorization_test.rb`,
        // whose `set_session_auth` sends `DEFAULT` between each named user.
        //
        // **The name is carried, not judged.** It used to be refused here, unconditionally, with
        // `22023 role "x" does not exist` — and the comment that stood here said that was "true of
        // every name", which it was, on a node with no roles. Roles landed and it became a lie
        // that no test could see: the session that had just created a role was told it does not
        // exist, because the refusal is upstream of the catalog and never asks it (run 87).
        //
        // `22023` is still the answer for a name that is not a role, and still not the `42704` an
        // undefined *object* gets — PostgreSQL reads an authorization name as a parameter value.
        // Measured. What changed is who decides: `crate::exec` does, where the catalog is.
        Set::SetSessionAuthorization(param) => match &param.kind {
            SetSessionAuthorizationParamKind::Default => Ok(plan::Statement::Session(
                plan::SessionStatement::SetSessionAuthorization {
                    name: None,
                    local: matches!(param.scope, ContextModifier::Local),
                },
            )),
            SetSessionAuthorizationParamKind::User(name) => Ok(plan::Statement::Session(
                plan::SessionStatement::SetSessionAuthorization {
                    name: Some(ident(name)),
                    local: matches!(param.scope, ContextModifier::Local),
                },
            )),
        },
        other => Err(SqlError::unsupported(set_feature_name(other))),
    }
}

/// `SHOW <parameter>`. Only the one this node has; every other name is PostgreSQL's `42704`.
///
/// A `SHOW` of an unknown parameter is *not* contract C2's `0A000`: the statement is one this node
/// executes, and what is missing is the parameter, which is the condition PostgreSQL reports.
fn lower_show(variable: &[Ident]) -> Result<plan::Statement> {
    // **The names PostgreSQL spells with spaces come first**, because after the join below they
    // are indistinguishable from a namespaced one: `SHOW TIME ZONE` and `SHOW esker.read_as_of`
    // are both two idents and the AST does not record which separator was written.
    if let Some(statement) = lower_multi_word_show(variable) {
        return Ok(statement);
    }
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

/// A cursor statement, from what [`crate::parse::read_cursor`] read out of the source.
///
/// The `DECLARE`'s query travels as **text** and is parsed here, through the one parser every
/// other query goes through — the same arrangement `PREPARE`'s body uses, and for the same
/// reason: the hand-written reader knows the statement's shape and nothing about expressions.
fn lower_cursor(read: &crate::parse::CursorRead) -> Result<plan::Statement> {
    use crate::parse::CursorRead;

    let cursor = match read {
        CursorRead::Declare {
            name,
            quoted,
            hold,
            query,
        } => {
            // **Refused by name.** A holdable cursor outlives the transaction that made it, which
            // means keeping its snapshot alive past the commit — there is nothing here that does
            // that, and answering with a cursor that quietly saw newer rows would be worse than
            // saying so.
            if *hold {
                return Err(SqlError::unsupported("DECLARE ... WITH HOLD"));
            }
            let mut statements = crate::parse::parse_statements(query)?;
            let [_] = statements.as_slice() else {
                return Err(SqlError::unsupported(
                    "DECLARE CURSOR FOR more than one statement",
                ));
            };
            let plan::Statement::Select(select) = statements.remove(0).lower()? else {
                return Err(SqlError::unsupported(
                    "DECLARE CURSOR FOR a statement that is not a query",
                ));
            };
            plan::CursorStatement::Declare {
                name: fold_identifier(name, *quoted).0,
                query: select,
            }
        }
        CursorRead::Fetch {
            name,
            quoted,
            direction,
            only_move,
        } => plan::CursorStatement::Fetch {
            name: fold_identifier(name, *quoted).0,
            direction: *direction,
            only_move: *only_move,
        },
        CursorRead::Close(named) => plan::CursorStatement::Close(
            named
                .as_ref()
                .map(|(name, quoted)| fold_identifier(name, *quoted).0),
        ),
        // A count past an `int4` is the grammar's syntax error, at the digits: `42601`, measured.
        CursorRead::Malformed { at } => return Err(SqlError::SyntaxAtOrNear(at.clone())),
    };
    Ok(plan::Statement::Cursor(cursor))
}

/// What an `EXPLAIN ANALYZE` of anything but a `SELECT` is refused with.
///
/// One spelling, because `EXPLAIN … EXECUTE` makes the same refusal from the executor — the
/// statement it explains is substituted after this lowering has run, so the check happens twice
/// and must not say two things.
pub(crate) const EXPLAIN_ANALYZE_NOT_A_SELECT: &str =
    "EXPLAIN ANALYZE of a statement that is not a SELECT";

/// The option list an `EXPLAIN` carries, or `None` when the statement is not one.
///
/// **Shared by the lowering and by [`Parsed::explain_options`]**, because the statement *inside* an
/// `EXPLAIN` is not always lowerable where the options are wanted: `EXPLAIN … EXECUTE p1(1)`
/// explains a statement the session holds and the executor cannot see, so the two halves arrive
/// separately and only the options are in this tree. Two readers of one option list is how one of
/// them comes to accept a spelling the other refuses.
fn explain_settings(statement: &Statement) -> Result<Option<ExplainOptions>> {
    let Statement::Explain {
        describe_alias,
        analyze,
        verbose,
        query_plan,
        estimate,
        format,
        options,
        ..
    } = statement
    else {
        return Ok(None);
    };
    refuse_if(*query_plan, "EXPLAIN QUERY PLAN")?;
    refuse_if(*estimate, "EXPLAIN ESTIMATE")?;
    let _ = describe_alias;
    // **The two spellings are one vocabulary.** `EXPLAIN ANALYZE VERBOSE` and
    // `EXPLAIN (ANALYZE, VERBOSE)` mean the same thing on a real server, so the legacy keywords are
    // folded into the option list rather than handled beside it — and `FORMAT` outside parentheses
    // is a syntax error there, which is why only the parenthesised list can carry one (measured,
    // `EXPLAIN FORMAT JSON SELECT 1`). `VERBOSE` is accepted and ignored here as it is inside the
    // parentheses, so the legacy keyword needs no field of its own.
    let _ = verbose;
    let mut settings = ExplainOptions {
        analyze: *analyze,
        ..ExplainOptions::default()
    };
    // **`FORMAT` outside the parentheses is a syntax error on a real server**, and `sqlparser`
    // parses it only for the dialects where it is not. Answering a plan here would be answering
    // where PostgreSQL raises, which ADR 0031 calls the worst class of divergence — so it is
    // refused in PostgreSQL's own words. Measured: `EXPLAIN FORMAT JSON SELECT 1` and
    // `EXPLAIN ANALYZE FORMAT JSON SELECT 1`.
    if format.is_some() {
        return Err(SqlError::SyntaxAtOrNear("FORMAT".to_owned()));
    }
    for option in options.iter().flatten() {
        settings.set(option)?;
    }
    settings.validate()?;
    Ok(Some(settings))
}

/// The three parameter names PostgreSQL's grammar spells with spaces, and no others.
///
/// **A closed set copied from the grammar, not a rule inferred from a shape.** Measured on
/// PostgreSQL 19: `SHOW TIME ZONE`, `SHOW TRANSACTION ISOLATION LEVEL` and
/// `SHOW SESSION AUTHORIZATION` are three productions of their own, while `SHOW TRANSACTION READ
/// ONLY` and `SHOW NOSUCH THING` are `42601` — so "two words means spaces" is not a rule a real
/// server has, and reading one into the parser would accept statements PostgreSQL refuses.
///
/// Each answers under PostgreSQL's own spelling of the parameter rather than the user's, which is
/// what makes `SHOW TIME ZONE` and `SHOW timezone` name the same column.
fn lower_multi_word_show(variable: &[Ident]) -> Option<plan::Statement> {
    let words: Vec<String> = variable
        .iter()
        .map(|ident| ident.value.to_ascii_uppercase())
        .collect();
    let name = match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["TIME", "ZONE"] => "timezone",
        ["TRANSACTION", "ISOLATION", "LEVEL"] => "transaction_isolation",
        // **`sqlparser` eats the `SESSION` keyword and drops it**, so this arrives as the single
        // ident `AUTHORIZATION` and is indistinguishable from a bare `SHOW AUTHORIZATION`. That
        // one is `42601` on a real server, so taking the word accepts a statement PostgreSQL
        // refuses — an over-acceptance, which is the direction contract C1 leaves open, and the
        // alternative is refusing the form the suite actually sends.
        ["AUTHORIZATION" | "SESSION_AUTHORIZATION"] => {
            return Some(plan::Statement::Session(
                plan::SessionStatement::ShowSessionAuthorization,
            ));
        }
        _ => return None,
    };
    Some(plan::Statement::Session(
        plan::SessionStatement::ShowParameter(name.to_owned()),
    ))
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
/// Refuses a `SET` value that is a bare `$name`, the way a real server's parser does.
///
/// `$` outside a string starts a parameter, so `SET search_path = $user,public` never reaches a
/// GUC at all on PostgreSQL — it is `syntax error at or near "$"`, and the working spelling is
/// `'$user'`. `sqlparser` hands it to us as a placeholder instead of refusing it, so this is where
/// the difference is made. See [`SqlError::SetValueSyntax`] for the capture.
fn refuse_placeholders(values: &[Expr]) -> Result<()> {
    for value in values {
        if let Expr::Value(literal) = value
            && let Value::Placeholder(_) = &literal.value
        {
            return Err(SqlError::SetValueSyntax("$".to_owned()));
        }
    }
    Ok(())
}

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

/// `ALTER TABLE … RESET (…)`, which is accepted for every name there is.
///
/// A real server validates nothing here — not the parameter, not even its namespace (measured; the
/// matching `SET` refuses both) — so this maps names to actions and refuses none of them. Only one
/// name has anywhere to be forgotten from, and `columnar_replicas` is Esker's own
/// ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5).
///
/// **`ONLY` is refused with the sentence it gets on every other `ALTER TABLE`.** A real server
/// takes it; this node does not implement inheritance and says so by name rather than letting the
/// refusal table name `RESET` for it.
fn lower_alter_table_reset(reset: &crate::parse::AlterTableReset) -> Result<plan::AlterTable> {
    refuse_if(reset.only, "ALTER TABLE ONLY")?;
    Ok(plan::AlterTable {
        name: fold_identifier(&reset.name, reset.quoted).0,
        if_exists: reset.if_exists,
        actions: reset
            .parameters
            .iter()
            .map(|parameter| {
                if parameter == "columnar_replicas" {
                    // The one that forgets something. Zero and absent are different histories to a
                    // placement driver, so this deletes the record rather than storing a zero.
                    plan::AlterTableAction::SetColumnarReplicas { replicas: None }
                } else if parameter == "retention" {
                    plan::AlterTableAction::SetRetention { retention_ms: None }
                } else {
                    plan::AlterTableAction::AcceptStorageParameter
                }
            })
            .collect(),
    })
}

/// `ALTER TABLE t SET (<parameter> = <value>)`, of which this node has exactly one.
///
/// PostgreSQL's storage-parameter syntax, which is where a per-table knob belongs and which needs
/// no grammar of our own. Every parameter but `retention` is `0A000` **naming the parameter**: a
/// user who wrote `fillfactor` is told about `fillfactor`, not about `ALTER TABLE`.
fn lower_storage_parameters(
    options: &[sqlparser::ast::SqlOption],
    namespace: Option<&str>,
) -> Result<plan::AlterTableAction> {
    use sqlparser::ast::SqlOption;

    // **A namespace is semantic, not syntactic.** `sqlparser` cannot read the dot, so
    // `crate::parse::strip_parameter_namespace` lifts it off and the answer is decided here, where
    // the names are known. Measured: `toast.` is accepted by a real server and `esker.` is
    // `22023 unrecognized parameter namespace "esker"` — an error about a *name*, which is why
    // answering `42601` for either would break contract C1.
    if let Some(namespace) = namespace {
        if !namespace.eq_ignore_ascii_case("toast") {
            return Err(SqlError::UnrecognizedParameterNamespace(
                namespace.to_owned(),
            ));
        }
        // `toast.<anything>` is accepted and changes nothing here: this node has no TOAST, so the
        // parameter has nowhere to land and a real server's answer is the tag either way. **Not
        // `SetColumnarReplicas { replicas: None }`** — that one is `RESET`, and it would delete
        // the table's columnar setting on the way past.
        return Ok(plan::AlterTableAction::AcceptStorageParameter);
    }

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
/// A stored `DEFAULT` under the column's typmod — **for an `interval` and for nothing else**.
///
/// Every other parameterised type stores the literal it was written with, unmodified, and only
/// meets its typmod when a row is inserted. Measured on 19beta1, one table, and the last row is
/// the proof that this is not caution:
///
/// ```text
/// a interval(3)  DEFAULT '1.23456 seconds'      'PT1.235S'::interval(3)   <- rounded
/// b time(2)      DEFAULT '01:02:03.456'         '01:02:03.456'::time without time zone
/// c timestamp(1) DEFAULT '2020-01-01 00:00:00.55'
///                        '2020-01-01 00:00:00.55'::timestamp without time zone
/// d numeric(6,2) DEFAULT 1.555                  1.555
/// e varchar(3)   DEFAULT 'abcdef'               'abcdef'::character varying
///                                               -- accepted at CREATE TABLE, and
///                                               -- `INSERT ... DEFAULT VALUES` is then 22001
/// ```
fn fit_default(value: Datum, ty: ColumnType, typmod: i32) -> Result<Datum> {
    match ty {
        ColumnType::Interval => value::fit_to_typmod(value, ty, typmod),
        _ => Ok(value),
    }
}

pub(super) fn column_default(
    expr: &Expr,
    ty: ColumnType,
    typmod: i32,
) -> Result<(Option<Datum>, Option<String>)> {
    let expr = unwrap_nested(expr);
    refuse_default_shapes(expr)?;
    // A literal, with the sign or the cast a user wrote around it: what PostgreSQL's coercion
    // folds, and nothing more. `1 + 1` is *not* folded by a real server either — it prints back as
    // `(1 + 1)`, measured — so the fold here stops exactly where the server's does.
    // **Which shapes this intercepts, and which fall through to [`lower_cast`]** — worth naming,
    // because the boundary is not where it looks and reading it wrong sends you to the wrong
    // function. Three are caught here: a bare literal, a signed one (which returns early and
    // leaves the *text* to the deparser), and a literal under **one** cast. Everything else goes
    // to lowering — including `(-1)::text`, whose inner is a `UnaryOp` rather than a `Value`, and
    // `((1)::bigint)::text`, whose inner is another cast. That is why three of the deparse
    // census's four `DEFAULT` rows were `lower_cast`'s to fix (`debts-v1.1.md` #42) and the
    // fourth, `(42)::text`, already agreed: it is the one shape of the four that lands here.
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
        // **A bit-string literal is its bits, and its stored expression names its *own* type.**
        // `bit_string_test.rb` never writes one — it declares a *string* default and
        // `ActiveRecord` renders it back out as `B'00000011'`, which is how the file meets this
        // path. Two things go wrong without this arm and the `other.to_string()` below is both:
        // the text handed to `bit`'s input function was `B'00000011'`, envelope and all, which is
        // what `"B" is not a valid binary digit` was; and the expression a real server stores is
        // `'0011'::"bit"` even on a `bit varying(4)` column, because a `B'…'` literal is a `bit`
        // and the cast to the column's type is not part of what `pg_get_expr` prints.
        let bits = match literal {
            Value::SingleQuotedByteStringLiteral(digits) => Some(value::bit::from_text(digits)?),
            Value::HexStringLiteral(digits) => Some(value::bit::from_hex(digits)?),
            _ => None,
        };
        if let Some(bits) = bits {
            let value = Datum::from_text(ty, &bits)?;
            return Ok((Some(value), Some(format!("'{bits}'::\"bit\""))));
        }
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
        let value = fit_default(Datum::from_text(ty, &text)?, ty, typmod)?;
        return Ok((
            Some(value),
            cast.map(|to| cast_default_text(&text, literal, to)),
        ));
    }
    // Everything else stays an expression. **Lowering it here is what makes a function this node
    // does not have refused when the table is created** rather than when the first row is written,
    // which is where a real server raises it — and it is also what refuses `random() * 100` by
    // naming the operator, since this node has no arithmetic at all.
    //
    // **A name the vocabulary lacks is the one thing lowering cannot decide**, because a user may
    // have declared a function of that name and only the catalog knows. So it is carried out
    // rather than refused, and `crate::exec::ddl` — which can see the catalog — refuses the ones
    // nobody declared. Everything else about the expression is still refused here.
    if let Err(error) = lower_expr(expr)
        && unsupported_function_name(&error).is_none()
    {
        return Err(error);
    }
    // **`nextval('s')` is stored as `nextval('s'::regclass)`**, which is not the text that was
    // written. PostgreSQL does not keep the text at all — it keeps a parsed node, and `nextval`
    // takes a `regclass`, so the `unknown` literal is coerced and the coercion is what
    // `pg_get_expr` deparses. Measured: a column declared
    // `DEFAULT nextval('postgresql_serials_id_seq')` reads back with the cast.
    //
    // It matters because it is what a *hand-written* default looks like to a schema dumper.
    // `serial_test.rb`'s `test_schema_dump_with_not_serial` matches on the `::regclass` form to
    // decide the column is not a serial, and a dumper handed back the text as typed does not
    // recognise it. A **serial's** own default already prints this way
    // (`catalog::pg_attribute`), from the sequence rather than from any stored text — this is the
    // other half, and now both spellings agree.
    //
    // Normalised here rather than at every reader: the matcher is `sequence_literal_name`, the
    // one `lower_set_default` uses, so there is still one reader of this grammar.
    if let Expr::Function(function) = expr
        && let Ok(name) = unqualified_function_name(function)
        && name.eq_ignore_ascii_case("nextval")
        && let Ok([argument]) = <[&Expr; 1]>::try_from(function_arguments(function, "nextval")?)
        && let Some(sequence) = sequence_literal_name(argument)
    {
        return Ok((None, Some(format!("nextval('{sequence}'::regclass)"))));
    }
    // **The same parentheses a generated column takes**, because it is the same
    // `pg_get_expr(adbin, adrelid)` printing the same `pg_attrdef` row: measured,
    // `DEFAULT (1 + 1)` prints `(1 + 1)`, `DEFAULT 7` prints `7`, `DEFAULT upper('a')` prints
    // `upper('a'::text)` and `DEFAULT (1 > 0)` prints `(1 > 0)`. The comment in
    // `catalog::pg_attribute::default_expression` has said so since the volatile-default unit —
    // "unlike a computed default such as `DEFAULT 1 + 1`, which a real server prints as
    // `(1 + 1)`" — and nothing applied it.
    Ok((None, Some(expr_shape(expr).printed(&expr.to_string()))))
}

/// The function name out of the `0A000` this module raises for one it does not have.
///
/// **Lives beside the two places that build that message**, so the two cannot drift apart
/// unnoticed: it is a reading of this module's own sentence and not of PostgreSQL's.
pub(crate) fn unsupported_function_name(error: &SqlError) -> Option<&str> {
    match error {
        SqlError::FeatureNotSupported(text) => text.strip_prefix("the function "),
        _ => None,
    }
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

/// `json` or `jsonb` when an expression is **written** as one, and `None` otherwise.
///
/// Syntax only, which is all this layer has and all the guard above needs: a cast (`x::jsonb`) or
/// a typed string (`JSONB '…'`). A `json`-typed *column* is invisible here and is caught where the
/// plan carries its type, in `crate::exec::cursor`.
fn json_cast_name(expr: &Expr) -> Option<&'static str> {
    let data_type = match unwrap_nested(expr) {
        Expr::Cast { data_type, .. } => data_type,
        Expr::TypedString(typed) => &typed.data_type,
        _ => return None,
    };
    match lower_type(data_type) {
        Ok((ColumnType::Json, _)) => Some("json"),
        Ok((ColumnType::Jsonb, _)) => Some("jsonb"),
        _ => None,
    }
}

/// The sequence a `nextval` argument names: `'s'` and `'s'::regclass` are the same thing.
fn sequence_literal_name(argument: &Expr) -> Option<String> {
    let inner = match unwrap_nested(argument) {
        Expr::Cast { expr, .. } => unwrap_nested(expr),
        other => other,
    };
    match inner {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text)
            | Value::DollarQuotedString(DollarQuotedString { value: text, .. }) => {
                Some(sequence_reference(text))
            }
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
    refuse_if(create.external, "CREATE EXTERNAL TABLE")?;
    refuse_if(create.global.is_some(), "CREATE GLOBAL/LOCAL TABLE")?;
    refuse_if(create.transient, "CREATE TRANSIENT TABLE")?;
    refuse_if(create.volatile, "CREATE VOLATILE TABLE")?;
    refuse_if(create.iceberg, "CREATE ICEBERG TABLE")?;
    refuse_if(create.like.is_some(), "CREATE TABLE ... LIKE")?;
    refuse_if(create.clone.is_some(), "CREATE TABLE ... CLONE")?;

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
        refuse_pseudo_type(ty, &column_name)?;
        let mut not_null = false;
        let mut default = None;
        let mut default_expr: Option<String> = None;
        // `bigserial` is the type saying it; `GENERATED ... AS IDENTITY` is an option saying it.
        // Both end here, because what they produce is the same record.
        let mut sequence = serial_identity(&column.data_type);
        let mut generated: Option<String> = None;
        let mut collation: Option<String> = None;
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
                        validated: true,
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
                //
                // **A user-defined type's default is folded by the executor, not here** — the
                // same guard `ALTER TABLE … ADD COLUMN` has, and this statement did not. The
                // column's `ty` is an `int2` placeholder until the catalog has been read (ADR
                // 0050), so folding `'blue'` against it hands a label to the `int2` input
                // function: `invalid input syntax for type smallint: "blue"`, which is exactly
                // what `t.enum … default: "blue"` produced. `text` keeps the label as written and
                // `exec::ddl` turns it into the ordinal, where the type's labels are known.
                ColumnOption::Default(expr) => {
                    let written = if user_type_name.is_some() {
                        ColumnType::Text
                    } else {
                        ty
                    };
                    (default, default_expr) = column_default(expr, written, typmod)?;
                }
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
                // **`COLLATE "C"` and `COLLATE "POSIX"` name the ordering this node has** —
                // byte order, which is what a memcomparable key already gives, so honouring one
                // costs nothing and ignoring it would cost the client the order it asked for
                // ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)).
                // Every other name is `42704`, the sqlstate a real server gives for a collation it
                // does not have, because this node genuinely does not have that ordering.
                ColumnOption::Collation(name) => {
                    // A user-defined type is an enum, whose stored form is a smallint (ADR 0050)
                    // and whose `ty` here is a placeholder — so ask about the *written* type, not
                    // the placeholder, and let a `COLLATE` on an enum be the `42804` it is.
                    let written = if user_type_name.is_some() {
                        ColumnType::Int2
                    } else {
                        ty
                    };
                    collation = Some(column_collation(name, written)?);
                }
                other => return Err(SqlError::unsupported(column_option_name(other))),
            }
        }
        // An identity column is `NOT NULL` whether or not it says so, here as there.
        if sequence.is_some() {
            not_null = true;
        }
        columns.push(plan::Column {
            collation,
            name: column_name,
            ty,
            user_type_name,
            typmod,
            default_expr,
            not_null,
            default,
            sequence,
            generated,
            // Applied by `Parsed::lower`, which is the only place that can see which spelling the
            // source used: the tree it lowers says `STORED` for every one of them.
            generated_virtual: false,
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

    // **`TEMPORARY` and `TEMP` are one keyword** and `sqlparser` folds them into one flag, so
    // there is nothing to tell apart here. `UNLOGGED` reaches this crate through the source
    // rewrite instead (`crate::parse::strip_unlogged`), and the two are set in different places —
    // which is what makes `CREATE TEMPORARY UNLOGGED TABLE` a *syntax error* here as it is on a
    // real server, rather than a table that is quietly one of the two.
    let persistence = if create.temporary {
        catalog::Persistence::Temporary
    } else {
        catalog::Persistence::Permanent
    };
    // **No clause is `PRESERVE ROWS`**, which is why the two share an arm: PostgreSQL's default is
    // the clause spelled out, not a fourth state.
    let on_commit = match create.on_commit {
        None | Some(sqlparser::ast::OnCommit::PreserveRows) => catalog::OnCommit::PreserveRows,
        Some(sqlparser::ast::OnCommit::DeleteRows) => catalog::OnCommit::DeleteRows,
        Some(sqlparser::ast::OnCommit::Drop) => catalog::OnCommit::Drop,
    };
    // **`ON COMMIT` is a temporary table's clause and nothing else's**, and PostgreSQL says so in
    // its own class: `42P16`, an invalid *table definition*, rather than a syntax error or a
    // refusal. Measured.
    if create.on_commit.is_some() && !create.temporary {
        return Err(SqlError::OnCommitNotTemporary);
    }

    Ok(plan::CreateTable {
        // `UNLOGGED` is overridden in `Parsed::lower`, which is where the stripped keyword is in
        // reach; `TEMPORARY` is a flag the parser does read, so it is decided here.
        persistence,
        on_commit,
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
fn lower_alter_table(
    alter: &sqlparser::ast::AlterTable,
    parameter_namespace: Option<&str>,
) -> Result<plan::AlterTable> {
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
            actions.push(lower_storage_parameters(options, parameter_namespace)?);
            continue;
        }
        if let AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } = operation
        {
            let mut lowered = lower_added_constraint(&table_name, constraint)?;
            // **A foreign key's and a check's**, which are the two PostgreSQL takes it on. What
            // it skips is the scan of the rows already there; the constraint is enforced for every
            // row written afterwards either way — measured, an `INSERT` violating a `NOT VALID`
            // check is `23514`.
            if *not_valid {
                match &mut lowered {
                    plan::AlterTableAction::AddForeignKey(key) => key.validated = false,
                    plan::AlterTableAction::AddCheck(check) => check.validated = false,
                    _ => return Err(SqlError::unsupported("ADD CONSTRAINT ... NOT VALID")),
                }
            }
            actions.push(lowered);
            continue;
        }
        if let AlterTableOperation::DisableTrigger { name }
        | AlterTableOperation::EnableTrigger { name } = operation
        {
            let disabled = matches!(operation, AlterTableOperation::DisableTrigger { .. });
            actions.push(lower_trigger_state(&table_name, name, disabled)?);
            continue;
        }
        if let AlterTableOperation::ValidateConstraint { name } = operation {
            actions.push(plan::AlterTableAction::ValidateConstraint(ident(name)));
            continue;
        }
        // `ALTER COLUMN c SET DEFAULT <expr>` and `DROP DEFAULT`. The **type is not known here** —
        // a plan is lowered without the catalog — so a literal is not folded until the executor
        // has the column, which is also where `22P02` for one the type will not take comes from.
        if let AlterTableOperation::AlterColumn { column_name, op } = operation {
            use sqlparser::ast::AlterColumnOperation;
            // `SET NOT NULL` / `DROP NOT NULL` are their own action: they change a column's
            // nullability rather than its default, and the executor has to scan for the first.
            if matches!(
                op,
                AlterColumnOperation::SetNotNull | AlterColumnOperation::DropNotNull
            ) {
                actions.push(plan::AlterTableAction::SetNotNull {
                    column: ident(column_name),
                    not_null: matches!(op, AlterColumnOperation::SetNotNull),
                });
                continue;
            }
            // `ALTER COLUMN c TYPE t [USING e]`. The `USING` is read only far enough to tell
            // "cast this column to this type" — which is all `change_column` ever writes — from
            // anything else, which is refused by name because there is no per-row evaluator.
            if let AlterColumnOperation::SetDataType {
                data_type, using, ..
            } = op
            {
                let (ty, typmod, user_type) = lower_column_type(data_type)?;
                refuse_pseudo_type(ty, &ident(column_name))?;
                if let Some(name) = user_type {
                    return Err(SqlError::unsupported(format!(
                        "ALTER TABLE ... ALTER COLUMN ... TYPE {name}"
                    )));
                }
                // **A cast of the column, or any other expression.** The first is checked before
                // a row is touched — its target type is known — and the second is evaluated per
                // row, which is what PostgreSQL does with either.
                let (using, using_expr) = match using {
                    None => (None, None),
                    Some(expr) => match using_cast_target(expr, &ident(column_name)) {
                        Some(cast_to) => (Some(lower_column_type(cast_to)?.0), None),
                        None => (None, Some(lower_expr(expr)?)),
                    },
                };
                actions.push(plan::AlterTableAction::SetColumnType {
                    column: ident(column_name),
                    ty,
                    typmod,
                    using,
                    using_expr,
                    // Filled in by `Parsed::lower`, which is where the clause the parser could not
                    // read arrives (`crate::parse::strip_alter_column_collation`).
                    collation: None,
                });
                continue;
            }
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
        if let AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } = operation
        {
            actions.push(plan::AlterTableAction::RenameColumn {
                from: ident(old_column_name),
                to: ident(new_column_name),
            });
            continue;
        }
        if let AlterTableOperation::RenameTable { table_name } = operation {
            // `AS` is MySQL's spelling of the same thing; PostgreSQL writes `TO` and that is what
            // `rename_table` sends, so the other one is named rather than quietly accepted.
            let sqlparser::ast::RenameTableNameKind::To(name) = table_name else {
                return Err(SqlError::unsupported("ALTER TABLE ... RENAME AS"));
            };
            actions.push(plan::AlterTableAction::RenameTo(relation_name(name)?));
            continue;
        }
        if let AlterTableOperation::DropConstraint {
            name,
            if_exists,
            drop_behavior,
        } = operation
        {
            actions.push(plan::AlterTableAction::DropConstraint {
                name: ident(name),
                if_exists: *if_exists,
                cascade: matches!(drop_behavior, Some(sqlparser::ast::DropBehavior::Cascade)),
            });
            continue;
        }
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
        refuse_pseudo_type(ty, &ident(&column_def.name))?;
        let mut not_null = false;
        let mut collation: Option<String> = None;
        let mut primary_key = false;
        let mut generated: Option<String> = None;
        let mut default = None;
        let mut default_expr: Option<String> = None;
        for option in &column_def.options {
            let named = match &option.option {
                // A **folded** default is stored as the column's missing value and the decoder
                // pads with it, so no row is rewritten (ADR 0019's pad rule generalised;
                // `catalog::ColumnDef::missing`).
                //
                // An **expression** default is carried through to the executor, which does
                // rewrite the rows. This was refused here on the argument that accepting it and
                // padding NULL would be a wrong answer rather than a gap — right about the
                // consequence, and the answer was to stop padding rather than to keep refusing.
                // The executor can see the rows and fills each one from the expression, which is
                // the road `ADD COLUMN … GENERATED ALWAYS AS` already took.
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
                    (default, default_expr) = column_default(expr, ty, typmod)?;
                    continue;
                }
                ColumnOption::NotNull => {
                    not_null = true;
                    continue;
                }
                // A generated column added by `ALTER`, which is what `t.virtual` sends when a
                // migration adds one. The expression is lowered the same way `CREATE TABLE`'s is;
                // the executor computes it for the rows already there.
                ColumnOption::Generated {
                    generated_as,
                    sequence_options,
                    generation_expr,
                    generation_expr_mode,
                    ..
                } => {
                    match lower_generated(
                        *generated_as,
                        sequence_options.as_deref(),
                        generation_expr.as_ref(),
                        generation_expr_mode.as_ref(),
                    )? {
                        Ok(expr) => generated = Some(expr),
                        // An identity column added by `ALTER` is its own feature: it needs a
                        // sequence and a value for every row already there.
                        Err(_) => {
                            return Err(SqlError::unsupported(
                                "ALTER TABLE ... ADD COLUMN ... GENERATED AS IDENTITY",
                            ));
                        }
                    }
                    continue;
                }
                // `PRIMARY KEY` is carried rather than refused; the executor decides, because
                // whether it can be added is a question about the rows.
                ColumnOption::PrimaryKey(_) => {
                    primary_key = true;
                    continue;
                }
                ColumnOption::Unique(_) => "ALTER TABLE ... ADD COLUMN ... UNIQUE",
                // `NULL` is the default and says nothing; honouring it is honouring nothing.
                ColumnOption::Null => continue,
                // The same clause `CREATE TABLE` takes, and it has to be here too:
                // `add_column … collation: "C"` is one of `collation_test.rb`'s five, and the
                // column it adds is read back by the same `attcollation <> typcollation`.
                ColumnOption::Collation(name) => {
                    collation = Some(column_collation(name, ty)?);
                    continue;
                }
                other => return Err(SqlError::unsupported(column_option_name(other))),
            };
            return Err(SqlError::unsupported(named));
        }
        // `NOT NULL` without a `DEFAULT` was refused here, on the argument that it "needs a value
        // for every row already stored" and there is "nothing to pad with". **That is half the
        // rule**: an *empty* table has no row to hold a NULL, so there is nothing to refuse — and
        // every test in the suite that sends this adds a column to an empty table. The question is
        // about the rows, so it is asked by the executor, which can see them.
        // **A `serial` is carried too, for the reason `NOT NULL` above it is.** It would have to
        // create a sequence *and* fill every row already stored from it, and the second half is
        // only true of a table that has rows — which the executor can see and this cannot.
        actions.push(plan::AlterTableAction::AddColumn {
            primary_key,
            column: plan::Column {
                collation,
                name: ident(&column_def.name),
                ty,
                user_type_name,
                typmod,
                // **Carried, and the executor backfills from it.** A volatile default cannot
                // be one constant in the catalog, so a row that predates the column has to be
                // written — which is a fact about the rows and therefore the executor's, like the
                // `NOT NULL` and `serial` decisions above.
                default_expr,
                not_null,
                default,
                sequence: serial_identity(&column_def.data_type),
                // `ADD COLUMN … GENERATED ALWAYS AS (…) STORED` would have to compute the
                // expression for every row already there, which is a backfill and not a catalog
                // write — refused by name with every other option this action does not take.
                generated,
                generated_virtual: false,
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
                    validated: true,
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

/// The advisory-lock function of that name, or `None`.
///
/// Matched on the whole name rather than a prefix, because the blocking and the `try` families are
/// two behaviours and not two spellings: reading `pg_advisory_lock` as a `try` would answer a wait
/// with an immediate failure, which is the worse kind of wrong — a migrator told it holds a lock it
/// does not. The blocking pair waits; see [`plan::AdvisoryCall::blocks`].
fn advisory_call(name: &str) -> Option<plan::AdvisoryCall> {
    let folded = name.to_ascii_lowercase();
    match folded.as_str() {
        "pg_advisory_lock" => Some(plan::AdvisoryCall::Lock),
        "pg_advisory_lock_shared" => Some(plan::AdvisoryCall::LockShared),
        "pg_try_advisory_lock" => Some(plan::AdvisoryCall::TryLock),
        "pg_try_advisory_lock_shared" => Some(plan::AdvisoryCall::TryLockShared),
        "pg_advisory_unlock" => Some(plan::AdvisoryCall::Unlock),
        "pg_advisory_unlock_shared" => Some(plan::AdvisoryCall::UnlockShared),
        "pg_advisory_unlock_all" => Some(plan::AdvisoryCall::UnlockAll),
        _ => None,
    }
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
    // `UNIQUE`, which `add_unique_constraint` sends and which every `remove_unique_constraint`
    // test has to send first. It builds a constraint's index rather than a bare one — the
    // distinction `DROP CONSTRAINT` and `DROP INDEX` disagree about.
    if let TableConstraint::Unique(key) = constraint {
        let (deferrable, deferred) = unique_deferrable(key.characteristics.as_ref())?;
        return Ok(plan::AlterTableAction::AddUnique(plan::UniqueConstraint {
            name: key.name.as_ref().map(ident),
            columns: index_columns(&key.columns)?,
            nulls_not_distinct: key.nulls_distinct == NullsDistinctOption::NotDistinct,
            deferrable,
            deferred,
        }));
    }
    // **`UNIQUE USING INDEX` has no column list** — the index supplies the columns — so it is its
    // own `sqlparser` variant, and its own action because a unique constraint here *is* an index
    // with its `constraint` field set: promoting one sets that field rather than building
    // anything.
    if let TableConstraint::UniqueUsingIndex(promote) = constraint {
        let (deferrable, deferred) = unique_deferrable(promote.characteristics.as_ref())?;
        return Ok(plan::AlterTableAction::AddUniqueUsingIndex(
            plan::UniqueUsingIndex {
                name: promote.name.as_ref().map(ident),
                index: ident(&promote.index_name),
                deferrable,
                deferred,
            },
        ));
    }
    // `PRIMARY KEY` over columns the table already has, which is what `change_table`'s
    // `t.primary_key :id` sends when the column is there. It declares a key and re-keys nothing:
    // the rows keep whatever identity they were created with
    // (`crate::catalog::TableDef::row_id`).
    if let TableConstraint::PrimaryKey(key) = constraint {
        return Ok(plan::AlterTableAction::AddPrimaryKey {
            name: key.name.as_ref().map(ident),
            columns: index_columns(&key.columns)?,
        });
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
        validated: true,
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
    // **`INITIALLY DEFERRED` really waits**, the way a deferrable `UNIQUE` already does
    // (`crate::exec::deferred`): the check is registered against the transaction and re-examined
    // at `COMMIT`. It was refused by name until the transaction could owe one — accepting the
    // clause while checking at the statement would refuse a transaction PostgreSQL commits, which
    // is a wrong answer rather than a gap.
    let (deferrable, initially_deferred) = match &key.characteristics {
        None => (false, false),
        Some(characteristics) => {
            refuse_if(
                characteristics.enforced.is_some(),
                "FOREIGN KEY ... ENFORCED, which is MySQL's",
            )?;
            let deferred = characteristics.initially == Some(DeferrableInitial::Deferred);
            (
                characteristics.deferrable.unwrap_or(false) || deferred,
                deferred,
            )
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
        on_update: referential_action(key.on_update.as_ref()),
        on_delete: referential_action(key.on_delete.as_ref()),
        // The `NOT VALID` that may follow belongs to the `ALTER TABLE ... ADD CONSTRAINT` and not
        // to the constraint's own grammar, so it is applied by the caller that can see it.
        validated: true,
        deferrable,
        initially_deferred,
    })
}

/// `ON UPDATE`/`ON DELETE`, defaulting to `NO ACTION` the way a real server does.
///
/// All five, including the two that **write** into the child rather than refusing or removing.
///
/// PostgreSQL 15 added a column list — `SET NULL (a, b)` — narrowing which columns are cleared.
/// The parser this crate uses has no variant for it, so it does not reach here; nothing
/// `ActiveRecord` writes uses it, and the whole clause is one `Option` away when something does.
fn referential_action(
    action: Option<&sqlparser::ast::ReferentialAction>,
) -> catalog::ReferentialAction {
    use sqlparser::ast::ReferentialAction as Written;
    match action {
        None | Some(Written::NoAction) => catalog::ReferentialAction::NoAction,
        Some(Written::Restrict) => catalog::ReferentialAction::Restrict,
        Some(Written::Cascade) => catalog::ReferentialAction::Cascade,
        Some(Written::SetNull) => catalog::ReferentialAction::SetNull,
        Some(Written::SetDefault) => catalog::ReferentialAction::SetDefault,
    }
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
    // **`gin` and `gist` are recorded; `hash` and `brin` are still refused**
    // ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)).
    // The two that are recorded are the two the suite writes, and what is built underneath is the
    // ordered index every index here is — the catalog says what was asked for and nothing claims a
    // trigram search is accelerated. The two that are refused have no operator class this node
    // knows either, so recording one would be a name with nothing behind it.
    let access_method = match &create.using {
        None | Some(IndexType::BTree) => catalog::BTREE_ACCESS_METHOD.to_owned(),
        Some(using) => {
            let name = using.to_string().to_ascii_lowercase();
            if !matches!(name.as_str(), "gin" | "gist") {
                // **PostgreSQL's own sentence comes first when there is an `INCLUDE`**, and it is
                // a different complaint: `amcaninclude` is a property of the access method,
                // checked before anything about the index is built, so `USING hash (…) INCLUDE
                // (…)` is refused for the payload rather than for the method. Measured for `hash`
                // and for `brin`, one sentence with the name substituted.
                if create.include.is_empty() {
                    return Err(SqlError::unsupported(format!("an index USING {using}")));
                }
                return Err(SqlError::AccessMethodWithoutInclude(name));
            }
            name
        }
    };
    Ok(plan::CreateIndex {
        access_method,
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
        // Every entry arrives as an identifier, because that is all the parser's target can hold.
        // An entry that was an *expression* is a placeholder here and becomes one again in
        // `lower_inline`, where `Parsed` is in scope to say which.
        Some(ConflictTarget::Columns(columns)) => columns
            .iter()
            .map(|name| {
                plan::ConflictKey::Column(
                    fold_identifier(&name.value, name.quote_style.is_some()).0,
                )
            })
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
    // The predicate is not in the tree — `sqlparser` stops at the keyword — and is attached where
    // `Parsed` is in scope (`parse::Parsed::conflict_predicate`, applied in `lower_inline`).
    Ok(plan::OnConflict {
        target,
        predicate: None,
        action,
    })
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
/// Lowers `a AND b AND …` or `a OR b OR …` **iteratively**, into a tree of about `log2(n)` levels.
///
/// The spine is walked with a loop rather than by recursion, so the lowering of a long chain costs
/// one frame plus the deepest operand rather than one frame per term; the fold that follows is what
/// keeps every *later* walk over the tree short. See the `And | Or` arm of [`lower_expr`] for why
/// reshaping a boolean chain is allowed.
fn lower_boolean_chain(op: &BinaryOperator, left: &Expr, right: &Expr) -> Result<plan::Expr> {
    let folded = if matches!(op, BinaryOperator::And) {
        plan::BinaryOp::And
    } else {
        plan::BinaryOp::Or
    };
    // Right to left down the spine, because that is the way the tree leans; reversed afterwards so
    // the operands are in the order they were written, which is what a reader of an `EXPLAIN` and
    // anything matching on the tree expects to see.
    let mut operands = vec![right];
    let mut spine = left;
    while let Expr::BinaryOp {
        op: inner,
        left: rest,
        right: operand,
    } = spine
        && inner == op
    {
        operands.push(operand);
        spine = rest;
    }
    operands.push(spine);
    operands.reverse();

    // **Through `lower_condition`, which is what the operand of an `AND` is owed.** An unadorned
    // string literal in a boolean context is *read* as a boolean rather than refused — `WHERE
    // 'true' AND true` runs on a real server and `WHERE 'text' AND true` is `22P02 invalid input
    // syntax for type boolean`, a value error and not a type one. Lowering the operands with plain
    // `lower_expr` turned both of those into `42804`, which two corpus lines caught at once.
    let mut level = Vec::with_capacity(operands.len());
    for operand in operands {
        level.push(lower_condition(operand, true)?);
    }
    balance(folded, level)
}

/// Folds operands pairwise until one is left: `n` terms become a tree `⌈log2(n)⌉` deep.
///
/// An odd operand is carried to the next round rather than paired with a synthetic `true`, which
/// would be a value the client did not write showing up in an `EXPLAIN`.
fn balance(op: plan::BinaryOp, mut level: Vec<plan::Expr>) -> Result<plan::Expr> {
    while level.len() > 1 {
        let mut folded = Vec::with_capacity(level.len().div_ceil(2));
        let mut operands = level.into_iter();
        while let Some(left) = operands.next() {
            folded.push(match operands.next() {
                Some(right) => plan::Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                None => left,
            });
        }
        level = folded;
    }
    // The caller always passes both sides of a binary operator, so there is at least one.
    level.pop().ok_or_else(|| {
        SqlError::Internal("a boolean chain lowered to no operands at all".to_owned())
    })
}

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
        // `title COLLATE "C" DESC`, which `unsafe_raw_sql_test.rb` sends and which this node can
        // answer *exactly*: `C` is byte order and a memcomparable key is already in byte order, so
        // the clause asks for the ordering the rows would have had. The name is still checked —
        // `42704` for one this node does not have — because accepting `en_US.UTF-8` here and
        // sorting by bytes would return the rows in an order nobody asked for (ADR 0076).
        //
        // **The type check reaches as far as the type does.** A literal carries one, so
        // `SELECT 1 COLLATE "C"` is the measured `42804`; a column reference does not have one
        // until the executor resolves it against a scope, and `COLLATE` on a non-collatable
        // *column* is accepted here where PostgreSQL raises. Declared in
        // `tests/corpus/pg19_collation.txt` rather than left to be found.
        Expr::Collate { expr, collation } => {
            let lowered = lower_expr(expr)?;
            // **The name is checked and the clause is kept.** It used to be dropped, on the
            // reasoning that both names this node has mean byte order so there was nothing for the
            // plan to carry — true of the *bytes* and false of the text and of the rule: a stored
            // expression prints the clause back (`upper((t COLLATE "C"))`), and two explicit
            // clauses that disagree are `42P21`. A clause that is dropped can do neither
            // ([ADR 0096](../../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md)).
            let collation = collation_name(collation)?;
            if let plan::Expr::Literal(literal) = &lowered
                && let Some(ty) = literal_type(literal)
                && !catalog::pg_attribute::collatable(ty)
            {
                return Err(SqlError::CollationNotSupported(ty.name()));
            }
            Ok(plan::Expr::Collate {
                operand: Box::new(lowered),
                collation,
            })
        }
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
            // `s.t.c`. **The qualifier goes through `relation_name`**, which is the one place
            // this crate decides what `schema.relation` means — `public` dropped because a public
            // table's stored name is bare, `pg_temp` and `pg_catalog` kept as lookup prefixes.
            // Comparing the text instead would refuse `public.t.c` over a bare `FROM t`, which a
            // real server answers, and there would be a second parser of a grammar that already
            // has one.
            [schema, table, column] => Ok(plan::Expr::Column {
                table: Some(relation_name(&ObjectName(vec![
                    sqlparser::ast::ObjectNamePart::Identifier(schema.clone()),
                    sqlparser::ast::ObjectNamePart::Identifier(table.clone()),
                ]))?),
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
        Expr::IsDistinctFrom(left, right) => {
            lower_distinct(left, right, plan::BinaryOp::Distinct)
        }
        Expr::IsNotDistinctFrom(left, right) => {
            lower_distinct(left, right, plan::BinaryOp::NotDistinct)
        }
        // **`a BETWEEN x AND y` is `a >= x AND a <= y`**, and the rewrite is the whole feature:
        // every rule a corpus can ask about falls out of it rather than needing one of its own.
        // The ends are inclusive because `>=` and `<=` are; reversed bounds match nothing because
        // nothing is both above 3 and below 2; a NULL bound is **three-valued AND** rather than
        // "NULL anywhere means NULL", so `3 BETWEEN NULL AND 2` is `false` and `1 BETWEEN NULL AND
        // 2` is NULL; and a type mismatch is `42883 operator does not exist: character varying >=
        // integer` — a real server's own message, naming `>=` rather than `BETWEEN`, which is what
        // says PostgreSQL rewrites it too.
        //
        // `NOT BETWEEN` is `NOT (…)` around the pair, which inherits the NULL: `1 NOT BETWEEN NULL
        // AND 2` is NULL and not true. Measured, all of it.
        //
        // `BETWEEN SYMMETRIC` is refused by name before the parser sees it
        // (`crate::parse`'s unsupported list) and stays that way: `sqlparser` 0.62.0's `Between`
        // has no flag for it, so there is nothing to lower even if the keyword got through.
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            // **The negated form is a different pair of comparisons, not a `NOT` around this
            // one.** PostgreSQL expands `a NOT BETWEEN 1 AND 10` to `((a < 1) OR (a > 10))` and
            // prints that back through every reader; this node wrapped the positive pair and
            // printed `(NOT ((a >= 1) AND (a <= 10)))`, which is the same *answer* and not the
            // same *definition*.
            //
            // The two agree on every NULL, which is what had to be checked before swapping them:
            // measured on 19beta1 with a NULL value, a NULL low and a NULL high, `NOT BETWEEN`,
            // the `OR` form and the `NOT`-wrapped form give the same three NULLs and the same
            // trues and falses elsewhere. The doc that used to sit here said the wrap was what
            // carried the NULL through — true of it, and true of the `OR` form as well.
            let value = lower_expr(expr)?;
            let (low, high) = (lower_expr(low)?, lower_expr(high)?);
            let (op, first, second) = if *negated {
                (plan::BinaryOp::Or, plan::BinaryOp::Lt, plan::BinaryOp::Gt)
            } else {
                (
                    plan::BinaryOp::And,
                    plan::BinaryOp::GtEq,
                    plan::BinaryOp::LtEq,
                )
            };
            Ok(plan::Expr::Binary {
                op,
                left: Box::new(plan::Expr::Binary {
                    op: first,
                    left: Box::new(value.clone()),
                    right: Box::new(low),
                }),
                right: Box::new(plan::Expr::Binary {
                    op: second,
                    left: Box::new(value),
                    right: Box::new(high),
                }),
            })
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(plan::Expr::Not(Box::new(lower_expr(expr)?))),
        // **A comparison over `json` is refused and one over `jsonb` is not**, for the reason the
        // `||` arm above parts them: `json` has no comparison operator on a real server, so the
        // honest answer is the one a real server gives, while `jsonb` has a complete btree and
        // *answers*. One sentence would have to be wrong about one of them.
        //
        // `jsonb` therefore falls through to the ordinary lowering and `exec::query` rewrites it
        // into a `jsonb_compare` against zero, where the operand's type is known — the parser
        // cannot see a column's.
        Expr::BinaryOp { left, op, right }
            if is_comparison(op)
                && matches!(
                    (json_cast_name(left), json_cast_name(right)),
                    (Some("json"), _) | (_, Some("json"))
                ) =>
        {
            Err(SqlError::UndefinedOperator {
                left: json_cast_name(left).unwrap_or("json").to_owned(),
                op: comparison_symbol(op),
                right: json_cast_name(right).unwrap_or("json").to_owned(),
            })
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
        // **A boolean chain is a row of siblings, not a thousand generations.**
        //
        // `a OR b OR c` parses leaning left, so a predicate of *n* terms arrives as a tree *n*
        // deep. Lowering it one frame per term is what the guard above exists to stop — it was
        // added when a 500-term chain overflowed the resolver — and the guard then refused
        // `or_test.rb`'s 1001-relation `.or` chain with `54001`, where PostgreSQL 19 answers a
        // number. Measured on the oracle: twenty thousand terms flat is fine there, and so is five
        // thousand levels of brackets, so nothing about this shape is too complex for a server.
        //
        // The chain is collected **in a loop** and folded into a balanced tree, so a thousand terms
        // is a dozen levels rather than a thousand and every one of the forty walks over
        // `plan::Expr` is short. The depth bound is untouched: what changes is that siblings stop
        // being counted as generations, which is what they always were.
        //
        // Sound because `AND` and `OR` are associative — in three-valued logic too, where `OR`
        // takes the largest of false < null < true and `AND` the smallest — and because
        // PostgreSQL defines the evaluation order of a boolean expression's operands as **not
        // guaranteed**, so no client may depend on the shape either. Nothing prints a
        // `plan::Expr` back as SQL (`pg_get_expr` reads stored text), so no deparse can see it.
        Expr::BinaryOp {
            op: op @ (BinaryOperator::And | BinaryOperator::Or),
            left,
            right,
        } => lower_boolean_chain(op, left, right),
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
                // **`~~` and friends are `LIKE` spelled as operators**, which is what
                // `ActiveRecord` writes for a citext match and what `citext_test.rb` reads. One
                // lowering for both spellings, so the folding rule has one home.
                op @ (BinaryOperator::PGLikeMatch
                | BinaryOperator::PGILikeMatch
                | BinaryOperator::PGNotLikeMatch
                | BinaryOperator::PGNotILikeMatch) => {
                    return Ok(plan::Expr::Like {
                        operand: Box::new(lower_expr(left)?),
                        pattern: Box::new(lower_expr(right)?),
                        negated: matches!(
                            op,
                            BinaryOperator::PGNotLikeMatch | BinaryOperator::PGNotILikeMatch
                        ),
                        case_insensitive: matches!(
                            op,
                            BinaryOperator::PGILikeMatch | BinaryOperator::PGNotILikeMatch
                        ),
                        escape: None,
                    });
                }
                // **`->` means two things** — an hstore's fetch and a JSON document's — and the
                // values cannot tell them apart, because a `jsonb` is a canonical `Datum::Text`
                // and so is a string. Told apart the way `||` is: here when a **cast** wrote the
                // type down, and in the evaluator by `Expr::Ordinal`'s declared type when the
                // operand is a column.
                BinaryOperator::Arrow => {
                    let func = match json_cast_type(left) {
                        Some(ColumnType::Json) => plan::CatalogFunc::JsonFetch,
                        Some(_) => plan::CatalogFunc::JsonbFetch,
                        None => plan::CatalogFunc::HstoreFetch,
                    };
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                // **`->>` needs no dispatch**: it is not an hstore operator, so every one of them
                // is a JSON fetch whatever the operand was declared.
                BinaryOperator::LongArrow => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::JsonFetchText,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::Question => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreHasKey,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                // **`@>` is spelled the same for an hstore and a range**, so it lowers to one
                // call and the evaluator dispatches on the operands — the rule the `||` regression
                // taught: an operator this crate carries for one type must not answer for
                // another's, and the only place that can be decided is where the values are.
                // **`~=` is carried, not refused here.** Which types have it is the opposite of
                // which types have the operators around it — the geometric shapes do and the
                // document types do not — so the answer needs the operand's type, and the parser
                // has none for a column.
                BinaryOperator::TildeEq => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::SameAs,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::AtArrow => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::HstoreContains,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                // `b <@ a` is `a @> b` with the arguments the other way round, so there is one
                // containment rule and not two.
                BinaryOperator::ArrowAt => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::RangeContains,
                        args: vec![lower_expr(right)?, lower_expr(left)?],
                    })));
                }
                // **`@@` in both argument orders is one call**, and the evaluator decides which
                // operand is the vector — the rule this match already states for `||` and `@>`.
                BinaryOperator::AtAt => {
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func: plan::CatalogFunc::TsMatch,
                        args: vec![lower_expr(left)?, lower_expr(right)?],
                    })));
                }
                BinaryOperator::StringConcat => {
                    // **A `jsonb` operand takes `||` away from string concatenation**, and this
                    // is the only layer that can see one: a literal cast is folded to its value
                    // before the executor runs, and `jsonb` has no `Datum` of its own — it is a
                    // `Datum::Text`, so `'{"a":1}'::jsonb || '{"b":2}'::jsonb` would concatenate
                    // two documents into a string that is not a document. A **wrong answer**
                    // where a refusal is a gap, which
                    // [ADR 0031](../../../docs/adr/0031-the-rails-suite-is-the-measure.md) ranks
                    // the other way round, so it is refused here with the `0A000` the operator
                    // gave before `||` over text existed. A jsonb *column* is caught in
                    // `exec::cursor`, where an `Expr::Ordinal` still carries its type.
                    //
                    // PostgreSQL's answer is document **merge**, right operand winning a
                    // duplicate key. Building it needs a representation of its own —
                    // `docs/plans/jsonb-representation.md`, and ADR 0042's rule is why.
                    // **`json` has no `||` at all** — `42883 operator does not exist: json ||
                    // json`, measured — and `jsonb` has one that merges documents, which is
                    // `value::json::concat`. So the two spellings part company here, where the
                    // cast is still written down: a `json` operand is the undefined operator a
                    // real server names, and a `jsonb` one goes through to the evaluator.
                    if let (Some(left_ty), Some(right_ty)) =
                        (json_cast_name(left), json_cast_name(right))
                        && (left_ty == "json" || right_ty == "json")
                    {
                        return Err(SqlError::UndefinedOperator {
                            left: left_ty.to_owned(),
                            op: "||",
                            right: right_ty.to_owned(),
                        });
                    }
                    // A cast that says `jsonb` on either side makes this the merge rather than
                    // any of the five concatenations, and it is the only layer that can see one:
                    // the literal is canonicalised into a `Datum::Text` before the executor runs.
                    // **Both sides must be `jsonb`**, and an unadorned literal counts as one.
                    // There is no `jsonb || text` operator, so a jsonb column beside a *text*
                    // column falls back to `text || text` — measured, `body || plain` is
                    // `{"a": 1}x` and not a merge. What a bare literal does instead is get
                    // coerced: `'{"a":1}'::jsonb || 'tail'` is
                    // `22P02 invalid input syntax for type json`, because `tail` was read as a
                    // document and is not one.
                    let jsonb_side = |expr: &Expr| json_cast_name(expr) == Some("jsonb");
                    let coercible =
                        |expr: &Expr| jsonb_side(expr) || matches!(unwrap_nested(expr), Expr::Value(_));
                    let func = if (jsonb_side(left) && coercible(right))
                        || (jsonb_side(right) && coercible(left))
                    {
                        plan::CatalogFunc::JsonbConcat
                    } else {
                        plan::CatalogFunc::HstoreConcat
                    };
                    return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                        func,
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
            // **An untyped string literal in a boolean context is *read* as a boolean**, not
            // refused: `WHERE 'true' AND true` runs on a real server and `WHERE 'text' AND
            // true` is `22P02 invalid input syntax for type boolean: "text"` — a *value* error
            // rather than a type one, because an unadorned literal takes the type its context
            // wants and only then fails to be read as one. A `character varying` **column**
            // cannot: it already has a type, and that is the `42804` next door. Measured, all
            // three.
            let boolean = matches!(op, plan::BinaryOp::And | plan::BinaryOp::Or);
            Ok(plan::Expr::Binary {
                op,
                left: Box::new(lower_condition(left, boolean)?),
                right: Box::new(lower_condition(right, boolean)?),
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
            any: false,
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
        // **`TRIM`'s three forms are three functions**, which is what a real server's own
        // `pg_get_expr` says when it deparses one: `TRIM(BOTH …)` is `btrim`, `LEADING` is
        // `ltrim`, `TRAILING` is `rtrim`, and no keyword at all is `BOTH`. The characters are a
        // **set** and not a prefix — `TRIM(BOTH 'ab' FROM 'abcba')` is `c` — so the second
        // argument goes through unchanged and the evaluator does the work
        // (`tests/captures/pg19_greatest_trim.txt`).
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => {
            let func = match trim_where {
                Some(TrimWhereField::Leading) => plan::CatalogFunc::Ltrim,
                Some(TrimWhereField::Trailing) => plan::CatalogFunc::Rtrim,
                Some(TrimWhereField::Both) | None => plan::CatalogFunc::Btrim,
            };
            let mut args = vec![lower_expr(expr)?];
            // Two spellings of the same second argument: `TRIM(BOTH 'x' FROM y)` puts it in
            // `trim_what`, and PostgreSQL's `trim(y, 'x')` in `trim_characters`.
            if let Some(what) = trim_what {
                args.push(lower_expr(what)?);
            } else if let Some(chars) = trim_characters {
                for one in chars {
                    args.push(lower_expr(one)?);
                }
            }
            Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func,
                args,
            })))
        }
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
        } => {
            // **A row on the left**, which is what a composite primary key sends:
            // `WHERE (shop_id, id) IN (SELECT shop_id, id FROM …)`. It is lowered as a list of
            // operands rather than as a row *expression* — there is no general row value in this
            // crate, and one shape does not need one.
            let operands = match expr.as_ref() {
                Expr::Tuple(items) => items
                    .iter()
                    .map(lower_expr)
                    .collect::<Result<Vec<_>>>()?,
                other => vec![lower_expr(other)?],
            };
            Ok(plan::Expr::Subquery(Box::new(
                plan::SubqueryExpr::compared_row(
                    plan::SubqueryKind::In { negated: *negated },
                    operands,
                    Box::new(lower_query(subquery)?),
                ),
            )))
        }
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
        // **An array on the right, with any of the six operators and either quantifier.**
        //
        // Two of the twelve spellings have an `IN` form and the lowering prefers it when it can
        // see the list: `= ANY (ARRAY[1,2])` is `x IN (1,2)` and `<> ALL (ARRAY[1,2])` is
        // `x NOT IN (1,2)` — the same rule over a list known at plan time, and expanding it is
        // what lets an index seek use it. PostgreSQL agrees the two are one thing and prints both
        // as `(x <> ALL (ARRAY[1, 2]))`, measured (`tests/corpus/pg19_all_quantifier.txt`).
        //
        // Everything else — `> ALL`, `= ALL`, `<> ANY` — and every array the lowering *cannot*
        // see, a column being the case that matters (`a.attnum = ANY(i.indkey)`), keeps the
        // operator and the quantifier and is decided per row.
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => lower_quantified(left, compare_op, right, false),
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => lower_quantified(left, compare_op, right, true),
        // `CASE WHEN … THEN … [ELSE …] END`, and the **simple** form `CASE x WHEN 1 THEN …`
        // beside it. The operand is **carried, not desugared**: a real server keeps it in its
        // `CaseExpr` and prints `CASE x` back, so rewriting it to `WHEN x = 1` here would store a
        // definition nobody wrote and `pg_get_indexdef` would answer `ActiveRecord` with something
        // it never sent. The equality is the *evaluator's* business
        // (`exec::cursor`), and it is `=` rather than `IS NOT DISTINCT FROM`:
        // `CASE NULL WHEN NULL THEN 1 ELSE 2 END` is `2`, measured.
        //
        // This was `0A000 CASE <expression> WHEN ..., the simple form is not supported` until the
        // deparse census asked what a real server prints for it.
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            Ok(plan::Expr::Case {
                operand: operand.as_deref().map(lower_expr).transpose()?.map(Box::new),
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
        // **`SUBSTR` is a node, not a call.** `sqlparser` reads `substr(x, 2)` and
        // `substring(x FROM 2 FOR 3)` into the same `Expr::Substring`, so a `CatalogFunc` alone
        // never sees it — which is why `substr` stayed `0A000` while `split_part` beside it
        // worked. The two spellings mean the same thing and both arrive here.
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            shorthand,
            ..
        } => {
            let mut args = vec![lower_expr(expr)?];
            let Some(from) = substring_from else {
                // `SUBSTRING(x FOR n)` with no `FROM` is `SUBSTRING(x FROM 1 FOR n)` on a real
                // server; nothing sends it, so it is named rather than assumed.
                return Err(SqlError::unsupported("SUBSTRING with no FROM"));
            };
            args.push(lower_expr(from)?);
            if let Some(count) = substring_for {
                args.push(lower_expr(count)?);
            }
            // The spelling decides the output column's name and nothing else — measured.
            Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: if *shorthand {
                    plan::CatalogFunc::Substr
                } else {
                    plan::CatalogFunc::Substring
                },
                args,
            })))
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
    if let Expr::Subquery(query) = strip_nesting(right) {
        return Ok(plan::Expr::Subquery(Box::new(
            plan::SubqueryExpr::compared(
                plan::SubqueryKind::Quantified { op, all },
                lower_expr(left)?,
                Box::new(lower_query(query)?),
            ),
        )));
    }
    // **The array form of the same node.** The `IN` shortcut is taken only for the two spellings
    // that have one and only when the list is visible here; `lower_array` answers `None` for a
    // column, which is the case the per-row variant exists for.
    let operand = Box::new(lower_expr(left)?);
    if let Some(list) = match (op, all) {
        (plan::BinaryOp::Eq, false) | (plan::BinaryOp::NotEq, true) => lower_array(right)?,
        _ => None,
    }
    // **An empty array is not an empty `IN` list, because there is no such thing.**
    // `Expr::InList`'s own doc says PostgreSQL's grammar has no empty one, and its evaluator is
    // written for that: it answers NULL for a NULL operand before it counts the list. So
    // `NULL = ANY (ARRAY[]::integer[])` came back NULL through the shortcut where a real server
    // answers `f` — the quantifier's first rule is that an empty right-hand side settles it with
    // no comparison, and only the array node knows it is empty.
    .filter(|list| !list.is_empty())
    {
        return Ok(plan::Expr::InList {
            operand,
            list,
            negated: all,
            // The written spelling was `= ANY` / `<> ALL`, and one rule depends on knowing it.
            any: true,
        });
    }
    // **A constructor stays a constructor**, which is the one thing `pg_get_expr` keeps that a
    // fold destroys. Measured on 19beta1, three spellings of the same array in a generated column:
    //
    // ```text
    // (c1 = ALL (ARRAY[1,2]))        ->  (c1 = ALL (ARRAY[1, 2]))
    // (c1 = ALL ('{1,2}'::int[]))    ->  (c1 = ALL ('{1,2}'::integer[]))
    // (c1 = ALL ('{1,2}'))           ->  (c1 = ALL ('{1,2}'::integer[]))
    // ```
    //
    // PostgreSQL prints the node it parsed, so the constructor and the literal are two answers.
    // `lower_expr` folds a constant `ARRAY[…]` into a `Datum::Array` — right for evaluation, and
    // it makes the two spellings one node, so the printer could only ever give the literal form.
    // The `IN` shortcut above never had this problem, because its list is elements and its deparse
    // writes them back as `ARRAY[…]`; this keeps the same shape for the spellings that have no
    // `IN` form. The elements are already lowered by `lower_array`, so nothing is parsed twice.
    if let Some(elements) = lower_array(right)?.filter(|list| !list.is_empty())
        && matches!(strip_nesting(right), Expr::Array(_))
    {
        return Ok(plan::Expr::QuantifiedArray {
            operand,
            op,
            all,
            array: Box::new(plan::Expr::Array {
                elements,
                element: None,
            }),
        });
    }
    Ok(plan::Expr::QuantifiedArray {
        operand,
        op,
        all,
        array: Box::new(lower_expr(strip_nesting(right))?),
    })
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
    // `current_user`, `session_user` and `user`, which PostgreSQL spells without parentheses and
    // `sqlparser` hands over as a zero-argument call. The same three names, one value: this node
    // has no `SET ROLE`, which is the only thing that makes the first two differ on a real server.
    if name.eq_ignore_ascii_case("current_user")
        || name.eq_ignore_ascii_case("session_user")
        || name.eq_ignore_ascii_case("user")
    {
        refuse_wrong_arity(function, "current_user", 0)?;
        return Ok(plan::Expr::CurrentUser);
    }
    // The advisory-lock functions this node answers, blocking forms included — see
    // [`plan::AdvisoryCall`] for which are still refused and why.
    if let Some(call) = advisory_call(&name) {
        // **`pg_advisory_unlock_all()` is the one with no arguments**, and `sqlparser` spells an
        // empty argument list either way depending on how the call was written, so both are it.
        let no_arguments = matches!(function.args, FunctionArguments::None)
            || matches!(&function.args, FunctionArguments::List(list) if list.args.is_empty());
        if matches!(call, plan::AdvisoryCall::UnlockAll) {
            if !no_arguments {
                return Err(SqlError::UndefinedFunction(format!(
                    "{}() with arguments",
                    call.name()
                )));
            }
            return Ok(plan::Expr::Advisory {
                call,
                args: Vec::new(),
            });
        }
        let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args else {
            return Err(SqlError::UndefinedFunction(format!("{}()", call.name())));
        };
        if args.len() != 1 && args.len() != 2 {
            return Err(SqlError::UndefinedFunction(format!(
                "{}() with {} arguments",
                call.name(),
                args.len()
            )));
        }
        let mut lowered = Vec::with_capacity(args.len());
        for arg in args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                return Err(SqlError::unsupported(format!(
                    "{}() with that argument",
                    call.name()
                )));
            };
            lowered.push(lower_expr(expr)?);
        }
        return Ok(plan::Expr::Advisory {
            call,
            args: lowered,
        });
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
    // **`mod(x, y)` computes `x % y` and prints as `mod`.** PostgreSQL's `%` for `int8` is
    // implemented by `int8mod` — the same C function `mod()` calls — and the two agree on every
    // sign combination, measured, so there is one remainder in this crate and the call delegates
    // to it (`exec::cursor`'s `CatalogFunc::Mod` arm).
    //
    // **It used to be rewritten into the arithmetic node outright, and that threw the spelling
    // away.** `pg_get_indexdef` prints the node the tree holds, so `mod(id, 10)` came back
    // `id % 10` where a real server prints `mod(id, 10)` — in every form, whole and per column,
    // plain and pretty, and `postgresql_adapter_test#test_expression_index` asserts that string
    // exactly. `%` and `mod` are two spellings on a real server too, so they are two nodes here.
    if name == "mod" {
        refuse_wrong_arity(function, "mod", 2)?;
        if let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args
            && let [
                FunctionArg::Unnamed(FunctionArgExpr::Expr(left)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(right)),
            ] = args.as_slice()
        {
            return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::Mod,
                args: vec![lower_expr(left)?, lower_expr(right)?],
            })));
        }
        return Err(SqlError::UndefinedFunction("mod(unknown)".to_owned()));
    }
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
    // **`ROW(a, b, …)` is a record constructor, not a function**, and what it builds is the
    // composite *text* — PostgreSQL's `ROW(…)` has no type of its own until it is assigned to a
    // column that has one, and the assignment is where the arity is checked because that is where
    // the catalog is. `INSERT … VALUES (1, ROW('Paris','Champs-Élysées'))` is what
    // `composite_test.rb` sends.
    if name.eq_ignore_ascii_case("row")
        && let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args
    {
        return lower_row_constructor(args);
    }
    let Some(func) = plan::AggregateFunc::from_name(&name) else {
        // **A name this vocabulary lacks may be a function the catalog holds**, and lowering
        // cannot see the catalog — the same seam a column `DEFAULT` naming one already sits on.
        // So the call is carried out rather than refused, and `Executor::resolve_user_function`
        // raises the identical `0A000` for a name nobody declared. A name with no argument list at
        // all is not a call and keeps its own message.
        if let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args {
            let mut carried = vec![plan::Expr::Literal(plan::Literal::String(name.clone()))];
            for arg in args {
                let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
                    return Err(SqlError::unsupported(format!("the function {name}")));
                };
                carried.push(lower_expr(expr)?);
            }
            return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::UserFunc,
                args: carried,
            })));
        }
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
    // **`VIRTUAL` is not refused, and the old argument for refusing it was wrong.** It read: a
    // node that took the word and stored anyway "would answer the same value after the source
    // changed under it". It would not — a generated expression reads only its own row, so any
    // change to the source rewrites the row and recomputes. Measured: the two kinds agree on every
    // query, on `UPDATE`, and on refusing a non-DEFAULT write, and differ only in
    // `pg_attribute.attgenerated`. The word reaches here as `STORED` anyway
    // (`crate::parse::strip_virtual_generated`); which one was written travels on `Parsed`.
    let _ = mode;
    refuse_if(
        generated_as == GeneratedAs::ByDefault,
        "GENERATED BY DEFAULT AS (expression)",
    )?;
    // The **normalised** text, the way a `CHECK` and an index predicate are stored: `pg_get_expr`
    // prints this back, so the parentheses a user wrote must not survive into the catalog — and
    // the ones its deparser *adds* must be there whatever was written.
    //
    // That is [`catalog::ExprShape`], already measured for an index expression and asked here for
    // the same reason: `pg_get_expr(adbin, adrelid)` is one function and it parenthesises one kind
    // of node. Measured on 19beta1 over a table of generated columns, one shape per row:
    //
    // ```text
    // (c1 + 1)              -> (c1 + 1)          (c1)                  -> c1
    // (c1 > 0)              -> (c1 > 0)          (7)                   -> 7
    // (c1 IS NOT NULL)      -> (c1 IS NOT NULL)  (length(t))           -> length(t)
    // (- c1)                -> (- c1)            (COALESCE(c1, 0))     -> COALESCE(c1, 0)
    // ```
    //
    // Without it `virtual_column_test#test_schema_dumping` failed on one pair of parentheses:
    // `t.virtual "column2", type: :integer, as: "column1 + 1"` where the suite matches
    // `as: "\(column1 \+ 1\)"`. The catalog was identical on both servers and the dump was
    // otherwise complete, `stored: true` and the nested casts included — the whole of the failure
    // was the pair this adds.
    //
    // **What it still does not do is the nested pairs.** A real server prints `(c1 * 2 + 3)` as
    // `((c1 * 2) + 3)`, because its deparser parenthesises every operator node and not only the
    // top one; this stores text, so only the outermost is knowable here. Recorded in
    // `docs/plans/phase-9-rails.md` rather than half-done: closing it means deparsing the stored
    // tree, which is a different unit and touches the index key list too.
    let expr = unwrap_nested(expr);
    Ok(Ok(expr_shape(expr).printed(&expr.to_string())))
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
    // **`date_trunc`'s overloads are on the value's type**, and a real server picks between them
    // at resolution — which is here, where the *written* type of a literal is still visible. An
    // unadorned `42` is an `integer` to PostgreSQL's resolver and an `int8` to this node's value
    // layer, so refusing it at evaluation would name `bigint` and quote back a signature the user
    // did not write. A column falls through as `unknown` and is caught by the value it produces.
    if func == plan::CatalogFunc::DateTrunc
        && let FunctionArguments::List(FunctionArgumentList { args, .. }) = &function.args
        && let Some(value) = args.get(1)
    {
        let named = argument_type_name(value);
        if !matches!(
            named.as_str(),
            "unknown"
                | "timestamp without time zone"
                | "timestamp with time zone"
                | "interval"
                | "date"
        ) {
            return Err(SqlError::UndefinedFunctionTypes(format!(
                "date_trunc({}, {named})",
                argument_type_name(&args[0])
            )));
        }
    }
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

    // **`nextval('s'::regclass)` names the same sequence as `nextval('s')`**, and it is the
    // spelling a real server hands back: `pg_get_expr` deparses the coercion `nextval(regclass)`
    // forces on the literal, so a client that reads a default and sends it on writes the cast.
    // This refused it until the third reader of this grammar became a caller of the first —
    // `sequence_literal_name`, which `lower_set_default` and `column_default` already use.
    let named = |expr: &Expr| -> Result<String> {
        sequence_literal_name(expr).ok_or_else(|| {
            SqlError::unsupported(format!(
                "the sequence name {expr}, which is not a string literal"
            ))
        })
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

/// **An element that is itself an array stacks; it does not nest.**
///
/// `ARRAY['{1,2}'::int[]]` is an `int[]` with another dimension. The constant fold built an
/// `ArrayValue` whose `element` was `int[]` and whose values were arrays — a shape `ArrayValue`
/// does not have, and everything downstream read `text[]`. The runtime constructor learned this in
/// `exec::cursor`; this is the same rule reached through the other door, and
/// `ArrayValue::stacked` is the one place it lives. The *literal* half of r1's 196-row wire census
/// is exactly this door.
///
/// `None` when the elements are not arrays, which is the ordinary constructor below.
fn stack_folded_arrays(
    element: ColumnType,
    values: &[Option<Datum>],
) -> Result<Option<plan::Expr>> {
    let Some(fallback) = esker_keys::array::ArrayValue::element_of(element) else {
        return Ok(None);
    };
    let parts: Vec<Option<&esker_keys::array::ArrayValue>> = values
        .iter()
        .map(|value| match value {
            Some(Datum::Array(array)) => Some(array),
            _ => None,
        })
        .collect();
    let stacked = esker_keys::array::ArrayValue::stacked(&parts, fallback)
        .ok_or(SqlError::ArrayExpressionDimensions)?;
    Ok(Some(plan::Expr::Literal(plan::Literal::Typed(Box::new(
        Datum::Array(stacked),
    )))))
}

/// `ARRAY[…]` lowered — **the node is kept and the element type is settled here**.
///
/// The constructor builds an array from expressions where a literal builds one from text, and the
/// element type is the elements' — PostgreSQL's `select_common_type`, narrowed to the array types
/// this node has. Every element that is a constant is read here, where what was *written* is still
/// visible; an element that needs a row is left to `exec::query::resolve`, which settles the type
/// only when this function could not (`debts-v1.1.md` #42).
///
/// **It used to fold the whole thing into a `Literal::Typed(Datum::Array)`**, and that was the
/// last entry of the deparse census's group D: `(ARRAY[1, 2])::text` printed
/// `('{1,2}'::integer[])::text` where a real server prints the constructor it kept, because after
/// the fold the two spellings are the same value and nothing can tell them apart. Keeping the node
/// cost three other rules that had been leaning on the fold, each measured before it was changed:
/// `resolve` re-settling an element type the fold had already decided, `= ANY`'s reader expecting
/// a folded value, and the `unknown` rule below.
///
/// **An array of arrays still folds**, through [`stack_folded_arrays`]: the outer array is built
/// from the inner values rather than from their printed form, which is what makes
/// `ARRAY['{1,2}'::int[]]` an `integer[]` (`tests/array_of_array.rs`).
///
/// **`ARRAY[]` is an error and `'{}'::int[]` is not.** An empty constructor has no elements to take
/// a type from, so PostgreSQL answers `42P18` with a hint; an empty *literal* has its type from the
/// cast and is a perfectly good empty array. Measured, both.
fn lower_array_constructor(elements: &[Expr]) -> Result<plan::Expr> {
    let mut texts: Vec<Option<String>> = Vec::with_capacity(elements.len());
    let mut element = None;
    for expr in elements {
        // **A cast of a string constant is a constant too, whatever it casts to.** A cast is how
        // an element type is written down at all — `ARRAY['2010-01-01'::date]` is a `date[]` and
        // `ARRAY['2010-01-01']` is a `text[]` — and the type the cast names *is* the array's, so
        // this arm both folds the value and settles the element type. It was two named types
        // while only `hstore` and `tsrange` had been measured this way; every one of them is
        // measured now, from `ARRAY['{"a":1}'::jsonb]` to `ARRAY['ABC'::citext]`.
        if let Expr::Cast {
            expr: inner,
            data_type,
            ..
        } = strip_nesting(expr)
            && let Ok((cast_to, NO_TYPMOD)) = lower_type(data_type)
            && let Expr::Value(value) = strip_nesting(inner)
            && let Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) = &value.value
        {
            // **A `regtype` over a name the value layer does not know belongs to the catalog**,
            // not to this fold: `ARRAY['example_type'::regtype]` is what `postgresql_adapter_test`
            // sends about a `CREATE DOMAIN`, and folding it here read the name with
            // `value::named_type`, which only knows the built-ins, and answered `42704` for a type
            // that exists. The runtime constructor lowers each element through `lower_expr`, where
            // the regtype cast already has its catalog seam.
            // **A `regclass` is that seam every time**, not only for a name the value layer does
            // not know: *every* relation name is a catalog lookup, and `Datum::from_text` has no
            // catalog. This arm did not exist while `regclass` was not a column type — `lower_type`
            // refused it, so the guard above never matched — and the moment it became one
            // (`debts-v1.1.md` #35) `ARRAY['t'::regclass]` started folding here and answering
            // `0A000 a relation name read as a regclass without a catalog`. One face fixed, the
            // next promoted; the runtime constructor below is where it belongs.
            // **And `regclass[]` for the same reason as `regclass`**: a name per element is a
            // catalog lookup either way, and `Datum::from_text` has none. The element-level
            // spelling `ARRAY['t'::regclass]` was already here; the array spelling
            // `ARRAY['{t}'::regclass[]]` reaches the same fold with the array as its element type
            // and answered `0A000` — one door of the pair had the guard.
            if matches!(cast_to, ColumnType::RegClass | ColumnType::RegClassArray)
                || (cast_to == ColumnType::RegType && value::named_type(text)?.is_none())
            {
                return Ok(plan::Expr::Array {
                    elements: elements
                        .iter()
                        .map(lower_expr)
                        .collect::<Result<Vec<_>>>()?,
                    element: None,
                });
            }
            element = Some(cast_to);
            texts.push(Some(text.clone()));
            continue;
        }
        // **Not a constant, so the whole constructor becomes a runtime one.** `ARRAY[casttarget]`
        // is what `ActiveRecord`'s case-insensitivity probe sends, and an element that is a column
        // has no value until there is a row — so the elements are lowered as expressions and the
        // element type is settled where a scope exists (`exec::query::resolve`). Everything
        // already folded above is discarded rather than mixed in: one constructor is built one
        // way, and a half-folded one would have two rules for what its type is.
        let Expr::Value(value) = strip_nesting(expr) else {
            return Ok(plan::Expr::Array {
                elements: elements
                    .iter()
                    .map(lower_expr)
                    .collect::<Result<Vec<_>>>()?,
                element: None,
            });
        };
        // The widest element type wins, in PostgreSQL's own order: a string makes the whole array
        // `text`, a decimal makes it `numeric`, and integers alone leave it an integer array.
        let (text, wanted) = match &value.value {
            Value::Null => (None, None),
            Value::Number(digits, _) if digits.contains('.') => {
                (Some(digits.clone()), Some(ColumnType::Numeric))
            }
            // **The literal ladder, inside the constructor** (ADR 0087): an integer element is an
            // `int4` when it fits one, so `ARRAY[1,2,3]` is an `integer[]` as it is on a real
            // server, and `ARRAY[1,3000000000]` is a `bigint[]` because the widening below settles
            // on the wider of the two. A number too large for either is left to `from_text`, which
            // raises the `22003` a real server raises.
            Value::Number(digits, _) => (
                Some(digits.clone()),
                Some(match digits.parse::<i32>() {
                    Ok(_) => ColumnType::Int4,
                    Err(_) => ColumnType::Int8,
                }),
            ),
            // **A quoted string is `unknown` and contributes *no* type**, which is
            // `select_common_type`'s own rule and the one `exec::query::quantified_element_type`
            // already states one file over: an unknown takes the type the typed elements settle,
            // and only an array of nothing but unknowns is `text`. Measured on 19beta1, 2026-09-10:
            // `ARRAY['1 month'::interval, '1 year', '1 hour']` is `interval[]`,
            // `ARRAY['a'::name, 'b']` is `name[]`, `ARRAY['2020-01-01'::date, '2020-01-02']` is
            // `date[]`, `ARRAY[1, '2']` is `integer[]` with `[2]` reading `2`, and `ARRAY['x','y']`
            // and `ARRAY[NULL]` are both `text[]`.
            //
            // It said `Some(Text)` here, and "a string makes the whole array `text`" was the
            // widening rule below — so a constructor mixing a cast with a bare string came out a
            // `text[]`, which is what made
            // `INSERT INTO iv(terms) VALUES (ARRAY['1 month'::interval, '1 year'])` a
            // `42804 column "terms" is of type interval[] but expression is of type text[]` the
            // moment the constructor stopped folding into a value that `assign` could re-read.
            // Two readers of one fact, and the folded one was the one nobody had put to the
            // oracle.
            //
            // **The value is still read with the settled type**, which is what makes
            // `ARRAY[true, 'x']` `22P02 invalid input syntax for type boolean: "x"` here as it is
            // there: the texts are collected now and `Datum::from_text` runs after the loop.
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                (Some(text.clone()), None)
            }
            // **`ARRAY[true,false]` is a `boolean[]`**, and the keyword is the value: a boolean
            // literal is not an `unknown` string that happens to read as one, which is why it
            // does not widen to `text` beside a string the way a number does not either.
            Value::Boolean(value) => (
                Some(if *value { "true" } else { "false" }.to_owned()),
                Some(ColumnType::Bool),
            ),
            other => {
                return Err(SqlError::unsupported(format!(
                    "an ARRAY constructor holding {other}"
                )));
            }
        };
        element = match (element, wanted) {
            // `text` used to win over everything here, which was the other half of the arm above:
            // a bare string said `text` and `text` then swallowed the array. A `text` that arrives
            // now was **written** — `'x'::text` — and takes its turn like any other type.
            (Some(ColumnType::Numeric), _) | (_, Some(ColumnType::Numeric)) => {
                Some(ColumnType::Numeric)
            }
            // **The wider integer wins**, which the first-wins arm below cannot do: without this
            // `ARRAY[1,3000000000]` would settle on the `int4` its first element asked for and
            // then refuse its second with a `22003` that a real server does not raise.
            (Some(ColumnType::Int8), Some(ColumnType::Int4))
            | (Some(ColumnType::Int4), Some(ColumnType::Int8)) => Some(ColumnType::Int8),
            (known, None) => known,
            (None, wanted) => wanted,
            (known, _) => known,
        };
        texts.push(text);
    }
    // **All-unknown is `text`, and only an *empty* constructor has no type at all.**
    // `ARRAY['x','y']`, `ARRAY[NULL]` and `ARRAY[NULL,'x']` are each `text[]` on 19beta1;
    // `ARRAY[]` is `42P18 cannot determine type of empty array`, which is what this error says.
    if elements.is_empty() {
        return Err(SqlError::EmptyArrayType);
    }
    let element = element.unwrap_or(ColumnType::Text);
    let mut values = Vec::with_capacity(texts.len());
    for text in texts {
        values.push(match text {
            None => None,
            Some(text) => Some(Datum::from_text(element, &text)?),
        });
    }
    if let Some(stacked) = stack_folded_arrays(element, &values)? {
        return Ok(stacked);
    }
    Ok(plan::Expr::Array {
        elements: values
            .into_iter()
            .map(|value| {
                plan::Expr::Literal(match value {
                    None => plan::Literal::Null,
                    Some(value) => plan::Literal::Typed(Box::new(value)),
                })
            })
            .collect(),
        element: Some(element),
    })
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
/// An `ARRAY[…]` on the right of `= ANY`, as a list of elements **typed by the constructor**.
///
/// `lower_array_constructor` folds the whole thing to one array value; this takes that value apart
/// again, which is what the `IN`-list shape needs. Going through it rather than lowering each
/// element on its own is the point: the constructor is where the common type is chosen, and a list
/// of `unknown`s would take the column's type instead and answer where a real server raises.
fn lower_array_constructor_elements(expr: &Expr) -> Result<Option<Vec<plan::Expr>>> {
    let Expr::Array(array) = expr else {
        return Ok(None);
    };
    // **`ARRAY[]` has no elements to take a type from**, and on this side of `= ANY` it needs
    // none: an empty list matches nothing, which is what `'x' = ANY(ARRAY[]::text[])` is `f` for.
    // Asking the constructor would be `42P18 cannot determine type of empty array`, which is the
    // right answer where the array is a *value* and the wrong one here.
    if array.elem.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // **Two shapes come back, and both are the constructor's own elements.** A constructor over
    // constants keeps its node now (`debts-v1.1.md` #42), so its elements are already the list
    // this side wants; an array *of arrays* still folds to one value, and taking that apart is
    // the second arm. Anything else — a runtime constructor over columns — is `None`, and the
    // caller lowers each element itself.
    Ok(match lower_array_constructor(&array.elem)? {
        plan::Expr::Array { elements, .. } => Some(elements),
        plan::Expr::Literal(plan::Literal::Typed(value)) => match *value {
            Datum::Array(array) => Some(
                array
                    .values
                    .into_iter()
                    .map(|element| {
                        plan::Expr::Literal(match element {
                            Some(value) => plan::Literal::Typed(Box::new(value)),
                            None => plan::Literal::Null,
                        })
                    })
                    .collect(),
            ),
            _ => None,
        },
        _ => None,
    })
}

fn lower_array(expr: &Expr) -> Result<Option<Vec<plan::Expr>>> {
    Ok(Some(match expr {
        // **A constructor settles its elements' type, and a bare list does not.** `id IN ('1','2')`
        // works on a real server because each literal is `unknown` and takes the column's type;
        // `id = ANY(ARRAY['1','2'])` is `42883 operator does not exist: bigint = text`, because
        // `ARRAY['1','2']` is a `text[]` **value** — `pg_typeof` says so — and the coercion does
        // not reach into one. Lowering the constructor is what settles them: it reads each element
        // at the array's own element type, so what comes out is typed rather than `unknown`, and
        // the comparison then refuses exactly where a real server does.
        Expr::Array(_) => match lower_array_constructor_elements(expr)? {
            Some(elements) => elements,
            None => return Ok(None),
        },
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
        // **A `regclass[]` keeps its cast**, because its elements are *names* and reading one
        // needs the catalog. Dropped, `'rc'::regclass = ANY('{rc}'::regclass[])` became the `IN`
        // list `('rc')` of bare strings, and the comparison then read `rc` at the left operand's
        // type through `Datum::from_text`, which has no catalog to ask: `0A000` for a statement a
        // real server answers `t`. Answering `None` here keeps it an expression, and the array is
        // built by the resolution that *does* have one.
        Expr::Cast {
            data_type: DataType::Array(inner),
            ..
        } if array_element(inner)
            .is_some_and(|inner| cast_target(inner) == Some(CastTarget::RegClass)) =>
        {
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
    // **`regclass[]` is lowered before the general path**, because its element's input function
    // is a catalog lookup and `Datum::from_text` has no catalog. A literal's names become one
    // `CatalogFunc::RegClass` each — the same resolution `'x'::regclass` gets, so a name nothing
    // answers to is the same `42P01` in the same place — and anything else becomes an ordinary
    // cast the row evaluator answers, where `env` is.
    if let DataType::Array(inner) = data_type
        && let Some(inner) = array_element(inner)
        && cast_target(inner) == Some(CastTarget::RegClass)
    {
        return lower_regclass_array(expr).map(Some);
    }
    if let DataType::Array(inner) = data_type
        && let Some(element) = array_element(inner)
        && let Ok((element, NO_TYPMOD)) = lower_type(element)
        && let Some(array) = esker_keys::array::ArrayValue::array_over(element)
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

/// `'{pg_class,pg_type}'::regclass[]`, and every other operand cast to one.
///
/// **Two doors, because a `regclass` has two directions and an array does not change that.** A
/// string literal is `array_in` over `regclassin`: the text is split into element *names*, each
/// resolved once per statement by the same [`plan::CatalogFunc::RegClass`] a scalar
/// `'x'::regclass` lowers to. That is what keeps `'{nosuchrel}'::regclass[]` the `42P01` a real
/// server raises, in the same place and with the same message, rather than a `22P02` from an
/// integer parser that was handed a name.
///
/// Anything else — an `oid[]` column, an `array_agg` — is an ordinary cast to
/// [`ColumnType::RegClassArray`], resolved per row in `exec::cursor`, which is the only layer that
/// has a catalog to ask.
///
/// The elements are split by `array_in` itself, read at `text`: quoting, escapes, `NULL` and the
/// `[1:2]=` bound prefix are that function's rules, and a second reader of the same grammar is how
/// two spellings of one literal come to disagree.
fn lower_regclass_array(expr: &Expr) -> Result<plan::Expr> {
    // `ARRAY[]::regclass[]` is the empty array, as it is at every other element type: the
    // constructor has no element to take a type from and the cast is what supplies one.
    if matches!(strip_nesting(expr), Expr::Array(array) if array.elem.is_empty()) {
        return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
            Datum::Array(esker_keys::array::ArrayValue::empty(ColumnType::RegClass)),
        ))));
    }
    // **A string literal is a name and everything else is a value**, which is the scalar rule one
    // dimension up. Asking `cast_literal_text` instead read through the cast in
    // `'{1259}'::oid[]::regclass[]` and handed `1259` to the *name* lookup, which is
    // `42P01 relation "1259" does not exist` for a statement a real server answers `{pg_class}`.
    let Some(text) = (if is_string_literal(expr) {
        cast_literal_text(expr)?
    } else {
        None
    }) else {
        return Ok(plan::Expr::Cast {
            operand: Box::new(lower_expr(expr)?),
            to: ColumnType::RegClassArray,
            typmod: NO_TYPMOD,
        });
    };
    let literal = value::array::from_text(&text, ColumnType::Text)?;
    let elements = literal
        .values
        .iter()
        .map(|value| match value {
            None => plan::Expr::Literal(plan::Literal::Null),
            Some(element) => plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::RegClass,
                args: vec![plan::Expr::Literal(plan::Literal::String(
                    element.to_text().unwrap_or_default(),
                ))],
            })),
        })
        .collect();
    Ok(plan::Expr::Array {
        elements,
        element: Some(ColumnType::RegClass),
    })
}

/// `'x'::regclass::text` — **the relation's name**, not the digits its oid prints as.
///
/// A `regclass` on a real server is an oid whose *output function* is the name, so the text of one
/// is `pg_class` and never `16xxx`. Both halves of that were already here: the forward cast
/// resolves a name to an oid once per statement, and [`plan::CatalogFunc::RegClassName`] is the
/// inverse per row. Composing them is the whole of it, and it keeps the `42P01` — a name nothing
/// answers to fails in the forward half, before anything prints.
///
/// **The forward form only.** `t.oid::regclass::text` is already the inverse, since the inner cast
/// lowers to `RegClassName` on its own; wrapping that again would ask the name-of-an-oid function
/// for the name of a name, which is `42804`.
fn lower_regclass_text(expr: &Expr, data_type: &DataType) -> Result<Option<plan::Expr>> {
    if !matches!(data_type, DataType::Text) {
        return Ok(None);
    }
    let Expr::Cast {
        expr: inner,
        data_type: inner_type,
        ..
    } = expr
    else {
        return Ok(None);
    };
    if cast_target(inner_type) != Some(CastTarget::RegClass) || !is_string_literal(inner) {
        return Ok(None);
    }
    // **The `::text` is a cast now**, not the identity on what `RegClassName` answers. That
    // function returned a `Datum::Text` while a `regclass` was described as 25; it answers a
    // `regclass` since it had to be described as 2205, so composing the two without this would
    // give `'x'::regclass::text` the type `regclass` — right characters, wrong declared type, in
    // the shape `ActiveRecord`'s `foreign_keys()` reads.
    Ok(Some(plan::Expr::Cast {
        operand: Box::new(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
            func: plan::CatalogFunc::RegClassName,
            args: vec![lower_cast(inner, inner_type)?],
        }))),
        to: ColumnType::Text,
        typmod: NO_TYPMOD,
    }))
}

/// One operand of `AND`/`OR`, with an unadorned string literal read as a boolean.
///
/// PostgreSQL types a bare literal from its context, so `'true'` in a boolean position *is* a
/// boolean and `'text'` is `22P02` — the value could not be read as one, which is a different
/// answer from a column whose declared type is wrong (`42804`). `boolean` is false everywhere
/// else, and then this is [`lower_expr`].
fn lower_condition(expr: &Expr, boolean: bool) -> Result<plan::Expr> {
    if boolean
        && let Expr::Value(value) = expr
        && let Value::SingleQuotedString(text) = &value.value
    {
        return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
            <Datum as PgDatum>::from_text(ColumnType::Bool, text)?,
        ))));
    }
    lower_expr(expr)
}

/// `IS [NOT] DISTINCT FROM`, **re-associated around an `AND` or `OR` the parser swallowed**.
///
/// `sqlparser` 0.62.0 reads the right operand of these two with a precedence *below* `AND` and
/// `OR`, so `a IS NOT DISTINCT FROM b AND c IS NOT DISTINCT FROM d` arrives as
/// `a IS NOT DISTINCT FROM (b AND c IS NOT DISTINCT FROM d)` — one comparison swallowing a whole
/// conjunction. PostgreSQL's grammar puts `IS` above `NOT`, `AND` and `OR` and below everything
/// else, so the fix is a rotation and not a guess: the swallowed operator moves out and the
/// comparison closes over the operand it should have had.
///
/// **Only `AND` and `OR` move.** Every other binary operator — `=`, `<`, `+`, `||` — binds
/// *tighter* than `IS` on a real server, so a right operand that is one of those was parsed
/// correctly and must stay where it is.
///
/// The shape is what `ActiveRecord`'s `upsert_all` writes (`postgresql_adapter.rb:675`), which is
/// how it was found: seven tests answering `42804 argument of AND/OR must be type boolean` because
/// the `AND`'s left operand was a *column's value* rather than a comparison.
fn lower_distinct(left: &Expr, right: &Expr, op: plan::BinaryOp) -> Result<plan::Expr> {
    if let Expr::BinaryOp {
        left: inner_left,
        op: inner_op,
        right: inner_right,
    } = right
    {
        let boolean = match inner_op {
            BinaryOperator::And => Some(plan::BinaryOp::And),
            BinaryOperator::Or => Some(plan::BinaryOp::Or),
            _ => None,
        };
        if let Some(boolean) = boolean {
            return Ok(plan::Expr::Binary {
                op: boolean,
                left: Box::new(lower_distinct(left, inner_left, op)?),
                right: Box::new(lower_expr(inner_right)?),
            });
        }
    }
    Ok(plan::Expr::Binary {
        op,
        left: Box::new(lower_expr(left)?),
        right: Box::new(lower_expr(right)?),
    })
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
#[expect(
    clippy::too_many_lines,
    reason = "one arm per cast shape, and each arm is a measured answer; splitting it would \
              hide which shapes are folded at plan time and which are not"
)]
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
    if let Some(printed) = lower_regclass_text(expr, data_type)? {
        return Ok(printed);
    }
    let Some(target) = cast_target(data_type) else {
        // **A cast to a name the catalog might know**, and it is asked *after* `cast_target`,
        // because `regclass`, `regtype` and `oid` are `DataType::Custom` too — every one of them
        // is a name `sqlparser` has no type for. `lower_type` refuses what is left because this
        // node's own type table does not have it, which is the right answer for a typo and the
        // wrong one for a type somebody declared: the same place `CREATE TABLE t (c mood)` was
        // before `lower_column_type` learned to carry the name. Carried here too and resolved once
        // per statement (ADR 0053); the operand goes with it, because the label is what is looked
        // up.
        // **A *qualified* name is carried too**, which is what `'some text'::schema_1.text` needs:
        // `schema_test.rb` puts a `CREATE DOMAIN schema_1.text` in a schema of its own and then
        // casts to it. This required a single part and so refused every one of them with
        // `0A000 the type schema_1.text is not supported`, while the *column-type* path beside it
        // resolved the same spelling — the parser was shared and the lookups were not. The name
        // goes on whole and `value::split_type_name` reads it where the catalog is.
        if lower_type(data_type).is_err()
            && let DataType::Custom(name, modifiers) = data_type
            && modifiers.is_empty()
            && (1..=2).contains(&name.0.len())
            && !is_serial_spelling(data_type)
            && name.0.iter().all(|part| part.as_ident().is_some())
        {
            return Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::UserCast,
                args: vec![
                    plan::Expr::Literal(plan::Literal::String(name.to_string())),
                    lower_expr(expr)?,
                ],
            })));
        }
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
        // **An address cast keeps the prefix the output function hides, and `inet::cidr` masks
        // rather than refuses.** Both were measured and both differ from "print it and read it
        // back": `'192.168.1.1'::inet::text` is `192.168.1.1/32` where the field is
        // `192.168.1.1`, and `'192.168.1.5/24'::inet::cidr` is `192.168.1.0/24` where the same
        // text handed to `cidr_in` is `22P02 invalid cidr value`.
        if let Some(from) = source_type(expr)?
            && matches!(from, ColumnType::Inet | ColumnType::Cidr)
            && let Some(to) = lower_type(data_type).ok().map(|(ty, _)| ty)
            && let Some(text) = cast_literal_text(expr)?
        {
            let address = value::inet::from_text(&text, from == ColumnType::Cidr)?;
            match to {
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                    return Ok(plan::Expr::Literal(plan::Literal::String(
                        value::inet::to_cast_text(&address),
                    )));
                }
                ColumnType::Cidr | ColumnType::Inet => {
                    let cidr = to == ColumnType::Cidr;
                    let address = if cidr {
                        value::inet::masked_to_cidr(&address)
                    } else {
                        address
                    };
                    return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                        Datum::Inet {
                            family: address.family,
                            bits: address.bits,
                            cidr,
                            addr: address.addr,
                        },
                    ))));
                }
                _ => {}
            }
        }
        // **A `"char"` casts to and from an `int4` as the *byte*, not as the printed text.**
        // Measured: `'r'::"char"::int4` is 114 and `65::int4::"char"` is `A`, both **explicit**
        // casts in `pg_cast`. Through the ordinary text path the first was
        // `22P02 invalid input syntax for type integer: "r"`, which is the same shape
        // `uuid::bytea` had — a pair with a real conversion read as a re-parse.
        if source_type(expr)? == Some(ColumnType::Char)
            && lower_type(data_type).ok().map(|(ty, _)| ty) == Some(ColumnType::Int4)
            && let Some(text) = cast_literal_text(expr)?
        {
            // **Signed**, which one measurement is enough to see and reasoning is not:
            // `chr(200)::"char"::int4` is `-61` on 19beta1, not `195`. A `"char"` is one *byte*
            // and PostgreSQL's `chartoi4` reads it as `int8`, the C type, which is signed.
            return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Int4(value::char_type::to_int4(&text)),
            ))));
        }
        // The other direction, and it is the same fact: the number **is** the byte, so `65` is `A`
        // and not the first character of `65`.
        // **`int4` and no other width.** `pg_cast` has one row into a `"char"` from a number and
        // it is `integer`: `2::int2::"char"` and `2::int8::"char"` are each
        // `42846 cannot cast type smallint to "char"` on 19beta1, measured, and this arm used to
        // fold all three — two casts a real server refuses, answered. The gate below is what
        // refuses them now, and it is `pg_cast`'s own table doing it.
        if source_type(expr)? == Some(ColumnType::Int4)
            && lower_type(data_type).ok().map(|(ty, _)| ty) == Some(ColumnType::Char)
            && let Some(text) = cast_literal_text(expr)?
        {
            // **And only `-128..=127` is a byte.** `200::int4::"char"` is
            // `22003 "char" out of range` on a real server, where this arm used to wrap the value
            // round with a `rem_euclid` and answer — a number outside the type answering as if it
            // were inside it. The rule and the message are `value::convert_without_text`'s, so the
            // fold and the per-row cast cannot disagree about where the type ends.
            let number = text.parse::<i64>().unwrap_or(0);
            let converted = i32::try_from(number)
                .ok()
                .and_then(|number| {
                    value::convert_without_text(&Datum::Int4(number), ColumnType::Char)
                })
                .unwrap_or_else(|| {
                    Err(SqlError::IntegerLiteralOutOfRange(ColumnType::Char.name()))
                })?;
            // **And the node stays**, because a `"char"` is a `Datum::Text` and a `Datum::Text`
            // says `text`: `pg_typeof(65::int4::"char")` is `"char"` on a real server and was
            // `text` here, which is ADR 0086's rule with a third type in it — the value cannot
            // carry the type it was given, so the cast that gave it stays to say so.
            return Ok(plan::Expr::Cast {
                operand: Box::new(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                    converted,
                )))),
                to: ColumnType::Char,
                typmod: NO_TYPMOD,
            });
        }
        // **A `jsonb`'s kind is checked before its value is read, here too.** The evaluator's
        // `Expr::Cast` arm has this rule for a cast over a *column*; a cast over a **literal**
        // never reaches it, and `'{"a":1}'::jsonb::numeric` written out in full is exactly the
        // spelling a person tries first. One function, both readers
        // (`crate::value::json::cast_to_scalar`, `debts-v1.1.md` #44).
        if source_type(expr)? == Some(ColumnType::Jsonb)
            && let Ok((to, NO_TYPMOD)) = lower_type(data_type)
            && value::json::casts_to_scalar(to)
            && let Some(text) = cast_literal_text(expr)?
        {
            return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                value::json::cast_to_scalar(&text, to)?,
            ))));
        }
        // **`money::numeric` is the cents as a decimal, not the printed money read back.** The
        // output function writes `$567.89` and `numeric`'s input function refuses it, so the
        // ordinary text path made a conversion a real server performs into a `22P02` about the
        // dollar sign. The other direction needs nothing: `567.89` and `12345` are both spellings
        // `cash_in` reads.
        if source_type(expr)? == Some(ColumnType::Money)
            && lower_type(data_type).ok().map(|(ty, _)| ty) == Some(ColumnType::Numeric)
            && let Some(text) = cast_literal_text(expr)?
        {
            let cents = value::money::from_text(&text)?;
            return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::from_text(ColumnType::Numeric, &value::money::to_numeric_text(cents))?,
            ))));
        }
        // **A geometric conversion is computed too, and this is the second caller asking for it.**
        // The evaluator's `Cast` arm performs the fourteen per row; a literal never reaches it, and
        // the fold below reads the source's *text* with the target's input function — which for
        // four of the fourteen is readable and wrong. `'((0,0),(1,1))'::box::polygon` folded to the
        // two-point polygon `((1,1),(0,0))` where a real server gives the four corners, and an
        // **open** `'[(0,0),(1,1)]'::path::polygon` folded silently where a real server refuses it
        // `22023`. Wrong and green, both, and neither reachable from a column — which is why
        // `tests/corpus/pg19_geometric.txt` takes all fourteen through a literal and
        // `tests/cast_matrix.rs` takes them through a column.
        //
        // `line` is a shape with no conversions and reaches [`value::geometric_cast`] too, which
        // answers the same `42846` a real server does — it has no `pg_cast` row either way.
        if let Some(from) = source_type(expr)?
            && let Ok((to, _)) = lower_type(data_type)
            && from != to
            && value::is_geometric(from)
            && value::is_geometric(to)
            && let Some(text) = cast_literal_text(expr)?
        {
            return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                value::geometric_cast(&Datum::from_text(from, &text)?, to)?,
            ))));
        }
        // **The permission is `pg_cast`'s, and the fold has to ask it too.** `casts_to` is the one
        // gate for a cast over an *expression* (`exec::query`), and a folded literal never reached
        // it: `'101'::bit(3)::int2` read the digits as decimal and answered `101` where a real
        // server has no such row and says `42846`, and `5::int2::bit(4)` blamed the *value* with a
        // `22P02` for a pair that does not exist at all. Two readers of one fact, and this is the
        // second asking the first.
        //
        // **Asked for every pair, and it used to be a list.** It began with the bit strings,
        // gained `"char"` the day `int2 -> "char"` was found folding where a real server says
        // `42846`, and gained `money`, `regproc` and `regtype` the day after — three families in a
        // row, each found by a probe rather than by reading, each a pair this node **folded** and
        // a real server refuses (`int2 -> "char"`, `2::int2::money`, `'int4in'::regproc::int2`).
        // A list that grows every time somebody measures is the wrong shape.
        //
        // `casts_to` is `pg_cast`'s own table, and `exec::query` asks it for every cast over an
        // *expression*; there is no reason a cast over a **constant** should be licensed by a
        // different rule, and every time the two rules differed the constant was the wrong one.
        // Two readers of one fact, reading it the same way (`debts-v1.1.md` #43).
        //
        // **`refused_cast` runs first and keeps its own sentences** — the `date`/number pairs, the
        // `money` asymmetry, a `numeric` NaN — so this widens what is refused and changes nothing
        // that was already refused. What it adds is the pairs nobody had written a rule for.
        if let Some(from) = source_type(expr)?
            && let Ok((to, _)) = lower_type(data_type)
            && !catalog::pg_catalog::casts_to(from, to)
        {
            return Err(SqlError::CannotCast {
                from: from.name(),
                to: to.name(),
            });
        }
        // **A bit string and an integer convert; the fold below would read the digits.**
        // `5::int4::bit(4)` is `0101` on a real server and `cast_literal_text` hands the fold the
        // characters `5`, which `bit`'s input function refuses — `22P02 "5" is not a valid binary
        // digit` for a statement that has an answer. The other direction is worse, because it has
        // no refusal to stop it: `'101'::bit(3)::int` read `101` as decimal and answered `101`
        // where a real server says `5`. So neither is folded here; the `Cast` node carries the
        // typmod — the width the integer is written in, which is the whole of `bit(4)` — down to
        // `exec::cursor`, where `value::bit` holds both measured rules.
        if let Some(from) = source_type(expr)?
            && let Ok((to, typmod)) = lower_type(data_type)
            && (matches!(from, ColumnType::Int4 | ColumnType::Int8) && to == ColumnType::Bit
                || from == ColumnType::Bit && matches!(to, ColumnType::Int4 | ColumnType::Int8))
        {
            return Ok(plan::Expr::Cast {
                operand: Box::new(lower_expr(expr)?),
                to,
                typmod,
            });
        }
        if source_type(expr)? == Some(ColumnType::Numeric)
            && let Some(to) = lower_type(data_type).ok().map(|(ty, _)| ty)
            && matches!(to, ColumnType::Int8 | ColumnType::Int4 | ColumnType::Int2)
            && let Some(text) = cast_literal_text(expr)?
        {
            // **The rounding is kept under a `Cast` node, because rounding is not invertible.**
            // `(-1.5)::integer` is `-2`, and no rule over `-2` can say the expression was written
            // with `-1.5` — a real server holds `('-1.5'::numeric)::integer` and prints that
            // (`debts-v1.1.md` #30). The value is unchanged either way; what the node keeps is the
            // number that was written. Third clause of the same sentence ADR 0086 opened: a datum
            // carries its type, never its modifier, and never the value it was converted *from*.
            // `numeric_to_integer` still runs, because its **range** error belongs at parse time:
            // `(1e30)::integer` is `22003` before any row exists. Its value is discarded — the
            // conversion happens per row under the node, which is where a real server does it too.
            numeric_to_integer(&text, to)?;
            return Ok(plan::Expr::Cast {
                operand: Box::new(lower_expr(expr)?),
                to,
                typmod: NO_TYPMOD,
            });
        }
        return match cast_literal_text(expr)? {
            Some(text) => {
                // **The typmod applies**, which is the whole difference between `::timestamp` and
                // `::timestamp(3)`: the second rounds. Dropping it here read the text and then
                // ignored the number beside it, so `'…123456'::timestamp(3)` kept its microseconds
                // where a real server rounds to `.123`. Same function the write path uses, so a
                // cast and an `INSERT` cannot disagree about what `(3)` means.
                let (ty, typmod) = lower_type(data_type)?;
                // **A cast, so the cast's rule** — `truncate_to_typmod` and not
                // `fit_to_typmod`, which is the row write's. Asking the wrong one made
                // `'abcdef'::varchar(3)` a `22001` where a real server answers `abc`, and it was
                // invisible from the *other* side of the same seam, where `bit` was wired the
                // opposite way round (`debts-v1.1.md` #36).
                // **A conversion a real server performs without the text is performed here
                // too.** The line below is the fold's I/O conversion — the literal's characters
                // read by the *target's* input function — and for a pair whose `pg_cast` method is
                // `f` or `b` that is the wrong road: `1.5::float8::int4` was
                // `22P02 invalid input syntax for type integer: "1.5"` for a value a real server
                // rounds to `2`, and `'\x4142'::bytea::int4` was the same shape. The value this
                // produces then goes through exactly the same questions as any other — the
                // `fold_keeps_the_digits` guard below keeps the node when the conversion loses
                // the digits, which is why `1.5::float8::int4` still *prints* as the cast it was
                // written as. Same table the evaluator asks (`debts-v1.1.md` #43).
                //
                // A `numeric` source is left out on purpose: its road is already below and is
                // #30's, and short-circuiting it here would fold away a node a real server prints.
                let converted = match source_type(expr)? {
                    Some(from) if from != ty && from != ColumnType::Numeric => {
                        Datum::from_text(from, &text)
                            .ok()
                            .and_then(|datum| value::convert_without_text(&datum, ty))
                            .transpose()?
                    }
                    _ => None,
                };
                let value = match converted {
                    Some(value) => value,
                    None => value::truncate_to_typmod(Datum::from_text(ty, &text)?, ty, typmod)?,
                };
                // **A folded cast still carries the type it named.** Several types share one
                // `Datum` — `text`, `varchar`, `bpchar` and `name` are all a `Datum::Text` — so
                // folding `'x'::name` to its value alone threw the *declared* type away and the
                // `RowDescription` said `text`, OID 25, where a real server says 19. A column of
                // the type answered correctly all along; it was the bare cast that could not,
                // which is why no corpus saw it (`tests/captures/pg19_name_array.txt`).
                //
                // The `Cast` node is kept only when the value cannot speak for itself. It is a
                // no-op on the value — the datum below it is already this type's — and it is what
                // `expr_type` reads.
                //
                // **A modifier is the second thing a datum cannot say.** `1.5::numeric(10,2)` fits
                // to `1.50` and the datum is a `numeric` like any other, so the fold above used to
                // apply and the `RowDescription` said bare `numeric` where a real server says
                // `numeric(10,2)` — `debts-v1.1.md` #28, and the same sentence this comment
                // already makes one clause further: a value carries its type and never its
                // modifier. Keeping the node costs a no-op cast at evaluation and is what
                // `exec::query::typmod_of` reads.
                // **The digits that were written, under the node.** Where the conversion lost
                // them, the operand has to be the literal as it was — folding it and then wrapping
                // the *result* would keep a node over a constant nobody wrote. Same shape as the
                // rounding arm above, and the conversion happens per row, which is where a real
                // server does it.
                if !fold_keeps_the_digits(&text, ty, &value) {
                    return Ok(plan::Expr::Cast {
                        operand: Box::new(lower_expr(expr)?),
                        to: ty,
                        typmod,
                    });
                }
                // **A fold that discards a node PostgreSQL prints is lossy too**, which is the
                // whole of `debts-v1.1.md` #42. The arm above keeps the node when the conversion
                // loses *digits*; this keeps it when the literal's **own type** is not the
                // target's — which is exactly when a real server keeps it. Measured on 19beta1
                // through `pg_get_expr` over a `DEFAULT`:
                //
                // ```text
                // (1)::bigint      (1)::bigint                (1)::numeric   (1)::numeric
                // (1.5)::float8    (1.5)::double precision    (1)::smallint  (1)::smallint
                // (-1)::bigint     ('-1'::integer)::bigint    (-1)::text     ('-1'::integer)::text
                // (-1.5)::numeric  '-1.5'::numeric            <- the one that folds: same type
                // ```
                //
                // The last row is the rule stated from the other side, and it is why this is a
                // type comparison rather than a list: a real server folds a cast away exactly
                // when the constant already *is* the target type, and keeps it otherwise.
                //
                // **The operand is the literal as written, not the folded value**, for the reason
                // the digits arm gives one clause up: folding it and then wrapping the result
                // would keep a node over a constant nobody wrote. An unknown literal has no type
                // of its own, so `'x'::jsonb` and `'2020-01-01'::date` are untouched by this and
                // still fold — which is what a real server does with them too.
                //
                // [`source_type`] already reads through a *nested* cast, and that is what makes
                // `((1)::bigint)::text` two nodes rather than one `Datum::Text`: the outer cast
                // sees `bigint` where it used to see the text the inner one had folded to.
                //
                // **The array half of this is not here and is not closed.** A *typed array
                // literal* — `'{1,2}'::int[]`, and an `ARRAY[…]` of constants — is folded to its
                // own text one layer down, in [`lower_array_constructor`], so the element type is
                // lost before this arm ever sees it: `SELECT ARRAY['{1,2}'::int[]]` answers
                // `text` where a real server answers `integer[]`, and `'{pg_class}'::regclass[]`
                // is `0A000`. Same shape as the rule above, different site, and it is b4's —
                // pinned by the `#[ignore]`d expectations in `tests/array_of_array.rs`, which is
                // the file to read before touching the fold below.
                // **For every target, and it used to be only `text`.** `ToText` is the target
                // type's *output* function, which every type has, so deferring a cast to `text`
                // to evaluation always cost nothing; a cast to anything else has to be performed
                // by the evaluator's own `Cast` arm, and that arm once knew fewer conversions
                // than this fold did. Keeping the node for those turned three green tests red at
                // once, each naming a different half of the same gap, and they were left written
                // here so the next attempt would start from them:
                //
                // ```text
                // 567.89::numeric::money   42846 cannot cast type numeric to money
                // 1::money UNION 2::numeric   the refusal changed sentence
                // 1::oid = 1::int8         42883 operator does not exist: oid = bigint
                // ```
                //
                // **All three are paid, and re-measuring is what said so** rather than reading:
                // widening the guard again turned **one** of the three red, not three. The first
                // two went when `debts-v1.1.md` #43's first mechanism closed — the evaluator has
                // every `pg_cast` conversion now, all 38 — and nobody had gone back to say the
                // blocker had shrunk.
                //
                // The third was never an evaluator gap at all: a comparison retypes a *literal*
                // against the other side, and a `Cast` node is not a literal, so `1::int8` stopped
                // being something `reconcile` could meet an `oid` with. It cost three fixes, each
                // hidden behind the one before it and each a second reader of one measured fact —
                // `Literal::comparable_with` and `exec::query::retype` both asked the
                // **assignment** rule about an oid-ish literal, and `Datum::pg_cmp` had no arm for
                // an oid against an `int2` or an `int4` at all. That last one was answering
                // `Equal` for every such pair, which `tests/oid_type.rs` now pins.
                //
                // So this is `debts-v1.1.md` #42's remaining half: a cast whose operand is not
                // already of the target type keeps its node, and the constant under it is the one
                // that was written.
                if let Some(from) = source_type(expr)?
                    && from != ty
                {
                    if ty == ColumnType::Text {
                        return Ok(plan::Expr::ToText {
                            operand: Box::new(lower_expr(expr)?),
                            strip_blanks: false,
                            enum_labels: None,
                        });
                    }
                    return Ok(plan::Expr::Cast {
                        operand: Box::new(lower_expr(expr)?),
                        to: ty,
                        typmod,
                    });
                }
                if value.column_type() == Some(ty) && typmod == NO_TYPMOD {
                    return Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(value))));
                }
                Ok(plan::Expr::Cast {
                    operand: Box::new(plan::Expr::Literal(plan::Literal::Typed(Box::new(value)))),
                    to: ty,
                    typmod,
                })
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
            // **Every other target, per row.** `SELECT $1::integer` is what `connection_test.rb`
            // sends and `CURRENT_TIMESTAMP::date` is what `assignment_cast_date.rs` has carried as
            // a declared divergence: neither operand is a constant, so neither folds, and a cast
            // to anything but `text` had nothing to become.
            //
            // **Permission comes from `pg_cast`**, the rows a client can read — a pair with none
            // is `42846`, which is what `'2020-01-01'::date::int` is on a real server. Reusing the
            // table rather than writing a second one is what keeps the two answers the same.
            None => match lower_type(data_type) {
                Ok((to, typmod)) => {
                    let operand = lower_expr(expr)?;
                    Ok(plan::Expr::Cast {
                        operand: Box::new(operand),
                        to,
                        typmod,
                    })
                }
                Err(_) => Err(SqlError::unsupported(format!("a cast to {data_type}"))),
            },
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
            // **A column's `::regtype::oid` is two casts, run.** This arm answers the *literal*
            // form from the name alone — `'int4'::regtype::oid` is 23 without a row — and a
            // column has no name to read at parse time, so it takes the ordinary path: the
            // `regtype` is built per row and the `::oid` is the reinterpretation `pg_cast` calls
            // implicit. Without this, `t::regtype::oid` was
            // `0A000 the cast t::oid is not supported`, which names a cast nobody wrote.
            let Ok(name) = cast_operand(inner, data_type) else {
                return Ok(plan::Expr::Cast {
                    operand: Box::new(lower_expr(expr)?),
                    to: ColumnType::Oid,
                    typmod: NO_TYPMOD,
                });
            };
            // **A name the catalog might know**, which is where `ActiveRecord`'s
            // `lookup_cast_type` lands: `SELECT 'color'::regtype::oid` over a type a
            // `CREATE TYPE` made. Lowering has no catalog, so the name is carried and the
            // executor answers (ADR 0053), exactly as a cast *to* a user type already is.
            let Some(named) = value::named_type(&name)? else {
                return Ok(user_regtype(&name, true));
            };
            // **An `oid`, not a `bigint`.** `'23'::oid` has been a real `ColumnType::Oid` since
            // that type's own unit and this spelling had not caught up, so
            // `pg_typeof('int4'::regtype::oid)` answered `bigint` where a real server says `oid`.
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                Datum::Oid(named.oid()),
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
            // **The `::oid` is a cast now, not a no-op.** It was dropped here while a `regclass`
            // *was* a `bigint` and the pair meant the same value; a `regclass` is its own type
            // since it had to be described as 2205, so `'rc'::regclass::oid` has to actually
            // convert — measured, `'pg_class'::regclass::oid` is `1259` and `::text` of that is
            // the digits, where `::text` of the `regclass` is the name.
            Ok(plan::Expr::Cast {
                operand: Box::new(lower_regclass(inner, data_type)?),
                to: ColumnType::Oid,
                typmod: NO_TYPMOD,
            })
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
        // **The inverse, and per row**: an oid rather than a name. `t.typelem::regtype` is how
        // `ActiveRecord` reads what an array type is over, and its operand is a catalog column.
        // `23::regtype` is `integer` on a real server too, so a number goes this way as well.
        (CastTarget::RegType, _) if !is_string_literal(expr) => {
            Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::RegTypeName,
                args: vec![lower_expr(expr)?],
            })))
        }
        // `ARRAY[…]::oidvector`, which `ActiveRecord`'s case-insensitivity probe compares against
        // `pg_proc.proargtypes`. The elements' oids, space separated — digits and not names.
        (CastTarget::OidVector, _) => {
            // **A string is read as a vector, an array is built into one.** `'23 25'::oidvector`
            // was `42846 cannot cast type text to oidvector` here while `'1 2'::int2vector`
            // answered — the same spelling one type over, and the asymmetry was only that
            // `oidvector` is a `CastTarget` (`sqlparser` has no `DataType` for it) while
            // `int2vector` reaches `lower_type` and the ordinary literal path.
            if let Some(text) = cast_literal_text(expr)?
                && is_string_literal(expr)
            {
                // **Under a `Cast` node, because the datum cannot say what it is.** A vector is a
                // `Datum::Text` here, so folding to the value alone answered `text` from
                // `pg_typeof` where a real server says `oidvector` — the sentence ADR 0086 is,
                // reached by a path that does not go through the fold that carries it.
                return Ok(plan::Expr::Cast {
                    operand: Box::new(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                        Datum::from_text(ColumnType::OidVector, &text)?,
                    )))),
                    to: ColumnType::OidVector,
                    typmod: NO_TYPMOD,
                });
            }
            Ok(plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
                func: plan::CatalogFunc::OidVector,
                args: vec![lower_expr(expr)?],
            })))
        }
        // `'integer'::regtype` on its own, which answers the name PostgreSQL prints it by.
        (CastTarget::RegType, _) => {
            let name = cast_operand(expr, data_type)?;
            let Some(named) = value::named_type(&name)? else {
                return Ok(user_regtype(&name, false));
            };
            // **A `regtype`, not the text of one** (ADR 0077). It prints identically — the name
            // — so every answer that only reads the value is unchanged; what moves is
            // `pg_typeof`, which now says `regtype` as a real server does, and the comparison,
            // which is the oid's and so meets `castsource` and `proargtypes` where they are.
            Ok(plan::Expr::Literal(plan::Literal::Typed(Box::new(
                value::regtype_of_oid(named.oid()),
            ))))
        }
        // `'23'::oid`. **This is a cast to a real type now**, not a special form that happens to
        // read digits: `oid` is `ColumnType::Oid` since its own unit, so the reading is
        // `value::oid::from_text` and a negative one wraps instead of being refused. The two
        // arms above still come first, because `'x'::regtype::oid` is asking a different
        // question — what OID does this *name* have — and answers before any value is read.
        (CastTarget::Oid, _) => {
            // **A cast of an expression is an ordinary cast.** `oid` reaches this function at all
            // only because `sqlparser` has no `DataType` for it, so the name arrives as a custom
            // one; folding a literal here is a convenience, and a *column* has nothing to fold.
            // Without this arm `typinput::oid` — a statement a real server answers — was
            // `0A000 the cast typinput::oid is not supported` (ADR 0098).
            let Ok(text) = cast_operand(expr, data_type) else {
                return Ok(plan::Expr::Cast {
                    operand: Box::new(lower_expr(expr)?),
                    to: ColumnType::Oid,
                    typmod: NO_TYPMOD,
                });
            };
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

/// Which of `json` and `jsonb` a cast wrote, for the `->` that has to answer one of them.
///
/// The **outermost** cast wins, which is what `'{"a":1}'::json::jsonb -> 'a'` asks for.
fn json_cast_type(expr: &Expr) -> Option<ColumnType> {
    match expr {
        Expr::Nested(inner) => json_cast_type(inner),
        Expr::Cast {
            expr, data_type, ..
        } => match data_type {
            DataType::JSON => Some(ColumnType::Json),
            DataType::JSONB => Some(ColumnType::Jsonb),
            _ => json_cast_type(expr),
        },
        _ => None,
    }
}

/// The arithmetic operator a token is, or `None` for one that compares or combines.
///
/// `^` is exponentiation here and `#` is the bitwise XOR, which is the one pairing a reader
/// coming from C gets wrong: the two symbols swap meanings against every other language.
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
        BinaryOperator::BitwiseAnd => plan::ArithOp::BitAnd,
        BinaryOperator::BitwiseOr => plan::ArithOp::BitOr,
        BinaryOperator::PGBitwiseXor => plan::ArithOp::BitXor,
        BinaryOperator::PGBitwiseShiftLeft => plan::ArithOp::ShiftLeft,
        BinaryOperator::PGBitwiseShiftRight => plan::ArithOp::ShiftRight,
        _ => return None,
    })
}

/// Whether an operator compares, as against combines.
/// A comparison operator's symbol, as `'static` text an error message can hold.
///
/// `BinaryOperator`'s `Display` says the same thing and gives a `String`; [`SqlError`]'s operator
/// field is `&'static str`, so the six that [`is_comparison`] admits are written out. Anything
/// else cannot reach here and answers `=`, which is the operator every implied comparison is.
fn comparison_symbol(op: &BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        _ => "=",
    }
}

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
    // **One rule, one place.** The rounding lives beside every other cast between two datums
    // (`value::numeric_to_integer`), because the fold is no longer the only caller: a lossy cast
    // keeps its node and converts per row (`debts-v1.1.md` #30). Asked here only for the
    // **range** error, which belongs at parse time.
    value::numeric_to_integer(value::numeric::from_text(text)?, to)
}

/// The `42846` a pair of types with no cast between them gets, or `None` for a pair that has one.
///
/// Only the pairs a `date` is one half of, because it is the only type here that PostgreSQL
/// refuses to cast to a number: every other pair in this crate either has a cast or fails on the
/// value. Both directions, measured — `'2020-01-01'::date::int` and `1::date`.
#[expect(
    clippy::too_many_lines,
    reason = "one table of the pairs with no cast at all; splitting it would hide which pairs \
              those are, which is the only thing this function says"
)]
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
        // **A money casts to `numeric` and to a string, and takes `numeric` and the integers.**
        // Measured one target at a time, and the asymmetry is the point: `567.89::numeric::money`
        // and `12345::int8::money` both work, and `'567.89'::float8::money`,
        // `money::float8` and `money::int8` are each `42846`. A type whose value is cents does not
        // travel through a float, in either direction.
        (Some(ColumnType::Money), Some(to))
            if !stringy(to) && !matches!(to, ColumnType::Numeric | ColumnType::Money) =>
        {
            Some(SqlError::CannotCast {
                from: ColumnType::Money.name(),
                to: to.name(),
            })
        }
        (Some(from), Some(ColumnType::Money))
            if !stringy(from)
                && !matches!(
                    from,
                    ColumnType::Numeric
                        | ColumnType::Int2
                        | ColumnType::Int4
                        | ColumnType::Int8
                        | ColumnType::Money
                ) =>
        {
            Some(SqlError::CannotCast {
                from: from.name(),
                to: ColumnType::Money.name(),
            })
        }
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

/// A `regtype` over a name only the catalog can resolve, carried for the executor.
///
/// `oid` says which half was asked for: the number, which is what
/// `SELECT 'color'::regtype::oid` wants, or the name it prints as.
fn user_regtype(name: &str, oid: bool) -> plan::Expr {
    // **The name is carried as written and read once, by `value::split_type_name`.** It was read
    // here too — strip one leading and one trailing quote, fold otherwise — and that is a
    // different grammar from the executor's: `'"public"."mood"'` lost its outer quotes and became
    // the single name `public"."mood`, while `'"public.mood"'` lost them and *split*, so the two
    // answered each other's answers. One grammar, one parser.
    plan::Expr::CatalogFunc(Box::new(plan::CatalogFuncCall {
        func: plan::CatalogFunc::UserRegType,
        args: vec![
            plan::Expr::Literal(plan::Literal::String(name.to_owned())),
            plan::Expr::Literal(plan::Literal::Bool(oid)),
        ],
    }))
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
            // **And a bit-string literal is one**, as the *bits* it stands for rather than as
            // what was typed: `X'F'::bit(4)` is `1111` and the cast reads four characters, not
            // one. `B'1010'::bit(2)` truncating to `10` is then the ordinary cast rule.
            Value::SingleQuotedByteStringLiteral(bits) => Some(value::bit::from_text(bits)?),
            Value::HexStringLiteral(digits) => Some(value::bit::from_hex(digits)?),
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
            // **Through the `Cast` node a folded cast keeps** (ADR 0086). That node exists so the
            // declared type survives a value that cannot carry it, and it is a no-op on the value
            // — so what the outer cast reads is the literal under it. Without this arm a chain
            // whose inner step is one of those types stopped folding and took the per-row path
            // instead, which is how `'r'::"char"::int4` became
            // `22P02 invalid input syntax for type integer: "r"`.
            plan::Expr::Cast { operand, .. } => Ok(match operand.as_ref() {
                plan::Expr::Literal(plan::Literal::Typed(value)) => match value.as_ref() {
                    Datum::Text(text) => Some(text.clone()),
                    other => other.to_text(),
                },
                plan::Expr::Literal(plan::Literal::String(text)) => Some(text.clone()),
                _ => None,
            }),
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
    // **One reader for the literal, not two.** This function used to accept a single-quoted string
    // and nothing else, so `'26'::oid` answered and `26::oid` was
    // `0A000 the cast 26::oid is not supported` — a refusal for a spelling a real server takes and
    // the one a person writes. [`cast_literal_text`] already knows every shape a literal has here:
    // a number, a signed number, a bit string, and a cast chain whose inner step folded (ADR 0086).
    // Asking it is the whole fix, and it is the third time in this crate that one grammar had two
    // readers and the narrow one was the bug.
    cast_literal_text(expr)?
        .ok_or_else(|| SqlError::unsupported(format!("the cast {expr}::{data_type}")))
}

/// Whether folding this literal to `ty` still holds the number that was **written**.
///
/// Asked only of the two floats, and that is the whole of the rule: a float has 53 bits, so
/// `(9223372036854775807)::double precision` comes back `9.223372036854776e+18` and the digits
/// that were written are gone. A real server never folds the cast at all — it holds
/// `('9223372036854775807'::bigint)::double precision` — and this node reconstructs that form from
/// the folded constant wherever the value survives ([`exec::ddl::numeric_constant`], debt #24);
/// where it does not, the node is what carries it (`debts-v1.1.md` #30).
///
/// **Not asked of the other types, deliberately.** A spelling difference is not a loss:
/// `'2020-1-1'::date` folds to a `date` that prints `2020-01-01`, and a real server normalises it
/// the same way — keeping a node there would print a constant nobody wrote. The float case is a
/// difference in the *value*, which is a different thing from a difference in the spelling.
fn fold_keeps_the_digits(text: &str, ty: ColumnType, value: &Datum) -> bool {
    if !matches!(ty, ColumnType::Real | ColumnType::Double) {
        return true;
    }
    value
        .to_text()
        .is_some_and(|rendered| rendered == text.trim())
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
    /// PostgreSQL's `oidvector`: a list of oids, printed space separated.
    OidVector,
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
        "oidvector" => Some(CastTarget::OidVector),
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
                // **A literal past `int8` is a `numeric`, not an error.** PostgreSQL gives an
                // unadorned integer the smallest type that holds it, and `numeric` is the last
                // rung: `pg_typeof(9223372036854775807)` is `bigint` and
                // `pg_typeof(9223372036854775808)` is `numeric` — measured. So
                // `WHERE id = 9223372036854775808` against a `bigint` column is a comparison the
                // column is promoted for and is simply false, which is `or_test.rb`'s *or with
                // large number* answering one row where this node raised `22003`.
                //
                // The sign is part of the literal and is folded before the choice — `-9223372036854775808`
                // is a `bigint` and `-9223372036854775809` a `numeric` — which is why `text`
                // carries it into the parse rather than being negated afterwards.
                //
                // The only way a run of digits fails to parse as an `i64` is by not fitting in
                // one: the lexer has already kept `.`, `e` and `E` out of it above.
                match text.parse() {
                    Ok(fits) => plan::Literal::Integer(fits),
                    // **A real `numeric`, not this crate's `Literal::Decimal`.** That variant is
                    // `double precision` here — a divergence this node declares for `SELECT 1.5`
                    // — and routing an out-of-range integer through it would answer
                    // `double precision` where PostgreSQL says `numeric`, trading one wrong type
                    // for another. A typed literal carries the value itself and types as what it
                    // is.
                    Err(_) => plan::Literal::Typed(Box::new(Datum::Numeric(
                        value::numeric::from_text(&text)?,
                    ))),
                }
            }
        }
        Value::SingleQuotedString(text)
        | Value::DollarQuotedString(DollarQuotedString { value: text, .. }) => {
            refuse_if(negated, "a negated string literal")?;
            plan::Literal::String(text.clone())
        }
        // **`B'1010'` and `X'ff'` are one type through two alphabets.** Both are a `bit` with no
        // typmod — `pg_typeof(B'1010')` and `pg_typeof(X'ff')` are both `bit` — so the length is
        // the value's and nothing here carries an `n`. `sqlparser` gives them as two `Value`
        // variants and case does not matter to it: `b'1010'` and `x'ff'` arrive the same way.
        //
        // Reaching a `Literal::Typed` rather than a `Literal::String` is the point of the unit:
        // as a string the `B` went to `bit`'s input function with the rest, and
        // `"B" is not a valid binary digit` is that envelope arriving at the value parser.
        Value::SingleQuotedByteStringLiteral(bits) => {
            refuse_if(negated, "a negated bit-string literal")?;
            plan::Literal::Typed(Box::new(Datum::Bit {
                varying: false,
                bits: value::bit::from_text(bits)?,
            }))
        }
        Value::HexStringLiteral(digits) => {
            refuse_if(negated, "a negated bit-string literal")?;
            plan::Literal::Typed(Box::new(Datum::Bit {
                varying: false,
                bits: value::bit::from_hex(digits)?,
            }))
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
/// The pseudo-type a projected cast names, and the two refusals a value of one gets.
///
/// **Only NULL may be cast to a pseudo-type**, which is what "no value has this type" means, and
/// the two ways of writing a value get two different codes — measured:
///
/// ```text
/// SELECT 1::anyarray        42846 cannot cast type integer to anyarray
/// SELECT '{1,2}'::anyarray  0A000 cannot accept a value of type anyarray
/// ```
///
/// A *typed* operand has no cast to offer, so it is `42846`; an unadorned literal is `unknown`,
/// which every type accepts as input, so the refusal moves to the type itself and becomes the
/// `0A000` that says nothing can be one of these.
///
/// Only the projection asks. A pseudo-type elsewhere keeps the untyped NULL it has always been:
/// nothing in the corpus writes one, and inventing an answer for `WHERE x = NULL::anyarray` would
/// be inventing it.
fn pseudo_cast(expr: &Expr) -> Result<Option<plan::PseudoType>> {
    let Expr::Cast {
        expr: operand,
        data_type,
        ..
    } = expr
    else {
        return Ok(None);
    };
    let DataType::Custom(name, _) = data_type else {
        return Ok(None);
    };
    let Some(pseudo) = plan::PseudoType::by_name(&name.to_string()) else {
        return Ok(None);
    };
    match operand.as_ref() {
        Expr::Value(value) if matches!(value.value, Value::Null) => Ok(Some(pseudo)),
        // An unadorned literal is `unknown`, and the refusal is about the target.
        Expr::Value(value) if matches!(value.value, Value::SingleQuotedString(_)) => {
            Err(SqlError::CannotAcceptPseudoType(pseudo.name.to_owned()))
        }
        // The source type as PostgreSQL's resolver names it, which is the same spelling a
        // `42883` quotes back for a function argument.
        other => Err(SqlError::CannotCastToPseudoType {
            from: argument_type_name(&FunctionArg::Unnamed(FunctionArgExpr::Expr(other.clone()))),
            to: pseudo.name,
        }),
    }
}

/// One function because they are one grammar: `*`, `t.*`, an expression, an expression with an
/// alias, and the five `SELECT * EXCLUDE`-style modifiers that are each `0A000` naming themselves.
/// Two copies of this is two places for `RETURNING *` to stop meaning what `SELECT *` means.
fn lower_projection(items: &[SelectItem]) -> Result<Vec<plan::SelectItem>> {
    items
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) => Ok(plan::SelectItem::Expr {
                // Filled by `Executor::resolve_user_cast`, which is where the catalog is.
                user_type: None,
                pseudo: pseudo_cast(expr)?,
                expr: lower_expr(expr)?,
                alias: None,
            }),
            SelectItem::ExprWithAlias { expr, alias } => Ok(plan::SelectItem::Expr {
                user_type: None,
                pseudo: pseudo_cast(expr)?,
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
                        // `s.t.*` reaches the same rules a `FROM s.t` does, for the reason
                        // above: one grammar, one parser. `object_name` refuses qualification by
                        // design and is right to — it names extensions, schemas and databases,
                        // which have no schema of their own.
                        Ok(plan::SelectItem::QualifiedWildcard(
                            match name.0.as_slice() {
                                [_] => object_name(name)?,
                                _ => relation_name(name)?,
                            },
                        ))
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

/// `FOR UPDATE` / `FOR SHARE`, and the two modifiers that say what to do about a row somebody
/// else holds.
///
/// All three run now (ADR 0057 §5). They were refused by name for as long as this node took no row
/// locks at all, because each of `NOWAIT` and `SKIP LOCKED` promises something a client can check —
/// a `55P03` and a missing row — and answering every row would have been a wrong answer rather than
/// a missing feature. The lock is real now, so the promises can be kept.
///
/// `FOR NO KEY UPDATE` and `FOR KEY SHARE` never arrive here: `sqlparser` 0.62.0's `LockType` has
/// only the two, so both are refused by name in `crate::parse`'s table before this runs.
fn lower_locking(locks: &[sqlparser::ast::LockClause]) -> Result<Vec<plan::Locking>> {
    use sqlparser::ast::{LockType, NonBlock};

    locks
        .iter()
        .map(|lock| {
            Ok(plan::Locking {
                strength: match lock.lock_type {
                    LockType::Update => plan::LockStrength::Update,
                    LockType::Share => plan::LockStrength::Share,
                },
                of: lock.of.as_ref().map(relation_name).transpose()?,
                wait: match lock.nonblock {
                    None => plan::LockWait::Wait,
                    Some(NonBlock::Nowait) => plan::LockWait::NoWait,
                    Some(NonBlock::SkipLocked) => plan::LockWait::SkipLocked,
                },
            })
        })
        .collect()
}

/// The shapes PostgreSQL will not lock, in its own words.
///
/// Every one measured (`tests/corpus/pg19_row_locking.txt`), and the message carries **the clause
/// the user wrote** rather than a fixed `FOR UPDATE` — which is the half a single hard-coded
/// sentence gets wrong.
fn refuse_unlockable_shape(select: &plan::Select) -> Result<()> {
    let Some(lock) = select.locking.first() else {
        return Ok(());
    };
    let clause = lock.strength.clause();
    if select.distinct {
        return Err(SqlError::LockingNotAllowedWith {
            lock: clause,
            clause: "DISTINCT clause",
        });
    }
    if !select.group_by.is_empty() {
        return Err(SqlError::LockingNotAllowedWith {
            lock: clause,
            clause: "GROUP BY clause",
        });
    }
    // Only the target list, which is where PostgreSQL looks: an aggregate anywhere in it makes the
    // statement one row per group, and a group is not a row anything can hold.
    if select.projection.iter().any(|item| {
        matches!(item, plan::SelectItem::Expr { expr, .. } if crate::exec::aggregate::contains_aggregate(expr))
    }) {
        return Err(SqlError::LockingNotAllowedWith {
            lock: clause,
            clause: "aggregate functions",
        });
    }
    // **The nullable side is the one an outer join may fill with NULLs**: a `LEFT JOIN`'s inner
    // side, and — a `FULL JOIN` having two — every relation in the statement once one is present,
    // the `FROM` table included. Locking the other side of a `LEFT JOIN` is legal, so what decides
    // it there is which relation the clause names rather than the join itself. Measured: `FOR
    // UPDATE` and `FOR UPDATE OF <either side>` over a full join are both refused, with the same
    // sentence as the left join's.
    let has_full = select
        .joins
        .iter()
        .any(|join| join.kind == plan::JoinKind::Full);
    let nullable: Vec<&str> = select
        .joins
        .iter()
        .filter(|join| has_full || join.kind == plan::JoinKind::Left)
        .map(|join| join.table.referred_as())
        .chain(
            select
                .from
                .as_ref()
                .filter(|_| has_full)
                .map(plan::TableRef::referred_as),
        )
        .collect();
    for lock in &select.locking {
        // A clause with no `OF` locks every relation, so any nullable one is enough to refuse it;
        // with an `OF` it is that relation alone that has to be lockable.
        let locks_a_nullable_side = match &lock.of {
            None => !nullable.is_empty(),
            Some(of) => nullable.contains(&of.as_str()),
        };
        if locks_a_nullable_side {
            return Err(SqlError::LockingNullableSide(lock.strength.clause()));
        }
        // `OF x` names a relation **as the query refers to it**, so an alias has taken the table's
        // own name away — the same rule every other qualifier follows.
        if let Some(of) = &lock.of
            && !select
                .from
                .iter()
                .chain(select.joins.iter().map(|join| &join.table))
                .any(|table| table.referred_as() == of)
        {
            return Err(SqlError::LockingRelationNotInFrom {
                relation: of.clone(),
                lock: lock.strength.clause(),
            });
        }
    }
    Ok(())
}

/// A query written in parentheses, with whatever was written outside them merged in.
///
/// **PostgreSQL merges rather than nests**, and refuses only when both levels write the same
/// clause — measured, each sentence its own:
///
/// ```text
/// (SELECT id FROM ex) ORDER BY id DESC LIMIT 1                -> the outer clauses apply
/// WITH w AS (…) (SELECT n FROM w)                             -> so does an outer WITH
/// (SELECT id FROM ex FOR UPDATE) FOR UPDATE                   -> locks merge, silently
/// ((SELECT … ORDER BY id LIMIT 2)) ORDER BY id DESC           42601: multiple ORDER BY clauses not allowed
/// (SELECT … ORDER BY id LIMIT 2) LIMIT 1                      42601: multiple LIMIT clauses not allowed
/// (SELECT … OFFSET 0) OFFSET 0                                42601: multiple OFFSET clauses not allowed
/// WITH a AS (…) (WITH b AS (…) SELECT n FROM b)               42601: multiple WITH clauses not allowed
/// ```
///
/// The refusals are the half that makes this more than unwrapping a parenthesis: a fix that
/// simply took the inner query would answer four statements a real server rejects, and would
/// silently drop one of each doubled pair to do it.
///
/// Checked in `insertSelectOptions`'s order — `ORDER BY`, `OFFSET`, `LIMIT`, `WITH` — which is the
/// order the sentences come out in when a statement doubles more than one.
///
/// **A loop over the layers, not a recursion.** The scanner admits `MAX_NESTING_DEPTH` bracket
/// levels and the parser holds them as nested `Query`s; peeling one per stack frame, with a clone
/// of what was left at each, put a thousand frames and a quadratic copy on the caller's 2 MiB —
/// the parser's own recursion limit is four times the scanner's and bounds nothing here. Every
/// layer is visited by reference and exactly one `Query`, the innermost, is cloned and built up.
fn lower_parenthesised(outer: &Query, inner: &Query) -> Result<plan::Select> {
    let mut layers: Vec<&Query> = vec![outer, inner];
    // `copied` first, so the loop holds a `&Query` and not a borrow of the vector it pushes into.
    while let Some(SetExpr::Query(next)) = layers.last().copied().map(|query| query.body.as_ref()) {
        layers.push(next);
    }
    // Innermost first: that is the order `gram.y` merges in, and the order the sentences come out.
    let (innermost, enclosing) = layers
        .split_last()
        .map_or((outer, &layers[..]), |(last, rest)| (*last, rest));
    let mut merged = innermost.clone();
    for outer in enclosing.iter().rev() {
        merge_query_clauses(outer, &mut merged)?;
    }
    lower_query(&merged)
}

/// One enclosing layer's clauses merged into the query inside it, or the `42601` a doubled clause is.
fn merge_query_clauses(outer: &Query, inner: &mut Query) -> Result<()> {
    let (outer_limit, outer_offset) = limit_halves(outer.limit_clause.as_ref());
    let (inner_limit, inner_offset) = limit_halves(inner.limit_clause.as_ref());
    if outer.order_by.is_some() && inner.order_by.is_some() {
        return Err(SqlError::DoubledClause("ORDER BY"));
    }
    if outer_offset && inner_offset {
        return Err(SqlError::DoubledClause("OFFSET"));
    }
    if outer_limit && inner_limit {
        return Err(SqlError::DoubledClause("LIMIT"));
    }
    if outer.with.is_some() && inner.with.is_some() {
        return Err(SqlError::DoubledClause("WITH"));
    }
    if outer.with.is_some() {
        inner.with.clone_from(&outer.with);
    }
    if outer.order_by.is_some() {
        inner.order_by.clone_from(&outer.order_by);
    }
    inner.limit_clause = merge_limits(inner.limit_clause.take(), outer.limit_clause.clone());
    // **Concatenated rather than chosen**: two `FOR UPDATE`s are legal and merge on a real server,
    // which is the one clause here that does not collide.
    inner.locks.extend(outer.locks.iter().cloned());
    Ok(())
}

/// Which halves of a `LIMIT`/`OFFSET` are written, `sqlparser` carrying both in one field.
fn limit_halves(clause: Option<&LimitClause>) -> (bool, bool) {
    match clause {
        None => (false, false),
        Some(LimitClause::LimitOffset { limit, offset, .. }) => (limit.is_some(), offset.is_some()),
        // `LIMIT a, b` is MySQL's spelling of both halves at once.
        Some(LimitClause::OffsetCommaLimit { .. }) => (true, true),
    }
}

/// The two levels' `LIMIT`/`OFFSET` in one clause. Each half is written by at most one of them:
/// a doubled half is refused before this runs.
fn merge_limits(inner: Option<LimitClause>, outer: Option<LimitClause>) -> Option<LimitClause> {
    match (inner, outer) {
        (None, clause) | (clause, None) => clause,
        (
            Some(LimitClause::LimitOffset {
                limit: inner_limit,
                offset: inner_offset,
                limit_by: inner_by,
            }),
            Some(LimitClause::LimitOffset {
                limit: outer_limit,
                offset: outer_offset,
                limit_by: outer_by,
            }),
        ) => Some(LimitClause::LimitOffset {
            limit: inner_limit.or(outer_limit),
            offset: inner_offset.or(outer_offset),
            limit_by: if inner_by.is_empty() {
                outer_by
            } else {
                inner_by
            },
        }),
        // `LIMIT a, b` carries both halves, so anything beside it has already been refused.
        (_, outer) => outer,
    }
}

/// One arm of a set operation, lowered.
///
/// A **parenthesised** arm is a query and may carry its own `ORDER BY` or `LIMIT` — measured, and
/// the bare form of either before `UNION` is `42601` there — so it goes through [`lower_query`]
/// whole. Any other arm is lowered through the same function on a *copy of the outer query with
/// the set's clauses cleared*, which is what keeps this out of that function's body: one arm is a
/// query with one clause different, not a second lowering to write.
fn lower_set_arm(template: &Query, body: &SetExpr) -> Result<plan::Select> {
    if let SetExpr::Query(inner) = body {
        return lower_query(inner);
    }
    // **Built field by field, never `template.clone()`**: the template is the whole set, and its
    // body is the left-leaning tree of every arm — a derived `Clone` walks that tree one frame per
    // operator, so cloning it once per arm was a thousand nested frames (and a thousand nested
    // drops) for a chain the scanner admits, on a 2 MiB worker. The set's clauses — `WITH`,
    // `ORDER BY`, `LIMIT` — are not the arm's, every one of them measured on the oracle.
    let arm = Query {
        with: None,
        body: Box::new(body.clone()),
        order_by: None,
        limit_clause: None,
        fetch: template.fetch.clone(),
        locks: template.locks.clone(),
        for_clause: template.for_clause.clone(),
        settings: template.settings.clone(),
        format_clause: template.format_clause.clone(),
        pipe_operators: template.pipe_operators.clone(),
    };
    lower_query(&arm)
}

#[allow(
    clippy::too_many_lines,
    reason = "most of it is the refusal list, which is the point: one line per clause not honoured"
)]
fn lower_query(query: &Query) -> Result<plan::Select> {
    refuse_if(query.fetch.is_some(), "FETCH FIRST")?;
    refuse_if(query.for_clause.is_some(), "FOR XML/JSON")?;
    refuse_if(query.settings.is_some(), "SETTINGS")?;
    refuse_if(query.format_clause.is_some(), "FORMAT")?;
    refuse_if(!query.pipe_operators.is_empty(), "a pipe operator")?;

    let SetExpr::Select(select) = query.body.as_ref() else {
        // **Parentheses around a query are grouping, and the grammar sees through them.**
        // `gram.y`'s `insertSelectOptions` merges what is written outside the parentheses into the
        // query inside; there is no nesting to lower. `postgresql_adapter_prevent_writes_test.rb`
        // sends `/*action:index*/((SELECT …))`, and this arm is why it is a `SELECT` again.
        if let SetExpr::Query(inner) = query.body.as_ref() {
            return lower_parenthesised(query, inner);
        }
        // **`VALUES …` on its own is a query**, so it takes the clauses a query takes: its rows
        // are a relation with no name, and the `ORDER BY`, `LIMIT` and `OFFSET` above it are the
        // ordinary ones over the columns it names itself.
        if let SetExpr::Values(values) = query.body.as_ref() {
            let (limit, offset) = lower_limit_offset(query)?;
            return Ok(plan::Select {
                // `VALUES` names no relation, so there is nothing a locking clause could hold —
                // and PostgreSQL agrees: `VALUES (1) FOR UPDATE` is a syntax error there.
                locking: Vec::new(),
                set_arms: Vec::new(),
                projection: vec![plan::SelectItem::Wildcard],
                from: Some(plan::TableRef {
                    values: Some(Box::new(lower_values(values, &[], "")?)),
                    name: String::new(),
                    alias: None,
                    derived: None,
                    function: None,
                    hidden_cte: false,
                    written: None,
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
        // **A set operation is arms, and the clauses outside them are the set's.** Flattened in
        // `crate::parse::set_operation` — a file of its own, so that this stays one line — and
        // then given the outer `ORDER BY`, `LIMIT`, `OFFSET` and `WITH`, which belong to the whole
        // set on a real server and are already the first arm's fields.
        if matches!(query.body.as_ref(), SetExpr::SetOperation { .. }) {
            let arm = |body: &SetExpr| lower_set_arm(query, body);
            let set = super::set_operation::lower(query.body.as_ref(), &arm)?;
            let (limit, offset) = lower_limit_offset(query)?;
            let mut set = plan::Select {
                order_by: lower_order_by(query)?,
                limit,
                offset,
                ..set
            };
            lower_with(query.with.as_ref(), &mut set)?;
            return Ok(set);
        }
        // `TABLE t` after the rewrite -- its own feature.
        return Err(SqlError::unsupported(format!(
            "the query body {}",
            query.body
        )));
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
        locking: lower_locking(&query.locks)?,
        set_arms: Vec::new(),
    };
    // **After the statement is lowered, because the rules are about the lowered shape**: whether
    // there is a `DISTINCT`, a `GROUP BY`, an aggregate in the target list, a nullable join side,
    // or a relation the `OF` names and the `FROM` does not.
    refuse_unlockable_shape(&lowered)?;
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
/// Whether a `WITH` item's body names the item itself — the thing that makes it recursive.
///
/// The parser's own text is the cheapest total answer here and the safest: walking the AST for a
/// table factor would have to know every place a relation can be named, and a shape it had not
/// been taught would read as "not recursive" and then inline forever. A word match over the
/// rendered body can only err the other way — calling something recursive that is not — and that
/// error is a refusal rather than a loop.
fn references(body: &Query, name: &str) -> bool {
    let text = body.to_string();
    let lowered = text.to_ascii_lowercase();
    let wanted = name.to_ascii_lowercase();
    lowered
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == wanted)
}

/// One lowered `WITH` item: its name, its body, its column aliases, and its recursive term.
type LoweredItem = (
    String,
    plan::Select,
    Vec<String>,
    Option<Box<plan::RecursiveTerm>>,
);

/// A `WITH` item's body: the term that runs first, and the one that runs until nothing is new.
struct RecursiveBody {
    /// The non-recursive term, or the whole body when there is no recursion.
    body: plan::Select,
    /// The recursive term, with its self-reference already replaced by the working table.
    step: Option<Box<plan::RecursiveTerm>>,
}

/// Lowers a `WITH` item's body, splitting it in two when it names itself.
///
/// **Every refusal here is PostgreSQL's, measured** (`tests/captures/pg19_recursive_cte.txt`), and
/// they fall in two classes that are not interchangeable: a shape the standard forbids is `42P19`,
/// and a shape PostgreSQL has simply not built is `0A000`. `ORDER BY` and `LIMIT` *inside* the
/// body are in the second class — not illegal, unimplemented — which is the way round reasoning
/// does not put them.
fn lower_recursive_or_query(
    query: &Query,
    recursive: bool,
    name: &str,
    columns: &[String],
) -> Result<RecursiveBody> {
    if !recursive || !references(query, name) {
        return Ok(RecursiveBody {
            body: lower_query(query)?,
            step: None,
        });
    }
    // `ORDER BY` and `LIMIT` belong to the body here, not to the statement: the outer query's own
    // are lowered by whoever called this.
    if query.order_by.is_some() {
        return Err(SqlError::NotImplementedInRecursive(
            "ORDER BY in a recursive query",
        ));
    }
    if query.limit_clause.is_some() {
        return Err(SqlError::NotImplementedInRecursive(
            "LIMIT in a recursive query",
        ));
    }
    let shape = || SqlError::RecursiveQueryShape(name.to_owned());
    let SetExpr::SetOperation {
        op,
        set_quantifier,
        left,
        right,
    } = query.body.as_ref()
    else {
        // A body that is not a set operation at all, which is the shape
        // `WITH RECURSIVE t AS (SELECT n FROM t)` has.
        return Err(shape());
    };
    // `INTERSECT` and `EXCEPT` are the same sentence: the form is `UNION [ALL]` and nothing else.
    if !matches!(op, sqlparser::ast::SetOperator::Union) {
        return Err(shape());
    }
    // **The seed may not name the CTE**, and that is its own sentence rather than the shape one.
    if references_set_expr(left, name) {
        return Err(SqlError::RecursiveReferenceInSeed(name.to_owned()));
    }

    let seed = lower_set_arm(query, left)?;
    let mut step = lower_set_arm(query, right)?;

    // Counted over **table factors**: `JOIN t ON c.firm_id = t.id` names `t` twice and references
    // the relation once, so a word count would refuse the statement the suite sends.
    // The alias list renames the working table's columns exactly as it renames the CTE's:
    // `WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n …)` reads `i` from the round
    // before it, and the seed called that column `?column?`.
    let references = plan::cte::plant_working_table(&mut step, name, &seed, columns);
    if references == 0 {
        // The word is in the body but no `FROM` entry is the CTE — a string literal, or a column
        // of that name. Not recursive, so it is an ordinary item after all.
        return Ok(RecursiveBody {
            body: lower_query(query)?,
            step: None,
        });
    }
    if references > 1 {
        return Err(SqlError::RecursiveReferenceTwice(name.to_owned()));
    }
    // **Only the nullable side is refused.** `FROM t LEFT JOIN c` keeps every row of `t` and is
    // legal — and unbounded, which is how the first capture of this feature hung a server.
    if plan::cte::on_a_nullable_side(&step) {
        return Err(SqlError::RecursiveReferenceInOuterJoin(name.to_owned()));
    }
    if step.projection.iter().any(|item| {
        matches!(item, plan::SelectItem::Expr { expr, .. }
            if crate::exec::aggregate::contains_aggregate(expr))
    }) {
        return Err(SqlError::AggregateInRecursiveTerm);
    }
    Ok(RecursiveBody {
        body: seed,
        step: Some(Box::new(plan::RecursiveTerm {
            select: step,
            // `UNION` without `ALL`, which is what makes the dedup a termination rule.
            distinct: !matches!(set_quantifier, sqlparser::ast::SetQuantifier::All),
        })),
    })
}

/// Whether one arm of a set operation names `name`, by the same word match [`references`] uses.
fn references_set_expr(body: &SetExpr, name: &str) -> bool {
    let text = body.to_string().to_ascii_lowercase();
    let wanted = name.to_ascii_lowercase();
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == wanted)
}

fn lower_with(with: Option<&sqlparser::ast::With>, into: &mut plan::Select) -> Result<()> {
    let Some(with) = with else { return Ok(()) };
    // **`RECURSIVE` is a keyword about the *bodies*, not about the list.** A `WITH RECURSIVE`
    // whose body does not reference itself is an ordinary `WITH` on a real server and answers —
    // measured — so the keyword alone decides nothing. A body that *does* name itself cannot be
    // inlined, because substituting it never terminates: it is split into its two terms below and
    // iterated to a fixed point by `exec::recursive`.

    // Every name up front, because deciding whether a reference is a *forward* one needs the list
    // the body being lowered is not yet part of.
    let all_names: Vec<String> = with
        .cte_tables
        .iter()
        .map(|cte| ident(&cte.alias.name))
        .collect();
    let mut named: Vec<String> = Vec::new();
    let mut bodies: Vec<LoweredItem> = Vec::new();
    for cte in &with.cte_tables {
        // `AS MATERIALIZED` and `AS NOT MATERIALIZED` are **accepted and change nothing**, which
        // is not the usual "reject rather than ignore": both spellings return the same rows on a
        // real server (measured), because what they choose is a plan and not an answer.
        refuse_if(cte.from.is_some(), "a WITH item with a FROM identifier")?;
        let name = ident(&cte.alias.name);
        plan::cte::refuse_duplicate(&named, &name)?;
        // Data-modifying CTEs are the read path's write half and are a unit of their own.
        let columns: Vec<String> = cte
            .alias
            .columns
            .iter()
            .map(|column| ident(&column.name))
            .collect();
        let body = match cte.query.body.as_ref() {
            SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) => {
                return Err(SqlError::unsupported("a data-modifying WITH item"));
            }
            _ => lower_recursive_or_query(&cte.query, with.recursive, &name, &columns)?,
        };
        // **Mutual recursion**, which PostgreSQL has not built either and says so with its own
        // sentence — not the `42P01` a forward reference gets without `RECURSIVE`. Measured.
        if with.recursive {
            let later = &all_names[bodies.len() + 1..];
            for name in later {
                if references(&cte.query, name) {
                    return Err(SqlError::NotImplementedInRecursive(
                        "mutual recursion between WITH items",
                    ));
                }
            }
        }
        let RecursiveBody { mut body, step } = body;
        // Each item sees the ones before it and **not itself**: `WITH t AS (SELECT id FROM t)` is
        // the same `42P01` a forward reference gets, measured.
        for (earlier, earlier_body, earlier_columns, _) in &bodies {
            plan::cte::inline(&mut body, earlier, earlier_body, earlier_columns);
        }
        // What is left of the list is what this body may not reference -- itself included -- and a
        // reference to one of those is only an error if the catalog has no such relation.
        plan::cte::mark_hidden(&mut body, &all_names[bodies.len()..]);
        named.push(name.clone());
        bodies.push((name, body, columns, step));
    }

    for (name, body, columns, step) in &bodies {
        match step {
            // **A recursive item is substituted like any other**, and what changes is only what
            // the derived table is: a seed and a step, planned into a fixpoint, where an ordinary
            // one is a body planned once. Everything above it — a qualifier, an `ORDER BY`, a
            // second reference in the outer query — reads it as the relation it already reads a
            // CTE as.
            Some(step) => {
                plan::cte::inline_recursive(into, name, body, step, columns);
            }
            None => {
                plan::cte::inline(into, name, body, columns);
            }
        }
        // Carried whether anything referenced it or not: **an unreferenced CTE is still
        // analysed**, measured, and inlining alone would never look at one.
        let mut derived = plan::Derived::from_cte(Box::new(body.clone()), columns.clone());
        derived.recursive.clone_from(step);
        into.ctes.push(plan::TableRef {
            values: None,
            name: name.clone(),
            alias: None,
            derived: Some(Box::new(derived)),
            function: None,
            hidden_cte: false,
            written: None,
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
        JoinOperator::FullOuter(constraint) => {
            // **`USING` is refused on a full join, and only on a full join.** A `USING` column is
            // merged into one, and for an inner or left join the left side's value is always the
            // right answer — equal by the condition, or NULL only where the right row is missing.
            // A full join has the third case the merge has no answer for: the row where the *left*
            // is missing, whose merged value is the right's. PostgreSQL spells that `COALESCE`;
            // taking the left value here would answer NULL for every row kept by the new half.
            if matches!(constraint, JoinConstraint::Using(_)) {
                return Err(SqlError::unsupported("a FULL JOIN with USING"));
            }
            (plan::JoinKind::Full, Some(constraint))
        }
        // Each of these keeps rows an inner join drops, so running one as an inner join would
        // silently return fewer rows than the user asked for -- the worst thing a join can do.
        other => {
            return Err(SqlError::unsupported(match other {
                JoinOperator::Right(_) | JoinOperator::RightOuter(_) => "a RIGHT JOIN",
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
                written: None,
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
                    written: None,
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
                written: dropped_qualifier(name),
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
                    written: None,
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
                written: None,
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
/// The type a `USING` casts this column to, or `None` if it is anything but such a cast.
///
/// `change_column` writes `USING CAST("c" AS timestamp)` and `USING c::integer`; both spell the
/// conversion the statement already names, so they license it without asking for any computation
/// this node cannot do. **Anything else is refused by name** — `USING string_to_array(c, ',')`
/// included — because evaluating it would need a per-row expression evaluator that does not exist,
/// and ignoring it would silently answer a different question than the one asked.
fn using_cast_target<'a>(expr: &'a Expr, column: &str) -> Option<&'a DataType> {
    let Expr::Cast {
        expr: inner,
        data_type,
        ..
    } = unwrap_nested(expr)
    else {
        return None;
    };
    let names_the_column = matches!(unwrap_nested(inner), Expr::Identifier(name) if ident(name) == column)
        || matches!(unwrap_nested(inner), Expr::CompoundIdentifier(parts)
            if parts.last().is_some_and(|part| ident(part) == column));
    names_the_column.then_some(data_type)
}

/// `42P16` for a column declared as a pseudo-type, which is what `void` is.
///
/// Measured: `CREATE TABLE zz (c void)` is `42P16 column "c" has pseudo-type void`, and so is
/// `ALTER TABLE ... ALTER COLUMN c TYPE void` and `ADD COLUMN d void` — the check is per column and
/// PostgreSQL names it. `void` is in this crate's vocabulary because a *function* returns one
/// (`pg_advisory_lock`), never because a row holds one, and this is the line that keeps those two
/// facts from turning into each other.
fn refuse_pseudo_type(ty: ColumnType, column: &str) -> Result<()> {
    if ty == ColumnType::Void {
        return Err(SqlError::PseudoTypeColumn {
            column: column.to_owned(),
            ty: "void",
        });
    }
    Ok(())
}

fn lower_column_type(data_type: &DataType) -> Result<(ColumnType, i32, Option<String>)> {
    match lower_type(data_type) {
        Ok((ty, typmod)) => Ok((ty, typmod, None)),
        Err(error) => match data_type {
            // **One part or two.** A type may live in a schema — a domain does
            // ([ADR 0065](../../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md))
            // — and `schema_test.rb` creates `schema_1.text` and then declares columns of it. A
            // one-part guard here meant a qualified type could be **created and never
            // referenced**: `CREATE DOMAIN r77s.ds` succeeded and `CREATE TABLE r77s.t (v r77s.ds)`
            // was `0A000 the type r77s.ds is not supported`, about a type the catalog held.
            DataType::Custom(name, modifiers)
                if modifiers.is_empty()
                    && (1..=2).contains(&name.0.len())
                    && !is_serial_spelling(data_type) =>
            {
                let Ok(stored) = relation_name(name) else {
                    return Err(error);
                };
                // The type is unknown here and the placeholder says so: `Int2` is what an enum's
                // ordinal is, and the executor replaces it for any other kind. Nothing reads it
                // before then — a `CREATE TABLE` plan is executed, never evaluated.
                Ok((ColumnType::Int2, NO_TYPMOD, Some(stored)))
            }
            _ => Err(error),
        },
    }
}

/// Whether a custom type name is one of the `serial` spellings, which are integers plus a sequence
/// and never a user-defined type — a table with a column called `serial` would otherwise resolve
/// against the catalog and get a worse error than the one [`lower_type`] already gives it.
/// The column type one of PostgreSQL's built-in names spells, where `sqlparser` has no
/// variant of its own for it — the six ranges and `point`.
fn range_type_name(name: &str) -> Option<ColumnType> {
    match name.to_ascii_lowercase().as_str() {
        "tsrange" => Some(ColumnType::TsRange),
        "tstzrange" => Some(ColumnType::TstzRange),
        "int4range" => Some(ColumnType::Int4Range),
        "daterange" => Some(ColumnType::DateRange),
        "numrange" => Some(ColumnType::NumRange),
        "int8range" => Some(ColumnType::Int8Range),
        // **`point` arrives the same way**: `sqlparser` has no variant for it either, so a
        // geometric name is a `Custom` one exactly as a range name is, and this is the
        // table that says which `Custom` names are types this node has.
        "point" => Some(ColumnType::Point),
        // **And `money`**, which `sqlparser` does have a variant for on some dialects and not
        // this one — a `Custom` name here like the rest. `money_test.rb` writes `t.money`, which
        // the adapter sends as the bare word.
        "money" => Some(ColumnType::Money),
        // **The three network types**, which `sqlparser` has no variant for either.
        // `network_test.rb` writes `t.inet`, `t.cidr` and `t.macaddr` in one `create_table`.
        "inet" => Some(ColumnType::Inet),
        "cidr" => Some(ColumnType::Cidr),
        "macaddr" => Some(ColumnType::MacAddr),
        // **The five `geometric_test.rb` declares in one `create_table`, and `line` beside them.**
        // `sqlparser` has no variant for any of the six, so each is a `Custom` name here.
        "lseg" => Some(ColumnType::Lseg),
        "box" => Some(ColumnType::Box),
        "path" => Some(ColumnType::Path),
        "polygon" => Some(ColumnType::Polygon),
        "circle" => Some(ColumnType::Circle),
        "line" => Some(ColumnType::Line),
        // **And `xml`**, which `sqlparser` has no variant for either. `xml_test.rb` writes
        // `t.xml "payload"`, which the adapter sends as the bare word.
        "xml" => Some(ColumnType::Xml),
        // **And `ltree`**, the third extension type to reach a column, by the road `hstore` and
        // `citext` take: `sqlparser` has no variant, so it is a `Custom` name here.
        "ltree" => Some(ColumnType::Ltree),
        // **A pattern, not a path** — see [`crate::value::ltree`]'s `matches`.
        "lquery" => Some(ColumnType::LQuery),
        // **`"bit"` quoted is not `bit` bare, and the difference is a typmod.** The keyword in a
        // cast is the grammar's `bit(1)` — `'101'::bit` is `1`, truncated — while the *quoted*
        // name is the type with no length, so `'101'::"bit"` is `101`. Measured, and it is why a
        // real server prints a bit default as `'00000011'::"bit"`: the bare spelling would throw
        // away every bit but the first when the default is re-read. `sqlparser` gives a keyword
        // as `DataType::Bit` and only ever reaches this table with the quoted form, which the
        // guard above has already unquoted.
        "bit" => Some(ColumnType::Bit),
        "varbit" | "bit varying" => Some(ColumnType::VarBit),
        _ => None,
    }
}

fn is_serial_spelling(data_type: &DataType) -> bool {
    serial_identity(data_type).is_some()
}

#[expect(
    clippy::too_many_lines,
    reason = "one function per spelling a type can be written in, over the whole vocabulary: the \
              quoted names, the three that carry a number, the arrays, and the table of custom \
              names. Splitting it would put half the spellings somewhere other than beside the \
              other half, which is the mistake the `\"char\"` unit found when two readers of one \
              name grammar disagreed"
)]
pub(super) fn lower_type(data_type: &DataType) -> Result<(ColumnType, i32)> {
    let plain = |ty| Ok((ty, NO_TYPMOD));
    // **A quoted type name is a type name.** `'101'::"bit"`, `'101'::"varchar"` and `'1'::"int4"`
    // are all ordinary casts on a real server — the quotes say "this is an identifier", not "this
    // is a different type". `sqlparser` keeps them in `ObjectName::to_string`, so every `Custom`
    // guard below compared `"bit"` with `bit` and missed. This node writes the spelling itself:
    // a `B'…'` default is stored as `'00000011'::"bit"`, because `bit` is reserved, and that text
    // is re-parsed for every row the default fills.
    if let DataType::Custom(name, modifiers) = data_type
        && modifiers.is_empty()
        && let [only] = name.0.as_slice()
        && let Some(part) = only.as_ident()
        && part.quote_style.is_some()
    {
        // **`pg_type` first, and `"char"` is why.** Stripping the quotes and re-reading is right
        // for every name where the quoted and the bare spelling are the same type — `"bit"`,
        // `"varchar"`, `"int4"` all answer the same either way — and wrong for the one where they
        // are **not**: bare `char` is `bpchar` and `"char"` is oid 18. A quoted name is an
        // identifier, so it is looked up the way an identifier is; anything `pg_type` does not
        // hold (a domain, an enum) falls through to the strip below and reaches the user-type path.
        if let Some(ty) = value::internal_type_by_name(&part.value) {
            return Ok((ty, NO_TYPMOD));
        }
        return lower_type(&DataType::Custom(
            ObjectName::from(vec![Ident::new(part.value.clone())]),
            Vec::new(),
        ));
    }
    // **A `regclass` is a column type** since the row stopped carrying the printed name beside
    // the oid — see `refuse_a_regclass_array_column` above for the half that is still not one.
    if matches!(data_type, DataType::Regclass) {
        return plain(ColumnType::RegClass);
    }
    // **The typmod is the length**, not the length plus a header: `character_maximum_length` for
    // `bit(8)` is 8 and `format_type(1560, 8)` is `bit(8)`, both measured. A bare `bit` keeps
    // `NO_TYPMOD` and reads back as `bit(1)`, which is where that rule lives.
    //
    // **`varbit` and `bit varying` are one type under two spellings**, and `sqlparser` gives them
    // two `DataType` variants — `VarBit` for the one word, `BitVarying` for the two. Only the
    // second was read here, so `'101'::bit varying` answered and `'101'::varbit` was
    // `0A000 the type VARBIT is not supported`, and with it every statement that reaches the type
    // through that spelling: the array, the aggregates, the operators, a column declaration. One
    // missing variant, six wire probes. The printed name of both is `bit varying` — measured,
    // `pg_typeof('101'::varbit)` and `'varbit'::regtype` both answer it — so nothing downstream
    // has to know which spelling was written.
    if let DataType::Bit(length) | DataType::BitVarying(length) | DataType::VarBit(length) =
        data_type
    {
        let ty = if matches!(data_type, DataType::Bit(_)) {
            ColumnType::Bit
        } else {
            ColumnType::VarBit
        };
        // **A bare `bit` is `bit(1)`, everywhere a type is written.** `'101'::bit` is `1` on a
        // real server — truncated, not kept — and a bare `bit` column reports
        // `character_maximum_length` 1 with `atttypmod` 1 behind it. Only a `B'…'` *literal* has
        // no length at all, and `crate::value::format_type` is where that case answers `"bit"`.
        // A bare `bit varying` really is unbounded, so the two do not share this rule.
        let bare = if matches!(data_type, DataType::Bit(_)) {
            1
        } else {
            NO_TYPMOD
        };
        return Ok((
            ty,
            length.map_or(bare, |n| i32::try_from(n).unwrap_or(i32::MAX)),
        ));
    }
    match data_type {
        // **`int8[]` is a column type**, over every element type this node has. The element's own
        // declaration is read first and **its typmod is the array's**: `character varying(255)[]`
        // and `numeric(10,2)[]` are real declarations that `ActiveRecord` writes, the length
        // belongs to the element, and `format_type` prints it back inside the element's name.
        DataType::Array(inner) => {
            let Some(element) = array_element(inner) else {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            };
            let (element, typmod) = lower_type(element)?;
            let Some(array) = esker_keys::array::ArrayValue::array_over(element) else {
                return Err(SqlError::unsupported(format!("the type {data_type}")));
            };
            Ok((array, typmod))
        }
        // The three that take a number. Each is checked against PostgreSQL's own limit, because a
        // length this node accepted and a real server refused would be a table that exists here
        // and not there.
        DataType::Varchar(Some(length)) | DataType::CharacterVarying(Some(length)) => {
            Ok((ColumnType::Varchar, string_typmod(length, "varchar")?))
        }
        // **`interval(p)` carries a typmod; `interval <fields>` still does not.** The precision is
        // a width and this node keeps it; the field mask says which fields a value *keeps*, which
        // is semantics, and stays the declared divergence `tests/interval.rs` records.
        //
        // A precision past six is **reduced rather than refused** — measured, `interval(7)` is
        // `WARNING: INTERVAL(7) precision reduced to maximum allowed, 6` and the column is created
        // as `interval(6)`. The clamp lives in `value::interval_typmod_of_precision`; the warning
        // itself is not emitted here, because lowering has no notice channel, and that gap is
        // named in `tests/interval_precision.rs`.
        DataType::Interval {
            fields: None,
            precision: Some(precision),
        } => Ok((
            ColumnType::Interval,
            value::interval_typmod_of_precision(
                u32::try_from(*precision).unwrap_or(value::MAX_TIME_PRECISION),
            ),
        )),
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
        // The same for the zoned spelling, which `change_column` sends as `timestamptz(6)`. The
        // bound and the fall-through are the arm above's, because the difference between the two
        // types is the label and not the precision.
        DataType::Timestamp(Some(precision), TimezoneInfo::Tz | TimezoneInfo::WithTimeZone)
            if *precision <= 6 =>
        {
            Ok((
                ColumnType::TimestampTz,
                value::typmod_of_precision(u32::try_from(*precision).unwrap_or(6)),
            ))
        }
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
        // **`sqlparser` has a variant for each of these**, so they never reach the `Custom` arm
        // below and a name-based resolver would never have seen them: the refusal was the
        // catch-all's, which is why it shouted `TSVECTOR` in `sqlparser`'s own Display casing.
        DataType::TsVector => ColumnType::TsVector,
        DataType::TsQuery => ColumnType::TsQuery,
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
        // `citext`, the other extension type, by the same road and for the same reason.
        DataType::Custom(name, modifiers)
            if modifiers.is_empty() && name.to_string().eq_ignore_ascii_case("citext") =>
        {
            ColumnType::Citext
        }
        // The range types, which `sqlparser` also has no variant for. Built in on a real server
        // rather than an extension's, so they need no `CREATE EXTENSION` in front of them.
        //
        // **Matched by name and not by a guard on `Custom` alone**: a catch-all here swallows
        // `serial`, whose arm is below, and turns every `id serial primary key` into
        // `0A000 the type serial is not supported`.
        DataType::Custom(name, modifiers)
            if modifiers.is_empty() && range_type_name(&name.to_string()).is_some() =>
        {
            range_type_name(&name.to_string()).unwrap_or(ColumnType::TsRange)
        }
        // `bigserial` and `serial` are `bigint`/`integer` plus a sequence, and `sqlparser` 0.62
        // has no variant for either -- both arrive as a custom type name. `smallserial` arrives
        // the same way and falls through to the refusal below until `int2` lands, which names
        // what the user wrote.
        other if serial_identity(other).is_some() => match serial_width(other) {
            Some(ty) => ty,
            None => return Err(SqlError::unsupported(format!("the type {other}"))),
        },
        // **A type's *internal* name is a type name.** `bpchar` is what PostgreSQL calls
        // `character(n)` in `pg_type`, and `'a'::bpchar`, `c bpchar` and `CREATE DOMAIN d AS
        // bpchar` are all ordinary on a real server — the name is not a second-class spelling, it
        // is the one the catalog itself reports. Anything not in the grammar arrives here as a
        // custom name, so this is the one place the three paths meet; without it `bpchar` was a
        // *user* type nobody had declared and the answer was `0A000 the type bpchar is not
        // supported` about a type this node has.
        // **A quoted name is an identifier and is looked up in `pg_type` alone.** The one place
        // it matters is `"char"`: bare `char` is `bpchar` and the quoted spelling is oid 18, and
        // `ObjectName`'s `Display` drops the quote style — so the decision is made here, where the
        // identifier still carries it.
        DataType::Custom(name, modifiers)
            if modifiers.is_empty()
                && name.0.len() == 1
                && let Some(part) = name.0.first()
                && let Some(ident) = part.as_ident()
                && ident.quote_style.is_some()
                && let Some(ty) = value::internal_type_by_name(&ident.value) =>
        {
            ty
        }
        DataType::Custom(name, modifiers)
            if modifiers.is_empty()
                && name.0.len() == 1
                && let Ok(Some(ty)) = value::type_by_name(&name.to_string()) =>
        {
            ty
        }
        // **A known type written with a modifier it does not take** is PostgreSQL's own `42601`
        // and not a missing-type refusal: `'x'::name(10)` is
        // `type modifier is not allowed for type "name"`. Asked of `value::named_type`, which is
        // the one parser that knows which types take one — a second list here would be a second
        // place for the two to disagree.
        DataType::Custom(name, modifiers)
            if !modifiers.is_empty()
                && name.0.len() == 1
                && matches!(value::type_by_name(&name.to_string()), Ok(Some(_))) =>
        {
            // Asked for its *refusal*: `named_type` validates the modifier against the type and
            // raises PostgreSQL's sentence when the type takes none. A type that does take one
            // never arrives here — every such type has a `DataType` variant of its own — so a
            // success falls through to the refusal below rather than inventing a typmod.
            let spelled = format!("{}({})", name, modifiers.join(", "));
            value::named_type(&spelled)?;
            return Err(SqlError::unsupported(format!("the type {data_type}")));
        }
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
            refuse_if(column.operator_class.is_some(), "an index operator class")?;
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
///
/// **An operator class is no longer one of them in a `CREATE INDEX`** — it is recorded there
/// ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md))
/// — and it is still refused in a *constraint*, where a real server's grammar has no place for
/// one: `UNIQUE (a text_pattern_ops)` is a syntax error there.
fn index_key_options(column: &IndexColumn) -> Result<()> {
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
                // **`sqlparser` already parses it**, so the parser needed nothing: an operator
                // class is a name after the column, folded like every other unquoted identifier.
                opclass: column
                    .operator_class
                    .as_ref()
                    .map(|name| name.to_string().to_ascii_lowercase()),
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
        // **And a bare column reference**, which is `Value` because that is what PostgreSQL
        // prints it as: `GENERATED ALWAYS AS (c1) STORED` comes back `c1`, measured. It cannot
        // arise as an *index* key — a column there is a column key and never an expression — so
        // this arm only matters now that a generated column asks the same question.
        // **And the two shapes that carry their own brackets**: an `ARRAY[…]` constructor and a
        // subscript. Both were falling to the `Operator` arm below and taking a pair they do not
        // own — `(arr[1])` where a real server prints `arr[1]`, and one more again in the key
        // list. They are `Value` for the same reason a call is `Call`: the brackets already say
        // where the expression ends, so nothing has to be added to keep it together. Measured
        // through every reader — `pg_get_expr(indexprs)`, the key list, a generated column, a
        // `DEFAULT` and a `CHECK` — in `tests/corpus/pg19_deparse_census.txt`'s group C, and
        // `(ARRAY[a, b])[1]` is both at once and still one pair.
        Expr::Value(_)
        | Expr::Cast { .. }
        | Expr::TypedString { .. }
        | Expr::Case { .. }
        | Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Array(_)
        | Expr::CompoundFieldAccess { .. } => ExprShape::Value,
        _ => ExprShape::Operator,
    }
}

/// The name as written, when [`relation_name`] threw a qualifier away.
///
/// **Only `public.`**, because it is the only schema this node spells *out* of a stored name: a
/// relation there is stored bare (`catalog::SCHEMA_SEPARATOR`), so the qualifier is gone by the
/// time anything can fail to find it. Every other schema is part of the stored name and quotes
/// itself back for free — `relation "nosuchschema.sometable" does not exist` was already right.
///
/// `pg_catalog.` is **not** one of these any more: it is a stored qualifier now, so it quotes
/// itself back through [`catalog::display_name`] the way any other schema does.
fn dropped_qualifier(name: &ObjectName) -> Option<String> {
    let parts: Option<Vec<&str>> = name
        .0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.as_str()))
        .collect();
    let [schema, relation] = parts.as_deref()? else {
        return None;
    };
    schema
        .eq_ignore_ascii_case(PUBLIC_SCHEMA)
        .then(|| format!("{PUBLIC_SCHEMA}.{}", fold_identifier(relation, false).0))
}

/// A relation **anywhere a relation is named** — a `FROM` clause, a `DROP`, an `INSERT INTO`, a
/// `CREATE INDEX ... ON` — which is where a schema may be written.
///
/// **A qualifier is where to look, not decoration on a name**, and each of the three this node
/// knows keeps that in a different way:
///
/// * **`pg_catalog.x` keeps its qualifier**, in the stored form a user schema uses. `pg_class` is
///   a relation on its own here too, because `pg_catalog` is in the implicit search path — but
///   `pg_catalog.books` must be `42P01` however many `books` there are in `public`, and a lowering
///   that dropped the qualifier could not tell the two apart. Nothing is ever *written* under this
///   prefix (creating in it is `42501`), so it is a lookup key and never a record's name.
/// * **`information_schema.tables` keeps its qualifier as part of the name**, because a bare
///   `tables` is **not** a relation on a real server — `42P01` — and answering it here would
///   invent one. That schema is not in the search path, which is the same fact from the other
///   side (`catalog::information_schema`).
/// * **`public.x` is `x`.** A relation in `public` is stored with no qualifier at all, which is
///   what keeps every key written before schemas existed readable — so this is the one qualifier
///   that cannot survive to resolution. What it wrote is carried beside the name instead
///   ([`plan::TableRef::written`]), which is what makes `public.pg_class` the `42P01` a real
///   server gives rather than the catalog's `pg_class`.
///
/// Every other schema is part of the stored name, separated by a NUL rather than a dot.
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
        // **The qualifier stays**, in the stored form a user schema uses: `pg_catalog` is a real
        // namespace here now (`catalog::RESERVED_SCHEMAS`), and dropping it made
        // `pg_catalog.books` find the *user's* `books` and `pg_catalog.nosuch` say
        // `relation "nosuch" does not exist` where PostgreSQL quotes the qualifier back.
        // Nothing is ever *written* under this prefix — creating in it is `42501` — so it is a
        // lookup key and never a record's name.
        // **`pg_temp` with no number is the session's own**, and the session is not in reach
        // here — so it is stored as the bare word and rewritten to `pg_temp_<n>` where the name is
        // resolved (`crate::exec::Executor::resolve_unqualified`). The same shape the qualifier
        // below takes, and for the same reason: a qualifier is where to look.
        if schema.eq_ignore_ascii_case(catalog::PG_TEMP_ALIAS) {
            return Ok(catalog::qualify(
                catalog::PG_TEMP_ALIAS,
                &fold_identifier(relation, false).0,
            ));
        }
        if schema.eq_ignore_ascii_case(catalog::PG_CATALOG_SCHEMA) {
            return Ok(catalog::qualify(
                catalog::PG_CATALOG_SCHEMA,
                &fold_identifier(relation, false).0,
            ));
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
    Ok(plan::Statement::CreateType(plan::CreateType {
        name,
        kind,
        // Set by the caller that can see the `DO` block this came out of, if it came out of one.
        if_not_exists: false,
    }))
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

/// The `EXPLAIN` option list, as it accumulates.
///
/// # Why every option is *accepted* and only two of them are honoured
///
/// This server has no cost model, no buffer accounting and no per-node timing, so `COSTS`,
/// `BUFFERS`, `TIMING`, `WAL`, `MEMORY`, `SETTINGS`, `SUMMARY`, `GENERIC_PLAN` and `SERIALIZE`
/// change nothing about what comes back. Accepting them anyway is the [ADR
/// 0031](../../../../docs/adr/0031-rails-compatibility-is-measured.md) call: the plan text here
/// already differs from a real server's in every line, so a client that asks for buffer counts is
/// getting a different answer either way — and one of the two answers is a plan and the other is
/// a `0A000` that stops the statement. What is *not* acceptable is answering where PostgreSQL
/// raises, which is why the option names, their values and the three "requires `ANALYZE`" checks
/// are reproduced exactly (`tests/captures/pg19_explain_options.txt`, replayed by
/// `tests/corpus/pg19_routing_explain.txt`).
#[derive(Debug, Default)]
struct ExplainOptions {
    analyze: bool,
    /// One flag per name in [`RUN_ONLY`], which is why it is an array and not three fields: the
    /// order is PostgreSQL's own check order, and the checks happen **after** the whole list is
    /// read rather than where each option is met — `EXPLAIN (TIMING, ANALYZE)` is legal.
    run_only: [bool; RUN_ONLY.len()],
    format: plan::ExplainFormat,
}

/// The options that are only meaningful about a run, in the order PostgreSQL checks them.
const RUN_ONLY: [&str; 3] = ["WAL", "TIMING", "SERIALIZE"];

impl ExplainOptions {
    /// Reads one `name [value]` pair.
    fn set(&mut self, option: &UtilityOption) -> Result<()> {
        // PostgreSQL's grammar downcases an unquoted option name before it reaches either the
        // dispatch or the message that refuses it, so this does too.
        let name = ident(&option.name).to_ascii_lowercase();
        let slot = match name.as_str() {
            "analyze" => &mut self.analyze,
            "timing" => &mut self.run_only[1],
            "wal" => &mut self.run_only[0],
            "serialize" => &mut self.run_only[2],
            // Read and discarded: see the type's own note on why these are not refusals.
            "verbose" | "costs" | "buffers" | "settings" | "summary" | "memory"
            | "generic_plan" => {
                option_boolean(&name, option.arg.as_ref())?;
                return Ok(());
            }
            "format" => {
                let Some(value) = option.arg.as_ref().and_then(option_word) else {
                    return Err(SqlError::OptionRequiresParameter(name));
                };
                let Some(format) = plan::ExplainFormat::parse(&value) else {
                    return Err(SqlError::UnrecognizedExplainOptionValue {
                        option: "format",
                        value: value.to_ascii_lowercase(),
                    });
                };
                self.format = format;
                return Ok(());
            }
            _ => return Err(SqlError::UnrecognizedExplainOption(name)),
        };
        *slot = option_boolean(&name, option.arg.as_ref())?;
        Ok(())
    }

    /// The three checks PostgreSQL makes *after* reading the whole list, in its order.
    fn validate(&self) -> Result<()> {
        for (asked, name) in self.run_only.iter().zip(RUN_ONLY) {
            if *asked && !self.analyze {
                return Err(SqlError::ExplainOptionRequiresAnalyze(name));
            }
        }
        Ok(())
    }
}

/// An option's value read as a boolean, PostgreSQL's `defGetBoolean` rules: no argument at all is
/// `true`, and `true`/`false`/`on`/`off`/`1`/`0` are what it will read, quoted or not.
fn option_boolean(name: &str, arg: Option<&Expr>) -> Result<bool> {
    let Some(arg) = arg else { return Ok(true) };
    let word = option_word(arg).ok_or_else(|| SqlError::NonBooleanOption(name.to_owned()))?;
    match word.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "t" | "y" | "yes" => Ok(true),
        "false" | "off" | "0" | "f" | "n" | "no" => Ok(false),
        _ => Err(SqlError::NonBooleanOption(name.to_owned())),
    }
}

/// The one word an option's argument is, however it was spelled: a bare identifier, a number, a
/// quoted string or a boolean literal. Anything else — an expression, a function call — is not an
/// option value at all and answers `None`.
fn option_word(arg: &Expr) -> Option<String> {
    match arg {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::Value(value) => match &value.value {
            Value::Number(digits, _) => Some(digits.clone()),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Some(text.clone()),
            Value::Boolean(flag) => Some(flag.to_string()),
            _ => None,
        },
        _ => None,
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
