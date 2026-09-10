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

/// The two ways a `Bind`'s values can fail to line up with the statement, in PostgreSQL's order.
///
/// **Type resolution happens at `Parse` and the count is checked at `Bind`**, so a position nothing
/// mentions is `42P18` even when the count is also wrong — measured, `SELECT … WHERE n = $2` with
/// one value is `could not determine data type of parameter $1` and not a count error.
///
/// Neither applies to the simple query protocol, which never binds: a `$1` there is
/// `42P02 there is no parameter $1`, and [`Params::bound`] is what tells the two paths apart.
pub(super) fn refuse_unmatched_parameters(
    statement: &Statement,
    params: &Params<'_>,
) -> Result<()> {
    if !params.bound {
        return Ok(());
    }
    let mut referenced: Vec<bool> = Vec::new();
    for_each_expr(statement, &mut |expr| {
        if let Expr::Parameter(number) = expr {
            let at = (*number as usize).saturating_sub(1);
            if referenced.len() <= at {
                referenced.resize(at + 1, false);
            }
            referenced[at] = true;
        }
    });
    let required = referenced.len();
    // Every position up to whichever of the two is higher: a value supplied past the last one the
    // statement mentions is as untypable as a gap before it.
    for at in 0..required.max(params.values.len()) {
        if !referenced.get(at).copied().unwrap_or(false) {
            let number = u32::try_from(at + 1).unwrap_or(u32::MAX);
            return Err(SqlError::IndeterminateParameterType(number));
        }
    }
    if params.values.len() < required {
        return Err(SqlError::BindParameterCount {
            supplied: params.values.len(),
            required,
        });
    }
    Ok(())
}

use crate::catalog::TableDef;
use crate::error::{Result, SqlError};
use crate::pgwire::session::Params;
use crate::plan::{BinaryOp, Expr, Literal, Statement};
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// Every parameter's type, indexed from zero for `$1`.
/// The type a **bound parameter** for this column takes, which is not always the column's own.
///
/// An enum is stored as its ordinal ([ADR 0050](../../../docs/adr/0050-an-enum-is-its-ordinal.md)),
/// so `ColumnDef::ty` is an `int2` — and reading a bound `'ok'` with that gives
/// `22P02 invalid input syntax for type smallint: "ok"`, which is what run 89 saw from every
/// `ActiveRecord` write to an enum column, because `ActiveRecord` prepares. A **label** is what a
/// client sends, so the parameter is read as text and `assign::into_enum` maps it to the ordinal
/// on the way into the row, exactly as it does for a literal.
///
/// **The kind, not the presence.** This first asked only whether the column had a user type at
/// all, and every `CREATE TYPE` sets that: a **domain** and a **user-defined range** took `text`
/// too, and then the assignment refused them with `42804 column "price" is of type numeric but
/// expression is of type text`. Two `range_test.rb` tests and one in `domain_test.rb` went red on
/// it while every enum test stayed green, because an enum is the one kind whose storage type is
/// not what a client sends. So the question is which kind it is, and only `Enum` answers `text`.
fn parameter_type(table: &TableDef, at: usize) -> ColumnType {
    let column = &table.columns[at];
    match column.user_type.and_then(|oid| table.enums.get(&oid)) {
        Some(def) if matches!(def.kind, crate::catalog::TypeKind::Enum { .. }) => ColumnType::Text,
        // A domain is its base type and a range is its own; a user type the table did not hydrate
        // is the column's own type too, which is what this answered before enums were special.
        _ => column.ty,
    }
}

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
    // **A cast is how a parameter's type is written down.** `SELECT $1::integer` says the
    // parameter is an `integer`, and a real server reports it as one — which is what
    // `connection_test.rb` prepares and then describes. Taken before the walk below so that a
    // cast wins over anything the statement's shape would otherwise infer, exactly as it does on
    // a real server: the cast is the client's own declaration.
    for_each_expr(statement, &mut |expr| {
        if let Expr::Cast { operand, to, .. } = expr
            && let Expr::Parameter(number) = operand.as_ref()
        {
            let at = (*number as usize).saturating_sub(1);
            if found.len() <= at {
                found.resize(at + 1, None);
            }
            found[at].get_or_insert(*to);
        }
    });
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
            // **The same rule [`substitute_placeholders`] applies, and for the same reason.** A
            // bound value carries a *representation*; the parameter has a **type**. For the kinds
            // that share `text`'s storage — `json`, `jsonb`, `xml`, `hstore`'s array, `void`, the
            // two vectors — `Datum::from_text` answers a `Datum::Text`, so `column_type()` says
            // `text` and `WHERE jsonb_col = $1` becomes `42883 operator does not exist:
            // jsonb = text`. That is `Execute` answering something `Describe` does not, which is
            // the mirror of the defect `8dd0b4a8` fixed one function along: it taught the
            // *placeholder* to carry the type and left the bound value behind, because the census
            // that found it was a `Describe` census.
            //
            // Run 116's `query_cache_test#test_query_cache_handles_mutated_binds` is the red, and
            // `jsonb` is the type it names — `hstore` was already covered by the other half.
            //
            // The cast carries the type the representation cannot, and only where they differ, so
            // a parameter whose datum already answers its own type gains no node.
            Ok(value) => {
                // **Only where the two would not resolve against each other anyway.** A NULL
                // has no type to lose (`column_type()` is `None`), and a `varchar` parameter is a
                // `Datum::Text` whose family *is* text — wrapping either adds a node for nothing,
                // and the first one broke `LIMIT $1` with a NULL bound, which reaches
                // `Expr::evaluate` where only a literal is allowed. `same_family` is the measured
                // authority here for the reason `debts-v1.1.md` #43 made it one everywhere else:
                // it is the table that says which pairs a comparison resolves.
                let carries_its_type = match value.column_type() {
                    None => true,
                    Some(actual) => actual == ty || crate::exec::query::same_family(actual, ty),
                };
                let literal = Expr::Literal(Literal::Typed(Box::new(value)));
                *expr = if carries_its_type {
                    literal
                } else {
                    Expr::Cast {
                        operand: Box::new(literal),
                        to: ty,
                        typmod: crate::value::NO_TYPMOD,
                    }
                };
            }
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
                        seen(*number, parameter_type(table, *target));
                    }
                }
            }
        }
        // **A `DECLARE`'s query is a query.** A `$1` inside one is typed and bound exactly as it
        // would be in the bare `SELECT`, which is what delegating here buys — and the alternative,
        // an empty arm, is a parameter that reaches execution unbound.
        Statement::Cursor(crate::plan::CursorStatement::Declare { query, .. }) => {
            walk_select(query, tables, seen);
        }

        Statement::Select(select) => walk_select(select, tables, seen),
        Statement::Update(update) => {
            if let Some(table) = tables.first() {
                for (name, value) in &update.assignments {
                    match (table.column(name), value) {
                        // `SET c = $1`: the column's own type, which is the whole of it.
                        (Some(at), Expr::Parameter(number)) => {
                            seen(*number, parameter_type(table, at));
                        }
                        // **`SET c = <expression holding $1>`**, which is what a counter cache
                        // sends and what the `$1`-is-`text` bug was: the value is an arithmetic
                        // expression, so the arm above never saw the parameter and it kept the
                        // fallback. Walking it types the parameter from what it is *added to*,
                        // which is how PostgreSQL resolves it.
                        (_, value) => walk_predicate(value, &under_own_names(tables), tables, seen),
                    }
                }
            }
            let named = update_relations(update, tables);
            for predicate in update
                .filter
                .iter()
                .chain(update.joins.iter().filter_map(|join| join.on.as_ref()))
            {
                walk_predicate(predicate, &named, tables, seen);
            }
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &delete.filter {
                walk_predicate(filter, &under_own_names(tables), tables, seen);
            }
        }
        Statement::Explain(explain) => walk(&explain.statement, tables, seen),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::DropExtension(_)
        | Statement::AlterIndexRename(_)
        | Statement::CreateSchema(_)
        | Statement::CreateRole(_)
        | Statement::DropRole(_)
        | Statement::CreateView(_)
        | Statement::DropView(_)
        | Statement::CreateMaterializedView(_)
        | Statement::CreateTableAs(_)
        | Statement::RefreshMaterializedView(_)
        | Statement::DropMaterializedView(_)
        | Statement::CreateDatabase(_)
        | Statement::DropDatabase(_)
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
        | Statement::CreateType(_)
        | Statement::Raise { .. }
        | Statement::Truncate(_)
        | Statement::DropType(_)
        | Statement::AlterType(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        // `FETCH`, `MOVE` and `CLOSE` name a cursor and carry no expression at all; a
        // `DECLARE`'s query is walked by the arm above.
        | Statement::Cursor(_)
        | Statement::TimeMachine(_) => {}
    }
}

