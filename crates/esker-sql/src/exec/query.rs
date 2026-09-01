//! `SELECT`, planned: choosing an access path and resolving every name to a position.
//!
//! [`plan`] turns a lowered `SELECT` into a tree of [`Node`]s, and [`crate::exec::cursor`] pulls
//! rows through that tree. The split is the usual one and it is worth keeping: everything here
//! decides *what to read*, and nothing here reads anything.
//!
//! # What the planner is allowed to decide
//!
//! Rule-based, and the rules are few enough to name:
//!
//! 1. A `WHERE` that pins the **whole primary key** to constants is a point read, not a scan.
//! 2. A `WHERE` that pins the whole key of a **unique index** is a lookup in that index followed by
//!    a point read of the row it names.
//! 3. A `WHERE` that bounds a single-column primary key **narrows the scanned range** instead of
//!    filtering every row out of it.
//!
//! Anything else is a sequential scan with a filter over it, which is always correct and sometimes
//! slow. `EXPLAIN` prints which one was chosen, because a plan a user cannot see is a plan they
//! cannot fix.
//!
//! Only conjunctions count towards any of it. A constant under an `OR` is not *required* by the
//! query, and reading only its range would silently lose the rows the other branch matches — which
//! is the kind of wrong answer nothing reports.

use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::aggregate;
use crate::plan::{BinaryOp, Expr, Node, Select, SelectItem, SortKey};
use crate::row::{self, RowSchema};
use crate::value::PgType;
use crate::value::{ColumnType, Datum};

/// The tables a column reference in one query may name, in the order their columns appear in a
/// row.
///
/// A join's output row is the outer table's columns followed by the inner table's, so a scope of
/// two tables is also the map from a name to a position in that combined row — which is the whole
/// of what resolution needs, and the reason this is a list rather than a pair.
#[derive(Debug, Default)]
pub(super) struct Scope<'a> {
    /// In the order their columns appear in a row — which for a join is the order the executor
    /// reads them, outer first, and **not** necessarily the order the user wrote them in.
    tables: Vec<&'a TableDef>,
    /// The name a qualifier must write for each of `tables`: its **alias** where it has one, and
    /// the table's own name otherwise.
    ///
    /// Parallel to `tables` rather than read off them, because an alias *replaces* the name: after
    /// `FROM pg_type AS t`, `t.oid` resolves and `pg_type.oid` is `42P01` naming the alias in a
    /// `HINT`. Matching `TableDef::name` would answer both, which is a query a real server refuses.
    names: Vec<String>,
    /// Indexes into `tables`, in the order the user wrote them. `SELECT *` expands in this order,
    /// because the columns a user gets back must not depend on which side the planner chose to
    /// drive the loop from.
    written: Vec<usize>,
    /// `USING (a, b, …)`: the columns the two sides share as one.
    ///
    /// Two things follow from a column being merged, and both were measured. `SELECT *` returns it
    /// **once and first**, ahead of either table's other columns; and a bare reference to it is no
    /// longer ambiguous, where the same query written with `ON l.a = r.a` answers `42702`.
    ///
    /// The merged value is the **left** side's, with no `COALESCE` needed and none available: for
    /// an inner join the two are equal by the condition, and for a left join the right one is
    /// either equal or NULL. There is no third case here, because there is no `RIGHT` or `FULL`
    /// join in this crate to make one.
    using: Vec<String>,
}

impl<'a> Scope<'a> {
    /// No tables: `SELECT 1`.
    fn empty() -> Self {
        Scope {
            tables: Vec::new(),
            names: Vec::new(),
            written: Vec::new(),
            using: Vec::new(),
        }
    }

    /// One table under its own name, which is every statement that does not write an alias.
    pub(super) fn single(table: &'a TableDef) -> Self {
        Scope::single_as(table, table.name.clone())
    }

    /// One table under the name the query refers to it by.
    fn single_as(table: &'a TableDef, name: String) -> Self {
        Scope {
            tables: vec![table],
            names: vec![name],
            written: vec![0],
            using: Vec::new(),
        }
    }

