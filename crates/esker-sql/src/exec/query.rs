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
    /// The scope of the statement this one is a subquery of, or `None` for a statement that is not
    /// one.
    ///
    /// This is the whole of correlation. A name the inner scope has is resolved there and the
    /// outer scope is never asked — **the inner shadows the outer**, measured and not obvious:
    /// `SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = id)` returns *no rows*,
    /// because `b` has an `id` of its own and the query is `b.a_id = b.id`. A name it does not
    /// have walks out one level at a time and becomes an [`crate::plan::Expr::Outer`] carrying how
    /// far it walked.
    outer: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    /// No tables: `SELECT 1`.
    pub(super) fn empty() -> Self {
        Scope {
            tables: Vec::new(),
            names: Vec::new(),
            written: Vec::new(),
            using: Vec::new(),
            outer: None,
        }
    }

    /// One table under its own name, which is every statement that does not write an alias.
    pub(super) fn single(table: &'a TableDef) -> Self {
        Scope::single_as(table, table.name.clone())
    }

    /// The target row and the **proposed** row side by side, for an `ON CONFLICT … DO UPDATE`.
    ///
    /// PostgreSQL puts both in scope while the assignments are evaluated: a bare column or one
    /// qualified with the table's name is the row **already there**, and `excluded.c` is the row
    /// that would have been inserted. Two entries of one table under two names is exactly that,
    /// and it makes the ordinals run `0..n` for the first and `n..2n` for the second — which is
    /// the shape of the concatenated row the evaluator is handed.
    pub(super) fn conflicting(table: &'a TableDef) -> Self {
        Scope {
            tables: vec![table, table],
            names: vec![table.name.clone(), "excluded".to_owned()],
            written: vec![0, 1],
            using: Vec::new(),
            outer: None,
        }
    }

    /// One table under the name the query refers to it by.
    pub(super) fn single_as(table: &'a TableDef, name: String) -> Self {
        Scope {
            tables: vec![table],
            names: vec![name],
            written: vec![0],
            using: Vec::new(),
            outer: None,
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
            outer: None,
        }
    }

    /// The same scope, with the statement this one is a subquery of behind it.
    pub(super) fn under(mut self, outer: Option<&'a Scope<'a>>) -> Self {
        self.outer = outer;
        self
    }

    /// N tables, in the order the query wrote them — which for a chain of joins is also the
    /// order the executor reads them, because a chain is planned left-deep with no swapping.
    ///
    /// `using` is empty by construction: `USING` in a chain is refused in the lowering, since the
    /// merge it performs compounds in ways an equality cannot express (`plan::Select::joins`).
    pub(super) fn chain(entries: &[(&'a TableDef, String)]) -> Self {
        Scope {
            tables: entries.iter().map(|(table, _)| *table).collect(),
            names: entries.iter().map(|(_, name)| name.clone()).collect(),
            written: (0..entries.len()).collect(),
            using: Vec::new(),
            outer: None,
        }
    }

    /// The same chain, told which order the user wrote the tables in.
    ///
    /// `entries` is the order the executor **joins** in, which for a reordered comma list is not
    /// the order they were written (`comma_list_order`). Only [`Scope::written`] cares, and what it
    /// decides is what `SELECT *` returns — so a reordering that forgot this would silently give
    /// back the same columns in a different order, which no `ORDER BY` and no test of one table's
    /// values would notice. `written[i]` is where the table the user wrote *i*-th ended up.
    fn chain_written(entries: &[(&'a TableDef, String)], written: Vec<usize>) -> Self {
        Scope {
            written,
            ..Scope::chain(entries)
        }
    }

    /// The **primary key of the table a resolved position belongs to**, as positions in this
    /// scope, or `None` when that table has no declared key.
    ///
    /// What it is for is PostgreSQL's functional dependency: a `GROUP BY` that contains a table's
    /// primary key leaves one row per group *of that table*, so every other column of it has
    /// exactly one value and needs no aggregate. `ActiveRecord` writes that constantly —
    /// `group(:id)` on a relation selecting `*`.
    ///
    /// **Per table, which is what the offset is here for.** `GROUP BY f.id` frees every column of
    /// `f` and none of `a`, and a rule that asked only "is some key grouped" would accept a query
    /// that really is ambiguous. Measured, in a join and in one select list.
    pub(super) fn key_of(&self, at: usize) -> Option<Vec<usize>> {
        let mut start = 0;
        for table in &self.tables {
            if at < start + table.columns.len() {
                if table.primary_key.is_empty() {
                    return None;
                }
                return Some(table.primary_key.iter().map(|key| start + key).collect());
            }
            start += table.columns.len();
        }
        None
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
        let (_, at, column) = self.lookup(qualifier, name)?;
        Ok((at, column))
    }

    /// The enum the column at a resolved position was declared as, or `None`.
    ///
    /// A position is an index into the **concatenated** row, so this walks the tables the way
    /// [`Scope::offset`] builds it: the labels live on the table
    /// (`crate::catalog::TableDef::enums`) and the oid on the column, so both halves have to be
    /// found together (ADR 0050).
    fn user_type_at(&self, at: usize) -> Option<&'a crate::catalog::TypeDef> {
        let mut start = 0;
        for table in &self.tables {
            let end = start + table.columns.len();
            if at < end {
                let column = table.columns.get(at - start)?;
                return super::assign::enum_of(table, column);
            }
            start = end;
        }
        None
    }

    /// The same lookup, saying **how many scopes out** the name was found.
    ///
    /// `0` is this row and anything above it is a correlated reference. Two rules, both measured:
    ///
    /// * **the inner scope shadows the outer**, so a name this level has is never looked for
    ///   further out — `SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = id)` is
    ///   `b.a_id = b.id` and returns no rows;
    /// * a name **no** level has, and a qualifier no level has, keep the errors they already had:
    ///   `42703 column a.nope does not exist` and `42P01 missing FROM-clause entry for table "z"`,
    ///   reported from the outermost level so the message names what the user wrote.
    ///
    /// Ambiguity stays a question about **one** level: PostgreSQL resolves innermost-first and two
    /// tables of one level sharing a name is the `42702` it already was.
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<(usize, usize, &ColumnDef)> {
        match self.resolve_here(qualifier, name) {
            Ok((at, column)) => Ok((0, at, column)),
            // **Only two failures walk outward**, and they are the two that mean "not here":
            // a qualifier this level does not have, and a bare name none of its tables has.
            // Everything else is *this* level's answer and is final — above all
            // `42703 column b.nope does not exist`, where the qualifier resolved here and the
            // column did not. Walking out from that reports the outer query's missing `b`
            // instead, which is a different mistake with a different fix. Measured.
            Err(error @ (SqlError::MissingFromEntry(_) | SqlError::UndefinedColumn(_))) => {
                match self.outer {
                    None => Err(error),
                    Some(outer) => {
                        let (level, at, column) = outer.lookup(qualifier, name)?;
                        Ok((level + 1, at, column))
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    /// The relation a column reference belongs to, walking outward exactly as [`Scope::lookup`]
    /// does — or `None` when the name resolves to nothing, which is the caller's error to report.
    ///
    /// It exists for one question: **is this column one of the catalog's attnum vectors?** A
    /// `ColumnDef` carries a name and a type and neither says which relation it came from, and
    /// `pg_constraint.conkey` is only an `int2vector` because of the relation it is in.
    fn relation_of(&self, qualifier: Option<&str>, name: &str) -> Option<&TableDef> {
        if let Some(qualifier) = qualifier {
            if let Ok(index) = self.entry(qualifier)
                && self.tables[index].column(name).is_some()
            {
                return Some(self.tables[index]);
            }
        } else if let Some(table) = self
            .tables
            .iter()
            .find(|table| table.column(name).is_some())
        {
            return Some(table);
        }
        self.outer
            .and_then(|outer| outer.relation_of(qualifier, name))
    }

    /// The lookup at this level only.
    fn resolve_here(&self, qualifier: Option<&str>, name: &str) -> Result<(usize, &ColumnDef)> {
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
pub(super) struct OutputColumn {
    /// The name a client is told, which is the alias where there is one.
    pub(super) name: String,
    /// What the value physically is — the type the wire's `RowDescription` carries, unless
    /// [`OutputColumn::user_type`] replaces it.
    pub(super) ty: ColumnType,
    /// PostgreSQL's `atttypmod` for the declaration, or `crate::value::NO_TYPMOD`.
    pub(super) typmod: i32,
    /// The **user-defined type** this column was declared as, or `None`.
    ///
    /// Carried beside the storage type rather than instead of it, because both are needed and they
    /// answer different questions: `ty` is how the value in the row is read, and this is what the
    /// client is told and what the value is rendered *as* — an enum's ordinal goes onto the wire
    /// as its label (ADR 0050). Kept in the same struct as `ty` rather than in a list beside it, so
    /// that a column can never have one and not the other.
    pub(super) user_type: Option<crate::catalog::TypeDef>,
}

pub(super) struct Planned {
    /// The tree to pull rows through.
    pub(super) node: Node,
    /// One name and type per output column, for `RowDescription`.
    /// One name, type and **typmod** per output column, for `RowDescription`. The typmod is
    /// `NO_TYPMOD` for everything but a plain column reference, which is PostgreSQL's rule.
    pub(super) columns: Vec<OutputColumn>,
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
    matching_rows_as(filter, tenant, table, table.name.clone())
}

/// The same, under the name the statement refers to the table by.
///
/// **An alias replaces the name.** `UPDATE t AS a SET … WHERE t.id = 1` is
/// `42P01 invalid reference to FROM-clause entry for table "t"` on a real server, with a HINT
/// naming the alias — measured — and a scope built on the table's own name would answer it
/// instead of refusing.
pub(super) fn matching_rows_as(
    filter: Option<&Expr>,
    tenant: u64,
    table: &TableDef,
    name: String,
) -> Result<Node> {
    let mut node = access_path(filter, tenant, table)?;
    if let Some(filter) = filter {
        let scope = Scope::single_as(table, name);
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
    plan_under(select, tenant, table, inners, None)
}

/// The same, under the scope of the statement this one is a subquery of.
///
/// Everything a correlated subquery needs is here and it is one argument: the scopes this builds
/// carry `outer`, so a name the sub-select does not have resolves one level out and becomes an
/// [`crate::plan::Expr::Outer`]. Nothing else in the planner changes shape.
pub(super) fn plan_under(
    select: &Select,
    tenant: u64,
    table: Option<&TableDef>,
    inners: &[&TableDef],
    outer: Option<&Scope<'_>>,
) -> Result<Planned> {
    // A **chain** of joins takes the other path: left-deep, in the order written, with no choice
    // of driving side. That is not a simplification of what one join does below — it is what the
    // SQL means. `A LEFT JOIN B ON … JOIN C ON …` is `((A LJ B) JOIN C)`, and swapping any step
    // would change which rows the NULL-extension survives; measured, and the whole reason the two
    // paths are not merged. The single-join case keeps its choice because an inner join of two
    // tables really is commutative and the probe only works on the inner side.
    if inners.len() > 1 {
        return plan_chain(select, tenant, table, inners, outer);
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
    // A **derived** side never drives the choice, and neither does a statement with a correlated
    // subquery in it. The second is not taste: a swap moves each table's columns to a different
    // place in the joined row, and an `Expr::Outer` is a *position* in that row — resolved against
    // the written order by `written_scope` before this function ran. Swapping after that would
    // read the wrong column, silently.
    let swappable = select
        .from
        .as_ref()
        .is_none_or(|from| from.derived.is_none())
        && only_join.is_none_or(|join| join.table.derived.is_none())
        && !has_correlated_subquery(select);
    let (scope, swapped) = drive_from(
        named_table,
        named_inner,
        condition.as_ref().filter(|_| swappable),
        left_join,
        using,
    );
    let scope = scope.under(outer);
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
        // A set-returning function is a third kind of source, beside a relation and a derived
        // table: no key range, no statistics, and rows that exist only once its arguments are
        // evaluated. It is checked before the derived plan because it has neither.
        Some(table) if outer_entry.is_some_and(|entry| entry.function.is_some()) => function_node(
            outer_entry.unwrap_or_else(|| unreachable!()),
            table,
            &Scope::empty().under(outer),
        )?,
        // Rows written into the statement, which is a source with even less to it than a function:
        // no arguments, no key range, and the row count is the length of the list.
        Some(table) if outer_entry.is_some_and(|entry| entry.values.is_some()) => {
            super::values::node(outer_entry.unwrap_or_else(|| unreachable!()), table)?
        }
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
        // The inner side may be a function too — `FROM generate_subscripts(…) i, generate_subscripts(…) j` is a join of two of them — and it is read exactly as a derived side is,
        // because both are plans rather than key ranges.
        // **Implicitly `LATERAL`.** A set-returning function on the right of a comma or a `JOIN`
        // may name the entries to its **left** — `FROM lt l, unnest(l.tags) u` is what
        // `ActiveRecord`'s schema dump writes with `generate_subscripts` — so its arguments
        // resolve in a scope of exactly those, and the rows are recomputed per outer row where the
        // join is executed. Left, and not the whole `FROM`: the same function first is
        // `42P01 missing FROM-clause entry`, measured.
        // The outer side alone, which is everything to this entry's left in a two-entry `FROM`.
        // **Under the name the `FROM` gave it**, not the table's own: `FROM lt l, unnest(l.tags)`
        // names the alias, and a scope built from the relation would answer `42P01` for `l`.
        let left = match named_table {
            Some((table, name)) => Scope::single_as(table, name.to_owned()).under(outer),
            None => Scope::empty().under(outer),
        };
        let inner_function = source_function(inner_entry, inner, &left)?;
        node = join_node(
            node,
            condition.as_ref(),
            left_join,
            &scope,
            inner,
            inner_function
                .as_ref()
                .or_else(|| inner_entry.and_then(crate::plan::TableRef::derived_plan)),
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
    // **A set-returning call in the target list moves the sort above the projection.** The rows
    // do not exist until the projection has run — `SELECT generate_series(1,3) ORDER BY 2 DESC` is
    // three rows to order and one row before it — so a sort underneath would order the input and
    // leave the generated values in generation order, which is what it did. `DISTINCT` moves it
    // for its own reason, and both then resolve their keys against the **output** columns.
    let expands = exprs.iter().any(contains_set_func);
    let sort_keys = order_keys(
        select,
        scope,
        aggregation.as_ref(),
        &exprs,
        &columns,
        select.distinct || expands,
    )?;
    refuse_json_sort(&sort_keys)?;
    if !select.distinct && !expands && !sort_keys.is_empty() {
        node = Node::Sort {
            input: Box::new(node),
            keys: sort_keys.clone(),
        };
    }

    node = Node::Project {
        input: Box::new(node),
        exprs,
    };

    if expands && !select.distinct && !sort_keys.is_empty() {
        node = Node::Sort {
            input: Box::new(node),
            keys: sort_keys.clone(),
        };
    }

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

/// `0A000` for an `ORDER BY` over a `json` or `jsonb` column.
///
/// Sorting one would compare the stored text as bytes, and that is neither type's order: a `jsonb`
/// sorts by **kind** first (`Object > Array > Boolean > Number > String > Null`) and compares
/// numbers numerically, so `null` sorts below `1.00` where its bytes sort above it. `json` has no
/// ordering operators at all on a real server. Refusing is contract C2; answering from the bytes
/// would be a wrong answer, which is what ADR 0042 is about.
fn refuse_json_sort(keys: &[SortKey]) -> Result<()> {
    for key in keys {
        if let Expr::Ordinal { ty, .. } = &key.expr
            && matches!(ty, ColumnType::Json | ColumnType::Jsonb)
        {
            return Err(SqlError::unsupported(format!(
                "ORDER BY over a {} column",
                ty.name()
            )));
        }
    }
    Ok(())
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
    columns: &[OutputColumn],
    over_output: bool,
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
                && columns.iter().filter(|output| &output.name == name).count() > 1
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
        let expr = if select.distinct || over_output {
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
pub(super) type TargetList = (Vec<OutputColumn>, Vec<Expr>);

/// `RETURNING`, resolved against one table: the output columns and the expression per column.
///
/// The same [`Scope`], the same `resolve` and the same name-and-type rules a `SELECT`'s target
/// list gets, which is what makes `RETURNING *` and `SELECT *` return the same columns in the same
/// order under the same names. An aggregate is refused here rather than resolved: there is no
/// group in a statement that writes rows, and PostgreSQL says so.
pub(super) fn returning_columns(items: &[SelectItem], table: &TableDef) -> Result<TargetList> {
    returning_columns_over(
        items,
        Some(crate::plan::TableRef::bare(table.name.clone())),
        &[],
        &Scope::single(table),
    )
}

/// The same, over the relations an `UPDATE … FROM` has in scope.
///
/// `RETURNING` there may name a column of a `FROM` relation as readily as one of the row being
/// written — they are one row by the time it is evaluated — so the scope and the `FROM` shape are
/// the caller's rather than built from a single table here.
pub(super) fn returning_columns_over(
    items: &[SelectItem],
    from: Option<crate::plan::TableRef>,
    joins: &[crate::plan::Join],
    scope: &Scope<'_>,
) -> Result<TargetList> {
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            crate::exec::subquery::refuse_in(expr, "a RETURNING list")?;
        }
    }
    let select = Select {
        // A synthetic `SELECT` for a `RETURNING` list: nothing asked to lock anything.
        locking: Vec::new(),
        from,
        ctes: Vec::new(),
        joins: joins.to_vec(),
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
    let columns = output_columns(&select, scope, None)?;
    let exprs = projection_exprs(&select, scope, None)?;
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
/// # The `WHERE` is applied as the chain is built, not once on top of it
///
/// A **comma-separated `FROM` list** has no `ON` anywhere: every condition is in the `WHERE`. Left
/// on top, that is a cross product of every relation in the list with one filter above it, and run
/// 49 is what that costs — `ActiveRecord`'s `pk_and_sequence_for` is five catalog relations in one
/// such list, and it answered in 0 s over 30 relations, 4 s over 90 and never over the suite's 870.
/// Measured here at 6 tables against 12: **362 ms against 3.92 s**, for a control that went 325 µs
/// to 472 µs.
///
/// So each conjunct of the `WHERE` is applied at the **first step whose tables can answer it**
/// ([`pushdown`]), which is a filter on the intermediate result rather than on the product. What
/// is left over — a conjunct naming a table joined later, or one this pass will not touch — stays
/// on top exactly as before.
///
/// # It reorders a comma list, and nothing else
///
/// A two-table inner join is commutative and [`plan`] exploits that; a chain with a `LEFT JOIN`
/// anywhere in it is not free to, because an outer join fixes the order of everything after it —
/// `A LEFT JOIN B ON … JOIN C ON …` keeps only the rows the inner join matches, and swapping the
/// last two steps would keep the NULL-extended ones.
///
/// A **plain comma list** — every entry a stored relation or a catalog view, every join inner,
/// every `ON` absent — has none of that, and it is the shape that needs the ordering most, because
/// with no `ON` at all *nothing* bounds it. [`comma_list_order`] puts the tables a constant can
/// pin first and grows from there. Every other chain is planned exactly as written: a rule-based
/// planner that is right everywhere beats one that is faster on the shapes nobody sends.
///
/// The cost is that the inner side of each step is probed by key only when its `ON` allows it,
/// exactly as for one join, and read whole otherwise. That is the same trade one join makes.
fn plan_chain(
    select: &Select,
    tenant: u64,
    table: Option<&TableDef>,
    inners: &[&TableDef],
    enclosing: Option<&Scope<'_>>,
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

    // **A plain comma list may be reordered**; every other chain is planned as written. The
    // permutation is over `entries`, so everything below reads the tables through it and the row
    // an ordinal names is the row the executor builds.
    let order = comma_list_order(select, &entries);
    let reordered = order.iter().enumerate().any(|(at, to)| at != *to);
    let entries: Vec<(&TableDef, String)> = order
        .iter()
        .map(|at| (entries[*at].0, entries[*at].1.clone()))
        .collect();
    // The conjuncts of the `WHERE`, each waiting for the first step that can answer it. One that
    // never becomes answerable — and one this pass will not touch — is still on top at the end.
    let mut pending: Vec<&Expr> = select.filter.as_ref().map_or_else(Vec::new, conjuncts_of);

    // The `WHERE` cannot narrow the outer access path here: with more than one table it may
    // mention any of them, and a value from a table not yet read is not one a scan can seek on.
    let mut node = if reordered {
        // Only a plain relation or a catalog view can be reordered onto the front, which is what
        // `comma_list_order` checked before returning anything but the identity.
        access_path(None, tenant, entries[0].0)?
    } else {
        match select.from.as_ref() {
            // A set-returning function is a third kind of source, beside a relation and a derived
            // table: no key range, no statistics, and rows that exist only once its arguments are
            // evaluated.
            Some(entry) if entry.function.is_some() => {
                function_node(entry, outer, &Scope::empty().under(enclosing))?
            }
            // Rows written into the statement, which is a source with even less to it than a function:
            // no arguments, no key range, and the row count is the length of the list.
            Some(entry) if entry.values.is_some() => super::values::node(entry, outer)?,
            Some(entry) => match entry.derived_plan() {
                Some(plan) => plan.clone(),
                None => access_path(None, tenant, outer)?,
            },
            None => access_path(None, tenant, outer)?,
        }
    };
    // The first table's own conjuncts, before a single pair has been built. This is the step that
    // matters most in a comma list: `seq.relkind = 'S'` here is the difference between joining
    // every relation and joining the sequences.
    node = pushdown(node, &mut pending, &entries[..1], enclosing);

    // Grown one table at a time, so each step's `ON` sees exactly the tables to its left plus the
    // one being joined — which is what makes a reference to a table two steps back resolve, and a
    // reference to one further right an "undefined column" rather than a silent NULL.
    for at in 0..entries.len() - 1 {
        let scope = Scope::chain(&entries[..=at + 1]).under(enclosing);
        let inner = entries[at + 1].0;
        // A reordered chain is a comma list: every join inner, every `ON` absent, and no entry a
        // function or a derived table — so the step needs nothing from `select.joins`, whose order
        // no longer matches.
        node = if reordered {
            join_node(node, None, false, &scope, inner, None)?
        } else {
            let join = &select.joins[at];
            let left_join = join.kind == crate::plan::JoinKind::Left;
            // Implicitly `LATERAL`: the entries strictly to this one's left, which at step `at` is
            // everything up to and including the outer side of this join.
            let left = Scope::chain(&entries[..=at]).under(enclosing);
            let inner_function = source_function(Some(&join.table), inner, &left)?;
            join_node(
                node,
                join.on.as_ref(),
                left_join,
                &scope,
                inner,
                inner_function
                    .as_ref()
                    .or_else(|| join.table.derived_plan()),
            )?
        };
        // **Not below an outer join.** A `WHERE` conjunct applied before the NULL extension would
        // throw away the rows a `LEFT JOIN` exists to keep, which is the one rewrite of this kind
        // that changes an answer rather than a cost. Once a chain has taken an outer join, nothing
        // after it is pushed either: the rows above that step are the extended ones.
        if !select.joins[..=at]
            .iter()
            .any(|join| join.kind == crate::plan::JoinKind::Left)
        {
            node = pushdown(node, &mut pending, &entries[..=at + 1], enclosing);
        }
    }

    // The **written** order for the scope every output column is resolved against: `SELECT *`
    // returns the tables in the order the user wrote them whatever order they were joined in.
    let written: Vec<usize> = (0..entries.len())
        .map(|user_at| {
            order
                .iter()
                .position(|joined| *joined == user_at)
                .unwrap_or(user_at)
        })
        .collect();
    let scope = Scope::chain_written(&entries, written).under(enclosing);
    // Whatever is left, and the whole `WHERE` for a statement nothing was pushed out of. It is
    // resolved and checked here exactly as before, so an aggregate in a `WHERE` is still the
    // `WHERE`'s error and a column nobody has is still resolved against every table.
    if !pending.is_empty() {
        let predicate = all_of(&pending);
        let resolved = resolve(&predicate, &scope)?;
        check_predicate(&resolved, "WHERE", &scope)?;
        node = Node::Filter {
            input: Box::new(node),
            predicate: resolved,
        };
    }
    finish_plan(select, node, &scope, Some(entries[0].0))
}

/// One `AND`-ed predicate as its conjuncts, in the order written.
///
/// `OR` is **not** split: `a OR b` is one condition and applying either half alone would keep rows
/// the statement excludes. Only the `AND` spine comes apart, which is what makes every piece of it
/// independently true of any row the statement returns — the whole licence pushdown runs on.
fn conjuncts_of(predicate: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut stack = vec![predicate];
    while let Some(expr) = stack.pop() {
        match expr {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                stack.push(right);
                stack.push(left);
            }
            other => out.push(other),
        }
    }
    out.reverse();
    out
}

/// The conjuncts joined back into one predicate.
fn all_of(conjuncts: &[&Expr]) -> Expr {
    let mut out = conjuncts[0].clone();
    for conjunct in &conjuncts[1..] {
        out = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(out),
            right: Box::new((*conjunct).clone()),
        };
    }
    out
}

/// Whether a conjunct may be moved below the top of the plan.
///
/// **Three shapes stay where they are**, and none of them is about correctness of the filter: an
/// aggregate belongs to a `WHERE`'s own error (`check_predicate` names the clause, and a pushed
/// copy would name `JOIN/ON` for a statement the user wrote no join in), a set-returning call in a
/// predicate is refused there too, and a subquery may be **correlated** — it is planned against the
/// scope it was resolved in, and moving it under a different one is a question this pass does not
/// answer. Leaving them on top is what the plan did for all of them before.
fn is_pushable(expr: &Expr) -> bool {
    let mut has_subquery = false;
    super::bind::descend(expr, &mut |inner| {
        has_subquery |= matches!(inner, Expr::Subquery(_));
    });
    !has_subquery && !contains_set_func(expr) && !aggregate::contains_aggregate(expr)
}

/// Applies every pending conjunct the tables built so far can answer, and keeps the rest.
///
/// The test is [`resolve`] itself: a conjunct resolves against a scope exactly when every column
/// it names is in it. **A conjunct that fails to resolve here is not an error** — it is one naming
/// a table further right, and it stays pending for a later step or for the filter on top, which is
/// where a genuinely undefined column is reported against the whole scope with the message it
/// always had. Nothing is pushed on a failure, so no error is swallowed and none is moved.
fn pushdown(
    node: Node,
    pending: &mut Vec<&Expr>,
    entries: &[(&TableDef, String)],
    enclosing: Option<&Scope<'_>>,
) -> Node {
    if pending.is_empty() {
        return node;
    }
    let scope = Scope::chain(entries).under(enclosing);
    let mut ready = Vec::new();
    pending.retain(|conjunct| {
        if !is_pushable(conjunct) {
            return true;
        }
        match resolve(conjunct, &scope) {
            Ok(resolved) => {
                ready.push(resolved);
                false
            }
            Err(_) => true,
        }
    });
    let Some(first) = ready.first() else {
        return node;
    };
    let mut predicate = first.clone();
    for next in &ready[1..] {
        predicate = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(predicate),
            right: Box::new(next.clone()),
        };
    }
    Node::Filter {
        input: Box::new(node),
        predicate,
    }
}

/// The order to join a **plain comma list** in, or the identity for every other chain.
///
/// A comma list is the shape with nothing to bound it: no `ON` anywhere, so every step is a cross
/// product until the `WHERE` is reached. Ordering it is what turns `pk_and_sequence_for` from the
/// product of five catalog relations into a walk down one row's dependencies.
///
/// **Greedy, and the rule is the one a reader can check by eye**: a table a conjunct pins on its
/// own — `dep.refobjid = '"accounts"'::regclass` — goes first, because it is one row before
/// anything is joined to it; then, repeatedly, whichever remaining table shares a conjunct with the
/// tables already chosen, which is an equijoin rather than a product; and only then, whatever is
/// left, in the order written. Ties keep the written order, so the permutation is deterministic and
/// a statement with nothing to choose between is planned exactly as it was.
///
/// It reorders **only** when every entry is a stored relation or a catalog view and every join is
/// an inner one with no `ON`. A derived table, a set-returning function or a `VALUES` list in the
/// list means the identity, because the outer node for those is built from `select.from` and a
/// different first table would not be it; an outer join means the identity because reordering
/// across one changes the answer.
fn comma_list_order(select: &Select, entries: &[(&TableDef, String)]) -> Vec<usize> {
    let identity = || (0..entries.len()).collect::<Vec<_>>();
    let plain = |entry: Option<&crate::plan::TableRef>| {
        entry.is_none_or(|entry| {
            entry.function.is_none() && entry.values.is_none() && entry.derived_plan().is_none()
        })
    };
    if entries.len() < 3
        || !plain(select.from.as_ref())
        || !select.joins.iter().all(|join| {
            join.on.is_none()
                && join.kind != crate::plan::JoinKind::Left
                && plain(Some(&join.table))
        })
    {
        return identity();
    }
    let Some(filter) = select.filter.as_ref() else {
        return identity();
    };
    let conjuncts = conjuncts_of(filter);
    // Which entries a conjunct can be answered by: the smallest prefix-free set is what matters,
    // so each conjunct is tested against every single table and against every pair with a chosen
    // one. `resolve` against a one-table scope is the whole test.
    let answered_by_one = |at: usize| {
        let scope = Scope::chain(&entries[at..=at]);
        conjuncts
            .iter()
            .any(|conjunct| is_pushable(conjunct) && resolve(conjunct, &scope).is_ok())
    };
    let mut chosen: Vec<usize> = Vec::with_capacity(entries.len());
    let mut left: Vec<usize> = (0..entries.len()).collect();
    // The seed: the first table a constant pins, or the first table written.
    let seed = left
        .iter()
        .position(|at| answered_by_one(*at))
        .unwrap_or_default();
    chosen.push(left.remove(seed));
    while !left.is_empty() {
        let next = left
            .iter()
            .position(|at| {
                let mut with: Vec<(&TableDef, String)> = chosen
                    .iter()
                    .map(|c| (entries[*c].0, entries[*c].1.clone()))
                    .collect();
                with.push((entries[*at].0, entries[*at].1.clone()));
                let scope = Scope::chain(&with);
                // A conjunct this table completes and the tables so far could not answer alone:
                // that is an equality tying it to what is already in hand.
                let smaller = Scope::chain(&with[..with.len() - 1]);
                conjuncts.iter().any(|conjunct| {
                    is_pushable(conjunct)
                        && resolve(conjunct, &scope).is_ok()
                        && resolve(conjunct, &smaller).is_err()
                })
            })
            .unwrap_or_default();
        chosen.push(left.remove(next));
    }
    chosen
}

/// The rows an `UPDATE … FROM` writes: the target's row, then whatever its `FROM` chain put
/// beside it.
///
/// [`plan_chain`] without its projection, and that is the whole difference. A statement that
/// rewrites a row needs **all** of it — the columns it is not changing still have to be written
/// back, and the index entries it is replacing were built from the old ones — so there is nothing
/// to project down to. What comes out is the concatenated shape
/// `Scope::conflicting` already describes for `ON CONFLICT … DO UPDATE`: ordinals `0..n` are the
/// row being written and everything after them is what the `SET` expressions may read.
///
/// **The `FROM` entry is joined to the target with no condition.** `UPDATE t a SET … FROM x WHERE
/// …` means `t CROSS JOIN x` with the `WHERE` doing the tying, exactly as a comma in a `SELECT`'s
/// `FROM` does — so nothing is approximated by building it as one, and a join written *inside* the
/// `FROM` keeps its own `ON`.
pub(super) fn joined_target_rows(
    tenant: u64,
    entries: &[(&TableDef, String)],
    joins: &[crate::plan::Join],
    filter: Option<&Expr>,
) -> Result<Node> {
    let Some((target, _)) = entries.first() else {
        return Err(SqlError::Internal(
            "an UPDATE ... FROM with no table to write".to_owned(),
        ));
    };
    // The same rule a `SELECT` follows: the same table may appear twice under two names — which is
    // exactly what the `update_all` shape does — so a duplicate *name* is the error and a
    // duplicate table is not.
    for (at, (_, name)) in entries.iter().enumerate() {
        if !name.is_empty() && entries[..at].iter().any(|(_, earlier)| earlier == name) {
            return Err(SqlError::DuplicateTableName(name.clone()));
        }
    }
    // The `WHERE` cannot narrow the target's access path: it may name any of the relations, and a
    // value from one not yet read is not one a scan can seek on.
    let mut node = access_path(None, tenant, target)?;
    // Grown one relation at a time, so each step's `ON` sees the relations to its left plus the
    // one being joined — the same left-deep shape and the same reason as `plan_chain`.
    for (at, join) in joins.iter().enumerate() {
        let Some((inner, _)) = entries.get(at + 1) else {
            return Err(SqlError::Internal(
                "an UPDATE ... FROM join with no relation".to_owned(),
            ));
        };
        let scope = Scope::chain(&entries[..=at + 1]);
        let left = Scope::chain(&entries[..=at]);
        let inner_source = source_function(Some(&join.table), inner, &left)?;
        node = join_node(
            node,
            join.on.as_ref(),
            join.kind == crate::plan::JoinKind::Left,
            &scope,
            inner,
            inner_source.as_ref().or_else(|| join.table.derived_plan()),
        )?;
    }
    if let Some(filter) = filter {
        let scope = Scope::chain(entries);
        let predicate = resolve(filter, &scope)?;
        check_predicate(&predicate, "WHERE", &scope)?;
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }
    Ok(node)
}

/// Whether any subquery in this statement turned out to be correlated.
///
/// Read after `crate::exec::subquery::plan_subqueries` has planned them, which is where the answer
/// is decided — before that every `correlated` is still `false`.
fn has_correlated_subquery(select: &Select) -> bool {
    fn in_expr(expr: &Expr) -> bool {
        match expr {
            Expr::Subquery(sub) => sub.correlated || sub.operand.as_deref().is_some_and(in_expr),
            Expr::Binary { left, right, .. } => in_expr(left) || in_expr(right),
            Expr::Not(inner) => in_expr(inner),
            Expr::IsNull { operand, .. } => in_expr(operand),
            Expr::InList { operand, list, .. } => in_expr(operand) || list.iter().any(in_expr),
            Expr::Aggregate(call) => call.args.iter().any(in_expr),
            _ => false,
        }
    }
    let projected = select.projection.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => in_expr(expr),
        _ => false,
    });
    projected
        || select
            .joins
            .iter()
            .any(|join| join.on.as_ref().is_some_and(in_expr))
        || select
            .filter
            .iter()
            .chain(&select.having)
            .chain(&select.group_by)
            .any(in_expr)
        || select.order_by.iter().any(|item| in_expr(&item.expr))
}

/// The scope a statement's subqueries are planned under: every table it names, **in the order
/// written**.
///
/// Written order matters and is not free: [`drive_from`] may swap a two-table join, which changes
/// where each column sits in the row — and an [`crate::plan::Expr::Outer`] is a *position* in that
/// row. So a statement with a correlated subquery in it does not swap (`plan`'s `swappable`), and
/// this builds the layout that decision guarantees.
pub(super) fn written_scope<'a>(
    select: &Select,
    table: Option<&'a TableDef>,
    inners: &'a [&'a TableDef],
) -> Scope<'a> {
    let Some(table) = table else {
        return Scope::empty();
    };
    let named = |entry: &crate::plan::TableRef| entry.referred_as().to_owned();
    let left = select.from.as_ref().map(named).unwrap_or_default();
    // One join is `Scope::joined` rather than a two-entry chain, because only that carries a
    // `USING` merge -- and without it a bare reference to a merged column would be `42702` from a
    // subquery where the statement itself resolves it.
    if let ([inner], [join]) = (inners, select.joins.as_slice()) {
        return Scope::joined(
            (table, &left),
            (*inner, &named(&join.table)),
            false,
            &join.using,
        );
    }
    let mut entries: Vec<(&TableDef, String)> = vec![(table, left)];
    for (join, inner) in select.joins.iter().zip(inners) {
        entries.push((*inner, named(&join.table)));
    }
    Scope::chain(&entries)
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
        .find(|index| {
            // An expression index has no column key to match, so `key_columns` is `None` and it
            // is never the unique index a join probe follows (`catalog::IndexDef::key_columns`).
            index.unique && index.key_columns().is_some_and(|key| key == [inner_at])
        })
        .map(|index| crate::plan::Probe::UniqueIndex {
            index_id: index.id,
            index_name: index.name.clone(),
            outer: outer_at,
            primary_key_types: RowSchema::nullable(inner.primary_key_types()),
        })
}

/// The plan for a `FROM` entry that is neither a relation nor a derived table — a set-returning
/// function or a `VALUES` list — or `None` when it is one of those.
fn source_function(
    entry: Option<&crate::plan::TableRef>,
    def: &TableDef,
    left: &Scope<'_>,
) -> Result<Option<Node>> {
    match entry {
        Some(entry) if entry.function.is_some() => function_node(entry, def, left).map(Some),
        // A `VALUES` list is the fourth kind of source and the simplest: its rows are in the
        // statement, so there is nothing to resolve against the enclosing scope.
        Some(entry) if entry.values.is_some() => super::values::node(entry, def).map(Some),
        _ => Ok(None),
    }
}

/// A set-returning function standing where a relation does.
///
/// **Its arguments are resolved against the enclosing scope and nothing else.** A function in
/// `FROM` cannot see the rows it is itself producing, so the scope it resolves in is empty of
/// local tables — which is what turns `generate_subscripts(c.conkey, 1)` inside a correlated
/// subquery into an `Outer` reference that the row above supplies, and what makes a reference to
/// a table of this same `FROM` an "undefined column" rather than a silent NULL.
fn function_node(entry: &crate::plan::TableRef, def: &TableDef, scope: &Scope<'_>) -> Result<Node> {
    let mut call = entry.function.clone().unwrap_or_else(|| {
        Box::new(crate::plan::TableFunction {
            name: String::new(),
            args: Vec::new(),
            def: None,
        })
    });
    // **The entries to this one's left are the current scope, not an enclosing one.** A reference
    // resolved through `under` becomes an `Expr::Outer` — a correlated reference the subquery
    // machinery supplies — and there is no subquery here: the value comes from the outer row the
    // nested loop is holding, which is an ordinary `Ordinal` into it. Resolving it the other way
    // reached the row evaluator as "an outer reference … 1 scope out".
    for arg in &mut call.args {
        *arg = resolve(arg, scope)?;
    }
    Ok(Node::TableFunction {
        call,
        columns: def.row_schema(),
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
    // A sequence's relation has no rows of its own: its one row is the counter, read at open.
    if let Some(sequence_id) = crate::catalog::sequence_of_relation(table.id) {
        return Ok(Node::SequenceRead {
            sequence_id,
            state: None,
            columns,
        });
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
        // **A partial index is never read.** Choosing one is correct only when the query's own
        // predicate implies the index's, and this crate has no implication prover — picking it
        // otherwise would answer with the rows the index happens to hold, which is a correct
        // looking query missing rows, the same anomaly `readable()` above exists to prevent. It
        // still enforces its `UNIQUE`; it just never narrows a scan.
        if index.predicate.is_some() {
            continue;
        }
        if !index.unique {
            continue;
        }
        // **An expression index is never read**, for the reason a partial one is not: there is
        // no constant in the `WHERE` to pin an expression to, and pinning it to the column
        // underneath would be answering `WHERE b = 'X'` from an index on `lower(b)`.
        let Some(key_columns) = index.key_columns() else {
            continue;
        };
        if let Some(key) = pinned(&key_columns, &equalities)
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
        inherited: table.child_scans.clone(),
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
        inherited: table.child_scans.clone(),
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

/// Resolves a column reference against the relations an `UPDATE` has in scope — the row being
/// written, and whatever its `FROM` put beside it.
///
/// One entry point for both, because `SET body = c.body || '!'` is the same resolution as
/// `SET body = body || '!'` against a scope with one more table in it.
pub(super) fn resolve_against_scope(expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
    crate::exec::subquery::refuse_in(expr, "an UPDATE assignment")?;
    resolve(expr, scope)
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
        // **A tombstone is not one of the two.** `DROP COLUMN gone` keeps the column's slot and its
        // stored name, so a later `ADD COLUMN gone` puts a second `gone` in this list — and
        // counting it here made a re-added column `42702` on a real server's ordinary sequence of
        // migrations (ADR 0051). `TableDef::column` already skips it; this is the second place
        // that reads the raw list by name, and the one the compiler cannot point at.
        .filter(|column| column.name == name && !column.dropped)
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

#[allow(
    clippy::too_many_lines,
    reason = "one arm per expression shape; splitting it would hide the vocabulary rather than clarify it"
)]
pub(super) fn resolve(expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
    Ok(match expr {
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            escape,
        } => Expr::Like {
            operand: Box::new(resolve(operand, scope)?),
            pattern: Box::new(resolve(pattern, scope)?),
            negated: *negated,
            case_insensitive: *case_insensitive,
            escape: *escape,
        },
        Expr::RegexMatch {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => Expr::RegexMatch {
            operand: Box::new(resolve(operand, scope)?),
            pattern: Box::new(resolve(pattern, scope)?),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        // The strip is decided here, where the operand's type is still known.
        Expr::Scalar { func, operand } => Expr::Scalar {
            func: *func,
            operand: Box::new(resolve(operand, scope)?),
        },
        Expr::ToText { operand, .. } => {
            let operand = resolve(operand, scope)?;
            let strip_blanks = matches!(expr_type(&operand, scope), Ok(ColumnType::Bpchar));
            // The operand's output function, where the operand is an enum column: the label, not
            // the ordinal the row holds.
            let enum_labels = match &operand {
                Expr::Ordinal { at, .. } => match scope.user_type_at(*at).map(|def| &def.kind) {
                    Some(crate::catalog::TypeKind::Enum { labels }) => Some(labels.clone()),
                    _ => None,
                },
                _ => None,
            };
            Expr::ToText {
                operand: Box::new(operand),
                strip_blanks,
                enum_labels,
            }
        }
        Expr::Column { table, name } => {
            let (level, at, column) = scope.lookup(table.as_deref(), name)?;
            if level == 0 {
                Expr::Ordinal {
                    at,
                    ty: column.ty,
                    typmod: column.typmod,
                }
            } else {
                Expr::Outer {
                    level,
                    at,
                    ty: column.ty,
                    typmod: column.typmod,
                }
            }
        }
        Expr::Binary { op, left, right } => {
            let (left, right) = (resolve(left, scope)?, resolve(right, scope)?);
            // **An enum is reconciled before the ordinary rule, and by a different one.** The
            // column is an `int2` in the row, so the ordinary rule would read `'sad'` as a
            // smallint and answer `22P02 invalid input syntax for type smallint`. See
            // [`reconcile_enum`] for the three answers a real server gives here.
            if let Some((left, right)) = reconcile_enum(*op, &left, &right, scope)? {
                return Ok(Expr::Binary {
                    op: *op,
                    left: Box::new(left),
                    right: Box::new(right),
                });
            }
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
        } => resolve_in_list(operand, list, *negated, scope)?,
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, scope)?),
            negated: *negated,
        },
        // Both sides resolve, and **neither is reconciled against the other**: an array's elements
        // have no type of their own here — they are text in the catalog — so what gives them one
        // is the operand, at evaluation, exactly as the plan-time form types its list against the
        // operand it is compared with.
        Expr::AnyArray { operand, array } => {
            let operand = resolve(operand, scope)?;
            let array = resolve(array, scope)?;
            // **An array that knows its element type is not `unknown`.** `'{1,2}'` is an unknown
            // literal and takes the operand's type at evaluation, which is the arm below; an
            // `ARRAY['1','2']` is a `text[]` **value** — `pg_typeof` says so on a real server —
            // and `id = ANY(...)` of one is `42883 operator does not exist: bigint = text`,
            // naming the *element* type. Without this the evaluator read each element at the
            // operand's type and answered a row, which is a wrong row set rather than a wrong
            // error.
            if let Some(element) = expr_type(&array, scope)
                .ok()
                .and_then(esker_keys::array::ArrayValue::element_of)
                && let Ok(left) = expr_type(&operand, scope)
                && !same_family(left, element)
            {
                return Err(SqlError::UndefinedOperator {
                    left: left.name().to_owned(),
                    op: "=",
                    right: element.name().to_owned(),
                });
            }
            Expr::AnyArray {
                operand: Box::new(operand),
                array: Box::new(array),
            }
        }
        // **The element type comes from the array where the array knows it.** An array is text
        // here and its elements normally take their type from what they are compared against
        // (`retype_subscript`) — but that rule needs the comparison to be *in the same statement*,
        // and the schema dump puts the subscript inside a derived table and the comparison
        // outside it. `pg_constraint.conkey` is an `int2vector` on a real server, so its elements
        // are `int2` wherever they are read, and typing them here is what lets
        // `a.attnum = indexed_conkeys.conkey_elem` match instead of comparing an `int2` to a
        // `Datum::Text` and finding nothing.
        // Its operands are ordinary expressions and its **type is settled here**, where the scope
        // that gives each column a type is in hand. Falling through to the clone below would leave
        // the operands as `Expr::Column` and the evaluator would report them as having reached it
        // unresolved — the same trap `Expr::CatalogFunc` documents below.
        Expr::Negate(operand) => Expr::Negate(Box::new(resolve(operand, scope)?)),
        Expr::Arithmetic {
            op, left, right, ..
        } => resolve_arithmetic(*op, left, right, scope)?,
        Expr::Subscript {
            operand,
            index,
            element,
        } => Expr::Subscript {
            operand: Box::new(resolve(operand, scope)?),
            index: Box::new(resolve(index, scope)?),
            element: attnum_vector_element(operand, scope).unwrap_or(*element),
        },
        Expr::Case {
            branches,
            otherwise,
        } => resolve_case(branches, otherwise.as_deref(), scope)?,
        Expr::Coalesce(args) => resolve_coalesce(args, scope)?,
        // Its arguments are expressions of the row like a catalog function's — `unnest(tags)` is a
        // column reference — so they resolve the same way. Falling through to the clone below left
        // them as `Expr::Column` and the evaluator reported one as having reached it unresolved.
        Expr::SetFunc(call) => {
            let mut resolved = call.clone();
            for arg in &mut resolved.args {
                *arg = resolve(arg, scope)?;
            }
            Expr::SetFunc(resolved)
        }
        // Its arguments are ordinary expressions of the row — `format_type(a.atttypid,
        // a.atttypmod)` is two column references — so they resolve like any others. Falling
        // through to the clone below would leave them as `Expr::Column` and the evaluator would
        // report them as having reached it unresolved.
        Expr::CatalogFunc(call) => {
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(resolve(arg, scope)?);
            }
            // **`pg_typeof` of an enum column is folded here, where the type has a name.** The
            // evaluator reads a `Datum`'s own type and a `Datum` is an `int2`, so it would answer
            // `smallint` — the storage, which is the one thing about an enum a client must not be
            // told, and a *wrong value* rather than a refusal (ADR 0031's worst class). The name is
            // known at plan time and nowhere else, so this is where it is answered.
            match (call.func, args.first()) {
                (crate::plan::CatalogFunc::PgTypeof, Some(Expr::Ordinal { at, .. }))
                    if args.len() == 1 =>
                {
                    match scope.user_type_at(*at) {
                        Some(def) => Expr::Literal(Literal::String(def.name.clone())),
                        None => Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                            func: call.func,
                            args,
                        })),
                    }
                }
                // **`hstore[]` alone still needs the static type**: an array's elements carry
                // theirs and the array does not, so the value cannot say which array it is.
                // `hstore` itself no longer needs this — `Datum::Hstore` says so — and neither
                // does `citext`, which is why the list is one entry rather than three.
                (crate::plan::CatalogFunc::PgTypeof, Some(arg)) if args.len() == 1 => {
                    match expr_type(arg, scope) {
                        Ok(ty @ ColumnType::HstoreArray) => {
                            Expr::Literal(Literal::String(ty.name().to_owned()))
                        }
                        _ => Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                            func: call.func,
                            args,
                        })),
                    }
                }
                _ => Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                    func: call.func,
                    args,
                })),
            }
        }
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

/// `x IN (a, b)`, resolved: every element typed, and the pair checked for an operator.
///
/// The list is typed as a **whole** and not pairwise, which is a rule of its own — see the comments
/// inside.
fn resolve_in_list(
    operand: &Expr,
    list: &[Expr],
    negated: bool,
    scope: &Scope<'_>,
) -> Result<Expr> {
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
    // **An enum operand types the whole list before the common-type rule sees it**, for the reason
    // `reconcile_enum` exists: the column is an `int2` in the row, so `select_common_type` would
    // read `'sad'` as a smallint and answer `22P02 invalid input syntax for type smallint`. Each
    // item is reconciled against the operand on its own, which is what `x IN (a, b)` means — a set
    // of `=` — and each gives the same three answers a single `=` gives (ADR 0050).
    if let Expr::Ordinal { at, .. } = &operand
        && scope.user_type_at(*at).is_some()
    {
        let mut coerced = Vec::with_capacity(items.len());
        for item in items {
            match reconcile_enum(BinaryOp::Eq, &operand, &item, scope)? {
                Some((_, right)) => coerced.push(right),
                None => coerced.push(item),
            }
        }
        return Ok(Expr::InList {
            operand: Box::new(operand),
            list: coerced,
            negated,
        });
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
    Ok(Expr::InList {
        operand: Box::new(operand),
        list: resolved,
        negated,
    })
}

/// One `CASE`, resolved: its conditions checked for `boolean` and its branches given one type.
///
/// **The `ELSE` is resolved first**, which is not a stylistic ordering. PostgreSQL builds the list
/// it runs `select_common_type` over as `[ELSE, THEN₁, THEN₂, …]` — `transformCaseExpr` conses the
/// default onto the front — and the error message names the types **in that order**. Measured, over
/// a `bigint` column and a `text` one: `CASE WHEN true THEN id ELSE name END` is
/// `CASE types text and bigint cannot be matched` and `THEN name ELSE id` is the same sentence with
/// the two swapped. Resolving left to right would name them the wrong way round in both.
fn resolve_case(
    branches: &[crate::plan::CaseBranch],
    otherwise: Option<&Expr>,
    scope: &Scope<'_>,
) -> Result<Expr> {
    let mut otherwise = match otherwise {
        Some(expr) => Some(Box::new(resolve(expr, scope)?)),
        None => None,
    };
    let mut resolved = Vec::with_capacity(branches.len());
    for branch in branches {
        let when = resolve(&branch.when, scope)?;
        // A condition that is not boolean is `42804` here rather than a row that quietly
        // never matches. An **`unknown`** condition is left alone, and that is not a
        // detail: `CASE WHEN NULL THEN 'a' ELSE 'b' END` is `b` on a real server, because
        // a bare NULL takes the type it is used at — and `expr_type` calls a NULL `text`,
        // which would refuse it here. `WHEN 'x'` is left for the same reason, and reaches
        // the evaluator's own `42804`.
        if !matches!(when, Expr::Literal(Literal::Null | Literal::String(_)))
            && let Ok(ty) = expr_type(&when, scope)
            && ty != ColumnType::Bool
        {
            return Err(SqlError::DatatypeMismatch(format!(
                "argument of CASE/WHEN must be type boolean, not type {}",
                ty.name()
            )));
        }
        resolved.push(crate::plan::CaseBranch {
            when,
            then: resolve(&branch.then, scope)?,
        });
    }
    // The result type, over the same list and in the same order. The first branch with a
    // type decides it; a later one whose type is in a different family is `42804`, and an
    // `unknown` one is **converted** rather than refused — `ELSE 'x'` against a `bigint`
    // is `22P02 invalid input syntax for type bigint`, which is what `give_type` raises.
    let results = otherwise
        .iter()
        .map(AsRef::as_ref)
        .chain(resolved.iter().map(|branch| &branch.then));
    let mut common = None;
    for result in results {
        let Some(ty) = branch_type(result, scope) else {
            continue;
        };
        match common {
            None => common = Some(ty),
            Some(chosen) if same_family(chosen, ty) => {}
            Some(chosen) => {
                // The **resolved** type first and the offending one second, which is the
                // order the list is walked in and therefore the order PostgreSQL names
                // them: `THEN id ELSE name` is `CASE types text and bigint`, because the
                // `ELSE` is the head of the list and `text` is what it settled on first.
                return Err(SqlError::DatatypeMismatch(format!(
                    "CASE types {} and {} cannot be matched",
                    chosen.name(),
                    ty.name()
                )));
            }
        }
    }
    if let Some(ty) = common {
        if let Some(expr) = &mut otherwise {
            give_branch_type(expr, ty)?;
        }
        for branch in &mut resolved {
            give_branch_type(&mut branch.then, ty)?;
        }
    }
    Ok(Expr::Case {
        branches: resolved,
        otherwise,
    })
}

/// One `COALESCE`, resolved: its arguments given one type.
///
/// **The same rules a `CASE`'s results follow**, because they are the same rules on a real server —
/// `select_common_type` over the argument list — and only two things differ. The list is walked
/// **left to right**, where a `CASE`'s starts with its `ELSE`; and the message names `COALESCE`.
/// Both measured: `COALESCE(1, 'x'::text)` is `42804 COALESCE types integer and text cannot be
/// matched`, while `COALESCE(1, 'notanumber')` — an *unknown* rather than a typed argument — is
/// `22P02 invalid input syntax for type integer`, because an unknown is **coerced** to the common
/// type rather than compared with it. Same pair of arguments, two different failures.
fn resolve_coalesce(args: &[Expr], scope: &Scope<'_>) -> Result<Expr> {
    let mut resolved = Vec::with_capacity(args.len());
    for arg in args {
        resolved.push(resolve(arg, scope)?);
    }
    let mut common = None;
    for arg in &resolved {
        let Some(ty) = branch_type(arg, scope) else {
            continue;
        };
        match common {
            None => common = Some(ty),
            // **The wider of the two, not the first.** `COALESCE(1, 2.5)` is `numeric` on a real
            // server — the integer is promoted — so the common type is taken from the same
            // promotion table arithmetic uses (ADR 0046) rather than from whichever argument came
            // first. A pair with no promotion between them keeps the family test's answer.
            Some(chosen) if same_family(chosen, ty) => {
                common = Some(
                    crate::value::arith::result_type(crate::plan::ArithOp::Add, chosen, ty)
                        .unwrap_or(chosen),
                );
            }
            Some(chosen) => {
                return Err(SqlError::DatatypeMismatch(format!(
                    "COALESCE types {} and {} cannot be matched",
                    chosen.name(),
                    ty.name()
                )));
            }
        }
    }
    if let Some(ty) = common {
        for arg in &mut resolved {
            give_branch_type(arg, ty)?;
        }
    }
    Ok(Expr::Coalesce(resolved))
}

/// Whether an expression holds a set-returning call anywhere inside it.
fn contains_set_func(expr: &Expr) -> bool {
    let mut found = false;
    super::bind::descend(expr, &mut |expr| {
        found |= matches!(expr, Expr::SetFunc(_));
    });
    found
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
            left: left.name().to_owned(),
            op: op.symbol(),
            right: ty.name().to_owned(),
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
pub(crate) fn same_family(left: ColumnType, right: ColumnType) -> bool {
    // **`json` compares with nothing, including another `json`.** Measured:
    // `'{"a":1}'::json = '{"a":1}'::json` is `42883 operator does not exist: json = json` -- the
    // type has no equality operator at all, which is a property of it rather than a gap, and is
    // why `json` cannot be a key, `DISTINCT`ed or grouped either. So this is checked before the
    // families, because a family test says "the same type compares with itself" and here that is
    // the case PostgreSQL refuses.
    fn family(ty: ColumnType) -> u8 {
        match ty {
            // **A family of one each.** `'{1}'::int[] = '{1}'::int8[]` is `42883` on a real
            // server — an array's comparison is its element type's, and two element types are two
            // operators — so no two of these share a family and none shares one with a scalar.
            ColumnType::Int8Array => 20,
            ColumnType::Int4Array => 21,
            ColumnType::Int2Array => 25,
            ColumnType::NumericArray => 22,
            ColumnType::TextArray => 23,
            ColumnType::HstoreArray => 26,
            ColumnType::Int8
            | ColumnType::Int4
            | ColumnType::Int2
            | ColumnType::Double
            | ColumnType::Real
            // **A number**, and in the same family as the rest: `numeric = int4`, `numeric >
            // int8` and `numeric = float8` are all real operators on a real server, and
            // `pg_cmp` has an arm for each pairing. Keeping it apart would refuse `WHERE n > 0`,
            // which is the commonest thing anybody writes about a decimal column.
            // An `oid` is a number and compares with the integers: `26::oid = 26` is `t`.
            | ColumnType::Oid
            | ColumnType::Numeric => 0,
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => 1,
            ColumnType::Bool => 2,
            ColumnType::Bytea => 3,
            // A `date` is in the datetime family, not one of its own: `'2020-01-01'::date =
            // '2020-01-01'::timestamp` is `t` on a real server, and `pg_cmp` promotes the day to
            // the midnight it names to answer it. `date = integer` is `42883` there, which is what
            // keeping it out of family 0 says.
            ColumnType::TimestampTz | ColumnType::Timestamp | ColumnType::Date => 4,
            // `jsonb` **is** ordered -- `=`, `<>` and `<` all work and `ORDER BY` sorts by it --
            // and it is its own family: `json = jsonb` is `42883` like everything else about
            // `json`, and there is no implicit cast between `jsonb` and `text`. Measured.
            ColumnType::Jsonb => 5,
            // A family of its own: an hstore compares only with an hstore.
            ColumnType::Hstore => 27,
            // Its own family: a citext compares only with a citext and with an `unknown`.
            ColumnType::Citext => 28,
            // A family each: a range compares only with a range of the same subtype.
            ColumnType::TsRange => 29,
            ColumnType::TstzRange => 30,
            ColumnType::Int4Range => 31,
            ColumnType::TsRangeArray => 32,
            // **Measured, one pairing at a time**: `text[] = varchar[]`, `date[] =
            // timestamp[]`, `int4[] = int8[]` and `bpchar[] = text[]` are every one of them
            // `42883` on a real server — even where the *element* types compare. So the rule
            // above holds for all sixteen: an array's comparison is its element type's, and
            // two element types are two operators.
            ColumnType::BoolArray => 33,
            ColumnType::ByteaArray => 34,
            ColumnType::BpcharArray => 35,
            ColumnType::VarcharArray => 36,
            ColumnType::DateArray => 37,
            ColumnType::TimeArray => 38,
            ColumnType::TimestampArray => 39,
            ColumnType::TimestampTzArray => 40,
            ColumnType::IntervalArray => 41,
            ColumnType::RealArray => 42,
            ColumnType::DoubleArray => 43,
            ColumnType::UuidArray => 44,
            ColumnType::JsonArray => 45,
            ColumnType::JsonbArray => 46,
            ColumnType::OidArray => 47,
            ColumnType::CitextArray => 48,
            // **A family of one, and not the datetime family.** A `date` joins `timestamp`
            // because `date = timestamp` is a real operator; a `time` does not, because
            // `time = timestamp` and `time = date` are both `42883 operator does not exist` on
            // 19beta1 — measured, because putting it in family 4 by analogy would answer where a
            // real server raises, which is ADR 0031's worst class.
            ColumnType::Time => 6,
            // Its own family too: `uuid = text` and `uuid = integer` are both `42883` on a real
            // server, and its only comparisons are with another uuid.
            ColumnType::Uuid => 7,
            // Its own family: `interval = integer` is `42883` on a real server, and an interval
            // compares with another interval and with nothing else here.
            ColumnType::Interval => 8,
            // Unreachable: returned above, and kept as an arm rather than a `_` so that the next
            // type added here is a compile error rather than a silent family 9.
            ColumnType::Json => 9,
        }
    }
    // **And `json[]` with it.** `ARRAY['{"a":1}'::json] = ARRAY['{"a":1}'::json]` is
    // `42883 could not identify an equality operator for type json` — a different sentence
    // from the one above and the same reason: an array's equality is its element's, and
    // `json` has none to lend.
    if matches!(left, ColumnType::Json | ColumnType::JsonArray)
        || matches!(right, ColumnType::Json | ColumnType::JsonArray)
    {
        return false;
    }
    family(left) == family(right)
}

/// A comparison with a column declared as an **enum**, or `None` when neither side is one.
///
/// Three answers, all measured on 19beta1 against `mood` = `('sad','ok','happy')`:
///
/// * `current_mood = 'sad'` — an **unquoted** literal is `unknown` and is coerced to the enum, so
///   it becomes the label's ordinal and the comparison is between two `int2`s. That is what makes
///   the ordering and the equality the ordinal's, which is the whole of ADR 0050;
/// * `current_mood = 'sad'::text` is **`42883 operator does not exist: mood = text`** — a `text`
///   is not an `unknown` and there is no operator between the two. The distinction is the one
///   thing about this that reasoning gets backwards, because both spellings look like strings;
/// * `current_mood = 1` is the same `42883` naming `integer`, and a label nobody declared is
///   `22P02 invalid input value for enum mood: "angry"`.
///
/// A NULL on either side is left alone: a comparison with one is NULL whatever the types are.
fn reconcile_enum(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    scope: &Scope<'_>,
) -> Result<Option<(Expr, Expr)>> {
    if !op.is_comparison() {
        return Ok(None);
    }
    let enum_of = |expr: &Expr| match expr {
        Expr::Ordinal { at, .. } => scope.user_type_at(*at),
        _ => None,
    };
    let (def, other, flipped) = match (enum_of(left), enum_of(right)) {
        (Some(def), None) => (def, right, false),
        (None, Some(def)) => (def, left, true),
        // Neither side is one, or **both are**: two ordinals compare as they stand, and the
        // ordinary path is already right for them.
        _ => return Ok(None),
    };
    let coerced = match other {
        // Still nothing, whatever the type it was written with.
        Expr::Literal(Literal::Null | Literal::TypedNull(_)) => return Ok(None),
        // The `unknown` literal, and the only spelling that is coerced.
        Expr::Literal(Literal::String(text)) => {
            match crate::catalog::enum_ordinal(enum_labels(def)?, text) {
                Some(ordinal) => Expr::Literal(Literal::Typed(Box::new(Datum::Int2(ordinal)))),
                None => {
                    return Err(SqlError::InvalidEnumValue {
                        ty: def.name.clone(),
                        value: text.clone(),
                    });
                }
            }
        }
        // Anything with a type of its own, including a cast that folded to one.
        other => {
            let named = expr_type(other, scope)
                .map_or_else(|_| "unknown".to_owned(), |ty| ty.name().to_owned());
            let (left, right) = if flipped {
                (named, def.name.clone())
            } else {
                (def.name.clone(), named)
            };
            return Err(SqlError::UndefinedOperator {
                left,
                op: op.symbol(),
                right,
            });
        }
    };
    Ok(Some(if flipped {
        (coerced, right.clone())
    } else {
        (left.clone(), coerced)
    }))
}

/// An enum type's labels, or the internal error of a column carrying a type that is not one.
fn enum_labels(def: &crate::catalog::TypeDef) -> Result<&[String]> {
    match &def.kind {
        crate::catalog::TypeKind::Enum { labels } => Ok(labels),
        _ => Err(SqlError::Internal(
            "a column carrying a user type that is not an enum reached a comparison".to_owned(),
        )),
    }
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
        // **A subscript takes the other side's type**, which is the same rule an `= ANY`'s
        // elements follow and for the same reason: an array is text here, so its elements have no
        // type of their own and what gives them one is what they are compared against. Without
        // this, `a.attnum = d.indkey[0]` compares an `int2` to a `Datum::Text` and finds nothing —
        // an **empty join** rather than an error, which is the failure that looks like data.
        (Expr::Subscript { .. }, Expr::Ordinal { ty, .. }) => {
            (retype_subscript(&left, *ty), right.clone())
        }
        (Expr::Ordinal { ty, .. }, Expr::Subscript { .. }) => {
            (left.clone(), retype_subscript(&right, *ty))
        }
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
        // **Two literals that both have a type, and no operator between them.** The column case
        // is checked where a literal meets an `Ordinal`; this is the same rule for the case where
        // neither side is a column, and without it `'2020-01-01'::date = 1` compares a `Datum` to
        // a `Datum`, falls through `pg_cmp`'s cross-variant order and answers **`f`** — a value
        // where a real server raises, which is the worst class ADR 0031 ranks.
        (Expr::Literal(left_literal), Expr::Literal(right_literal)) => {
            match (literal_type(left_literal), literal_type(right_literal)) {
                (Some(a), Some(b)) if !same_family(a, b) => {
                    return Err(SqlError::UndefinedOperator {
                        left: a.name().to_owned(),
                        op: op.symbol(),
                        right: b.name().to_owned(),
                    });
                }
                _ => (left, right),
            }
        }
        // **Two columns whose types have no operator between them.** The same rule one line up,
        // for the case where neither side is a literal: `WHERE id = title` over a `bigint` and a
        // `text` is `42883 operator does not exist: bigint = text` on a real server, and this node
        // compared two `Datum`s of different variants, found none equal and answered **no rows**.
        // A query that silently finds nothing is what a suite reports as a wrong count rather than
        // as an error, which is why it ranks worse than a refusal.
        //
        // The message names the two types **in the order they were written** — `s = id` is
        // `text = bigint` — because they are two different operators that both do not exist.
        _ if matches!(
            (carried_type(&left), carried_type(&right)),
            (Some(a), Some(b)) if !same_family(a, b)
        ) =>
        {
            let (Some(a), Some(b)) = (carried_type(&left), carried_type(&right)) else {
                unreachable!("both types are Some in the guard above")
            };
            return Err(SqlError::UndefinedOperator {
                left: a.name().to_owned(),
                op: op.symbol(),
                right: b.name().to_owned(),
            });
        }
        _ => (left, right),
    })
}

/// One subscript, told which type to read its element as.
/// `int2` when this expression is one of the catalog's attnum vectors, or `None`.
///
/// The three of them — `pg_index.indkey`, `pg_constraint.conkey`, `pg_constraint.confkey` — are
/// `int2vector` and `int2[]` on a real server and `text` here, which is the trade every
/// `pg_catalog` column makes. Their **elements** are attnums either way, and that is a fact about
/// the relation rather than about the column's storage, so it is read from the relation.
fn attnum_vector_element(operand: &Expr, scope: &Scope<'_>) -> Option<ColumnType> {
    // **A real array knows its own element type**, so a subscript of one needs no rule: the
    // catalog's text vectors below are the case that does, because their element type is a fact
    // about the relation rather than about the value.
    if let Ok(ty) = expr_type(operand, scope)
        && let Some(element) = esker_keys::array::ArrayValue::element_of(ty)
    {
        return Some(element);
    }
    let Expr::Column { table, name } = operand else {
        return None;
    };
    let relation = scope.relation_of(table.as_deref(), name)?;
    let vector = match relation.name.as_str() {
        "pg_index" => name == "indkey",
        "pg_constraint" => name == "conkey" || name == "confkey",
        _ => false,
    };
    vector.then_some(ColumnType::Int2)
}

/// The type an expression **carries in the node**, without a scope to resolve it against.
///
/// [`reconcile`] has no scope — it is given two already-resolved expressions — so it can only ask
/// the ones that hold their own type. That is enough for the case it exists for: a column
/// (`Ordinal`), a correlated reference (`Outer`), an arithmetic result and a per-row cast to
/// `text`, which is every shape a cross-type comparison reached it as in the capture. Anything
/// else answers `None` and falls through, because a rule that guessed here would refuse a
/// statement a real server answers.
fn carried_type(expr: &Expr) -> Option<ColumnType> {
    match expr {
        Expr::Ordinal { ty, .. } | Expr::Outer { ty, .. } => Some(*ty),
        Expr::Arithmetic { ty, .. } => *ty,
        Expr::ToText { .. } => Some(ColumnType::Text),
        // **An `unknown` stops being one the moment it passes through a constructor.** Measured,
        // one constructor at a time: `pg_typeof((SELECT '1'))`, `pg_typeof(CASE WHEN true THEN
        // '1' ELSE '2' END)` and `pg_typeof(COALESCE('1','2'))` are each **`text`** on a real
        // server, and `id = ` any of them is `42883 operator does not exist: bigint = text`. The
        // coercion that makes `id = '1'` work does *not* reach through them: it applies to the
        // literal itself, and a constructor over literals is a value of a settled type.
        //
        // Without this each of the three compared a `Datum` to a `Datum`, found none equal and
        // answered **no rows** — a query that silently finds nothing, which a suite reports as a
        // wrong count rather than as an error and which is the worst class ADR 0031 ranks. It is
        // the same failure `tests/operator_types.rs` was written for, one shape further out.
        Expr::Subquery(sub) => Some(sub.value_type()),
        // The branches' common type, and `text` when no branch has one — PostgreSQL's own
        // `select_common_type` fallback, and the reason `COALESCE(NULL, '1')` is `text` too: a
        // NULL carries no type either, so a `COALESCE` of a NULL and an `unknown` is all-unknown.
        Expr::Coalesce(args) => Some(branch_common_type(args.iter())),
        Expr::Case {
            branches,
            otherwise,
        } => Some(branch_common_type(
            otherwise
                .iter()
                .map(AsRef::as_ref)
                .chain(branches.iter().map(|branch| &branch.then)),
        )),
        _ => None,
    }
}

/// The type a `CASE` or a `COALESCE` settles on: the first branch that carries one, else `text`.
///
/// **`text` is the answer and not `None`**, which is the whole point of asking: an all-`unknown`
/// constructor is a `text` value on a real server, so a comparison against a number is `42883`
/// rather than a silent no-match.
fn branch_common_type<'a>(branches: impl Iterator<Item = &'a Expr>) -> ColumnType {
    for branch in branches {
        if let Some(ty) = carried_type(branch).or_else(|| match branch {
            Expr::Literal(literal) => literal_type(literal),
            _ => None,
        }) {
            return ty;
        }
    }
    ColumnType::Text
}