/// The `SELECT` half of [`walk`], which is most of it: a `SELECT` has five places a parameter
/// can be typed from and the other statements have one each.
/// One relation a statement's predicates can name, under the name the statement calls it by.
///
/// **An alias *is* that name.** `INNER JOIN "categories" "group" ON "group"."id" = $1` calls the
/// relation `group`, and looking that qualifier up among the relations' *own* names finds nothing:
/// the parameter stayed untyped, fell back to `text`, and the statement was
/// `42883 operator does not exist: bigint = text` where a real server resolves `$1` to `bigint`.
///
/// Every statement r1 traced for that row is aliased and every one of them raised; the shapes that
/// worked in a probe were the ones written without an alias, which is why the bug survived a probe
/// that looked like the real thing.
type Named<'a> = (String, &'a TableDef);

/// The relations a `SELECT` names, each paired with the qualifier a column reference must use.
///
/// A derived table, a `VALUES` list and a set-returning function have no `TableDef` and are
/// skipped: their columns are not a catalog relation's and nothing here can type a parameter from
/// them. An entry whose relation is not among `tables` is skipped for the same reason.
fn named_relations<'a>(
    select: &crate::plan::Select,
    tables: &'a [std::sync::Arc<TableDef>],
) -> Vec<Named<'a>> {
    let mut named = Vec::new();
    for entry in select
        .from
        .iter()
        .chain(select.joins.iter().map(|join| &join.table))
    {
        if let Some(derived) = &entry.derived {
            // **A CTE is a derived table by the time this runs** — `plan::cte::inline` rewrites the
            // `WITH` list at lowering — so a column of a CTE reaches here with no `TableDef` behind
            // it and nothing typed the `$n` beside it: `WITH t AS (SELECT * FROM posts)
            // SELECT id FROM t WHERE tags_count > $1` was
            // `42883 operator does not exist: integer > text` (`with_test.rb`, two tests).
            //
            // **Only a derived table that re-projects one relation's columns unchanged**, which is
            // the case that is safe to answer: a bare `*` keeps every name and type of the relation
            // under it, so the outer name can stand for that relation. A projection that computes,
            // renames, or draws on two relations does not, and it is skipped exactly as before
            // rather than guessed at — `SELECT a AS b` would otherwise type `b` as `a`.
            if let Some(inner) = passes_columns_through(derived, tables) {
                named.push((
                    entry
                        .alias
                        .clone()
                        .unwrap_or_else(|| bare(&entry.name).to_owned()),
                    inner,
                ));
            }
            continue;
        }
        if entry.values.is_some() || entry.function.is_some() {
            continue;
        }
        // **Both sides are compared at the same qualification**, which is the half that was
        // missing. A `TableDef`'s name is unique across the tenant and therefore *qualified*
        // (`rails_pg_schema_user1.schema_things`), while what a statement writes is usually bare —
        // so a bare-to-qualified comparison matched nothing for every table outside the session's
        // default schema, and a `$1` compared with `id integer` was typed against nothing and fell
        // back to text: `operator does not exist: integer = text`
        // (`schema_authorization_test.rb`'s `test_auth_with_bind`).
        //
        // A qualified entry is matched whole, so that `FROM a.t JOIN b.t` cannot resolve both of
        // its halves to whichever `t` the list happens to hold first.
        let Some(def) = tables
            .iter()
            .find(|candidate| {
                if entry.name.contains(crate::catalog::SCHEMA_SEPARATOR) {
                    candidate.name == entry.name
                } else {
                    bare(&candidate.name) == entry.name
                }
            })
            .map(AsRef::as_ref)
        else {
            continue;
        };
        let relation = bare(&entry.name);
        named.push((
            entry.alias.clone().unwrap_or_else(|| relation.to_owned()),
            def,
        ));
    }
    named
}

/// The one relation a derived table re-projects unchanged, if that is all it does.
///
/// `WITH t AS (SELECT * FROM posts)` publishes `posts`'s columns under `t`, with the same names and
/// the same types, so a `$n` compared with one of them can be typed from `posts`. That is the whole
/// of what this answers, and the conditions are what keep it true:
///
/// * the projection is a single `*` or `q.*` — anything computed or renamed publishes a column that
///   is not the relation's, and `SELECT a AS b` would otherwise type `b` from `a`;
/// * no column alias list (`WITH t (x, y) AS …`), which renames every column at once;
/// * and one relation to pass the columns *from*, which is where the two spellings of the
///   wildcard part company. A bare `*` over a join publishes both relations' columns
///   concatenated, and "the table underneath" is not a thing — so it needs exactly one relation.
///   A qualified `a.*` **names** the one it publishes, so a join underneath is no ambiguity at
///   all. That is `bind_parameter_test.rb`'s statement: `SELECT authors.* FROM authors INNER JOIN
///   posts …` inside a derived table, whose `authors.id` PostgreSQL types as `bigint` and this
///   node typed as `text` — measured, `{text,text,bigint}` against `pg_prepared_statements`.
///
/// **It recurses**, because the statement that found this is three deep: `posts_with_tags` over
/// `posts`, `posts_with_tags_and_truthy` over that, and the `$1` in the third one.
fn passes_columns_through<'a>(
    derived: &crate::plan::Derived,
    tables: &'a [std::sync::Arc<TableDef>],
) -> Option<&'a TableDef> {
    if !derived.columns.is_empty() {
        return None;
    }
    let [only] = &derived.select.projection[..] else {
        return None;
    };
    let relations = named_relations(&derived.select, tables);
    match only {
        crate::plan::SelectItem::Wildcard => match relations.as_slice() {
            [(_, def)] => Some(*def),
            _ => None,
        },
        crate::plan::SelectItem::QualifiedWildcard(qualifier) => relations
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(qualifier))
            .map(|(_, def)| def),
        // Anything computed or renamed publishes a column that is not the relation's.
        crate::plan::SelectItem::Expr { .. } => None,
    }
}