    /// Two tables: the first is the one the loop is driven from, and `swapped` says whether that
    /// is the one the user wrote second.
    fn joined(
        outer: (&'a TableDef, &str),
        inner: (&'a TableDef, &str),
        swapped: bool,
        using: &[String],
    ) -> Self {
        Scope {
            tables: vec![outer.0, inner.0],
            names: vec![outer.1.to_owned(), inner.1.to_owned()],
            written: if swapped { vec![1, 0] } else { vec![0, 1] },
            using: using.to_vec(),
        }
    }

    /// The name a `42803` prints for a resolved position: `t.c`, qualified.
    ///
    /// PostgreSQL qualifies it even when the query has one table, and in a join it is the only
    /// form that says which one. A position past the end can only be a bug, and a name that says
    /// so beats a panic.
    pub(super) fn qualified_name(&self, at: usize) -> String {
        let mut start = 0;
        for (index, table) in self.tables.iter().enumerate() {
            if at < start + table.columns.len() {
                // The name the *query* used, which is the alias where there is one: a message
                // naming a table the user did not write is a message about somebody else's query.
                return format!("{}.{}", self.names[index], table.columns[at - start].name);
            }
            start += table.columns.len();
        }
        format!("<column {at}>")
    }

    /// The FROM entry a qualifier names.
    ///
    /// Two failures and they are different mistakes, which is why PostgreSQL gives them different
    /// sentences (measured, `tests/corpus/pg19_alias.txt`): a name nothing in the query has is
    /// `missing FROM-clause entry`, and the **table's own name where an alias replaced it** is
    /// `invalid reference to FROM-clause entry`, with a `HINT` naming the alias. Answering the
    /// first for both would tell a user their table is absent when it is right there.
    fn entry(&self, qualifier: &str) -> Result<usize> {
        if let Some(index) = self.names.iter().position(|name| name == qualifier) {
            return Ok(index);
        }
        if let Some(index) = self.tables.iter().position(|table| table.name == qualifier) {
            return Err(SqlError::InvalidFromReference {
                table: qualifier.to_owned(),
                alias: self.names[index].clone(),
            });
        }
        Err(SqlError::MissingFromEntry(qualifier.to_owned()))
    }

    /// Where `table`'s columns start in a row.
    fn offset(&self, table: usize) -> usize {
        self.tables[..table]
            .iter()
            .map(|table| table.columns.len())
            .sum()
    }

    /// The columns `t.*` expands to, or every column for `*`.
    ///
    /// A qualifier that names no table in the query is the same `42P01` a qualified *column*
    /// reference gives, which is what a real server answers for `SELECT wrong.* FROM o`.
    fn expand(
        &self,
        qualifier: Option<&str>,
    ) -> Result<std::vec::IntoIter<(usize, &'a ColumnDef)>> {
        if let Some(qualifier) = qualifier {
            let index = self.entry(qualifier)?;
            let offset = self.offset(index);
            let columns: Vec<_> = self.tables[index]
                .user_columns()
                .map(|(at, column)| (offset + at, column))
                .collect();
            return Ok(columns.into_iter());
        }
        // A merged column comes **first and once**: `SELECT * FROM l JOIN r USING (id)` is
        // `id, <l's others>, <r's others>`, measured. So the merged ones are emitted from the
        // left-hand table in the order the clause named them, and then every column of every table
        // that is not one of them.
        let mut columns = Vec::new();
        let written = self.written.first().copied().unwrap_or(0);
        for merged in &self.using {
            let at = self.tables[written]
                .column(merged)
                .ok_or_else(|| undefined_column(merged))?;
            columns.push((self.offset(written) + at, &self.tables[written].columns[at]));
        }
        for &index in &self.written {
            let offset = self.offset(index);
            for (at, column) in self.tables[index].user_columns() {
                if self.using.contains(&column.name) {
                    continue;
                }
                columns.push((offset + at, column));
            }
        }
        Ok(columns.into_iter())
    }

    /// A column reference, resolved to a position in the combined row.
    ///
    /// The three failures are PostgreSQL's, captured from a real server rather than invented:
    /// a qualifier naming a table the query does not have is `42P01 missing FROM-clause entry for
    /// table "x"`; a name no table has is `42703`; and a bare name **two** tables have is `42702
    /// column reference "x" is ambiguous` — which is an error rather than a silent choice of the
    /// first one, because the user's intent is genuinely unknown and guessing it returns the wrong
    /// column without saying so.
    fn resolve_column(&self, qualifier: Option<&str>, name: &str) -> Result<(usize, ColumnType)> {
        if let Some(qualifier) = qualifier {
            let index = self.entry(qualifier)?;
            let at = self.tables[index].column(name).ok_or_else(|| {
                if let Some(system) = SYSTEM_COLUMNS.iter().find(|system| **system == name) {
                    return SqlError::unsupported(format!("the system column {system}"));
                }
                SqlError::UndefinedQualifiedColumn {
                    qualifier: qualifier.to_owned(),
                    column: name.to_owned(),
                }
            })?;
            return Ok((self.offset(index) + at, self.tables[index].columns[at].ty));
        }

        // A `USING` column is **one** column, so a bare reference to it is not ambiguous — where
        // the same query written `ON l.id = r.id` answers `42702`. Its value is the left-hand
        // side's: equal to the right's for an inner join, and either equal or NULL for a left one,
        // so there is no third case and no `COALESCE` to need.
        if self.using.iter().any(|merged| merged == name) {
            let written = self.written.first().copied().unwrap_or(0);
            if let Some(at) = self.tables[written].column(name) {
                return Ok((
                    self.offset(written) + at,
                    self.tables[written].columns[at].ty,
                ));
            }
        }

        let mut found = None;
        for (index, table) in self.tables.iter().enumerate() {
            if let Some(at) = table.column(name) {
                if found.is_some() {
                    return Err(SqlError::AmbiguousColumn(name.to_owned()));
                }
                found = Some((self.offset(index) + at, table.columns[at].ty));
            }
        }
        found.ok_or_else(|| undefined_column(name))
    }
}

/// A planned query, with everything the executor needs to describe its output before running it.
#[derive(Debug)]
pub(super) struct Planned {
    /// The tree to pull rows through.
    pub(super) node: Node,
    /// One name and type per output column, for `RowDescription`.
    pub(super) columns: Vec<(String, ColumnType)>,
    /// The table's name, for `EXPLAIN`.
    pub(super) table: String,
    /// The table's column names, so `EXPLAIN` can print the names a user typed rather than the
    /// positions the executor resolved them to.
    pub(super) column_names: Vec<String>,
}

/// The access path and filter for every row of `table` a predicate matches — the half of a plan
/// that `UPDATE` and `DELETE` share with `SELECT`.
///
/// It yields whole rows, because a statement that rewrites a row needs all of it: the columns it
/// is not changing still have to be written back, and the index entries it is replacing were built
/// from the old ones.
pub(super) fn matching_rows(filter: Option<&Expr>, tenant: u64, table: &TableDef) -> Result<Node> {
    let mut node = access_path(filter, tenant, table)?;
    if let Some(filter) = filter {
        let scope = Scope::single(table);
        let predicate = resolve(filter, &scope)?;
        check_predicate(&predicate, "WHERE", &scope)?;
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }
    Ok(node)
}

/// Turns a lowered `SELECT` into a plan against a table.
///
/// `table` is `None` for `SELECT 1`, which has no table and one row.
pub(super) fn plan(
    select: &Select,
    tenant: u64,
    table: Option<&TableDef>,
    inner: Option<&TableDef>,
) -> Result<Planned> {
    // Which side drives the loop. An inner join is commutative, so this is free to choose — and
    // it has to choose, because the probe only works on the *inner* side: without this, `FROM c
    // JOIN o ON c.id = o.cid` reads the whole of `o` for every row of `c`, while the same query
    // written the other way round costs one key read per row. A user should not have to know
    // which order to type.
    // `USING (a, b)` is the equality `l.a = r.a AND l.b = r.b` **plus** a merge, so the condition
    // is built here and the merge is carried by the scope.
    let names = from_names(select)?;
    let named_table = table.map(|table| (table, names.0));
    let named_inner = inner.map(|inner| (inner, names.1));

    let using: &[String] = select.join.as_ref().map_or(&[], |join| &join.using);
    let condition = match (&select.join, named_table, named_inner) {
        (Some(join), Some(left), Some(right)) if !join.using.is_empty() => {
            Some(using_condition(&join.using, left, right)?)
        }
        (Some(join), ..) => join.on.clone(),
        _ => None,
    };
    let left_join = select
        .join
        .as_ref()
        .is_some_and(|join| join.kind == crate::plan::JoinKind::Left);

    let (scope, swapped) = drive_from(
        named_table,
        named_inner,
        condition.as_ref(),
        left_join,
        using,
    );
    let (outer_table, inner_table) = match (table, inner, swapped) {
        (Some(left), Some(right), false) => (Some(left), Some(right)),
        (Some(left), Some(right), true) => (Some(right), Some(left)),
        (table, _, _) => (table, None),
    };

    let mut node = match outer_table {
        None => Node::OneRow,
        // With a join the outer access path only gets the `WHERE` when the whole of it belongs to
        // the outer table. A predicate mentioning the inner one cannot narrow the outer scan --
        // its value is not known until an outer row has been read -- and handing it to
        // `access_path`, which resolves against one table, would be an error rather than a plan.
        Some(table) => {
            let usable = select
                .filter
                .as_ref()
                .filter(|filter| inner_table.is_none() || mentions_only(filter, table));
            access_path(usable, tenant, table)?
        }
    };

    if let Some(join) = &select.join {
        let inner = inner_table.ok_or_else(|| SqlError::UndefinedTable(join.table.name.clone()))?;
        node = join_node(node, condition.as_ref(), left_join, &scope, inner)?;
    }

    if let Some(filter) = &select.filter {
        let predicate = resolve(filter, &scope)?;
        check_predicate(&predicate, "WHERE", &scope)?;
        // A predicate the access path already guarantees is not re-checked -- but one it only
        // *narrowed* still is, because a range is not an equality.
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }

    // Aggregation sits between the filter and the sort. Everything above it is written about a
    // row that no table has -- the grouping keys followed by the aggregate values -- and
    // `Aggregation::rewrite` is what moves an expression from one to the other.
    let aggregation = aggregate::Aggregation::build(select, &scope)?;
    if let Some(aggregation) = &aggregation {
        node = Node::Aggregate {
            input: Box::new(node),
            keys: aggregation.keys.clone(),
            aggregates: aggregation.specs.clone(),
            having: aggregation.having.clone(),
            grouped: aggregation.grouped,
        };
    }

    let columns = output_columns(select, &scope, aggregation.as_ref())?;
    let exprs = projection_exprs(select, &scope, aggregation.as_ref())?;

    // The sort goes *below* the projection, so it can order on a column the target list does not
    // return -- `SELECT n FROM s1 ORDER BY id` is ordinary SQL, and a sort above the projection
    // could not see `id` at all. An `ORDER BY` naming an output alias is substituted first, which
    // is the other half of what PostgreSQL allows.
    //
    // `SELECT DISTINCT` is the one shape where that is not possible, and PostgreSQL says so
    // rather than working around it: deduplication happens over the target list, so a sort key
    // the target list does not contain has no defined position to sort at. Its keys are resolved
    // against the *output* columns and a key that is not one of them is `42P10`.
    let sort_keys = order_keys(select, &scope, aggregation.as_ref(), &exprs, &columns)?;
    if !select.distinct && !sort_keys.is_empty() {
        node = Node::Sort {
            input: Box::new(node),
            keys: sort_keys.clone(),
        };
    }

    node = Node::Project {
        input: Box::new(node),
        exprs,
    };

    if select.distinct {
        node = Node::Distinct {
            input: Box::new(node),
        };
        if !sort_keys.is_empty() {
            node = Node::Sort {
                input: Box::new(node),
                keys: sort_keys,
            };
        }
    }

    if select.limit.is_some() || select.offset.is_some() {
        node = Node::Limit {
            input: Box::new(node),
            offset: count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0),
            limit: count(select.limit.as_ref(), "LIMIT")?,
        };
    }

    Ok(Planned {
        node,
        columns,
        table: outer_table.map_or_else(|| "-".to_owned(), |table| table.name.clone()),
        // Every column of every table in scope, in row order, so `EXPLAIN` can print the name a
        // user typed for any position the executor resolved.
        column_names: scope
            .tables
            .iter()
            .flat_map(|table| table.columns.iter().map(|column| column.name.clone()))
            .collect(),
    })
}

/// The `ORDER BY` keys, resolved into whichever row space the sort will run over.
///
/// Three substitutions happen before a key is resolved, and all three are PostgreSQL's:
///
/// * an **output alias** — `ORDER BY c` for `count(*) AS c` — is replaced by what it names;
/// * an **integer literal** is a *position* in the target list, not a constant. Sorting by the
///   constant `1` is a no-op that silently ignores `DESC`, which is the wrong-answer shape this
///   whole crate is built to avoid;
/// * an **aggregate** is rewritten to its position in the aggregated row, so `ORDER BY count(*)`
///   orders the groups rather than failing to find a column.
///
/// Under `SELECT DISTINCT` the sort runs *above* the projection, so a key must be one of the
/// output columns; anything else is `42P10` with PostgreSQL's own sentence.
fn order_keys(
    select: &Select,
    scope: &Scope<'_>,
    aggregation: Option<&aggregate::Aggregation>,
    outputs: &[Expr],
    columns: &[(String, ColumnType)],
) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for item in &select.order_by {
        // A position is resolved against the **output list**, which is already expanded and
        // already in whichever row space the sort will run over -- so `SELECT * FROM t ORDER BY 1`
        // works, where resolving it against the unexpanded target list could not have.
        let resolved = if let Expr::Literal(crate::plan::Literal::Integer(position)) = &item.expr {
            let at = usize::try_from(*position)
                .ok()
                .filter(|at| (1..=outputs.len()).contains(at))
                .ok_or_else(|| {
                    SqlError::InvalidColumnReference(format!(
                        "ORDER BY position {position} is not in select list"
                    ))
                })?;
            outputs[at - 1].clone()
        } else {
            // A bare name that **more than one output column** is called is `42702`, and it is a
            // different ambiguity from a column reference's: the target list is what is ambiguous,
            // not the tables. `SELECT l.id, r.id, id FROM l JOIN r USING (id) ORDER BY id` is the
            // shape — the `id` in the list resolves fine and the three columns it produces do not.
            //
            // The narrow half of PostgreSQL's rule: it also *prefers* an output column to an input
            // one, which this does not do. Every case the corpus holds is covered by the
            // ambiguity alone, and a preference nothing has measured would be invented.
            if let Expr::Column { table: None, name } = &item.expr
                && columns.iter().filter(|(output, _)| output == name).count() > 1
            {
                return Err(SqlError::AmbiguousOrderBy(name.clone()));
            }
            let resolved = resolve(&dealias(&item.expr, select), scope)?;
            aggregate::check_not_nested(&resolved)?;
            match aggregation {
                None => resolved,
                Some(aggregation) => aggregation.rewrite(&resolved, scope)?,
            }
        };
        let expr = if select.distinct {
            let at = outputs
                .iter()
                .position(|output| output == &resolved)
                .ok_or_else(|| {
                    SqlError::InvalidColumnReference(
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                            .to_owned(),
                    )
                })?;
            Expr::Ordinal {
                at,
                ty: expr_type(&resolved, scope)?,
            }
        } else {
            resolved
        };
        keys.push(SortKey {
            expr,
            descending: item.descending,
            // PostgreSQL's default is NULLS LAST ascending and NULLS FIRST descending, which is
            // one rule: NULL is the largest value, and `DESC` reverses the order it sits in like
            // everything else.
            nulls_first: item.nulls_first.unwrap_or(item.descending),
        });
    }
    Ok(keys)
}