fn retype_subscript(expr: &Expr, ty: ColumnType) -> Expr {
    match expr {
        Expr::Subscript { operand, index, .. } => Expr::Subscript {
            operand: operand.clone(),
            index: index.clone(),
            element: ty,
        },
        other => other.clone(),
    }
}

/// The type a literal already carries, or `None` for the two that carry none.
///
/// `unknown` (a quoted string) is the one that takes a type from its neighbour; NULL has no type
/// and needs none. The other four are what PostgreSQL calls them, with the two divergences this
/// node declares: a bare integer constant is `int4` on a real server and `int8` here, and a
/// decimal constant is `numeric` there and `double precision` here — the same choice
/// `Literal::Decimal` already makes everywhere else in this crate, `SELECT 1.5` included.
pub(super) fn literal_type(literal: &Literal) -> Option<ColumnType> {
    match literal {
        // **The one NULL that has a type**, which is why the variant exists: everything that asks
        // this question — a subquery's column type, an operator's two sides, a `COALESCE`'s
        // unification — gets the cast's answer instead of `None`.
        Literal::TypedNull(ty) => Some(*ty),
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
/// Reads one `CASE` branch's constant **as the type the branches resolved to**.
///
/// Wider than [`give_type`], which only retypes an `unknown`, and the difference is what makes
/// `UPDATE t SET n = CASE WHEN n IS NULL THEN 0 ELSE n END` work over an `integer` column: the `0`
/// is an integer constant, `n` is the branch that has a type, and without narrowing the constant
/// to it the `CASE` evaluates to an `int8` that will not assign. A real server does the same thing
/// for the same reason — `select_common_type` coerces every input to the chosen type, not only the
/// untyped ones.
///
/// Only the three **spellings that carry no type of their own** are read this way. A
/// `Literal::Typed` was written with a cast and keeps what it was given; the family check above
/// has already refused it if that disagrees.
pub(super) fn give_branch_type(expr: &mut Expr, ty: ColumnType) -> Result<()> {
    if let Expr::Literal(
        literal @ (Literal::String(_) | Literal::Integer(_) | Literal::Decimal(_)),
    ) = expr
    {
        *literal = retype(ty, &literal.clone(), BinaryOp::Eq, false)?;
    }
    Ok(())
}

/// The type one `CASE` branch already has, or `None` for the two spellings that have none.
///
/// `unknown` is the point: a quoted string and a bare NULL take their type from the branch that
/// has one, so they are skipped here and coerced afterwards. Anything whose type cannot be read at
/// all — a parameter with no value yet — is skipped too rather than refused, because a `CASE` over
/// one is typed by its other branches on a real server as well.
fn branch_type(expr: &Expr, scope: &Scope<'_>) -> Option<ColumnType> {
    match expr {
        Expr::Literal(literal) => literal_type(literal),
        other => expr_type(other, scope).ok(),
    }
}

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
        left: left.to_owned(),
        op: op.symbol(),
        right: right.to_owned(),
    }
}