/// An `UPDATE`'s relations, each under the name its predicates use.
///
/// **The target may be aliased, and the same table may be in the `FROM` as well.** That pair is
/// `has_many_associations_test`'s statement and it is why every relation under its own name is not
/// enough:
///
/// ```sql
/// UPDATE "posts" "__active_record_update_alias" SET "author_id" = $1
///   FROM "posts" INNER JOIN "categorizations" ON …
///  WHERE "posts"."type" = $3 AND "posts"."author_id" = $4 AND "posts"."id" = $5
///    AND "posts"."id" = "__active_record_update_alias"."id"
/// ```
///
/// `posts` there is the **`FROM`** relation, and the target answers only to its alias. Naming both
/// `posts` made the lookup ambiguous and every `posts.`-qualified bind fell back to `text`:
/// measured `{bigint,boolean,text,text,text}` against PostgreSQL 19's
/// `{bigint,boolean,text,bigint,bigint}`, which is the `bigint = text` the suite reports.
///
/// `tables` arrives in [`table_names`]'s order — the target, then the `FROM` chain — so the names
/// are zipped onto it positionally, which is the same thing `returning_fields` relies on.
fn update_relations<'a>(
    update: &crate::plan::Update,
    tables: &'a [std::sync::Arc<TableDef>],
) -> Vec<Named<'a>> {
    let target = update
        .alias
        .clone()
        .unwrap_or_else(|| bare(&update.table).to_owned());
    let names = std::iter::once(target).chain(
        update
            .from
            .iter()
            .chain(update.joins.iter().map(|join| &join.table))
            .map(|entry| {
                entry
                    .alias
                    .clone()
                    .unwrap_or_else(|| bare(&entry.name).to_owned())
            }),
    );
    names
        .zip(tables.iter())
        .map(|(name, def)| (name, def.as_ref()))
        .collect()
}

/// Every relation under its own name, for the statements that have no `FROM` list to read aliases
/// from — a `DELETE`'s predicates, and an `UPDATE`'s assignments.
fn under_own_names(tables: &[std::sync::Arc<TableDef>]) -> Vec<Named<'_>> {
    tables
        .iter()
        // **Bare**, because that is the name a predicate writes: `UPDATE schema_things SET … WHERE
        // schema_things.id = $1` qualifies with the relation, never with its schema.
        .map(|def| (bare(&def.name).to_owned(), def.as_ref()))
        .collect()
}

/// A relation name without its schema.
///
/// **The separator is a NUL, not a dot** (`catalog::SCHEMA_SEPARATOR`), and getting that wrong is
/// invisible in the one schema everybody tests in: `catalog::qualify` leaves a `public` relation's
/// name *bare*, so a dot-splitting version of this strips nothing and still compares equal there.
/// It is every other schema that breaks, which is why this surfaced only under
/// `SET SESSION AUTHORIZATION`.
pub(super) fn bare(name: &str) -> &str {
    name.rsplit(crate::catalog::SCHEMA_SEPARATOR)
        .next()
        .unwrap_or(name)
}