/// A resolved target list: one name and type per output column, and the expression that fills it.
pub(super) type TargetList = (Vec<(String, ColumnType)>, Vec<Expr>);

/// `RETURNING`, resolved against one table: the output columns and the expression per column.
///
/// The same [`Scope`], the same `resolve` and the same name-and-type rules a `SELECT`'s target
/// list gets, which is what makes `RETURNING *` and `SELECT *` return the same columns in the same
/// order under the same names. An aggregate is refused here rather than resolved: there is no
/// group in a statement that writes rows, and PostgreSQL says so.
pub(super) fn returning_columns(items: &[SelectItem], table: &TableDef) -> Result<TargetList> {
    let scope = Scope::single(table);
    let select = Select {
        from: Some(crate::plan::TableRef::bare(table.name.clone())),
        join: None,
        projection: items.to_vec(),
        filter: None,
        distinct: false,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };
    for item in items {
        if let SelectItem::Expr { expr, .. } = item
            && aggregate::contains_aggregate(expr)
        {
            return Err(SqlError::AggregateNotAllowed(
                "aggregate functions are not allowed in RETURNING",
            ));
        }
    }
    let columns = output_columns(&select, &scope, None)?;
    let exprs = projection_exprs(&select, &scope, None)?;
    Ok((columns, exprs))
}