/// A `WHERE` clause has to be a boolean. PostgreSQL says so, and a `WHERE t` where `t` is text is
/// a mistake worth catching before it silently keeps every row.
fn check_predicate(expr: &Expr, clause: &'static str, scope: &Scope<'_>) -> Result<()> {
    // An `UPDATE` or `DELETE` reaches here without going through `Aggregation::build`, and
    // `WHERE count(*) > 1` is `42803` on all three statements. One check rather than three.
    // A `HAVING` is the one clause where an aggregate belongs, so only `WHERE` refuses it.
    // **A set-returning function is not allowed in a `WHERE`**, and PostgreSQL names the clause.
    // The projection is the only place that expands one, so without this the call reaches the row
    // evaluator and answers `XX000` — a bug report for a statement a real server declines politely.
    if contains_set_func(expr) {
        return Err(SqlError::SetFunctionNotAllowed(format!(
            "set-returning functions are not allowed in {clause}"
        )));
    }
    if clause == "WHERE" && aggregate::contains_aggregate(expr) {
        return Err(SqlError::AggregateNotAllowed(
            "aggregate functions are not allowed in WHERE",
        ));
    }
    match expr {
        Expr::Binary { .. }
        | Expr::Like { .. }
        | Expr::RegexMatch { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InList { .. }
        // A comparison like the rest, and the shape `ActiveRecord` puts a join condition in:
        // `JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)`.
        | Expr::AnyArray { .. }
        | Expr::Literal(Literal::Bool(_) | Literal::Null)
        | Expr::Ordinal {
            ty: ColumnType::Bool,
            ..
        } => Ok(()),
        // A `CASE` is a predicate exactly when its **branches** resolved to `boolean`. This list
        // is otherwise a list of shapes, because every other shape on it has one type; a `CASE`
        // is the first whose type is a property of what is inside it, so it is the first arm here
        // that has to ask. `WHERE CASE WHEN flag THEN true END` is a filter and
        // `WHERE CASE WHEN true THEN id ELSE id END` is `42804 … not type bigint`.
        Expr::Case { .. } if expr_type(expr, scope).is_ok_and(|ty| ty == ColumnType::Bool) => {
            Ok(())
        }
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
) -> Result<Vec<OutputColumn>> {
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
                columns.extend(scope.expand(qualifier_of(item))?.map(|(at, column)| {
                    OutputColumn {
                        name: column.name.clone(),
                        ty: column.ty,
                        typmod: column.typmod,
                        user_type: scope.user_type_at(at).cloned(),
                    }
                }));
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
                // **The type a client is told, for a column declared as a user-defined one.**
                // A plain column reference and an aggregate over one both keep it — `min(mood)` is
                // `mood` on a real server — and everything else loses it, because an expression
                // over an enum is an expression over its ordinal and has no name to give back.
                let user_type = match expr {
                    Expr::Column { table, name } => scope
                        .resolve_column(table.as_deref(), name)
                        .ok()
                        .and_then(|(at, _)| scope.user_type_at(at))
                        .cloned(),
                    Expr::Aggregate(call) if call.func.keeps_its_argument_type() => {
                        match call.arg() {
                            Some(Expr::Column { table, name }) => scope
                                .resolve_column(table.as_deref(), name)
                                .ok()
                                .and_then(|(at, _)| scope.user_type_at(at))
                                .cloned(),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                columns.push(OutputColumn {
                    name,
                    ty,
                    typmod,
                    user_type,
                });
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

/// An arithmetic operator with both operands resolved and its type settled.
fn resolve_arithmetic(
    op: crate::plan::ArithOp,
    left: &Expr,
    right: &Expr,
    scope: &Scope<'_>,
) -> Result<Expr> {
    let left = resolve(left, scope)?;
    let right = resolve(right, scope)?;
    let ty = arithmetic_type(op, &left, &right, scope)?;
    Ok(Expr::Arithmetic {
        op,
        left: Box::new(left),
        right: Box::new(right),
        ty: Some(ty),
    })
}

/// The type `left <op> right` has, with an **untyped NULL taking the other side's**.
///
/// `NULL::int4 / 0` is an `integer` on a real server and this node's lowering drops the cast — a
/// `NULL` is a `Literal::Null` whatever it was written as, because every comparison with one is
/// NULL and the type never mattered before. It matters here: without this the statement is
/// `42883 operator does not exist: text / integer` where PostgreSQL returns a NULL row.
fn arithmetic_type(
    op: crate::plan::ArithOp,
    left: &Expr,
    right: &Expr,
    scope: &Scope<'_>,
) -> Result<ColumnType> {
    let (left, right) = (operand(left, scope)?, operand(right, scope)?);
    let (left, right) = match (left, right) {
        (Operand::Fixed(left), Operand::Fixed(right)) => (left, right),
        // One side has no type of its own: the operator is the other side's, and an `unknown` that
        // will not read as that type is the `22P02` its input function raises.
        (Operand::Fixed(known), Operand::Unknown) | (Operand::Unknown, Operand::Fixed(known)) => {
            (known, known)
        }
        // **An integer constant takes the other side's width when it fits in it**, which is
        // PostgreSQL's own rule and the reason `-((-2147483648)::int4)` overflows: the `0` this
        // node lowers the negation to is an `int4` beside an `int4`, and the subtraction is done
        // at that width. A constant too big for the other side keeps its own.
        (Operand::Fixed(known), Operand::Integer(value))
        | (Operand::Integer(value), Operand::Fixed(known)) => {
            if fits(value, known) {
                (known, known)
            } else {
                (known, ColumnType::Int8)
            }
        }
        // Two constants, or a constant beside a NULL: this node's own literal type, which is
        // `int8` where a real server's is `int4` — the divergence `tests/unknown_literal.rs`
        // holds, visible here as a `bigint` where PostgreSQL reports an `integer`.
        (Operand::Integer(_), _) | (_, Operand::Integer(_)) => (ColumnType::Int8, ColumnType::Int8),
        // Both are `unknown`. PostgreSQL answers `operator is not unique: unknown + unknown`;
        // this node reports the `42883` its own resolution gives, naming the same missing
        // operator.
        (Operand::Unknown, Operand::Unknown) => (ColumnType::Text, ColumnType::Text),
    };
    crate::value::arith::result_type(op, left, right)
}

/// What one side of an arithmetic operator brings to the type resolution.
enum Operand {
    /// A type of its own: a column, a cast, an operator.
    Fixed(ColumnType),
    /// A NULL or a quoted string — PostgreSQL's `unknown`, which takes the other side's type.
    Unknown,
    /// An integer constant, which takes the other side's **width** when it fits.
    Integer(i64),
}

fn operand(expr: &Expr, scope: &Scope<'_>) -> Result<Operand> {
    Ok(match expr {
        // **A typed NULL is not unknown**: the cast gave it a type, so `NULL::text + 1` should
        // resolve the way `'x'::text + 1` does and not the way a bare `NULL + 1` does. It is the
        // one NULL that lands in `Fixed`.
        //
        // **Not captured** — see `crate::plan::Literal::TypedNull`. The rule follows from operator
        // resolution being by type, and the corpus that would pin it is owed.
        Expr::Literal(Literal::TypedNull(ty)) => Operand::Fixed(*ty),
        Expr::Literal(Literal::Null | Literal::String(_)) => Operand::Unknown,
        Expr::Literal(Literal::Integer(value)) => Operand::Integer(*value),
        other => Operand::Fixed(expr_type(other, scope)?),
    })
}

/// Whether an integer constant can be read as `ty` without changing value.
///
/// True for every float and for `numeric`, which is what makes `1.5::float8 + 2` a double rather
/// than an error: a constant beside an inexact type takes that type.
fn fits(value: i64, ty: ColumnType) -> bool {
    match ty {
        ColumnType::Int2 => i16::try_from(value).is_ok(),
        ColumnType::Int4 => i32::try_from(value).is_ok(),
        ColumnType::Int8 | ColumnType::Double | ColumnType::Real | ColumnType::Numeric => true,
        _ => false,
    }
}

pub(super) fn expr_type(expr: &Expr, scope: &Scope<'_>) -> Result<ColumnType> {
    Ok(match expr {
        // Both operands, then the promotion table — the same table the evaluator uses, so the
        // type a client is told matches the values it is sent.
        Expr::Negate(operand) => crate::value::arith::negate_type(expr_type(operand, scope)?)?,
        Expr::Arithmetic {
            op, left, right, ..
        } => arithmetic_type(*op, left, right, scope)?,
        Expr::Column { table, name } => scope.resolve_column(table.as_deref(), name)?.1.ty,
        // **The type the cast named**, which is the whole reason a typed NULL is a variant: this
        // is what a subquery's column reports, and reporting `text` for `NULL::bigint` made
        // `IN (SELECT NULL::bigint)` a `42883` where a real server matches nothing.
        Expr::Ordinal { ty, .. }
        | Expr::Outer { ty, .. }
        | Expr::Literal(Literal::TypedNull(ty)) => *ty,
        // A sequence function answers `bigint` on a real server, all four of them.
        Expr::Literal(Literal::Integer(_)) | Expr::Sequence(_) => ColumnType::Int8,
        // Every catalog function returns `text`, which is what makes them one variant.
        // **`||` is spelled the same for three types**, and its result is its operands': an
        // hstore concatenation is an hstore and everything else is `text` — measured,
        // `pg_typeof('x'::citext || 'y')` is `text`. This works only because a folded
        // `'a=>b'::hstore` constant is a `Datum::Hstore` and not a `Datum::Text`; while it was the
        // latter, the type was gone by the time anything could ask, and the two concatenations
        // were indistinguishable.
        Expr::CatalogFunc(call) if call.func == crate::plan::CatalogFunc::HstoreConcat => {
            let hstore = call
                .args
                .iter()
                .any(|arg| matches!(expr_type(arg, scope), Ok(ColumnType::Hstore)));
            if hstore {
                ColumnType::Hstore
            } else {
                ColumnType::Text
            }
        }
        Expr::CatalogFunc(call) => call.func.result_type(),
        Expr::Literal(Literal::Decimal(_)) => ColumnType::Double,

        // `abs` is the one scalar function that answers its argument's type rather than `text`.
        //
        // **An aggregate argument is `text` here rather than an error.** `abs(min(n))` is typed
        // while the aggregates are still un-rewritten — every other scalar function answers a
        // fixed type and never asks — and reporting the internal "reached `expr_type`" for a
        // statement a real server answers would be worse than reporting a type the executor then
        // corrects. The executor's own refusal is what the caller sees.
        Expr::Scalar {
            func: crate::plan::ScalarFunc::Abs,
            operand,
        } => match operand.as_ref() {
            Expr::Aggregate(_) => ColumnType::Text,
            operand => expr_type(operand, scope)?,
        },
        // Whatever the operand is, a cast to `text` answers `text` — that is what it is for.
        // The two text functions take text and answer text.
        Expr::Scalar { .. }
        | Expr::ToText { .. }
        // `name` on a real server and `text` here — the standing catalog trade; the array
        // spelling is `text` too, because this node has no array *value* to type.
        | Expr::CurrentSchema { .. }
        | Expr::CurrentDatabase
        | Expr::CurrentSetting { .. }
        | Expr::Advisory { .. }
        | Expr::Literal(Literal::String(_) | Literal::Null) => ColumnType::Text,
        Expr::Literal(Literal::Typed(value)) => value.column_type().unwrap_or(ColumnType::Text),
        Expr::Literal(Literal::Bool(_))
        | Expr::Like { .. }
        | Expr::RegexMatch { .. }
        | Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InList { .. }
        | Expr::AnyArray { .. } => ColumnType::Bool,
        // A scalar subquery has the type of the column it returns and the other four are
        // predicates, which is the whole of what `SubqueryExpr::value_type` says.
        Expr::Subquery(sub) => sub.value_type(),
        // The type the branches resolved to, read back in the order they were resolved in
        // (`resolve`'s `Expr::Case` arm) — the `ELSE` first. All-`unknown` is `text`, which is
        // PostgreSQL's own fallback and is why `CASE WHEN true THEN 'a' ELSE 'b' END` is `text`
        // rather than untyped.
        // The type the element is read **as**, which a comparison sets and which is `text` until
        // one does — the same rule an `= ANY`'s elements follow.
        // **The array's own element type wins over the node's.** A subscript's `element` is
        // filled where the expression is lowered, before anything knows what it is subscripting;
        // a real array carries its element type in the value, so it answers for itself and the
        // stored field is the fallback for the catalog's text vectors.
        Expr::Subscript {
            operand, element, ..
        } => expr_type(operand, scope)
            .ok()
            .and_then(esker_keys::array::ArrayValue::element_of)
            .unwrap_or(*element),
        Expr::Uuid(_) => ColumnType::Uuid,
        // Resolution has already given every argument the common type, so the first one that
        // **carries** a type is the answer. `branch_type` rather than `expr_type` is the whole of
        // it: a bare NULL answers `text` from the second and nothing from the first, and
        // `COALESCE(NULL, 2)` typed as `text` made `COALESCE(1, NULL) + COALESCE(NULL, 2)` the
        // `42883 operator does not exist: bigint + text` that a real server adds without blinking.
        // One generated **value**, not the set: the column a client is described is the element
        // type. `unnest` answers its array's element type and the two generators answer their own.
        Expr::SetFunc(call) => super::table_function::result_type(call, scope),
        Expr::Coalesce(args) => args
            .iter()
            .find_map(|arg| branch_type(arg, scope))
            .unwrap_or(ColumnType::Text),
        Expr::Case {
            branches,
            otherwise,
        } => otherwise
            .iter()
            .map(AsRef::as_ref)
            .chain(branches.iter().map(|branch| &branch.then))
            .find_map(|result| branch_type(result, scope))
            .unwrap_or(ColumnType::Text),
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