fn walk_select(
    select: &crate::plan::Select,
    tables: &[std::sync::Arc<TableDef>],
    seen: &mut impl FnMut(u32, ColumnType),
) {
    // **Every predicate, not only the `WHERE`.** A `HAVING` and a join's `ON` are
    // predicates over the same columns, and a parameter in one is typed by the column it
    // is compared against exactly as in a `WHERE`.
    let named = named_relations(select, tables);
    for predicate in select
        .filter
        .iter()
        .chain(select.having.iter())
        .chain(select.joins.iter().filter_map(|join| join.on.as_ref()))
        // A `GROUP BY` expression is typed the same way: `GROUP BY n > $1` compares a
        // column against a parameter exactly as a `WHERE` does.
        .chain(select.group_by.iter())
    {
        walk_predicate(predicate, &named, tables, seen);
    }
    // **And the target list**, which holds no predicate of its own but may hold a
    // *subquery* that does: `SELECT (SELECT count(*) FROM t WHERE n > $1)` types `$1`
    // from `n`, and reaching it means walking the projection the same way. Anything else
    // there matches no arm and costs one call.
    for item in &select.projection {
        if let crate::plan::SelectItem::Expr { expr, .. } = item {
            walk_predicate(expr, &named, tables, seen);
        }
    }
    // **A derived table's predicates too.** Its parameters are numbered in the same
    // statement, so `SELECT … FROM (SELECT … WHERE n > $1) AS x` types `$1` from `n` —
    // the relations are already all in `tables`, which is the whole statement's.
    for table in select
        .from
        .iter()
        .chain(select.joins.iter().map(|join| &join.table))
        .chain(select.ctes.iter())
    {
        if let Some(derived) = &table.derived {
            walk(&Statement::Select(derived.select.clone()), tables, seen);
        }
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

/// An expression's type where this inference can name it without a planner.
///
/// Deliberately partial: a column in one of the statement's tables, a constant, a `COALESCE` of
/// something it can name, and arithmetic over those. **`None` is the honest answer for everything
/// else**, and it is what leaves `$1 + $2` unresolved so that a real server's
/// `42725 operator is not unique: unknown + unknown` is what comes out, rather than a type this
/// function invented.
fn static_type(expr: &Expr, named: &[Named<'_>]) -> Option<ColumnType> {
    match expr {
        // The same lookup `column_type` does, aliases and all.
        Expr::Column { table, name } => column_type(named, table.as_deref(), name),
        // A bare integer constant is an `int8` here and an `integer` there — the standing
        // constant-width divergence, and it is the *resolution* that matters: `1 + $1` makes the
        // parameter a number either way, and reading `"4"` as an `int8` gives the same answer.
        Expr::Literal(literal) => super::query::literal_type(literal),
        // `COALESCE`'s type is its first argument that has one, which is what makes
        // `COALESCE(c, 0) + $1` an integer.
        Expr::Coalesce(items) => items.iter().find_map(|item| static_type(item, named)),
        Expr::Arithmetic { left, right, .. } => {
            static_type(left, named).or_else(|| static_type(right, named))
        }
        // **An aggregate inside a larger expression still types the parameter beside it.**
        // `(sum(salary) + 0) > $1` is `bigint` on a real server, read off
        // `pg_prepared_statements.parameter_types` — the arithmetic's type is the aggregate's, so
        // this arm is what the one above needs to see through.
        Expr::Aggregate(call) => aggregate_type(call, named),
        _ => None,
    }
}

/// The type an aggregate call answers with, when this pass can resolve its argument.
///
/// The table is [`crate::exec::aggregate::Aggregation::result_type`]'s, not a copy: `sum(int4)` is
/// `bigint` and `sum(int8)` is `numeric`, and a second opinion about which would put a
/// `ParameterDescription` on the wire that the executor then disagrees with.
///
/// `None` when there is nothing to say — an argument this pass cannot type, or an arity error the
/// planner is about to refuse — because the fallback for a parameter nobody typed is `text`, and
/// inventing a type here would be worse than that.
fn aggregate_type(call: &crate::plan::AggregateCall, named: &[Named<'_>]) -> Option<ColumnType> {
    let arg = match call.arg() {
        Some(arg) => Some(static_type(arg, named)?),
        // `count(*)`, which reads no value at all. Any other call with no argument is a wrong
        // arity, and `result_type` reads a missing argument as `count(*)`'s.
        None if call.star => None,
        None => return None,
    };
    super::aggregate::Aggregation::result_type(call.func, arg).ok()
}

/// A parameter compared against a column takes that column's type. That is the whole of the
/// inference in a `WHERE`, and it is what `WHERE id = $1` needs.
/// `named` is what the statement calls its relations — aliases included — and `tables` is every
/// relation it resolved. Both are needed: a qualifier is matched against the first, and a subquery
/// builds its own `named` list out of the second.
fn walk_predicate(
    expr: &Expr,
    named: &[Named<'_>],
    tables: &[std::sync::Arc<TableDef>],
    seen: &mut impl FnMut(u32, ColumnType),
) {
    match expr {
        Expr::Binary { op, left, right }
            if op.is_comparison() || *op == BinaryOp::And || *op == BinaryOp::Or =>
        {
            if op.is_comparison() {
                // **A parameter takes the type of whatever is on the other side**, and that is
                // one rule rather than a list of shapes. `static_type` is what knows the shapes:
                // a column (through `column_type`, aliases and qualifiers included), a literal,
                // a `COALESCE`, an arithmetic result, and an aggregate.
                //
                // It was three special cases, and the gaps between them were the bug. `count(…)
                // > $1` was typed because `count`'s result type is fixed whatever it counts;
                // every other aggregate "needs its argument resolved, which is the planner's job
                // and not this one's" — but the argument is resolved right here, by the same
                // lookup that types `WHERE salary > $1`. The cost was `HAVING (sum(salary) > $1)`
                // answering **zero rows** where a real server answers three: the parameter kept
                // the `text` fallback and the comparison became a text one
                // (`tests/having_bind.rs`,
                // `finder_test#test_find_with_group_and_sanitized_having_method`). `count` is the
                // one aggregate that cannot show this, which is why it survived as the example,
                // and `(sum(salary) + 0) > $1` was a second gap one shape further out.
                //
                // Measured off `pg_prepared_statements.parameter_types`: `sum(salary) > $1` is
                // `bigint`, `avg(salary) > $1` is `numeric`, `max(name) > $1` is `text`,
                // `(sum(salary) + 0) > $1` is `bigint`, and `count(*) > $1` is `bigint` like the
                // rest rather than as a special case.
                //
                // **Only when the other side has a type of its own**, which is why a quoted
                // string types nothing: `'a' = $1` leaves both `unknown` on a real server too,
                // and an ambiguous bare column name types nothing rather than the first match —
                // the planner will refuse the statement anyway, and guessing here would put a
                // `ParameterDescription` on the wire for a query that is about to fail.
                match (left.as_ref(), right.as_ref()) {
                    (other, Expr::Parameter(number)) | (Expr::Parameter(number), other) => {
                        if let Some(ty) = static_type(other, named) {
                            seen(*number, ty);
                        }
                    }
                    _ => {}
                }
            }
            walk_predicate(left, named, tables, seen);
            walk_predicate(right, named, tables, seen);
        }
        Expr::Binary { left, right, .. } => {
            walk_predicate(left, named, tables, seen);
            walk_predicate(right, named, tables, seen);
        }
        // **A parameter in an arithmetic expression takes the *other operand's* type.** This is
        // the shape run 51 reported as `operator does not exist: integer + text` and it is not an
        // operator at all: `ActiveRecord`'s counter-cache update is
        // `SET c = COALESCE(c, 0) + $1`, in which nothing is text — the parameter had simply
        // fallen through to the `text` default before anything looked at what it was added to.
        // PostgreSQL resolves the other way round: an untyped parameter takes its type from the
        // context it appears in, and `pg_prepared_statements.parameter_types` says `integer` for
        // this exact statement.
        //
        // **Only when the other side has a type of its own**: `$1 + $2` resolves nothing, which is
        // what makes it `42725 operator is not unique: unknown + unknown` rather than a guess.
        Expr::Arithmetic { left, right, .. } => {
            match (left.as_ref(), right.as_ref()) {
                (other, Expr::Parameter(number)) | (Expr::Parameter(number), other) => {
                    if let Some(ty) = static_type(other, named) {
                        seen(*number, ty);
                    }
                }
                _ => {}
            }
            walk_predicate(left, named, tables, seen);
            walk_predicate(right, named, tables, seen);
        }
        // `COALESCE(c, 0)` is where the counter-cache update's type comes from, so its arguments
        // are walked like any other expression — a parameter *inside* one is typed by whatever it
        // sits beside further out.
        Expr::Coalesce(items) => {
            for item in items {
                walk_predicate(item, named, tables, seen);
            }
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => {
            walk_predicate(operand, named, tables, seen);
        }
        // **`n IN ($1, $2, $3)` types every one of them from `n`.** Walking the list as
        // independent predicates types none — a bare `$1` matches no arm — and the statement then
        // compares an `integer` against `text`: `42883 operator does not exist: integer = text`,
        // which is what `ActiveRecord`'s `where(id: [1,2,3])` sends on every association load.
        Expr::InList { operand, list, .. } => {
            walk_predicate(operand, named, tables, seen);
            if let Expr::Column { table, name } = operand.as_ref()
                && let Some(ty) = column_type(named, table.as_deref(), name)
            {
                for item in list {
                    if let Expr::Parameter(number) = item {
                        seen(*number, ty);
                    }
                }
            }
            for item in list {
                walk_predicate(item, named, tables, seen);
            }
        }
        // **The subquery's own clauses type their own parameters.** `IN (SELECT id FROM t LIMIT
        // $1)` types `$1` as `bigint` because it is a count, and `IN (SELECT id FROM t WHERE
        // title = $1)` types it from `title` — a column of a table the outer statement never
        // names, which is why [`table_names`] collects a subquery's relations too. Walked as the
        // `SELECT` it is, so every rule above applies inside it without being restated.
        Expr::Subquery(sub) => {
            for operand in &sub.operands {
                walk_predicate(operand, named, tables, seen);
                // `$1 IN (SELECT n FROM t)` is the operand taking the **subquery's** column
                // type, which is the one rule here that reads across the boundary rather than
                // within it. `SubqueryExpr::column` cannot answer: it is filled by the planner and
                // this runs at `Parse`, before any plan exists. So the sub-select's single output
                // column is read the way every other type here is read — through the catalog, by
                // name — and anything more involved than a column reference keeps the `text`
                // fallback rather than being guessed at.
                // Only for a single operand: a row on the left is compared column by column
                // and the sub-select's *single* column is not what any of them meets.
                if let [Expr::Parameter(number)] = sub.operands.as_slice()
                    && let Some(ty) = single_column_type(&sub.select, tables)
                {
                    seen(*number, ty);
                }
            }
            walk(&Statement::Select(sub.select.clone()), tables, seen);
        }
        _ => {}
    }
}

/// The type of a sub-select's **single output column**, when it is a column this inference can
/// name.
///
/// Only the one-item target list, and only a plain column reference in it: those are the shapes
/// `ActiveRecord` writes — `IN (SELECT "t"."id" FROM …)` — and the type is then a fact in the
/// catalog rather than a guess. Everything else answers `None` and the parameter keeps `text`,
/// because a `ParameterDescription` put on the wire from a guess is worse than the fallback.
fn single_column_type(
    select: &crate::plan::Select,
    tables: &[std::sync::Arc<TableDef>],
) -> Option<ColumnType> {
    let [crate::plan::SelectItem::Expr { expr, .. }] = select.projection.as_slice() else {
        return None;
    };
    let Expr::Column { table, name } = expr else {
        return None;
    };
    column_type(&named_relations(select, tables), table.as_deref(), name)
}

/// One column's type, by name and an optional qualifier.
///
/// **An ambiguous bare name types nothing** rather than the first match: the planner will refuse
/// the statement anyway, and guessing here would put a `ParameterDescription` on the wire for a
/// query that is about to fail.
fn column_type(named: &[Named<'_>], qualifier: Option<&str>, name: &str) -> Option<ColumnType> {
    let mut found = None;
    for (called, candidate) in named {
        // **The qualifier and the name it is compared with must be at the same qualification.**
        // A three-part column — `"music"."albums"."id"`, which `ActiveRecord` writes for a model
        // whose `table_name` carries a schema — lowers with `table` set to the *qualified* relation
        // name (`music` NUL `albums`), while `called` is the alias or the bare name. So this
        // matched nothing, the parameter beside it kept `text`, and the statement came back
        // `42883 operator does not exist: bigint = text` — `schema_test.rb`'s
        // `test_habtm_table_name_with_schema`, reachable only once three-part names parsed at all.
        //
        // Comparing the bare forms can only ever match *more*, and the ambiguity guard below is
        // what makes that safe: two relations of one bare name leave `found` set twice and answer
        // `None`, which is the `text` fallback rather than a guess. Over-matching costs nothing a
        // real server does not also do — a qualifier naming a relation the `FROM` does not have is
        // `42P01` when the statement runs, and typing a parameter never decides that.
        if qualifier.is_some_and(|qualifier| qualifier != called && bare(qualifier) != called) {
            continue;
        }
        if let Some(at) = candidate.column(name) {
            if found.is_some() {
                return None;
            }
            found = Some(parameter_type(candidate, at));
        }
    }
    found
}

/// Visits every expression in a statement, for substitution.
/// Every expression of a `SELECT` a parameter can stand in — **all** of them.
///
/// This walked the projection, the `WHERE`, the `LIMIT`, the `OFFSET` and the `ORDER BY`, and a
/// parameter anywhere else survived substitution and reached the row evaluator, where it is
/// `42P02 there is no parameter $n`. That is the message run 46 counted **98 times across 19
/// files**: `HAVING count(*) > $1` is `calculations_test.rb` alone, twenty of them, and a join
/// condition or a derived table is most of the rest.
///
/// A clause added to `plan::Select` and not added here is the same bug again, which is why this is
/// written out field by field rather than as a catch-all.
fn walk_select_mut(select: &mut crate::plan::Select, visit: &mut impl FnMut(&mut Expr)) {
    for item in &mut select.projection {
        if let crate::plan::SelectItem::Expr { expr, .. } = item {
            walk_expr_mut(expr, visit);
        }
    }
    for expr in select
        .filter
        .iter_mut()
        .chain(select.having.iter_mut())
        .chain(select.limit.iter_mut())
        .chain(select.offset.iter_mut())
    {
        walk_expr_mut(expr, visit);
    }
    for expr in &mut select.group_by {
        walk_expr_mut(expr, visit);
    }
    for item in &mut select.order_by {
        walk_expr_mut(&mut item.expr, visit);
    }
    // A join's `ON`, and the relation on either side: a derived table is a `SELECT` of its own and
    // its parameters are numbered in the same statement.
    for join in &mut select.joins {
        if let Some(on) = &mut join.on {
            walk_expr_mut(on, visit);
        }
        walk_table_ref_mut(&mut join.table, visit);
    }
    if let Some(from) = &mut select.from {
        walk_table_ref_mut(from, visit);
    }
    for cte in &mut select.ctes {
        walk_table_ref_mut(cte, visit);
    }
}

/// A `FROM` entry: a derived table is a `SELECT`, walked as one.
///
/// **A `VALUES` list in a `FROM` is walked too**, and it was not: its rows are expressions in the
/// same statement, numbered in the same `$n` sequence, and skipping them left a parameter in
/// `SELECT v FROM (VALUES ($1)) t(v)` counted and never filled. Found by a cast to a user-defined
/// type reaching the row evaluator unresolved (ADR 0053) — the same hole, one pass over.
fn walk_table_ref_mut(table: &mut crate::plan::TableRef, visit: &mut impl FnMut(&mut Expr)) {
    if let Some(derived) = &mut table.derived {
        walk_select_mut(&mut derived.select, visit);
    }
    if let Some(values) = &mut table.values {
        for row in &mut values.rows {
            for expr in row {
                walk_expr_mut(expr, visit);
            }
        }
    }
}

/// The read-only twin of [`walk_select_mut`], and it has to agree with it clause for clause.
///
/// This one **sizes** the parameter list (`infer` takes the highest `$n` it sees) and the other
/// substitutes. A clause in one and not the other is a parameter counted and never filled, or
/// filled and never counted — so they are written as a pair and reviewed as a pair.
fn for_each_in_select<'a>(select: &'a crate::plan::Select, each: &mut impl FnMut(&'a Expr)) {
    for item in &select.projection {
        if let crate::plan::SelectItem::Expr { expr, .. } = item {
            each(expr);
        }
    }
    select
        .filter
        .iter()
        .chain(select.having.iter())
        .chain(select.limit.iter())
        .chain(select.offset.iter())
        .for_each(&mut *each);
    select.group_by.iter().for_each(&mut *each);
    for item in &select.order_by {
        each(&item.expr);
    }
    for join in &select.joins {
        if let Some(on) = &join.on {
            each(on);
        }
        for_each_in_table_ref(&join.table, each);
    }
    if let Some(from) = &select.from {
        for_each_in_table_ref(from, each);
    }
    for cte in &select.ctes {
        for_each_in_table_ref(cte, each);
    }
}

/// A `FROM` entry, for [`for_each_in_select`].
fn for_each_in_table_ref<'a>(table: &'a crate::plan::TableRef, each: &mut impl FnMut(&'a Expr)) {
    if let Some(values) = &table.values {
        for row in &values.rows {
            row.iter().for_each(&mut *each);
        }
    }
    if let Some(derived) = &table.derived {
        for_each_in_select(&derived.select, each);
    }
}

pub(super) fn walk_mut(statement: &mut Statement, visit: &mut impl FnMut(&mut Expr)) {
    match statement {
        Statement::Insert(insert) => {
            for row in &mut insert.rows {
                for expr in row {
                    walk_expr_mut(expr, visit);
                }
            }
            // `ON CONFLICT … DO UPDATE SET c = $1` — `upsert_all`'s shape, and a parameter here is
            // as ordinary as one in an `UPDATE`'s assignments.
            if let Some(crate::plan::OnConflict {
                action: crate::plan::ConflictAction::DoUpdate(assignments),
                ..
            }) = &mut insert.on_conflict
            {
                for (_, value) in assignments {
                    walk_expr_mut(value, visit);
                }
            }
        }
        Statement::Cursor(crate::plan::CursorStatement::Declare { query, .. }) => {
            walk_select_mut(query, visit);
        }

        Statement::Select(select) => walk_select_mut(select, visit),
        Statement::Update(update) => {
            for (_, value) in &mut update.assignments {
                walk_expr_mut(value, visit);
            }
            if let Some(filter) = &mut update.filter {
                walk_expr_mut(filter, visit);
            }
            // **Every clause, not the two that predate the `FROM`.** A `$1` left unsubstituted
            // reaches the row evaluator with nothing behind it, which is an `XX000` about a
            // statement a real server runs.
            for join in &mut update.joins {
                if let Some(on) = &mut join.on {
                    walk_expr_mut(on, visit);
                }
            }
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &mut delete.filter {
                walk_expr_mut(filter, visit);
            }
        }
        Statement::Explain(explain) => walk_mut(&mut explain.statement, visit),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::DropExtension(_)
        | Statement::AlterIndexRename(_)
        | Statement::CreateSchema(_)
        | Statement::CreateRole(_)
        | Statement::DropRole(_)
        | Statement::CreateView(_)
        | Statement::DropView(_)
        | Statement::CreateMaterializedView(_)
        | Statement::CreateTableAs(_)
        | Statement::RefreshMaterializedView(_)
        | Statement::DropMaterializedView(_)
        | Statement::CreateDatabase(_)
        | Statement::DropDatabase(_)
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
        | Statement::CreateType(_)
        | Statement::Raise { .. }
        | Statement::Truncate(_)
        | Statement::DropType(_)
        | Statement::AlterType(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        // `FETCH`, `MOVE` and `CLOSE` name a cursor and carry no expression at all; a
        // `DECLARE`'s query is walked by the arm above.
        | Statement::Cursor(_)
        | Statement::TimeMachine(_) => {}
    }
}

/// Every relation a `SELECT` names, **including inside a derived table**.
///
/// A parameter in `FROM (SELECT … WHERE n > $1) AS x` is typed from `n`, and `n` belongs to the
/// inner relation — so the inner relation has to be in the list the inference is given, or the
/// parameter falls back to `text` and the statement compares an `integer` against one.
fn collect_table_names<'a>(select: &'a crate::plan::Select, into: &mut Vec<&'a str>) {
    for table in select
        .from
        .iter()
        .chain(select.joins.iter().map(|join| &join.table))
        .chain(select.ctes.iter())
    {
        if !table.name.is_empty() && !into.contains(&table.name.as_str()) {
            into.push(table.name.as_str());
        }
        if let Some(derived) = &table.derived {
            collect_table_names(&derived.select, into);
        }
    }
    // **A subquery's relations are the statement's too**, because the inference is given one list
    // for the whole statement and a parameter under an `IN (SELECT … WHERE title = $1)` is typed
    // by a column of a table only the subquery names. Without this the list held the outer
    // table alone and `$1` fell back to `text`.
    for_each_in_select(select, &mut |expr| collect_subquery_tables(expr, into));
}

/// The relations named inside every subquery of one expression, for [`collect_table_names`].
fn collect_subquery_tables<'a>(expr: &'a Expr, into: &mut Vec<&'a str>) {
    descend(expr, &mut |expr| {
        if let Expr::Subquery(sub) = expr {
            collect_table_names(&sub.select, into);
        }
    });
}

pub(super) fn walk_expr_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    visit(expr);
    match expr {
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
        Expr::QuantifiedArray { operand, array, .. } => {
            walk_expr_mut(operand, visit);
            walk_expr_mut(array, visit);
        }
        Expr::Subscript { operand, index, .. } => {
            walk_expr_mut(operand, visit);
            walk_expr_mut(index, visit);
        }
        Expr::Coalesce(args) => {
            for arg in args {
                walk_expr_mut(arg, visit);
            }
        }
        // **The same blind spot one node later.** `ARRAY[…]` over expressions was added without
        // teaching these two walkers, and this match has a catch-all so the compiler could not
        // say so — the cost was a `'x'::regtype` over a catalog type sitting inside a constructor
        // and never being resolved, which reached the row evaluator and said so.
        Expr::Array { elements, .. } => {
            for element in elements {
                walk_expr_mut(element, visit);
            }
        }
        // **The arms below were the blind spot.** `descend` and this walk are the crate's general
        // ones — `has_sequence_call`, `has_set_func` and the parameter inference all go through
        // them — and neither descended into arithmetic, a negation, a `LIKE` pattern or a
        // set-returning call's arguments. `SELECT generate_series(1,2) + 10` found no call and
        // reached the row evaluator as an internal error; `WHERE a = $1 + 1` would not have seen
        // the parameter. A walker that is nearly total is worse than one that obviously is not.
        Expr::Not(operand)
        | Expr::IsNull { operand, .. }
        | Expr::Negate(operand)
        | Expr::Cast { operand, .. }
        | Expr::ToText { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::Scalar { operand, .. } => {
            walk_expr_mut(operand, visit);
        }
        Expr::Binary { left, right, .. }
        | Expr::Arithmetic { left, right, .. }
        | Expr::Like {
            operand: left,
            pattern: right,
            ..
        }
        | Expr::RegexMatch {
            operand: left,
            pattern: right,
            ..
        } => {
            walk_expr_mut(left, visit);
            walk_expr_mut(right, visit);
        }
        Expr::SetFunc(call) => {
            for arg in &mut call.args {
                walk_expr_mut(arg, visit);
            }
        }
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(operand) = operand {
                walk_expr_mut(operand, visit);
            }
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
        // **A subquery is a `SELECT` inside an expression, and its parameters are numbered in the
        // statement that holds it**: `WHERE id IN (SELECT id FROM t LIMIT $1)` is one `$1`, not a
        // statement of its own with a `$1` of its own. Without this arm the walk stopped at the
        // boundary and the parameter was never counted, never typed and never substituted —
        // `42P18 could not determine data type of parameter $1` at `Parse`, for a value the
        // client was about to send.
        //
        // The same omission the clause list above was written out to prevent, one level up: that
        // one visited five clauses of a `SELECT` and this one visited none of a nested `SELECT`'s.
        // **Both fields**, because the operand of an `IN` lives on the subquery expression rather
        // than beside it, so an arm that walked only the sub-`SELECT` would leave `$1 IN (SELECT
        // …)` behind.
        Expr::Subquery(sub) => {
            for operand in &mut sub.operands {
                walk_expr_mut(operand, visit);
            }
            walk_select_mut(&mut sub.select, visit);
        }
        _ => {}
    }
}

/// The table a statement is about, by name, so the inference has column types to work from.
pub(super) fn table_names(statement: &Statement) -> Vec<&str> {
    match statement {
        Statement::Insert(insert) => vec![insert.table.as_str()],
        // A join's two tables, outer first, which is the order their columns appear in a row.
        Statement::Cursor(crate::plan::CursorStatement::Declare { query, .. }) => {
            let mut names = Vec::new();
            collect_table_names(query, &mut names);
            names
        }
        Statement::Select(select) => {
            let mut names = Vec::new();
            collect_table_names(select, &mut names);
            names
        }
        // The table written, then whatever its `WHERE`'s subqueries read: `delete_all` on a
        // joined relation sends `DELETE FROM a WHERE (a.id) IN (SELECT a.id FROM a JOIN b … WHERE
        // b.title = $1)`, and `$1` is typed by a column of `b`.
        Statement::Update(update) => {
            // The table written, then its `FROM` chain **in the order the chain is built** —
            // `crate::exec::dml::update` puts the `FROM` entry first and its joins after it, and
            // `returning_fields` reads this list back positionally to rebuild that scope.
            let mut names = vec![update.table.as_str()];
            names.extend(
                update
                    .from
                    .iter()
                    .chain(update.joins.iter().map(|join| &join.table))
                    .map(|entry| entry.name.as_str()),
            );
            for expr in update
                .filter
                .iter()
                .chain(update.assignments.iter().map(|(_, value)| value))
                .chain(update.joins.iter().filter_map(|join| join.on.as_ref()))
            {
                collect_subquery_tables(expr, &mut names);
            }
            names
        }
        Statement::Delete(delete) => {
            let mut names = vec![delete.table.as_str()];
            if let Some(filter) = &delete.filter {
                collect_subquery_tables(filter, &mut names);
            }
            names
        }
        Statement::Explain(explain) => table_names(&explain.statement),
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::DropExtension(_)
        | Statement::AlterIndexRename(_)
        | Statement::CreateSchema(_)
        | Statement::CreateRole(_)
        | Statement::DropRole(_)
        | Statement::CreateView(_)
        | Statement::DropView(_)
        | Statement::CreateMaterializedView(_)
        | Statement::CreateTableAs(_)
        | Statement::RefreshMaterializedView(_)
        | Statement::DropMaterializedView(_)
        | Statement::CreateDatabase(_)
        | Statement::DropDatabase(_)
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
        | Statement::CreateType(_)
        | Statement::Raise { .. }
        | Statement::Truncate(_)
        | Statement::DropType(_)
        | Statement::AlterType(_)
        // DDL over a table, but nothing here needs its column types: a parameter cannot appear
        // in an `ALTER TABLE`, so there is nothing to infer against.
        | Statement::AlterTable(_)
        // Neither a session statement nor a time-machine verb is about a table this crate has
        // to resolve names against: the checkpoint verbs take a name that is their own, and a
        // `DIFF`'s table is resolved where it is scanned, in its own snapshot.
        | Statement::Session(_)
        // `FETCH`, `MOVE` and `CLOSE` name a cursor and carry no expression at all; a
        // `DECLARE`'s query is walked by the arm above.
        | Statement::Cursor(_)
        | Statement::TimeMachine(_) => Vec::new(),
    }
}

/// Whether a statement mentions a parameter at all, so the common case costs no walk of its own.
pub(crate) fn any(statement: &Statement, wanted: impl Fn(&Expr) -> bool) -> bool {
    let mut found = false;
    for_each_expr(statement, &mut |expr| {
        found = found || wanted(expr);
    });
    found
}

/// How many parameters a statement declares — the **highest** `$n` in it, not how many distinct
/// ones appear.
///
/// `SELECT $2` declares two on a real server, and `EXECUTE` supplying one is
/// `wrong number of parameters`. Counting the distinct placeholders would make that statement take
/// one argument and put it in the wrong position.
pub(crate) fn parameter_count(statement: &Statement) -> usize {
    let mut highest = 0;
    for_each_expr(statement, &mut |expr| {
        if let Expr::Parameter(number) = expr {
            highest = highest.max(*number as usize);
        }
    });
    highest
}

pub(super) fn has_parameters(statement: &Statement) -> bool {
    let mut found = false;
    for_each_expr(statement, &mut |expr| {
        found = found || matches!(expr, Expr::Parameter(_));
    });
    found
}

/// Every expression in a statement, read-only.
pub(super) fn for_each_expr<'a>(statement: &'a Statement, visit: &mut impl FnMut(&'a Expr)) {
    let mut each = |expr: &'a Expr| descend(expr, visit);
    match statement {
        Statement::Insert(insert) => {
            for row in &insert.rows {
                row.iter().for_each(&mut each);
            }
            if let Some(crate::plan::OnConflict {
                action: crate::plan::ConflictAction::DoUpdate(assignments),
                ..
            }) = &insert.on_conflict
            {
                for (_, value) in assignments {
                    each(value);
                }
            }
        }
        Statement::Cursor(crate::plan::CursorStatement::Declare { query, .. }) => {
            for_each_in_select(query, &mut each);
        }

        Statement::Select(select) => for_each_in_select(select, &mut each),
        Statement::Update(update) => {
            for (_, value) in &update.assignments {
                each(value);
            }
            update.filter.iter().for_each(&mut each);
            update
                .joins
                .iter()
                .filter_map(|join| join.on.as_ref())
                .for_each(&mut each);
        }
        Statement::Delete(delete) => delete.filter.iter().for_each(&mut each),
        Statement::Explain(explain) => for_each_expr(&explain.statement, visit),
        // Neither DDL nor a session statement can carry a parameter: there is no expression in
        // either that a `$1` could stand in.
        Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateExtension(_)
        | Statement::DropExtension(_)
        | Statement::AlterIndexRename(_)
        | Statement::CreateSchema(_)
        | Statement::CreateRole(_)
        | Statement::DropRole(_)
        | Statement::CreateView(_)
        | Statement::DropView(_)
        | Statement::CreateMaterializedView(_)
        | Statement::CreateTableAs(_)
        | Statement::RefreshMaterializedView(_)
        | Statement::DropMaterializedView(_)
        | Statement::CreateDatabase(_)
        | Statement::DropDatabase(_)
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
        | Statement::CreateType(_)
        | Statement::Raise { .. }
        | Statement::Truncate(_)
        | Statement::DropType(_)
        | Statement::AlterType(_)
        | Statement::AlterTable(_)
        | Statement::Session(_)
        // `FETCH`, `MOVE` and `CLOSE` name a cursor and carry no expression at all; a
        // `DECLARE`'s query is walked by the arm above.
        | Statement::Cursor(_)
        | Statement::TimeMachine(_) => {}
    }
}

pub(super) fn descend<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    visit(expr);
    if let Expr::Subquery(sub) = expr {
        // [`walk_expr_mut`]'s twin arm, and it has to agree with it: one sizes the parameter list
        // and the other fills it, so a shape in one and not the other is a parameter counted and
        // never filled, or filled and never counted.
        for operand in &sub.operands {
            descend(operand, visit);
        }
        for_each_in_select(&sub.select, &mut |expr| descend(expr, &mut *visit));
        return;
    }
    match expr {
        Expr::Not(operand)
        | Expr::IsNull { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::ToText { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::Negate(operand)
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
        Expr::QuantifiedArray { operand, array, .. } => {
            descend(operand, visit);
            descend(array, visit);
        }
        Expr::Subscript { operand, index, .. } => {
            descend(operand, visit);
            descend(index, visit);
        }
        Expr::Coalesce(args) => {
            for arg in args {
                descend(arg, visit);
            }
        }
        Expr::Array { elements, .. } => {
            for element in elements {
                descend(element, visit);
            }
        }
        // The same arms as `walk_expr_mut`'s, and for the same reason: these two are the crate's
        // general walks and a variant missing from one of them is a bug in whatever asks it.
        Expr::Binary { left, right, .. }
        | Expr::Arithmetic { left, right, .. }
        | Expr::Like {
            operand: left,
            pattern: right,
            ..
        }
        | Expr::RegexMatch {
            operand: left,
            pattern: right,
            ..
        } => {
            descend(left, visit);
            descend(right, visit);
        }
        Expr::SetFunc(call) => {
            for arg in &call.args {
                descend(arg, visit);
            }
        }
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(operand) = operand {
                descend(operand, visit);
            }
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
            let stand_in = Expr::Literal(Literal::Typed(Box::new(placeholder(ty))));
            // **A placeholder carries a representation; a parameter has a *type*.** For most types
            // those are the same thing and the datum answers for both. For the ones that share
            // `text`'s storage — `hstore`, `xml`, `void`, the two vectors — the stand-in *is* a
            // `Datum::Text`, so everything downstream read `text` and `WHERE hstore_col = $1` was
            // described as `42883 operator does not exist: hstore = text`. That is a `Describe`
            // answering something `Execute` does not: with a value bound, the parameter is read
            // *as* the inferred type and the same statement runs. r1's param census found it from
            // the wire, and it took a `Describe` between the `Parse` and the `Bind` to see —
            // `ActiveRecord`'s driver sends one and this crate's own tests did not.
            //
            // The cast carries the type the representation cannot, and only where they differ, so
            // no statement that already described correctly gains a node.
            *expr = if placeholder(ty).column_type() == Some(ty) {
                stand_in
            } else {
                Expr::Cast {
                    operand: Box::new(stand_in),
                    to: ty,
                    typmod: crate::value::NO_TYPMOD,
                }
            };
        }
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "one stand-in per type, in one match; splitting it would put a type's placeholder \
              somewhere other than beside every other type's"
)]
fn placeholder(ty: ColumnType) -> Datum {
    match ty {
        // Oid zero, which is `InvalidOid` and prints as its digits: what stands in is never read,
        // only its type is.
        ColumnType::RegType => crate::value::regtype_of_oid(0),
        // Oid 0, whose digits are what a real server prints for an oid no function has.
        ColumnType::RegProc => Datum::RegProc {
            oid: 0,
            name: "0".into(),
        },
        ColumnType::RegClass => crate::value::regclass_of_oid(0),
        ColumnType::RegTypeArray => Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
            ColumnType::RegType,
            1,
            Vec::new(),
        )),
        ColumnType::RegProcArray => Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
            ColumnType::RegProc,
            1,
            Vec::new(),
        )),
        ColumnType::RegClassArray => Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
            ColumnType::RegClass,
            1,
            Vec::new(),
        )),
        // The origin, which is a point like any other: what stands in is never read, only its
        // type is.
        ColumnType::Point => Datum::Point { x: 0.0, y: 0.0 },
        // Nothing, which is a money like any other: what stands in is never read, only its type.
        ColumnType::Money => Datum::Money(0),
        // `0.0.0.0/32` and `00:00:00:00:00:00`: what stands in is never read, only its type is.
        ColumnType::Inet | ColumnType::Cidr => Datum::Inet {
            family: esker_keys::value::INET_V4,
            bits: 32,
            cidr: ty == ColumnType::Cidr,
            addr: [0; 16],
        },
        ColumnType::MacAddr => Datum::MacAddr([0; 6]),
        // The shape's own zero, which is never read — only its type is.
        ColumnType::Lseg => Datum::Geometry {
            kind: Box::new(ty),
            text: "[(0,0),(0,0)]".to_owned(),
        },
        ColumnType::Box => Datum::Geometry {
            kind: Box::new(ty),
            text: "(0,0),(0,0)".to_owned(),
        },
        ColumnType::Path | ColumnType::Polygon => Datum::Geometry {
            kind: Box::new(ty),
            text: "((0,0))".to_owned(),
        },
        ColumnType::Circle => Datum::Geometry {
            kind: Box::new(ty),
            text: "<(0,0),0>".to_owned(),
        },
        ColumnType::Line => Datum::Geometry {
            kind: Box::new(ty),
            text: "{0,1,0}".to_owned(),
        },
        ColumnType::Bit | ColumnType::VarBit => Datum::Bit {
            varying: ty == ColumnType::VarBit,
            bits: String::new(),
        },
        // An empty array of the right element type: the shape a parameter takes before its value
        // arrives, and one that answers `column_type` correctly while it stands in.
        ColumnType::Int8Array
        | ColumnType::Int4Array
        | ColumnType::Int2Array
        | ColumnType::NumericArray
        | ColumnType::TextArray
        | ColumnType::HstoreArray
        | ColumnType::TsVectorArray
        | ColumnType::TsQueryArray
        | ColumnType::TsRangeArray
        | ColumnType::TstzRangeArray
        | ColumnType::Int4RangeArray
        | ColumnType::DateRangeArray
        | ColumnType::NumRangeArray
        | ColumnType::Int8RangeArray
        | ColumnType::PointArray
        | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray
        | ColumnType::BoolArray
        | ColumnType::ByteaArray
        | ColumnType::BpcharArray
        | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray
        | ColumnType::DateArray
        | ColumnType::TimeArray
        | ColumnType::TimestampArray
        | ColumnType::TimestampTzArray
        | ColumnType::IntervalArray
        | ColumnType::RealArray
        | ColumnType::DoubleArray
        | ColumnType::UuidArray
        | ColumnType::JsonArray
        | ColumnType::JsonbArray
        | ColumnType::OidArray
        | ColumnType::CitextArray
        | ColumnType::XmlArray
        | ColumnType::LtreeArray
        // **Five array types used to sit in an arm of their own with `money`'s element.** A
        // stand-in's whole job is to answer `column_type()` with the parameter's type, and
        // `ArrayValue::empty(Money)` answers `money[]` for all six — so `Describe` over
        // `WHERE bit_col = $1` emitted `CAST(money[] AS bit[])` and the cast table refused it,
        // correctly: nothing casts a money array to a bit array. **The stand-in was wrong, not
        // the cast.** `element_of` covers all six, and the arm below has always been right for
        // the forty-odd others (wire v3 family F8).
        | ColumnType::BitArray
        | ColumnType::VarBitArray
        | ColumnType::InetArray
        | ColumnType::CidrArray
        | ColumnType::MacAddrArray
        | ColumnType::MoneyArray => Datum::Array(esker_keys::array::ArrayValue::empty(
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
        // The empty hstore is the empty string too, and it is a real value rather than a NULL —
        // see `crate::value::hstore`.
        ColumnType::Text
        | ColumnType::Name | ColumnType::Char
        | ColumnType::Varchar
        | ColumnType::Bpchar
        | ColumnType::Hstore
        // The two vectors share text's representation: an empty one is an empty string.
        | ColumnType::Int2Vector
        | ColumnType::OidVector
        // **A void's is not a stand-in at all**: the empty string is its one real value, and it
        // lands in this arm because that is the value's representation.
        | ColumnType::Void
        | ColumnType::Xml => Datum::Text(String::new()),
        ColumnType::Citext => Datum::Citext(String::new()),
        ColumnType::TsVector => Datum::TsVector(String::new()),
        ColumnType::TsQuery => Datum::TsQuery(String::new()),
        // The empty range, which is a real value and not a NULL.
        ColumnType::TsRange
        | ColumnType::TstzRange
        | ColumnType::Int4Range
        | ColumnType::DateRange
        | ColumnType::NumRange
        | ColumnType::Int8Range
        | ColumnType::FloatRange
        | ColumnType::VarcharRange => Datum::Range {
            subtype: Box::new(esker_keys::row::range_subtype(ty)),
            text: "empty".to_owned(),
        },
        // The empty string is not a document, so a `json` placeholder is the smallest one that
        // is. It only ever stands in for a type while a `Describe` is answered.
        ColumnType::Json | ColumnType::Jsonb => Datum::Text("null".to_owned()),
        // The empty string *is* well-formed XML content, so an `xml` placeholder can be the
        // smallest thing there is. It only ever stands in for a type while a `Describe` is
        // answered.
        // The empty path, which is a value: zero labels, and never read — only its type is.
        ColumnType::Ltree => Datum::Ltree(String::new()),
        // `*`, which matches every path — never read, only its type is.
        ColumnType::LQuery => Datum::Text("*".to_owned()),
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