/// The names the query refers to its `FROM` entries by, outer as written first.
///
/// Not what the catalog calls them: an alias replaces the name, and this is what resolution
/// matches. **Two entries under one name is `42712`** rather than a scope where the first quietly
/// wins — `FROM t JOIN t ON true` would otherwise resolve every `t.c` to the outer side and answer
/// a self-join with one table's columns twice, with nothing to say so. The check is over these
/// names and not over the tables, which is why `FROM al AS t JOIN ar AS al` is legal: the alias
/// freed the name (measured, `tests/corpus/pg19_alias.txt`).
fn from_names(select: &Select) -> Result<(&str, &str)> {
    let left = select
        .from
        .as_ref()
        .map_or("", crate::plan::TableRef::referred_as);
    let right = select
        .join
        .as_ref()
        .map_or("", |join| join.table.referred_as());
    if !right.is_empty() && left == right {
        return Err(SqlError::DuplicateTableName(left.to_owned()));
    }
    Ok((left, right))
}

/// Which side of a join drives the loop, and the scope that follows from it.
///
/// **A left join may not swap.** An inner join is commutative, so this is free to choose — and it
/// has to choose, because the probe only works on the *inner* side: without it, `FROM c JOIN o ON
/// c.id = o.cid` reads the whole of `o` for every row of `c` while the same query written the
/// other way round costs one key read per row, and a user should not have to know which order to
/// type. A left join is not commutative: which side keeps its unmatched rows is the whole of what
/// it means, so driving the right side and NULL-extending would answer a `RIGHT JOIN` — the same
/// rows, in the wrong places, with nothing to say so.
fn drive_from<'a>(
    table: Option<(&'a TableDef, &str)>,
    inner: Option<(&'a TableDef, &str)>,
    on: Option<&Expr>,
    left_join: bool,
    using: &[String],
) -> (Scope<'a>, bool) {
    let (Some(left), Some(right)) = (table, inner) else {
        return match table {
            None => (Scope::empty(), false),
            Some((table, name)) => (Scope::single_as(table, name.to_owned()), false),
        };
    };
    if left_join {
        return (Scope::joined(left, right, false, using), false);
    }
    // As written first: a probe on the right-hand table keeps the order the user chose, which
    // keeps `EXPLAIN` easiest to read when both would work.
    if on.is_some_and(|on| {
        probe_for(on, &Scope::joined(left, right, false, using), right.0).is_some()
    }) {
        return (Scope::joined(left, right, false, using), false);
    }
    if on
        .is_some_and(|on| probe_for(on, &Scope::joined(right, left, true, using), left.0).is_some())
    {
        return (Scope::joined(right, left, true, using), true);
    }
    (Scope::joined(left, right, false, using), false)
}

/// Whether every column reference in an expression belongs to `table`.
fn mentions_only(expr: &Expr, table: &TableDef) -> bool {
    let mut only = true;
    for_each_column(expr, &mut |qualifier, name| {
        only &= qualifier.is_none_or(|qualifier| qualifier == table.name)
            && table.column(name).is_some();
    });
    only
}

fn for_each_column(expr: &Expr, visit: &mut impl FnMut(Option<&str>, &str)) {
    match expr {
        Expr::Column { table, name } => visit(table.as_deref(), name),
        Expr::Binary { left, right, .. } => {
            for_each_column(left, visit);
            for_each_column(right, visit);
        }
        Expr::Not(operand) | Expr::IsNull { operand, .. } => for_each_column(operand, visit),
        _ => {}
    }
}

/// The join, and the choice this whole unit exists to make: read the inner table once per outer
/// row, or read one key.
///
/// A probe is possible when the `ON` condition is an equality between one outer column and one
/// inner column, and that inner column is the inner table's whole primary key or the whole key of
/// one of its unique indexes. Those are the two access paths the planner already has for a
/// `WHERE` — a join is a `WHERE` whose right-hand side changes per row — and reusing them is why
/// this is a small addition rather than a second planner.
///
/// Anything else materialises the inner table and pairs every outer row with all of it, which is
/// always correct and, for a large inner table, always slow. `EXPLAIN` says which, because that is
/// the difference a user changes their schema over.
fn join_node(
    outer: Node,
    on: Option<&Expr>,
    left_join: bool,
    scope: &Scope<'_>,
    inner: &TableDef,
) -> Result<Node> {
    let probe = on
        .and_then(|on| probe_for(on, scope, inner))
        .unwrap_or(crate::plan::Probe::Materialize);
    // A probe answers the equality exactly, so the condition it came from is not re-checked. A
    // materialised inner side has nothing to answer it, so the whole condition is the filter.
    let residual = match (&probe, on) {
        (crate::plan::Probe::Materialize, Some(on)) => {
            let resolved = resolve(on, scope)?;
            check_predicate(&resolved, "JOIN/ON", scope)?;
            Some(resolved)
        }
        _ => None,
    };
    Ok(Node::NestedLoop {
        outer: Box::new(outer),
        left_join,
        inner_table_id: inner.id,
        inner_table: inner.name.clone(),
        inner_columns: inner.row_schema(),
        probe,
        residual,
    })
}

/// `USING (a, b)` as the condition it also is: `l.a = r.a AND l.b = r.b`, qualified by table so
/// that it resolves the same way an `ON` written by hand would.
///
/// A column one side lacks is `42703` naming **which** side, which is what a real server says and
/// is the difference between a typo and a join between the wrong two tables.
fn using_condition(
    columns: &[String],
    left: (&TableDef, &str),
    right: (&TableDef, &str),
) -> Result<Expr> {
    let mut condition: Option<Expr> = None;
    for column in columns {
        for ((table, _), side) in [(left, "left"), (right, "right")] {
            if table.column(column).is_none() {
                return Err(SqlError::UsingColumnMissing {
                    column: column.clone(),
                    side,
                });
            }
        }
        // Qualified by the name the query refers to each side by, not by the table's own: the
        // condition this builds is resolved against the same scope everything else is, where an
        // alias has taken the table's name away.
        let equality = Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column {
                table: Some(left.1.to_owned()),
                name: column.clone(),
            }),
            right: Box::new(Expr::Column {
                table: Some(right.1.to_owned()),
                name: column.clone(),
            }),
        };
        condition = Some(match condition {
            None => equality,
            Some(built) => Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(built),
                right: Box::new(equality),
            },
        });
    }
    condition.ok_or_else(|| SqlError::Internal("an empty USING clause".to_owned()))
}

