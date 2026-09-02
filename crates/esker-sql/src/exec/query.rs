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

use crate::catalog::pg_catalog;
use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Result, SqlError};
use crate::exec::aggregate;
use crate::plan::{BinaryOp, Expr, Literal, Node, Select, SelectItem, SortKey};
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

    /// N tables, in the order the query wrote them — which for a chain of joins is also the
    /// order the executor reads them, because a chain is planned left-deep with no swapping.
    ///
    /// `using` is empty by construction: `USING` in a chain is refused in the lowering, since the
    /// merge it performs compounds in ways an equality cannot express (`plan::Select::joins`).
    fn chain(entries: &[(&'a TableDef, String)]) -> Self {
        Scope {
            tables: entries.iter().map(|(table, _)| *table).collect(),
            names: entries.iter().map(|(_, name)| name.clone()).collect(),
            written: (0..entries.len()).collect(),
            using: Vec::new(),
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
    /// Where a column is in the joined row, and **the column itself**.
    ///
    /// It returns the definition rather than just the type because a caller wants the typmod too,
    /// and resolving twice — once for the type and once for the number beside it — is two lookups
    /// that can disagree about which `id` an ambiguous name meant.
    fn resolve_column(&self, qualifier: Option<&str>, name: &str) -> Result<(usize, &ColumnDef)> {
        if let Some(qualifier) = qualifier {
            let index = self.entry(qualifier)?;
            if duplicated(self.tables[index], name) {
                return Err(SqlError::AmbiguousColumn(name.to_owned()));
            }
            let at = self.tables[index].column(name).ok_or_else(|| {
                if let Some(system) = SYSTEM_COLUMNS.iter().find(|system| **system == name) {
                    return SqlError::unsupported(format!("the system column {system}"));
                }
                SqlError::UndefinedQualifiedColumn {
                    qualifier: qualifier.to_owned(),
                    column: name.to_owned(),
                }
            })?;
            return Ok((self.offset(index) + at, &self.tables[index].columns[at]));
        }

        // A `USING` column is **one** column, so a bare reference to it is not ambiguous — where
        // the same query written `ON l.id = r.id` answers `42702`. Its value is the left-hand
        // side's: equal to the right's for an inner join, and either equal or NULL for a left one,
        // so there is no third case and no `COALESCE` to need.
        if self.using.iter().any(|merged| merged == name) {
            let written = self.written.first().copied().unwrap_or(0);
            if let Some(at) = self.tables[written].column(name) {
                return Ok((self.offset(written) + at, &self.tables[written].columns[at]));
            }
        }

        let mut found = None;
        for (index, table) in self.tables.iter().enumerate() {
            if let Some(at) = table.column(name) {
                // Twice in **one** relation is ambiguous too, and only a derived table can be:
                // `SELECT a FROM (SELECT 1 AS a, 2 AS a) AS t` is `42702`, measured, while a real
                // table cannot have two columns of one name. Answering the first would be a wrong
                // column returned with nothing to say so.
                if found.is_some() || duplicated(table, name) {
                    return Err(SqlError::AmbiguousColumn(name.to_owned()));
                }
                found = Some((self.offset(index) + at, &table.columns[at]));
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
    /// One name, type and **typmod** per output column, for `RowDescription`. The typmod is
    /// `NO_TYPMOD` for everything but a plain column reference, which is PostgreSQL's rule.
    pub(super) columns: Vec<(String, ColumnType, i32)>,
    /// The table's name, for `EXPLAIN`.
    pub(super) table: String,
    /// The table's column names, so `EXPLAIN` can print the names a user typed rather than the
    /// positions the executor resolved them to.
    pub(super) column_names: Vec<String>,
    /// Which engine the planner chose and why, or `None` for a plan that was never considered for
    /// one — a `SELECT` with no table, a catalog view. Filled in by `crate::exec::fragment::route`,
    /// which is the only thing that decides one.
    pub(super) engine: Option<crate::plan::routing::Decision>,
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
    inners: &[&TableDef],
) -> Result<Planned> {
    // A **chain** of joins takes the other path: left-deep, in the order written, with no choice
    // of driving side. That is not a simplification of what one join does below — it is what the
    // SQL means. `A LEFT JOIN B ON … JOIN C ON …` is `((A LJ B) JOIN C)`, and swapping any step
    // would change which rows the NULL-extension survives; measured, and the whole reason the two
    // paths are not merged. The single-join case keeps its choice because an inner join of two
    // tables really is commutative and the probe only works on the inner side.
    if inners.len() > 1 {
        return plan_chain(select, tenant, table, inners);
    }
    let inner = inners.first().copied();
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

    let only_join = select.joins.first();
    let using: &[String] = only_join.map_or(&[], |join| &join.using);
    let condition = match (&only_join, named_table, named_inner) {
        (Some(join), Some(left), Some(right)) if !join.using.is_empty() => {
            Some(using_condition(&join.using, left, right)?)
        }
        (Some(join), ..) => join.on.clone(),
        _ => None,
    };
    let left_join = only_join.is_some_and(|join| join.kind == crate::plan::JoinKind::Left);

    // A **derived** side never drives the choice. `drive_from` swaps in order to reach a probe on
    // the inner table's key, and a derived table has none -- so a swap could only move the plan
    // that produces its rows to the side that is read once per outer row, for no gain.
    let swappable = select
        .from
        .as_ref()
        .is_none_or(|from| from.derived.is_none())
        && only_join.is_none_or(|join| join.table.derived.is_none());
    let (scope, swapped) = drive_from(
        named_table,
        named_inner,
        condition.as_ref().filter(|_| swappable),
        left_join,
        using,
    );
    let (outer_table, inner_table) = match (table, inner, swapped) {
        (Some(left), Some(right), false) => (Some(left), Some(right)),
        (Some(left), Some(right), true) => (Some(right), Some(left)),
        (table, _, _) => (table, None),
    };
    // Which `FROM` entry each side came from, so a derived one is read from its own plan. Swapped
    // with the tables, because `drive_from` may have chosen the right-hand side to drive from.
    let left_entry = select.from.as_ref();
    let right_entry = only_join.map(|join| &join.table);
    let (outer_entry, inner_entry) = if swapped {
        (right_entry, left_entry)
    } else {
        (left_entry, right_entry)
    };

    let mut node = match outer_table {
        None => Node::OneRow,
        // With a join the outer access path only gets the `WHERE` when the whole of it belongs to
        // the outer table. A predicate mentioning the inner one cannot narrow the outer scan --
        // its value is not known until an outer row has been read -- and handing it to
        // `access_path`, which resolves against one table, would be an error rather than a plan.
        // A derived table's "access path" is the sub-select's own plan. Nothing narrows it and
        // nothing seeks in it -- the `WHERE` below becomes a `Filter` over these rows, which is
        // what the same statement over an unindexed table already gets.
        Some(table) => {
            if let Some(plan) = outer_entry.and_then(crate::plan::TableRef::derived_plan) {
                plan.clone()
            } else {
                let usable = select
                    .filter
                    .as_ref()
                    .filter(|filter| inner_table.is_none() || mentions_only(filter, table));
                access_path(usable, tenant, table)?
            }
        }
    };

    if let Some(join) = only_join {
        let inner = inner_table.ok_or_else(|| SqlError::UndefinedTable(join.table.name.clone()))?;
        node = join_node(
            node,
            condition.as_ref(),
            left_join,
            &scope,
            inner,
            inner_entry.and_then(crate::plan::TableRef::derived_plan),
        )?;
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

    finish_plan(select, node, &scope, outer_table)
}

/// Everything above the joins and the `WHERE`: aggregation, the target list, the sort, `DISTINCT`
/// and `LIMIT`.
///
/// Shared by the one-join path and [`plan_chain`] rather than written twice. The two differ only
/// in how they build the node underneath and which scope they build it against; from here up, a
/// statement over four tables and a statement over one are planned by the same code, which is
/// what keeps `GROUP BY` and `ORDER BY` from acquiring a second set of rules for chains.
fn finish_plan(
    select: &Select,
    mut node: Node,
    scope: &Scope<'_>,
    outer_table: Option<&TableDef>,
) -> Result<Planned> {
    // Aggregation sits between the filter and the sort. Everything above it is written about a
    // row that no table has -- the grouping keys followed by the aggregate values -- and
    // `Aggregation::rewrite` is what moves an expression from one to the other.
    let aggregation = aggregate::Aggregation::build(select, scope)?;
    if let Some(aggregation) = &aggregation {
        node = Node::Aggregate {
            input: Box::new(node),
            keys: aggregation.keys.clone(),
            aggregates: aggregation.specs.clone(),
            having: aggregation.having.clone(),
            grouped: aggregation.grouped,
        };
    }

    let columns = output_columns(select, scope, aggregation.as_ref())?;
    let exprs = projection_exprs(select, scope, aggregation.as_ref())?;

    // The sort goes *below* the projection, so it can order on a column the target list does not
    // return -- `SELECT n FROM s1 ORDER BY id` is ordinary SQL, and a sort above the projection
    // could not see `id` at all. An `ORDER BY` naming an output alias is substituted first, which
    // is the other half of what PostgreSQL allows.
    //
    // `SELECT DISTINCT` is the one shape where that is not possible, and PostgreSQL says so
    // rather than working around it: deduplication happens over the target list, so a sort key
    // the target list does not contain has no defined position to sort at. Its keys are resolved
    // against the *output* columns and a key that is not one of them is `42P10`.
    let sort_keys = order_keys(select, scope, aggregation.as_ref(), &exprs, &columns)?;
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
        column_names: scope_column_names(scope),
        engine: None,
    })
}

/// Every column of every table in scope, in row order, so `EXPLAIN` can print the name a user
/// typed for any position the executor resolved.
fn scope_column_names(scope: &Scope<'_>) -> Vec<String> {
    scope
        .tables
        .iter()
        .flat_map(|table| table.columns.iter().map(|column| column.name.clone()))
        .collect()
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
    columns: &[(String, ColumnType, i32)],
) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for item in &select.order_by {
        // A position is resolved against the **output list**, which is already expanded and
        // already in whichever row space the sort will run over -- so `SELECT * FROM t ORDER BY 1`
        // works, where resolving it against the unexpanded target list could not have.
        let resolved = if let Expr::Literal(Literal::Integer(position)) = &item.expr {
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
                && columns.iter().filter(|(output, ..)| output == name).count() > 1
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
                typmod: typmod_of(&resolved, scope),
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
pub(super) type TargetList = (Vec<(String, ColumnType, i32)>, Vec<Expr>);

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
        joins: Vec::new(),
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
/// A chain of two or more joins: left-deep, in the order written.
///
/// Every step is a nested loop whose outer side is everything planned so far, so the scope grows
/// one table at a time and each `ON` resolves against **every** table to its left — which is what
/// lets `ON n.oid = t.relnamespace` reach back past the table joined in between, as
/// `ActiveRecord`'s `indexes()` does.
///
/// # What this deliberately does not do
///
/// **It does not reorder.** A two-table inner join is commutative and [`plan`] exploits that; a
/// chain is not free to, because a `LEFT JOIN` anywhere in it fixes the order of everything after
/// it — `A LEFT JOIN B ON … JOIN C ON …` keeps only the rows the inner join matches, and swapping
/// the last two steps would keep the NULL-extended ones. Rather than reorder the prefix that
/// happens to be all-inner and stop at the first outer join, it plans as written: a rule-based
/// planner that is right everywhere beats one that is faster on the shapes nobody sends.
///
/// The cost is that the inner side of each step is probed by key only when its `ON` allows it,
/// exactly as for one join, and read whole otherwise. That is the same trade one join makes.
fn plan_chain(
    select: &Select,
    tenant: u64,
    table: Option<&TableDef>,
    inners: &[&TableDef],
) -> Result<Planned> {
    let Some(outer) = table else {
        return Err(SqlError::Internal(
            "a chain of joins with no left-hand table".to_owned(),
        ));
    };
    let outer_name = select
        .from
        .as_ref()
        .map_or("", crate::plan::TableRef::referred_as)
        .to_owned();

    // Every table under the name the query refers to it by, left to right. The same table may
    // appear twice under two aliases — `pg_class t … pg_class i` — so a duplicate *name* is the
    // error and a duplicate table is not.
    let mut entries: Vec<(&TableDef, String)> = vec![(outer, outer_name)];
    for (join, inner) in select.joins.iter().zip(inners) {
        entries.push((*inner, join.table.referred_as().to_owned()));
    }
    for (at, (_, name)) in entries.iter().enumerate() {
        // The empty name is a derived table with no alias, which PostgreSQL 19 allows and which
        // nothing can refer to -- so two of them are two anonymous relations, not a duplicate.
        if !name.is_empty() && entries[..at].iter().any(|(_, earlier)| earlier == name) {
            return Err(SqlError::DuplicateTableName(name.clone()));
        }
    }

    // The `WHERE` cannot narrow the outer access path here: with more than one table it may
    // mention any of them, and a value from a table not yet read is not one a scan can seek on.
    let mut node = match select
        .from
        .as_ref()
        .and_then(crate::plan::TableRef::derived_plan)
    {
        Some(plan) => plan.clone(),
        None => access_path(None, tenant, outer)?,
    };
    // Grown one table at a time, so each step's `ON` sees exactly the tables to its left plus the
    // one being joined — which is what makes a reference to a table two steps back resolve, and a
    // reference to one further right an "undefined column" rather than a silent NULL.
    for (at, (join, inner)) in select.joins.iter().zip(inners).enumerate() {
        let scope = Scope::chain(&entries[..=at + 1]);
        let left_join = join.kind == crate::plan::JoinKind::Left;
        node = join_node(
            node,
            join.on.as_ref(),
            left_join,
            &scope,
            inner,
            join.table.derived_plan(),
        )?;
    }

    let scope = Scope::chain(&entries);
    if let Some(filter) = &select.filter {
        let predicate = resolve(filter, &scope)?;
        check_predicate(&predicate, "WHERE", &scope)?;
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }
    finish_plan(select, node, &scope, Some(outer))
}

fn from_names(select: &Select) -> Result<(&str, &str)> {
    let left = select
        .from
        .as_ref()
        .map_or("", crate::plan::TableRef::referred_as);
    let right = select
        .joins
        .first()
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
        Expr::InList { operand, list, .. } => {
            for_each_column(operand, visit);
            for item in list {
                for_each_column(item, visit);
            }
        }
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
    inner_plan: Option<&Node>,
) -> Result<Node> {
    // A computed relation has no primary key and no index, so there is nothing to probe with and
    // the inner side is read once into memory like any other unindexed join. Decided here rather
    // than left to `probe_for`, so that a view can never be reached through a key. A **derived
    // table** is the same case for the same reason: its rows come from a plan.
    let inner_view = pg_catalog::view_of(inner);
    let probe = on
        .filter(|_| inner_view.is_none() && inner_plan.is_none())
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
        inner_view,
        inner_table: inner.name.clone(),
        inner_columns: inner.row_schema(),
        inner_plan: inner_plan.map(|plan| Box::new(plan.clone())),
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
    // A `pg_catalog` relation is computed, so it has no key range to narrow and no index to seek
    // in: one access path, all of its rows, and the `WHERE` above it does the rest. Returned
    // before any key is built, so the reserved id a view's `TableDef` carries never reaches a
    // range (`crate::catalog::pg_catalog`).
    if let Some(view) = pg_catalog::view_of(table) {
        return Ok(Node::CatalogView { view, columns });
    }
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
/// Whether one relation has this column name more than once.
///
/// Only a **derived table** can: a `CREATE TABLE` with two columns of one name is `42701`, so for
/// every real relation this is `false` and costs one pass over a short list.
fn duplicated(table: &TableDef, name: &str) -> bool {
    table
        .columns
        .iter()
        .filter(|column| column.name == name)
        .nth(1)
        .is_some()
}

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
            let (at, column) = scope.resolve_column(table.as_deref(), name)?;
            Expr::Ordinal {
                at,
                ty: column.ty,
                typmod: column.typmod,
            }
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
        Expr::InList {
            operand,
            list,
            negated,
        } => {
            // `x IN (a, b)` is a set of `=`, so every item is typed the way `x = a` types it — but
            // the list is typed as a **whole** and not pairwise, which is a rule of its own.
            // PostgreSQL's `select_common_type` runs over the operand and every item at once, so
            // one typed item gives *every* `unknown` in the expression its type, the operand
            // included: `'01' IN ('1', 1)` is true, because the `1` makes it `1 IN (1, 1)`. A
            // left-to-right pairwise reconcile answers `f` for that, which is a wrong answer and
            // not a refusal. Measured, `tests/corpus/pg19_unknown.txt`.
            let mut operand = resolve(operand, scope)?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                items.push(resolve(item, scope)?);
            }
            // The coercion happens here, at plan time, and not when a row is scanned: PostgreSQL
            // answers `1 IN (NULL, 'x')` with `22P02` even though the NULL alone could have
            // decided it.
            if let Some(ty) = common_type(&operand, &items) {
                give_type(&mut operand, ty)?;
                for item in &mut items {
                    give_type(item, ty)?;
                }
            }
            // Then the ordinary pairwise rule, which is what still raises `42883` when two items
            // have types no `=` covers — `n IN ('one', 1)` over a `text` column.
            let mut resolved = Vec::with_capacity(items.len());
            for item in items {
                let (left, right) = reconcile(BinaryOp::Eq, operand, item)?;
                operand = left;
                resolved.push(right);
            }
            Expr::InList {
                operand: Box::new(operand),
                list: resolved,
                negated: *negated,
            }
        }
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, scope)?),
            negated: *negated,
        },
        // Only the **operand** is resolved here. Everything inside the sub-select was resolved
        // against the sub-select's own scope when `crate::exec::subquery::plan_subqueries` planned
        // it, and resolving it again here would type it against a row it will never see.
        Expr::Subquery(sub) => {
            let mut resolved = sub.clone();
            if let Some(operand) = &sub.operand {
                resolved.operand = Some(Box::new(subquery_operand(operand, sub, scope)?));
            }
            Expr::Subquery(resolved)
        }
        other => other.clone(),
    })
}

/// The left-hand side of an `IN`/`ANY`/`ALL`, typed against the subquery's column.
///
/// The subquery's single column is what the operand is compared against, so it types the operand
/// exactly as a column on the other side of an `=` would — which is why the whole of the rule is
/// [`reconcile`] against a stand-in for that column. The stand-in's *position* is never read: it
/// is discarded on the next line, and the only field of it that matters is the type.
///
/// What `reconcile` cannot decide is two operands that both already have types, because it has
/// nothing to resolve. That is the case the capture cares about — `WHERE n IN (SELECT a_id FROM
/// b)` over a `text` column is `42883 operator does not exist: text = bigint` — and it is checked
/// here rather than left to the evaluator, where it would be a silent `false`.
fn subquery_operand(
    operand: &Expr,
    sub: &crate::plan::SubqueryExpr,
    scope: &Scope<'_>,
) -> Result<Expr> {
    let operand = resolve(operand, scope)?;
    let Some((_, ty)) = sub.column else {
        return Ok(operand);
    };
    let op = sub.kind.comparison().unwrap_or(BinaryOp::Eq);
    let stand_in = Expr::Ordinal {
        at: 0,
        ty,
        typmod: crate::value::NO_TYPMOD,
    };
    let (operand, _) = reconcile(op, operand, stand_in)?;
    if let Expr::Ordinal { ty: left, .. } = &operand
        && !same_family(*left, ty)
    {
        return Err(SqlError::UndefinedOperator {
            left: left.name(),
            op: op.symbol(),
            right: ty.name(),
        });
    }
    Ok(operand)
}

/// Whether an operator exists between two types, as coarsely as this node's type surface allows.
///
/// PostgreSQL's answer comes out of `pg_operator` and its implicit casts; ours is the same
/// grouping [`crate::plan::Literal::comparable_with`] already uses for a literal against a column,
/// lifted to two columns. Coarse in the safe direction: it refuses only pairs that no cast in
/// PostgreSQL relates either, so it cannot turn a comparison a real server runs into an error.
fn same_family(left: ColumnType, right: ColumnType) -> bool {
    fn family(ty: ColumnType) -> u8 {
        match ty {
            ColumnType::Int8
            | ColumnType::Int4
            | ColumnType::Int2
            | ColumnType::Double
            | ColumnType::Real => 0,
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => 1,
            ColumnType::Bool => 2,
            ColumnType::Bytea => 3,
            ColumnType::TimestampTz | ColumnType::Timestamp => 4,
        }
    }
    family(left) == family(right)
}

/// Gives a literal the type of whatever it is being compared against, or says the comparison is
/// between types no operator covers.
///
/// "Whatever" is the part that was a bug: a quoted string is PostgreSQL's `unknown` and takes its
/// type from the *other operand*, which is a **column or a literal** and not only a column.
/// `SELECT 1 = '1'` is `t` on a real server and was `f` here, because with no column in the
/// expression nothing typed the `'1'` and an `int8` was compared against a `text`. Three cases,
/// measured in `tests/corpus/pg19_unknown.txt`:
///
/// 1. one side `unknown`, the other typed — the `unknown` is read by that type's input function,
///    and a string that will not read is that function's own error (`22P02`) rather than a `false`;
/// 2. **both** sides `unknown` — both are `text`, which is why `'1' = '01'` is `f` where
///    `1 = '01'` is `t`. Nothing is changed here and the evaluator's `Literal::String` is already
///    a `Datum::Text`;
/// 3. neither side `unknown` — nothing to resolve, and a pair no operator covers is `42883`.
fn reconcile(op: BinaryOp, left: Expr, right: Expr) -> Result<(Expr, Expr)> {
    Ok(match (&left, &right) {
        (Expr::Ordinal { ty, typmod, .. }, Expr::Literal(literal)) => (
            left.clone(),
            Expr::Literal(blank_pad(retype(*ty, literal, op, false)?, *ty, *typmod)),
        ),
        (Expr::Literal(literal), Expr::Ordinal { ty, typmod, .. }) => (
            Expr::Literal(blank_pad(retype(*ty, literal, op, true)?, *ty, *typmod)),
            right.clone(),
        ),
        // An `unknown` beside a literal that has a type. `Literal::String` is the only `unknown`
        // there is: a NULL has no type either, and needs none — a comparison with one is NULL
        // whatever type the other side turns out to be.
        (Expr::Literal(Literal::String(_)), Expr::Literal(other)) => match literal_type(other) {
            Some(ty) => (
                Expr::Literal(retype(ty, unknown_of(&left), op, true)?),
                right.clone(),
            ),
            None => (left, right),
        },
        (Expr::Literal(other), Expr::Literal(Literal::String(_))) => match literal_type(other) {
            Some(ty) => (
                left.clone(),
                Expr::Literal(retype(ty, unknown_of(&right), op, false)?),
            ),
            None => (left, right),
        },
        _ => (left, right),
    })
}

/// The type a literal already carries, or `None` for the two that carry none.
///
/// `unknown` (a quoted string) is the one that takes a type from its neighbour; NULL has no type
/// and needs none. The other four are what PostgreSQL calls them, with the two divergences this
/// node declares: a bare integer constant is `int4` on a real server and `int8` here, and a
/// decimal constant is `numeric` there and `double precision` here — the same choice
/// `Literal::Decimal` already makes everywhere else in this crate, `SELECT 1.5` included.
fn literal_type(literal: &Literal) -> Option<ColumnType> {
    match literal {
        Literal::Null | Literal::String(_) => None,
        Literal::Integer(_) => Some(ColumnType::Int8),
        Literal::Decimal(_) => Some(ColumnType::Double),
        Literal::Bool(_) => Some(ColumnType::Bool),
        Literal::Typed(value) => value.column_type(),
    }
}

/// The literal inside an `Expr::Literal`, for the two `reconcile` arms that have already matched
/// on it. A non-literal here is a pattern that cannot be reached.
fn unknown_of(expr: &Expr) -> &Literal {
    match expr {
        Expr::Literal(literal) => literal,
        // Unreachable: every caller has matched `Expr::Literal` in the same pattern.
        _ => &Literal::Null,
    }
}

/// The type an `IN` list resolves to as a whole — PostgreSQL's `select_common_type`, narrowed to
/// what this node's expressions can be.
///
/// The **first** operand with a type wins, operand or item, and every `unknown` in the expression
/// takes it. Where two typed operands disagree nothing is decided here: the pairwise `reconcile`
/// that follows is what raises `42883`, and it names the two types.
fn common_type(operand: &Expr, items: &[Expr]) -> Option<ColumnType> {
    std::iter::once(operand)
        .chain(items)
        .find_map(|expr| match expr {
            Expr::Ordinal { ty, .. } => Some(*ty),
            Expr::Literal(literal) => literal_type(literal),
            _ => None,
        })
}

/// Reads an `unknown` as `ty`, in place. Anything else is left exactly as it is — a literal that
/// already has a type keeps it, and it is `reconcile` that decides whether the pair has an
/// operator.
fn give_type(expr: &mut Expr, ty: ColumnType) -> Result<()> {
    if let Expr::Literal(literal @ Literal::String(_)) = expr {
        *literal = retype(ty, &literal.clone(), BinaryOp::Eq, false)?;
    }
    Ok(())
}

/// One literal, resolved against a column's type. A literal that will not assign is
/// `42883 operator does not exist`, which is what PostgreSQL answers rather than a type mismatch:
/// from its point of view there is simply no `text = integer` to call.
/// A literal being compared against a `character(n)`, in the form that column's values are stored.
///
/// PostgreSQL's `bpchar` comparison ignores trailing blanks on **both** sides. Here the stored
/// side is already padded to exactly `n`, so the same answer falls out of a plain byte comparison
/// once the literal is put in the same form: strip its trailing blanks and pad back to `n`. That
/// is why `c = 'x'`, `c = 'x  '` and `c = 'x    '` all find a `char(3)` holding `x`.
///
/// A literal whose stripped length exceeds `n` is left alone. It cannot equal any stored value —
/// every one of them is `n` characters — and that is a comparison that finds nothing rather than
/// an error, which is what a real server does too: the `22001` is for *storing*, not for asking.
///
/// Doing this here rather than in the evaluator is what keeps an index seek working: the key
/// built from a padded literal is the key the row wrote.
fn blank_pad(literal: Literal, ty: ColumnType, typmod: i32) -> Literal {
    if ty != ColumnType::Bpchar {
        return literal;
    }
    let Some(length) = crate::value::length_of_typmod(typmod) else {
        return literal;
    };
    // `retype` hands a text-shaped value back as `Literal::String`, not `Literal::Typed` — both
    // spellings reach here and both are the same value.
    let text = match &literal {
        Literal::String(text) => text.as_str(),
        Literal::Typed(datum) => match datum.as_ref() {
            Datum::Text(text) => text.as_str(),
            _ => return literal,
        },
        _ => return literal,
    };
    let trimmed = text.trim_end_matches(' ');
    let Some(short_by) = (length as usize).checked_sub(trimmed.chars().count()) else {
        return literal;
    };
    let mut padded = String::with_capacity(trimmed.len() + short_by);
    padded.push_str(trimmed);
    padded.extend(std::iter::repeat_n(' ', short_by));
    Literal::String(padded)
}

fn retype(
    ty: ColumnType,
    literal: &Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> Result<Literal> {
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
    literal: &Literal,
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
        | Expr::InList { .. }
        | Expr::Literal(Literal::Bool(_) | Literal::Null)
        | Expr::Ordinal {
            ty: ColumnType::Bool,
            ..
        } => Ok(()),
        // `EXISTS`, `IN` and `ANY`/`ALL` are predicates; a **scalar** subquery is not, and falls
        // through to the arm below so that `WHERE (SELECT max(id) FROM a)` gets PostgreSQL's own
        // sentence with the subquery's type in it — measured, `not type bigint`.
        Expr::Subquery(sub) if sub.value_type() == ColumnType::Bool => Ok(()),
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
) -> Result<Vec<(String, ColumnType, i32)>> {
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
                        .map(|(_, column)| (column.name.clone(), column.ty, column.typmod)),
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
                    // A scalar subquery takes the **subquery's own** column name and `EXISTS` is
                    // called `exists`; everything else about a subquery is `?column?`. Measured
                    // with `psql`, which a corpus of types and rows cannot record.
                    Expr::Subquery(sub) => sub.output_name().unwrap_or("?column?").to_owned(),
                    _ => "?column?".to_owned(),
                });
                // A typmod travels only with a **plain column reference**, which is
                // PostgreSQL's rule and the corpus's: `c || '|'` is `text` with none and
                // `min(c)` is `bpchar` with none, where a bare `c` is `character(3)`.
                let typmod = match expr {
                    Expr::Column { .. } if aggregation.is_none() => typmod_of(expr, scope),
                    _ => crate::value::NO_TYPMOD,
                };
                columns.push((name, ty, typmod));
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
                    let expr = Expr::Ordinal {
                        at,
                        ty: column.ty,
                        typmod: column.typmod,
                    };
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
/// The typmod a bare column reference carries, or `NO_TYPMOD` for anything else.
///
/// Only a plain column reference has one, which is PostgreSQL's rule rather than a simplification:
/// `c || '|'` over a `character(3)` is `text` with no modifier and `min(c)` is `bpchar` with none,
/// where a bare `c` is `character(3)`. An unresolvable column answers `NO_TYPMOD` rather than an
/// error, because whatever is wrong with it is reported by `expr_type` beside this.
pub(super) fn typmod_of(expr: &Expr, scope: &Scope<'_>) -> i32 {
    match expr {
        Expr::Column { table, name } => scope
            .resolve_column(table.as_deref(), name)
            .map_or(crate::value::NO_TYPMOD, |(_, column)| column.typmod),
        _ => crate::value::NO_TYPMOD,
    }
}

pub(super) fn expr_type(expr: &Expr, scope: &Scope<'_>) -> Result<ColumnType> {
    Ok(match expr {
        Expr::Column { table, name } => scope.resolve_column(table.as_deref(), name)?.1.ty,
        Expr::Ordinal { ty, .. } => *ty,
        // A sequence function answers `bigint` on a real server, all four of them.
        Expr::Literal(Literal::Integer(_)) | Expr::Sequence(_) => ColumnType::Int8,
        Expr::Literal(Literal::Decimal(_)) => ColumnType::Double,

        Expr::Literal(Literal::String(_) | Literal::Null) => ColumnType::Text,
        Expr::Literal(Literal::Typed(value)) => value.column_type().unwrap_or(ColumnType::Text),
        Expr::Literal(Literal::Bool(_))
        | Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InList { .. } => ColumnType::Bool,
        // A scalar subquery has the type of the column it returns and the other four are
        // predicates, which is the whole of what `SubqueryExpr::value_type` says.
        Expr::Subquery(sub) => sub.value_type(),
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