/// The probe an `ON` condition allows, or `None` for one that needs the inner table read whole.
fn probe_for(on: &Expr, scope: &Scope<'_>, inner: &TableDef) -> Option<crate::plan::Probe> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = on
    else {
        return None;
    };
    // The inner table's columns start where the outer's end, so a resolved position at or above
    // that offset belongs to the inner side.
    let boundary = scope.offset(scope.tables.len() - 1);
    let (left, right) = (resolve(left, scope).ok()?, resolve(right, scope).ok()?);
    let (Expr::Ordinal { at: a, .. }, Expr::Ordinal { at: b, .. }) = (&left, &right) else {
        return None;
    };
    let (outer_at, inner_at) = match (*a < boundary, *b < boundary) {
        (true, false) => (*a, *b - boundary),
        (false, true) => (*b, *a - boundary),
        // Both sides on one table joins nothing; it is a filter, and the materialised path
        // applies it correctly.
        _ => return None,
    };
    if inner.primary_key == [inner_at] {
        return Some(crate::plan::Probe::PrimaryKey { outer: outer_at });
    }
    inner
        .indexes
        .iter()
        .find(|index| index.unique && index.columns == [inner_at])
        .map(|index| crate::plan::Probe::UniqueIndex {
            index_id: index.id,
            index_name: index.name.clone(),
            outer: outer_at,
            primary_key_types: RowSchema::nullable(inner.primary_key_types()),
        })
}

/// Rule 1, 2 and 3 from `plan::query`: pin the whole primary key, bound its first column, or pin a
/// unique index's whole key. Otherwise a scan.
fn access_path(filter: Option<&Expr>, tenant: u64, table: &TableDef) -> Result<Node> {
    let columns = table.row_schema();
    let Some(filter) = filter else {
        return Ok(seq_scan(tenant, table, &columns, false));
    };
    let equalities = equality_constants(filter, table)?;

    // Rule 1: the whole primary key, pinned.
    if let Some(key) = pinned(&table.primary_key, &equalities) {
        return Ok(Node::PointGet {
            table_id: table.id,
            columns,
            key,
        });
    }

    // Rule 3: a unique index's whole key, pinned. A key with a NULL in it is not pinned at all --
    // `= NULL` is never true -- so this cannot pick up the many-NULLs case by accident.
    for index in &table.indexes {
        // **Only a public index may be read.** Anything earlier is either incomplete (the backfill
        // has not run) or not yet maintained by every node, and a plan that chose one would answer
        // a correct-looking query with missing rows — ADR 0020's "skip write-only" and "skip the
        // backfill", which are the two anomalies a *reader* can cause.
        if !index.state.readable() {
            continue;
        }
        if !index.unique {
            continue;
        }
        if let Some(key) = pinned(&index.columns, &equalities)
            && row::unique_index_key_is_unique_by_value(&key)
        {
            return Ok(Node::IndexLookup {
                table_id: table.id,
                columns,
                index_id: index.id,
                index_name: index.name.clone(),
                key,
                primary_key_types: RowSchema::nullable(table.primary_key_types()),
            });
        }
    }

    // Rule 2: a bound on the first primary key column narrows the range.
    Ok(narrowed_scan(tenant, table, &columns, filter))
}

fn seq_scan(tenant: u64, table: &TableDef, columns: &RowSchema, narrowed: bool) -> Node {
    let (start, end) = row::table_row_range(tenant, table.id);
    Node::SeqScan {
        table_id: table.id,
        columns: columns.clone(),
        start,
        end,
        narrowed,
    }
}

/// Rule 2: a bound on the *first* primary key column moves the ends of the scanned range.
///
/// Only the first, and only under `AND`. The key encoding is order-preserving, so `id > 5` on a
/// single-column key really does mean "start after the key for 5" — but `b > 5` on a key of
/// `(a, b)` does not mean anything about where to start, because the rows for `b = 9` are spread
/// through every value of `a`. The filter still runs either way; narrowing only decides how much
/// is read.
fn narrowed_scan(tenant: u64, table: &TableDef, columns: &RowSchema, filter: &Expr) -> Node {
    let (mut start, mut end) = row::table_row_range(tenant, table.id);
    let mut narrowed = false;

    if table.primary_key.len() == 1 {
        let first = table.primary_key[0];
        for (op, value) in bounds(filter, table, first) {
            let Ok(key) = row::row_key(tenant, table.id, &[value]) else {
                continue;
            };
            match op {
                BinaryOp::Gt if key >= start => {
                    start = successor(&key);
                    narrowed = true;
                }
                BinaryOp::GtEq if key > start => {
                    start = key;
                    narrowed = true;
                }
                BinaryOp::Lt if key < end => {
                    end = key;
                    narrowed = true;
                }
                BinaryOp::LtEq if successor(&key) < end => {
                    end = successor(&key);
                    narrowed = true;
                }
                _ => {}
            }
        }
    }

    Node::SeqScan {
        table_id: table.id,
        columns: columns.clone(),
        start,
        end,
        narrowed,
    }
}

/// Every `column <op> constant` on `ordinal` that the predicate *requires* — conjunctions only,
/// for the same reason `equality_constants` walks only `AND`.
fn bounds(expr: &Expr, table: &TableDef, ordinal: usize) -> Vec<(BinaryOp, Datum)> {
    let mut found = Vec::new();
    collect_bounds(expr, table, ordinal, &mut found);
    found
}

fn collect_bounds(
    expr: &Expr,
    table: &TableDef,
    ordinal: usize,
    found: &mut Vec<(BinaryOp, Datum)>,
) {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_bounds(left, table, ordinal, found);
            collect_bounds(right, table, ordinal, found);
        }
        Expr::Binary { op, left, right } if op.is_comparison() => {
            // `5 < id` is `id > 5` with the operands the other way round.
            let (name, literal, op) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(literal)) => (name, literal, *op),
                (Expr::Literal(literal), Expr::Column { name, .. }) => (name, literal, flip(*op)),
                _ => return,
            };
            if table.column(name) != Some(ordinal) {
                return;
            }
            let column = &table.columns[ordinal];
            if let Ok(value) = literal.assign(column.ty, &column.name)
                && !matches!(value, Datum::Null)
            {
                found.push((op, value));
            }
        }
        _ => {}
    }
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Every `column = constant` the predicate requires, as `(ordinal, value)`.
///
/// Only conjunctions count: a constant under an `OR` is not required, and treating it as though it
/// were would return the wrong rows. That is the whole reason this walks `AND` and stops.
fn equality_constants(expr: &Expr, table: &TableDef) -> Result<Vec<(usize, Datum)>> {
    let mut found = Vec::new();
    collect_equalities(expr, table, &mut found)?;
    Ok(found)
}

fn collect_equalities(
    expr: &Expr,
    table: &TableDef,
    found: &mut Vec<(usize, Datum)>,
) -> Result<()> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_equalities(left, table, found)?;
            collect_equalities(right, table, found)
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let pair = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(literal))
                | (Expr::Literal(literal), Expr::Column { name, .. }) => (name, literal),
                _ => return Ok(()),
            };
            let Some(ordinal) = table.column(pair.0) else {
                return Ok(());
            };
            let column = &table.columns[ordinal];
            // A literal that will not assign is not a plan decision; the filter will report it.
            if let Ok(value) = pair.1.assign(column.ty, &column.name)
                && !matches!(value, Datum::Null)
            {
                found.push((ordinal, value));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The values for `ordinals`, in order, when every one of them is pinned.
fn pinned(ordinals: &[usize], equalities: &[(usize, Datum)]) -> Option<Vec<Datum>> {
    ordinals
        .iter()
        .map(|ordinal| {
            equalities
                .iter()
                .find(|(at, _)| at == ordinal)
                .map(|(_, value)| value.clone())
        })
        .collect()
}

/// Resolves a column reference against one table — what `UPDATE`'s `SET` expressions need.
pub(super) fn resolve_against(expr: &Expr, table: &TableDef) -> Result<Expr> {
    resolve(expr, &Scope::single(table))
}

/// `ORDER BY x` where `x` is an output alias means the expression that alias names.
pub(super) fn dealias(expr: &Expr, select: &Select) -> Expr {
    let Expr::Column { table: None, name } = expr else {
        return expr.clone();
    };
    for item in &select.projection {
        if let SelectItem::Expr {
            expr: aliased,
            alias: Some(alias),
        } = item
            && alias == name
        {
            return aliased.clone();
        }
    }
    expr.clone()
}

/// Replaces every column name with its position and type. A name the table does not have is
/// `42703` here rather than a wrong answer later.
/// The six columns every PostgreSQL table has and this one does not, so that asking for one is
/// answered by name (contract C2) rather than with `42703 column "ctid" does not exist`, which
/// would be untrue — on a real server it does.
///
/// Measured rather than recalled: these six answer on a real PostgreSQL 19 and `oid` does not,
/// because a table's OID column went away in PostgreSQL 12.
///
/// `ctid` is the interesting one and the reason this list exists at all. It is a *physical*
/// address — `(block, offset)` — and it moves when the row is rewritten. The internal row id a
/// keyless table gets here is a *logical* identity that never moves, because an index entry points
/// at the row key and a key that moved would mean rewriting every index on every update. They are
/// not the same thing under different names, so `ctid` is refused rather than answered with
/// something that would behave differently the first time somebody updated a row.
fn undefined_column(name: &str) -> SqlError {
    if let Some(system) = SYSTEM_COLUMNS.iter().find(|system| **system == name) {
        return SqlError::unsupported(format!("the system column {system}"));
    }
    SqlError::UndefinedColumn(name.to_owned())
}

const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

pub(super) fn resolve(expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
    Ok(match expr {
        Expr::Column { table, name } => {
            let (at, ty) = scope.resolve_column(table.as_deref(), name)?;
            Expr::Ordinal { at, ty }
        }
        Expr::Binary { op, left, right } => {
            let (left, right) = (resolve(left, scope)?, resolve(right, scope)?);
            let (left, right) = if op.is_comparison() {
                // A literal has no type until something gives it one, and here that something is
                // the other operand. Without this the comparison would run between a `text` and an
                // `int8` and simply be false, which is a wrong answer rather than an error.
                reconcile(*op, left, right)?
            } else {
                (left, right)
            };
            Expr::Binary {
                op: *op,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        Expr::Not(operand) => Expr::Not(Box::new(resolve(operand, scope)?)),
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, scope)?),
            negated: *negated,
        },
        other => other.clone(),
    })
}

/// Gives a literal the type of the column it is being compared against, or says the comparison is
/// between types no operator covers.
fn reconcile(op: BinaryOp, left: Expr, right: Expr) -> Result<(Expr, Expr)> {
    Ok(match (&left, &right) {
        (Expr::Ordinal { ty, .. }, Expr::Literal(literal)) => (
            left.clone(),
            Expr::Literal(retype(*ty, literal, op, false)?),
        ),
        (Expr::Literal(literal), Expr::Ordinal { ty, .. }) => (
            Expr::Literal(retype(*ty, literal, op, true)?),
            right.clone(),
        ),
        _ => (left, right),
    })
}

/// One literal, resolved against a column's type. A literal that will not assign is
/// `42883 operator does not exist`, which is what PostgreSQL answers rather than a type mismatch:
/// from its point of view there is simply no `text = integer` to call.
fn retype(
    ty: ColumnType,
    literal: &crate::plan::Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> Result<crate::plan::Literal> {
    use crate::plan::Literal;
    if matches!(literal, Literal::Null) {
        return Ok(Literal::Null);
    }
    // Assignment would take `42` into a `text` column; comparison will not, and turning the error
    // into a silent `false` would be a wrong answer rather than a missing feature.
    if !literal.comparable_with(ty) {
        return Err(undefined_operator(ty, literal, op, literal_on_the_left));
    }
    match literal.assign(ty, "?column?") {
        // Reduced to a value of the column's own type, so the comparison is between two of them.
        Ok(value) => Ok(match value {
            Datum::Int8(value) => Literal::Integer(value),
            Datum::Bool(value) => Literal::Bool(value),
            Datum::Text(value) => Literal::String(value),
            other => Literal::Typed(Box::new(other)),
        }),
        // A literal of the right *category* that still will not read -- `WHERE ts = 'not a date'`
        // -- keeps the error its input function raised, which says what is actually wrong.
        Err(error) => Err(error),
    }
}

fn undefined_operator(
    ty: ColumnType,
    literal: &crate::plan::Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> SqlError {
    let (left, right) = if literal_on_the_left {
        (literal.type_name(), ty.name())
    } else {
        (ty.name(), literal.type_name())
    };
    SqlError::UndefinedOperator {
        left,
        op: op.symbol(),
        right,
    }
}

/// A `WHERE` clause has to be a boolean. PostgreSQL says so, and a `WHERE t` where `t` is text is
/// a mistake worth catching before it silently keeps every row.
fn check_predicate(expr: &Expr, clause: &'static str, scope: &Scope<'_>) -> Result<()> {
    // An `UPDATE` or `DELETE` reaches here without going through `Aggregation::build`, and
    // `WHERE count(*) > 1` is `42803` on all three statements. One check rather than three.
    // A `HAVING` is the one clause where an aggregate belongs, so only `WHERE` refuses it.
    if clause == "WHERE" && aggregate::contains_aggregate(expr) {
        return Err(SqlError::AggregateNotAllowed(
            "aggregate functions are not allowed in WHERE",
        ));
    }
    match expr {
        Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::Literal(crate::plan::Literal::Bool(_) | crate::plan::Literal::Null)
        | Expr::Ordinal {
            ty: ColumnType::Bool,
            ..
        } => Ok(()),
        // PostgreSQL names the type it got, and a user reading "must be type boolean" without it
        // has to work out which of their columns was the problem. Measured, both clauses:
        // `argument of WHERE must be type boolean, not type bigint`.
        other => Err(SqlError::DatatypeMismatch(format!(
            "argument of {clause} must be type boolean, not type {}",
            expr_type(other, scope).map_or("unknown", ColumnType::name)
        ))),
    }
}

/// The name and type of every output column.
///
/// A bare column keeps its name; anything else is `?column?`, which is PostgreSQL's own answer and
/// what `psql` prints as a header.
/// The table a `*` is qualified with, if any.
fn qualifier_of(item: &SelectItem) -> Option<&str> {
    match item {
        SelectItem::QualifiedWildcard(table) => Some(table),
        _ => None,
    }
}

fn output_columns(
    select: &Select,
    scope: &Scope<'_>,
    aggregation: Option<&aggregate::Aggregation>,
) -> Result<Vec<(String, ColumnType)>> {
    let mut columns = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                if scope.tables.is_empty() {
                    return Err(SqlError::Syntax {
                        message: "SELECT * with no tables specified is not valid".to_owned(),
                        position: None,
                        hint: None,
                    });
                }
                // The user's columns, so an internal row id stays hidden: `SELECT *` on a table
                // with no declared key returns what the user declared and nothing else. Across a
                // join it is every table's, left to right, which is the order PostgreSQL gives,
                // and `t.*` is one table's.
                columns.extend(
                    scope
                        .expand(qualifier_of(item))?
                        .map(|(_, column)| (column.name.clone(), column.ty)),
                );
            }
            SelectItem::Expr { expr, alias } => {
                // With an aggregation the type comes from the rewritten expression, because an
                // aggregate call has no type until the aggregation has resolved its argument.
                let ty = match aggregation {
                    None => expr_type(expr, scope)?,
                    Some(aggregation) => {
                        let rewritten = aggregation.rewrite(&resolve(expr, scope)?, scope)?;
                        expr_type(&rewritten, scope)?
                    }
                };
                // PostgreSQL names an aggregate's column after the function -- `count`, `sum` --
                // and not `?column?`. Measured; ActiveRecord reads results by name.
                let name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { name, .. } => name.clone(),
                    Expr::Aggregate(call) => call.func.name().to_owned(),
                    _ => "?column?".to_owned(),
                });
                columns.push((name, ty));
            }
        }
    }
    Ok(columns)
}

/// One expression per output column, with `*` expanded.
fn projection_exprs(
    select: &Select,
    scope: &Scope<'_>,
    aggregation: Option<&aggregate::Aggregation>,
) -> Result<Vec<Expr>> {
    let mut exprs = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                if scope.tables.is_empty() {
                    return Err(SqlError::Syntax {
                        message: "SELECT * with no tables specified is not valid".to_owned(),
                        position: None,
                        hint: None,
                    });
                }
                // `SELECT *` under a `GROUP BY` is every column asked for outside an aggregate,
                // so each one has to be a grouping key or it is `42803` -- which is what the
                // rewrite says, one column at a time, naming the first that is not.
                for (at, column) in scope.expand(qualifier_of(item))? {
                    let expr = Expr::Ordinal { at, ty: column.ty };
                    exprs.push(match aggregation {
                        None => expr,
                        Some(aggregation) => aggregation.rewrite(&expr, scope)?,
                    });
                }
            }
            SelectItem::Expr { expr, .. } => {
                let resolved = resolve(expr, scope)?;
                aggregate::check_not_nested(&resolved)?;
                exprs.push(match aggregation {
                    None => resolved,
                    Some(aggregation) => aggregation.rewrite(&resolved, scope)?,
                });
            }
        }
    }
    Ok(exprs)
}

/// What type an output column has. A literal with no column to take a type from falls back the way
/// PostgreSQL does: a quoted string is `text`, an integer is `bigint`.
pub(super) fn expr_type(expr: &Expr, scope: &Scope<'_>) -> Result<ColumnType> {
    use crate::plan::Literal;
    Ok(match expr {
        Expr::Column { table, name } => scope.resolve_column(table.as_deref(), name)?.1,
        Expr::Ordinal { ty, .. } => *ty,
        // A sequence function answers `bigint` on a real server, all four of them.
        Expr::Literal(Literal::Integer(_)) | Expr::Sequence(_) => ColumnType::Int8,
        Expr::Literal(Literal::Decimal(_)) => ColumnType::Double,

        Expr::Literal(Literal::String(_) | Literal::Null) => ColumnType::Text,
        Expr::Literal(Literal::Typed(value)) => value.column_type().unwrap_or(ColumnType::Text),
        Expr::Literal(Literal::Bool(_))
        | Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. } => ColumnType::Bool,
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),
        // An aggregate's type is the aggregation's business, and by the time a plan is typed
        // every one of them has been rewritten into an `Ordinal` carrying the answer. One here
        // means the rewrite was skipped.
        Expr::Aggregate(_) => {
            return Err(SqlError::Internal(
                "an aggregate reached expr_type without being rewritten".to_owned(),
            ));
        }
        // `DEFAULT` has the type of the column it is written into, and reaching here means it was
        // written somewhere with no column to take one from -- which PostgreSQL answers as a
        // syntax error and this node answers by name.
        Expr::Default => {
            return Err(SqlError::unsupported(
                "DEFAULT outside an INSERT value or an UPDATE assignment",
            ));
        }
    })
}

/// A `LIMIT` or `OFFSET` value: an integer, and not a negative one.
fn count(expr: Option<&Expr>, what: &'static str) -> Result<Option<usize>> {
    let Some(expr) = expr else { return Ok(None) };
    let value = expr.evaluate(ColumnType::Int8, what)?;
    match value {
        // `LIMIT NULL` means no limit, which is PostgreSQL's rule and not an oversight.
        Datum::Null => Ok(None),
        Datum::Int8(value) if value < 0 => Err(SqlError::NegativeLimit(what)),
        Datum::Int8(value) => Ok(Some(usize::try_from(value).unwrap_or(usize::MAX))),
        other => Err(SqlError::DatatypeMismatch(format!(
            "argument of {what} must be type bigint, not {other:?}"
        ))),
    }
}

/// The first key after `key`.
///
/// Appending a zero byte works for any key at all: nothing sorts between `k` and `k ++ 0x00`. Both
/// halves need it — the planner to move a range's end past an inclusive bound, the cursor to
/// resume a chunked scan after the last key it saw.
pub(super) fn successor(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}
