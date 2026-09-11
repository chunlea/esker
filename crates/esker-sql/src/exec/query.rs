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
use crate::plan::{
    BinaryOp, CatalogFunc, Expr, Literal, LockWait, Node, Select, SelectItem, SortKey,
};
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
        // The same implicit alias `plan::TableRef::referred_as` gives: a qualified relation
        // answers to its bare name.
        Scope::single_as(
            table,
            crate::catalog::split_qualified(&table.name).1.to_owned(),
        )
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

    /// The column a resolved position belongs to, or `None` for a position past the end.
    pub(super) fn column_def(&self, at: usize) -> Option<&ColumnDef> {
        let mut start = 0;
        for table in &self.tables {
            if at < start + table.columns.len() {
                return table.columns.get(at - start);
            }
            start += table.columns.len();
        }
        None
    }

    /// Every relation a locking clause names, with its key columns as positions in this scope.
    ///
    /// `of` is `None` for a bare `FOR UPDATE`, which locks **every** relation in the statement.
    ///
    /// A relation with no primary key is not here, and that is the rule rather than a shortcut: a
    /// derived table, a `VALUES` list, a set-returning function and a catalog view all carry an
    /// empty key because their rows are computed rather than stored, and PostgreSQL **accepts** a
    /// locking clause over each of them and locks nothing (measured, all five shapes). A table the
    /// user gave no key still has one — the hidden row id — so it locks like any other.
    pub(super) fn locked_relations(&self, of: Option<&str>) -> Vec<(&'a TableDef, Vec<usize>)> {
        let mut start = 0;
        let mut found = Vec::new();
        for (index, table) in self.tables.iter().enumerate() {
            let named = of.is_none_or(|want| self.names[index] == want);
            if named && !table.primary_key.is_empty() {
                found.push((
                    *table,
                    table.primary_key.iter().map(|at| start + at).collect(),
                ));
            }
            start += table.columns.len();
        }
        found
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
        // **A qualifier may name the relation's schema**, which the referable name does not carry:
        // `SELECT s1.things.name FROM s1.things` is a statement a real server answers, and the
        // qualifier arrives here as the *stored* name (`s1 NUL things`) because
        // `crate::parse::lower`'s three-part arm resolves it through `relation_name`.
        //
        // It matches only where no explicit alias replaced the name — `FROM s1.things t` makes
        // `s1.things.name` an `invalid reference` on a real server, with a `HINT` naming `t`, and
        // that is what the fallback below already answers.
        if qualifier.contains(crate::catalog::SCHEMA_SEPARATOR)
            && let Some(index) = self.tables.iter().position(|table| table.name == qualifier)
            && self
                .names
                .get(index)
                .is_some_and(|name| name == crate::catalog::split_qualified(qualifier).1)
        {
            return Ok(index);
        }
        let mut matches = self
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| *name == qualifier);
        if let Some((index, _)) = matches.next() {
            // **Two entries can answer to one bare name** now that a qualified relation keeps its
            // implicit alias: `FROM s1.things, s2.things` is a `FROM` PostgreSQL allows, and
            // `things.name` over it is `42P09` there rather than a silent choice of the first —
            // which is the same reason an ambiguous *column* is refused two screens down.
            if matches.next().is_some() {
                return Err(SqlError::AmbiguousTableReference(qualifier.to_owned()));
            }
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
    pub(super) fn expand(
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

    /// The **enum** the column at a resolved position was declared as, or `None`.
    ///
    /// A position is an index into the **concatenated** row, so this walks the tables the way
    /// [`Scope::offset`] builds it: the labels live on the table
    /// (`crate::catalog::TableDef::enums`) and the oid on the column, so both halves have to be
    /// found together (ADR 0050).
    ///
    /// Narrower than [`Scope::user_type_at`] on purpose: everything that rewrites a *value*
    /// belongs here, because an enum is the only kind whose value is not what is stored.
    fn enum_at(&self, at: usize) -> Option<&'a crate::catalog::TypeDef> {
        self.user_type_at(at)
            .filter(|def| matches!(def.kind, crate::catalog::TypeKind::Enum { .. }))
    }

    /// The user-defined type the column at a resolved position was declared as, of any kind.
    ///
    /// What a client is *told* — `pg_typeof`, the `RowDescription` oid, `information_schema` —
    /// comes from here, so that a `floatrange` column is called a `floatrange` and not the range
    /// representation that holds it.
    fn user_type_at(&self, at: usize) -> Option<&'a crate::catalog::TypeDef> {
        let mut start = 0;
        for table in &self.tables {
            let end = start + table.columns.len();
            if at < end {
                let column = table.columns.get(at - start)?;
                return super::assign::user_type_of(table, column);
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
#[derive(Debug, Clone)]
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
    /// A **pseudo-type** the projection was cast to: a type no value has, reported through the
    /// `RowDescription` and nowhere else. See [`crate::plan::PseudoType`].
    pub(super) pseudo: Option<crate::plan::PseudoType>,
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
    /// The rows to lock, one entry per relation a locking clause names (ADR 0057 §5). Empty for
    /// every statement without one — and for a locking clause over a relation with no stored
    /// identity, which PostgreSQL accepts and locks nothing for (measured: a derived table, a
    /// `VALUES` list, a set-returning function, a `CTE` and a catalog relation all answer rows).
    pub(super) locks: Vec<LockTarget>,
    /// The junk columns [`Planned::locks`] reads its keys from: how many trailing columns of a row
    /// the client must never see.
    pub(super) junk: usize,
    /// `(offset, limit)`, **withheld from the plan** when there is something to lock.
    ///
    /// PostgreSQL puts `Limit` *above* `LockRows`, and the difference is observable: with the
    /// first row held by somebody else, `LIMIT 1 … SKIP LOCKED` answers the *second* row, where a
    /// limit applied before the skip answers nothing at all. Measured against PG 19 with two
    /// sessions — and it is the case a queue is written for, so the order is the feature.
    pub(super) limit: Option<(usize, Option<usize>)>,
}

/// One relation a `SELECT … FOR UPDATE` locks, and where its key is in the row.
#[derive(Debug, Clone)]
pub(super) struct LockTarget {
    /// The table's **own** name, which is what PostgreSQL's `55P03` prints — not the alias the
    /// query used, measured: `could not obtain lock on row in relation "lk"` for
    /// `SELECT … FROM lk l … FOR UPDATE OF l NOWAIT`.
    pub(super) relation: String,
    /// The table, for the key encoding.
    pub(super) table_id: u64,
    /// Where the key columns are in the projected row: the junk columns, in key order.
    pub(super) key_at: Vec<usize>,
    /// What to do when the row is held.
    pub(super) wait: LockWait,
}

/// The access path and filter for every row of `table` a predicate matches — the half of a plan
/// that `UPDATE` and `DELETE` share with `SELECT`.
///
/// It yields whole rows, because a statement that rewrites a row needs all of it: the columns it
/// is not changing still have to be written back, and the index entries it is replacing were built
/// from the old ones.
pub(super) fn matching_rows(filter: Option<&Expr>, tenant: u64, table: &TableDef) -> Result<Node> {
    // The implicit alias, which is the bare relation name — `UPDATE s1.things SET … WHERE
    // things.id = 1` is a statement a real server takes. This is the `UPDATE`/`DELETE` half of
    // `plan::TableRef::referred_as`; the aliased case comes through `matching_rows_as` and is
    // already the alias.
    matching_rows_as(
        filter,
        tenant,
        table,
        crate::catalog::split_qualified(&table.name).1.to_owned(),
    )
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
/// Every arm's plan appended, with one output column list unified across them.
///
/// **The names are the first arm's and the types are every arm's**, which is measured and is two
/// rules rather than one: `SELECT i FROM t UNION ALL SELECT n FROM t` is a column called `i` of
/// type `numeric` (`tests/captures/pg19_set_operations.txt`).
///
/// Three refusals, each PostgreSQL's own:
///
/// * a different number of columns is `42601 each UNION query must have the same number of
///   columns` — the grammar's error, not a typing one;
/// * two typed columns with no common type are `UNION types text and integer cannot be matched`,
///   naming them in the arms' order;
/// * and an **unknown literal** is neither: it takes the other arm's type and then fails to parse
///   as it, which is why `SELECT 1 UNION ALL SELECT 'abc'` is `22P02 invalid input syntax for type
///   integer` — decided in the lowering, where a literal still is one.
pub(super) fn append(
    select: &Select,
    arms: Vec<(Option<(crate::plan::SetOp, bool)>, Planned)>,
) -> Result<Planned> {
    let ops: Vec<Option<(crate::plan::SetOp, bool)>> = arms.iter().map(|(op, _)| *op).collect();
    let mut arms = arms.into_iter().map(|(_, planned)| planned);
    let first = arms
        .next()
        .ok_or_else(|| SqlError::Internal("a set operation with no arms".to_owned()))?;
    let mut columns = first.columns.clone();
    let mut arm_types = vec![first.columns.iter().map(|c| c.ty).collect::<Vec<_>>()];
    let mut nodes = vec![first.node];
    for arm in arms {
        if arm.columns.len() != columns.len() {
            return Err(SqlError::SetOperationArity);
        }
        for (at, column) in arm.columns.iter().enumerate() {
            // **The same `select_common_type` a `COALESCE` and a `CASE` ask** — this path had
            // both of its passes and the right two sentences first, and `common_of` is that rule
            // written once so the other two stopped having their own.
            columns[at].ty = common_of(&[columns[at].ty, column.ty], Unifying::SetOperation)?;
            // A typmod survives only where both arms agree on it, the way a `CASE`'s does: a
            // `varchar(3)` beside a `varchar(5)` is a `varchar` with no length on a real server.
            if columns[at].typmod != column.typmod {
                columns[at].typmod = crate::value::NO_TYPMOD;
            }
            // And a user-defined type only where both arms are the same one.
            if columns[at].user_type != column.user_type {
                columns[at].user_type = None;
            }
        }
        arm_types.push(arm.columns.iter().map(|c| c.ty).collect());
        nodes.push(arm.node);
    }
    // **Every arm has to reach the type the set settled on, by an implicit cast**: agreeing on a
    // category is not enough. `money` beside `numeric` is one category with no implicit cast either
    // way, which a real server refuses as `42846 UNION could not convert type numeric to money` —
    // a different sentence from the categories' `42804`, measured beside it.
    for types in &arm_types {
        for (at, from) in types.iter().enumerate() {
            let to = columns[at].ty;
            if !reaches_implicitly(*from, to) {
                return Err(SqlError::SetOperationCannotConvert {
                    from: from.name(),
                    to: to.name(),
                });
            }
        }
    }
    // **Unifying the declared type is only half of it: the values have to follow.** An arm that
    // produced an `int4` where the set is a `bigint` would answer rows of two different types
    // under one column — `pg_typeof` reads the value, and a client binding by the declared type
    // would decode the wrong width. PostgreSQL coerces each arm's target list; this wraps the arms
    // that need it in a projection of casts and leaves the rest untouched.
    let nodes: Vec<Node> = nodes
        .into_iter()
        .zip(arm_types)
        .map(|(node, types)| coerce_arm(node, &types, &columns))
        .collect();
    let mut node = combine(nodes, &ops);
    // **The set's own clauses, over the set's own row.** An `ORDER BY` after the last arm sorts
    // the whole result on a real server, and what it may name is measured: the output name or the
    // ordinal, never the underlying column — `SELECT i AS c … UNION ALL … ORDER BY i` is
    // `column "i" does not exist` there, because by then the column is called `c`.
    let keys = set_order_keys(select, &columns)?;
    if !keys.is_empty() {
        node = Node::Sort {
            input: Box::new(node),
            keys,
        };
    }
    let offset = count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    let limit = count(select.limit.as_ref(), "LIMIT")?;
    if select.limit.is_some() || select.offset.is_some() {
        node = Node::Limit {
            input: Box::new(node),
            offset,
            limit,
        };
    }
    // **No routing, no locks, no junk.** A set operation is not routed to a columnar replica —
    // the arms may name different relations — and a locking clause over one is a syntax error on a
    // real server, so neither field can carry anything a caller would then have to undo.
    Ok(Planned {
        node,
        columns,
        table: first.table,
        column_names: first.column_names,
        engine: None,
        locks: Vec::new(),
        junk: 0,
        limit: None,
    })
}

/// An `ORDER BY` written after the last arm, resolved against the set's output columns.
///
/// **Two spellings and no third**, measured: the ordinal, and the name the *first arm* gave the
/// column. Anything else is `column "…" does not exist`, which is what a real server answers for
/// the underlying column of a renamed one — by the time the set exists, that name is gone.
///
/// A separate resolver from `order_keys` rather than a reuse of it, because the two resolve
/// against different things: that one has a scope with the tables in it, and a set operation has
/// no table — its row is the arms' agreement and nothing else.
fn set_order_keys(select: &Select, columns: &[OutputColumn]) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for item in &select.order_by {
        let at = match &item.expr {
            Expr::Literal(Literal::Integer(position)) => usize::try_from(*position)
                .ok()
                .filter(|at| (1..=columns.len()).contains(at))
                .map(|at| at - 1)
                .ok_or_else(|| {
                    SqlError::InvalidColumnReference(format!(
                        "ORDER BY position {position} is not in select list"
                    ))
                })?,
            Expr::Column { name, .. } => columns
                .iter()
                .position(|column| column.name == *name)
                .ok_or_else(|| SqlError::UndefinedColumn(name.clone()))?,
            // An expression over a set's row is legal on a real server and needs the row's own
            // scope to resolve; refused by name rather than answered against the first arm's,
            // which would be a different column of the same spelling.
            other => {
                return Err(SqlError::unsupported(format!(
                    "ORDER BY {other:?} over a set operation"
                )));
            }
        };
        keys.push(SortKey {
            expr: Expr::Ordinal {
                at,
                ty: columns[at].ty,
                typmod: columns[at].typmod,
            },
            ty: Some(columns[at].ty),
            descending: item.descending,
            // The same default as everywhere else: NULLs last ascending, first descending.
            nulls_first: item.nulls_first.unwrap_or(item.descending),
        });
    }
    Ok(keys)
}

/// The arms folded into one node, left to right, which is the associativity SQL has.
///
/// `a UNION ALL b UNION c` is `((a ∪all b) ∪ c)`: the `UNION` deduplicates *everything before it*
/// and not just the arm beside it. So consecutive `ALL`s collect into one [`Node::Append`] — a set
/// over three scans is one append and not two nested ones — and a deduplicating operator closes
/// the append it has collected and wraps it.
fn combine(nodes: Vec<Node>, ops: &[Option<(crate::plan::SetOp, bool)>]) -> Node {
    let mut pending: Vec<Node> = Vec::new();
    for (node, op) in nodes.into_iter().zip(ops) {
        pending.push(node);
        // The first arm has no operator; every other one either extends the append or closes it.
        if matches!(op, Some((_, false))) {
            pending = vec![Node::Distinct {
                input: Box::new(one_of(pending)),
            }];
        }
    }
    one_of(pending)
}

/// One node from what has been collected: the node itself when there is one, an append when more.
fn one_of(mut nodes: Vec<Node>) -> Node {
    if nodes.len() == 1 {
        return nodes.pop().unwrap_or(Node::OneRow);
    }
    Node::Append { arms: nodes }
}

/// One arm's rows as the set's types, or the arm unchanged when it already produces them.
///
/// A projection of casts, which is what PostgreSQL puts in each arm's target list. The `Ordinal`
/// carries the arm's *own* type, because that is what the value in the row is; the cast is what
/// makes it the set's.
fn coerce_arm(node: Node, arm: &[ColumnType], columns: &[OutputColumn]) -> Node {
    if arm.iter().zip(columns).all(|(from, to)| *from == to.ty) {
        return node;
    }
    let exprs = arm
        .iter()
        .zip(columns)
        .enumerate()
        .map(|(at, (from, to))| {
            let operand = Expr::Ordinal {
                at,
                ty: *from,
                typmod: crate::value::NO_TYPMOD,
            };
            if *from == to.ty {
                operand
            } else {
                Expr::Cast {
                    operand: Box::new(operand),
                    to: to.ty,
                    typmod: crate::value::NO_TYPMOD,
                }
            }
        })
        .collect();
    Node::Project {
        input: Box::new(node),
        exprs,
    }
}

/// The two types of one output column, unified the way `select_common_type` unifies them.
///
/// **Not the arithmetic promotion**, which is what this folded through before and which refused
/// `varchar` beside `text` as "cannot be matched" and made `int8` beside `real` a `double`.
/// Measured on PostgreSQL 19 (`tests/captures/pg19_union_types.txt`) the rule is three questions,
/// in order:
///
/// * two **categories** cannot be matched — `integer` and `text`, `date` and `text`, `time` and
///   `interval` are all `42804`;
/// * a **preferred** type stays — `text`, `float8`, `oid`, `timestamptz`, `interval`, `inet`,
///   `bool`, `varbit` — so `text` beside anything in its category is `text`;
/// * otherwise the later type is taken only when the earlier one casts to it implicitly **and not
///   the other way**. `int2` beside `int8` is `bigint` and `date` beside `timestamp` is
///   `timestamp`; `varchar` beside `text` stays **`character varying`**, because those two cast
///   each other implicitly and the first arm wins — the row a reader guesses wrong.
///
/// Whether every arm can then *reach* the chosen type is [`reaches_implicitly`]'s question, asked
/// in [`append`] once the type is known: `money` beside `numeric` and `json` beside `jsonb` agree
/// on a category and have no implicit cast, which is `42846` there and not `42804`.
/// Which construct is unifying, for the two sentences PostgreSQL gives when it cannot.
///
/// The words differ per construct and the **codes** do not: a pair in two categories is `42804`
/// and a pair in one category with no implicit cast is `42846`, for a `UNION`, a `COALESCE` and a
/// `CASE` alike. Measured for all three.
#[derive(Clone, Copy)]
pub(super) enum Unifying {
    /// `UNION`, `INTERSECT`, `EXCEPT`.
    SetOperation,
    /// `COALESCE`, whose arguments are walked left to right.
    Coalesce,
    /// A `CASE`'s results, whose list starts at the `ELSE`.
    Case,
}

impl Unifying {
    /// `42804`, for a pair whose `typcategory` letters differ.
    fn mismatch(self, left: ColumnType, right: ColumnType) -> SqlError {
        match self {
            Unifying::SetOperation => SqlError::SetOperationTypes {
                left: left.name(),
                right: right.name(),
            },
            Unifying::Coalesce => SqlError::DatatypeMismatch(format!(
                "COALESCE types {} and {} cannot be matched",
                left.name(),
                right.name()
            )),
            Unifying::Case => SqlError::DatatypeMismatch(format!(
                "CASE types {} and {} cannot be matched",
                left.name(),
                right.name()
            )),
        }
    }

    /// `42846`, for a branch that cannot reach the type the construct settled on.
    ///
    /// **`CASE/WHEN` where the other two say their own name**, measured:
    /// `CASE/WHEN could not convert type character to citext`.
    fn cannot_convert(self, from: ColumnType, to: ColumnType) -> SqlError {
        match self {
            Unifying::SetOperation => SqlError::SetOperationCannotConvert {
                from: from.name(),
                to: to.name(),
            },
            Unifying::Coalesce => SqlError::CannotConvertBranch {
                kind: "COALESCE",
                from: from.name(),
                to: to.name(),
            },
            Unifying::Case => SqlError::CannotConvertBranch {
                kind: "CASE/WHEN",
                from: from.name(),
                to: to.name(),
            },
        }
    }
}

/// Whether `from` reaches `to` by an **implicit** cast and nothing looser — `pg_cast`'s `i` rows,
/// element-wise for a pair of arrays.
///
/// [`reaches_implicitly`] is the same question with a number-beside-a-number fallback, and that
/// fallback is exactly wrong for choosing between two candidates: `int8 -> int4` is an
/// *assignment* on a real server, so a rule that reads it as implicit lets `int8` be displaced by
/// `int4` and `COALESCE(bigint, integer)` comes out `integer`. Measured: it is `bigint`.
fn coerces_implicitly(from: ColumnType, to: ColumnType) -> bool {
    use esker_keys::array::ArrayValue;
    if from == to {
        return true;
    }
    if let (Some(f), Some(t)) = (ArrayValue::element_of(from), ArrayValue::element_of(to)) {
        return coerces_implicitly(f, t);
    }
    implicit_cast(from, to)
}

/// **PostgreSQL's `select_common_type`, both of its passes**, for every construct that unifies a
/// list of types into one.
///
/// 1. The running candidate is the first type. A later type in a **different `typcategory`** is
///    `42804`. Otherwise the candidate is displaced only when it is *not* its category's preferred
///    type, it casts implicitly to the other, and the other does not cast back.
/// 2. Then **every** input must reach the candidate by an implicit cast, or it is `42846` — a
///    different sentence and a different code from the first pass, and the one a pair in one
///    category with no cast between them gets.
///
/// **An array's category is `A`, not its element's**, which is the half `unify` gets wrong by
/// recursing into elements before the category test: `"char"[]` beside `bigint[]` is
/// `42846 could not convert type bigint[] to "char"[]` on 19beta1 and was `42804` here, and
/// `"char"[]` beside `text[]` is answered `text[]` there and was refused here. Measured over every
/// same-category pair of the wire v3 probe list's 100 spellings, 5,556 shape-rows
/// (`tests/captures/pg19_branch_common_type.txt`).
///
/// **Written once for three callers.** The set-operation path had both passes and the right two
/// sentences; `COALESCE` and `CASE` had neither, which is a measured rule reaching only the caller
/// it was written for.
pub(super) fn common_of(types: &[ColumnType], kind: Unifying) -> Result<ColumnType> {
    let Some(&first) = types.first() else {
        return Ok(ColumnType::Text);
    };
    let mut chosen = first;
    for &next in &types[1..] {
        if next == chosen {
            continue;
        }
        if pg_catalog::typcategory(chosen) != pg_catalog::typcategory(next) {
            return Err(kind.mismatch(chosen, next));
        }
        if !is_preferred(chosen)
            && coerces_implicitly(chosen, next)
            && !coerces_implicitly(next, chosen)
        {
            chosen = next;
        }
    }
    for &ty in types {
        if !reaches_implicitly(ty, chosen) {
            return Err(kind.cannot_convert(ty, chosen));
        }
    }
    Ok(chosen)
}

pub(super) fn unify(left: ColumnType, right: ColumnType) -> Result<ColumnType> {
    use esker_keys::array::ArrayValue;
    if left == right {
        return Ok(left);
    }
    // An array's common type is its elements': `int4[]` beside `int8[]` is `bigint[]`.
    if let (Some(l), Some(r)) = (ArrayValue::element_of(left), ArrayValue::element_of(right)) {
        return Ok(ArrayValue::array_of(unify(l, r)?).unwrap_or(left));
    }
    if pg_catalog::typcategory(left) != pg_catalog::typcategory(right) {
        return Err(SqlError::SetOperationTypes {
            left: left.name(),
            right: right.name(),
        });
    }
    // **PostgreSQL's `select_common_type`, and it is asymmetric.** The running candidate keeps the
    // answer unless it is *not* its category's preferred type **and** it can be implicitly cast to
    // the other while the other cannot be cast back. Measured, and the asymmetry is visible in one
    // pair: `name` and `text` cast implicitly **both** ways, so neither displaces the other and the
    // arm that came first wins — `coalesce(name, text)` is `name` and `coalesce(text, name)` is
    // `text`. Every other pair here is order-free, because only one direction is implicit:
    // `numeric` beside `real` is `real` in both orders, since `numeric -> float4` is implicit and
    // `float4 -> numeric` is only an assignment.
    //
    // Written symmetrically once, and that was wrong in exactly that pair: it made
    // `coalesce(name, text)` a `text`, because `text` is preferred and a symmetric rule lets the
    // *right* side's preference win a tie the left had already taken.
    if !is_preferred(left) && implicit_cast(left, right) && !implicit_cast(right, left) {
        return Ok(right);
    }
    Ok(left)
}

/// `pg_type.typispreferred`, measured on PostgreSQL 19: one per category, and `oid` beside
/// `float8` in the numbers.
fn is_preferred(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Bool
            | ColumnType::TimestampTz
            | ColumnType::Inet
            | ColumnType::Double
            | ColumnType::Oid
            | ColumnType::Text
            | ColumnType::Interval
            | ColumnType::VarBit
    )
}

/// Whether `pg_cast` holds an **implicit** cast from one type to the other — read from the table
/// `pg_cast` itself is served from, so the two cannot disagree.
fn implicit_cast(from: ColumnType, to: ColumnType) -> bool {
    use crate::value::PgType as _;
    let (from, to) = (i64::from(from.oid()), i64::from(to.oid()));
    pg_catalog::CASTS
        .iter()
        .any(|(source, target, context, _)| *source == from && *target == to && *context == "i")
}

/// Whether an arm's value can be handed to the set's column: the same type, an implicit cast,
/// an array of either, or a number beside a number — which the promotion table vouches for where
/// the cast table has no row.
fn reaches_implicitly(from: ColumnType, to: ColumnType) -> bool {
    use esker_keys::array::ArrayValue;
    if from == to || implicit_cast(from, to) {
        return true;
    }
    if let (Some(f), Some(t)) = (ArrayValue::element_of(from), ArrayValue::element_of(to)) {
        return reaches_implicitly(f, t);
    }
    let number = |ty: ColumnType| {
        matches!(
            ty,
            ColumnType::Int2
                | ColumnType::Int4
                | ColumnType::Int8
                | ColumnType::Real
                | ColumnType::Double
                | ColumnType::Numeric
        )
    };
    number(from) && number(to)
}

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
    let join_kind = only_join.map_or(crate::plan::JoinKind::Inner, |join| join.kind);
    let left_join = join_kind != crate::plan::JoinKind::Inner;
    let condition = one_joins_condition(select, named_table, named_inner, left_join)?;

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
            join_kind,
            named_table.map_or(0, |(table, _)| table.row_schema().len()),
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
    let mut exprs = projection_exprs(select, scope, aggregation.as_ref())?;
    // The junk columns go on the end of the target list, so every position above — sort keys,
    // `DISTINCT`, the output columns — is the position it was before.
    let (locks, junk) = lock_targets(select, scope, &mut exprs);

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
    // **`SELECT DISTINCT` deduplicates over the target list**, so every column in it needs the
    // same equality operator class a grouping key needs. The third reader of one list; before it
    // was shared, `count(DISTINCT j)` refused and `SELECT DISTINCT j` answered.
    if select.distinct {
        for expr in &exprs {
            if let Expr::Ordinal { ty, .. } = expr
                && !crate::value::has_equality_operator(*ty)
            {
                return Err(SqlError::NoEqualityOperator(ty.name()));
            }
        }
    }
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

    let offset = count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    let limit = count(select.limit.as_ref(), "LIMIT")?;
    // **Withheld when there is something to lock**, and applied by the executor after the lock
    // pass — see [`Planned::limit`]. A statement that locks nothing keeps the node, so nothing
    // about an ordinary `LIMIT` moves.
    if (select.limit.is_some() || select.offset.is_some()) && locks.is_empty() {
        node = Node::Limit {
            input: Box::new(node),
            offset,
            limit,
        };
    }

    Ok(Planned {
        node,
        columns,
        // **The bare name, not the stored one.** Outside `public` a relation is stored as
        // `schema ++ NUL ++ name` (ADR 0071), and this field is read only by `EXPLAIN` — so
        // handing it on unsplit put a NUL byte in the middle of a `QUERY PLAN` row. PostgreSQL
        // prints the bare name here and qualifies it only under `VERBOSE`, which this node does
        // not honour (measured, `tests/alias.rs`).
        table: outer_table.map_or_else(
            || "-".to_owned(),
            |table| crate::catalog::split_qualified(&table.name).1.to_owned(),
        ),
        column_names: scope_column_names(scope),
        engine: None,
        junk,
        limit: if locks.is_empty() {
            None
        } else {
            Some((offset, limit))
        },
        locks,
    })
}

/// The key columns each locking clause needs, appended to the target list as **junk columns**.
///
/// PostgreSQL's own arrangement, and for the same reason: the key is not necessarily in the target
/// list — `SELECT n FROM lk FOR UPDATE` returns no `id` and still locks by `id` — so the projection
/// carries it and the executor drops it before the client sees a row.
///
/// Two clauses naming one relation lock it once, with the stricter wait winning: `NOWAIT` before
/// `SKIP LOCKED` before waiting, which is the order that never invents an answer.
fn lock_targets(
    select: &Select,
    scope: &Scope<'_>,
    exprs: &mut Vec<Expr>,
) -> (Vec<LockTarget>, usize) {
    let mut targets: Vec<LockTarget> = Vec::new();
    let visible = exprs.len();
    for lock in &select.locking {
        for (table, key) in scope.locked_relations(lock.of.as_deref()) {
            if let Some(existing) = targets
                .iter_mut()
                .find(|target| target.table_id == table.id)
            {
                existing.wait = stricter(existing.wait, lock.wait);
                continue;
            }
            let key_at = key
                .iter()
                .map(|&at| {
                    let ty = column_at(scope, at);
                    let position = exprs.len();
                    exprs.push(Expr::Ordinal {
                        at,
                        ty: ty.0,
                        typmod: ty.1,
                    });
                    position
                })
                .collect();
            targets.push(LockTarget {
                relation: table.name.clone(),
                table_id: table.id,
                key_at,
                wait: lock.wait,
            });
        }
    }
    (targets, exprs.len() - visible)
}

/// The type and typmod of a scope position, for a junk column that reads it.
fn column_at(scope: &Scope<'_>, at: usize) -> (ColumnType, i32) {
    scope
        .column_def(at)
        .map_or((ColumnType::Int8, crate::value::NO_TYPMOD), |column| {
            (column.ty, column.typmod)
        })
}

/// Which of two waits decides, when two clauses name one relation.
///
/// `NOWAIT` first, then `SKIP LOCKED`, then waiting. Both of the first two are promises about a
/// held row, so the one that refuses to invent an answer wins.
fn stricter(left: LockWait, right: LockWait) -> LockWait {
    match (left, right) {
        (LockWait::NoWait, _) | (_, LockWait::NoWait) => LockWait::NoWait,
        (LockWait::SkipLocked, _) | (_, LockWait::SkipLocked) => LockWait::SkipLocked,
        _ => LockWait::Wait,
    }
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
        let Expr::Ordinal { ty, .. } = &key.expr else {
            continue;
        };
        // **PostgreSQL's own refusal, for the types that genuinely have no ordering operator.**
        // Measured on 19beta1 for `xml`, `json` and `lseg`, all three the same sentence with the
        // same HINT — and `jsonb` is *not* among them, which is why the two halves of this
        // function say different things. Answering these from `pg_cmp`'s text comparison was a
        // number where a real server raises, ADR 0031's worst class.
        if matches!(
            ty,
            ColumnType::Json
                | ColumnType::Xml
                | ColumnType::Point
                | ColumnType::Lseg
                | ColumnType::Box
                | ColumnType::Path
                | ColumnType::Polygon
                | ColumnType::Circle
                | ColumnType::Line
        ) {
            return Err(SqlError::NoOrderingOperator(ty.name()));
        }
        // **A `jsonb` is ordered on a real server and is ordered here.** Its comparison is the
        // *document's* — kind first and numbers numerically — which the stored canonical text does
        // not reproduce, so what makes it sortable is the declared type travelling on the
        // `SortKey` to the comparator rather than a `Datum` of its own (`plan::SortKey::ty`).
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
            // **And the other half: an output column is preferred to an input one.** This
            // comment used to say that a preference nothing had measured would be invented — it
            // is measured now, on 19beta1, and the two spellings differ:
            //
            // ```text
            // SELECT m::text        FROM t ORDER BY m  -- sorts by the TEXT: m::text is named m
            // SELECT m::text AS x   FROM t ORDER BY m  -- sorts by the enum, the input column
            // ```
            //
            // Only a **bare** name is subject to it, and only when exactly one output column
            // carries that name: two is the `42702` below, which is a different ambiguity from a
            // column reference's — the target list is what is ambiguous, not the tables.
            // `SELECT l.id, r.id, id FROM l JOIN r USING (id) ORDER BY id` is that shape.
            if let Expr::Column { table: None, name } = &item.expr
                && columns.iter().filter(|output| &output.name == name).count() > 1
            {
                return Err(SqlError::AmbiguousOrderBy(name.clone()));
            }
            let resolved = resolve(&deshadow(&item.expr, select), scope)?;
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
            ty: expr_type(&expr, scope).ok(),
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
        set_arms: Vec::new(),
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
    // **Judged on `identity`, not on the referable name.** Two relations of one name in different
    // schemas share an implicit alias and are still two entries a query may have — measured, a real
    // server takes `FROM s1.things, s2.things` and refuses only the bare reference to it. Keying
    // this on `referred_as` refused the `FROM` itself.
    let identities: Vec<&str> = std::iter::once(
        select
            .from
            .as_ref()
            .map_or("", crate::plan::TableRef::identity),
    )
    .chain(select.joins.iter().map(|join| join.table.identity()))
    .collect();
    for (at, name) in identities.iter().enumerate() {
        // The empty name is a derived table with no alias, which PostgreSQL 19 allows and which
        // nothing can refer to -- so two of them are two anonymous relations, not a duplicate.
        if !name.is_empty() && identities[..at].contains(name) {
            return Err(SqlError::DuplicateTableName(
                crate::catalog::split_qualified(name).1.to_owned(),
            ));
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
    // **Not below a `FULL JOIN`, wherever it sits in the chain.** A conjunct on the first table
    // pushed into its scan removes rows *before* every join to its right; below an inner or a left
    // join that is the same answer, because those keep or drop rows by the left side's own
    // conjunct either way. A full join does not: an inner row whose partner was filtered out is
    // NULL-extended and kept, and the conjunct that would have removed it has already been spent.
    // So a chain with a full join anywhere in it pushes nothing, which costs a scan and never a row.
    let has_full = select
        .joins
        .iter()
        .any(|join| join.kind == crate::plan::JoinKind::Full);
    // The first table's own conjuncts, before a single pair has been built. This is the step that
    // matters most in a comma list: `seq.relkind = 'S'` here is the difference between joining
    // every relation and joining the sequences.
    if !has_full {
        node = pushdown(node, &mut pending, &entries[..1], enclosing);
    }

    // Grown one table at a time, so each step's `ON` sees exactly the tables to its left plus the
    // one being joined — which is what makes a reference to a table two steps back resolve, and a
    // reference to one further right an "undefined column" rather than a silent NULL.
    for at in 0..entries.len() - 1 {
        let scope = Scope::chain(&entries[..=at + 1]).under(enclosing);
        // **Not below an outer join.** A `WHERE` conjunct applied before the NULL extension would
        // throw away the rows a `LEFT JOIN` exists to keep, which is the one rewrite of this kind
        // that changes an answer rather than a cost. Once a chain has taken an outer join, nothing
        // after it is pushed either: the rows above that step are the extended ones.
        let inner_so_far = !has_full
            && !select.joins[..=at]
                .iter()
                .any(|join| join.kind != crate::plan::JoinKind::Inner);
        // **Taken before the step is built, so it can go *on* the join rather than above it.**
        // Everything these tables can answer and the tables to their left could not is, by
        // construction, a condition involving the one being joined here — which is what a join
        // condition is. Above the loop it costs `outer x inner` joined rows; on the loop a pair
        // that fails is dropped before a row exists. Debt #54, and `take_answerable` carries the
        // measurement.
        let answerable = inner_so_far
            .then(|| take_answerable(&mut pending, &entries[..=at + 1], enclosing))
            .flatten();
        // A reordered chain is a comma list: every join inner, every `ON` absent, and no entry a
        // function or a derived table — so the step needs nothing from `select.joins`, whose order
        // no longer matches.
        node = chain_step(
            node,
            select,
            &entries,
            &Step {
                at,
                reordered,
                enclosing,
                scope: &scope,
            },
            answerable,
        )?;
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
    let scope = Scope::chain(entries).under(enclosing);
    let Some(predicate) = take_answerable(pending, entries, enclosing) else {
        return node;
    };
    // Resolved here because a `Filter` holds a resolved predicate; a join resolves its own.
    let Ok(predicate) = resolve(&predicate, &scope) else {
        return node;
    };
    Node::Filter {
        input: Box::new(node),
        predicate,
    }
}

/// Takes from `pending` every conjunct the tables in `entries` can answer, and returns them as one
/// `AND` — **unresolved**, as written.
///
/// The selection half of [`pushdown`], split out because its answer has two destinations and only
/// one of them is a filter. A conjunct a *join step* can answer belongs **on the join**, not above
/// it, and the difference is the whole of debt #54:
///
/// ```text
/// FROM pg_class seq, pg_depend dep WHERE seq.oid = dep.objid     352 ms over 400 relations
///     Filter                       <- n x n joined rows are built, then thrown away
///       Nested Loop
///
/// FROM pg_class seq JOIN pg_depend dep ON seq.oid = dep.objid    3.6 ms over the same
///     Nested Loop
///       Join Filter                <- a pair that fails is dropped before a row is built
/// ```
///
/// Measured at three catalog sizes in `tests/catalog_join_slope.rs`: the comma form costs very
/// nearly what the **bare cross product** costs — 352 ms against 455 ms — while the same join
/// written with `ON` tracks a single catalog scan. It is not the comparison that is quadratic, it
/// is the rows.
///
/// **Unresolved on purpose**: a `Filter` takes a resolved predicate and a join resolves its own
/// `ON` against the scope it builds, so handing a join something already resolved would resolve it
/// twice. Resolution is still *attempted* here, because "can these tables answer it" has no other
/// answer.
fn take_answerable(
    pending: &mut Vec<&Expr>,
    entries: &[(&TableDef, String)],
    enclosing: Option<&Scope<'_>>,
) -> Option<Expr> {
    if pending.is_empty() {
        return None;
    }
    let scope = Scope::chain(entries).under(enclosing);
    let mut ready: Vec<&Expr> = Vec::new();
    pending.retain(|conjunct| {
        if !is_pushable(conjunct) {
            return true;
        }
        if resolve(conjunct, &scope).is_ok() {
            ready.push(conjunct);
            false
        } else {
            true
        }
    });
    let (first, rest) = ready.split_first()?;
    let mut predicate = (*first).clone();
    for next in rest {
        predicate = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(predicate),
            right: Box::new((*next).clone()),
        };
    }
    Some(predicate)
}

/// A single join's condition as written, **plus whatever the `WHERE` contributes to it**.
///
/// `FROM a, b WHERE a.x = b.y` puts the equality in the `WHERE`, where it becomes a filter *above*
/// the loop — so `outer x inner` joined rows are built and then thrown away. On the join, a pair
/// that fails is dropped before a row exists, which is why the identical query written `JOIN … ON`
/// was already linear while this one cost very nearly what a bare cross product costs
/// (`tests/catalog_join_slope.rs`: 352 ms against 455 ms over four hundred relations). Debt #54,
/// and the chain path applies the same rule at its own steps.
///
/// **Copied, not moved.** For an inner join `ON c` and `WHERE c` are the same statement said twice,
/// so leaving it in the `WHERE` cannot change an answer — and the filter above now sees only rows
/// that already matched, so it costs a comparison on the rows that survive rather than on the pairs
/// that never existed. Moving it would mean editing the `Select` this was handed.
///
/// **Inner joins only.** A `WHERE` conjunct applied before an outer join's NULL extension throws
/// away the rows that join exists to keep — the one rewrite of this kind that changes an answer
/// rather than a cost, and the line the chain path draws in the same words.
fn one_joins_condition(
    select: &Select,
    table: Option<(&TableDef, &str)>,
    inner: Option<(&TableDef, &str)>,
    left_join: bool,
) -> Result<Option<Expr>> {
    let only_join = select.joins.first();
    // As written: `USING (a, b)` is the equality it also is, an `ON` is itself, a bare comma join
    // is nothing at all — which is the case this function exists for.
    let written = match (&only_join, table, inner) {
        (Some(join), Some(left), Some(right)) if !join.using.is_empty() => {
            Some(using_condition(&join.using, left, right)?)
        }
        (Some(join), ..) => join.on.clone(),
        _ => None,
    };
    let (Some(left), Some(right)) = (table, inner) else {
        return Ok(written);
    };
    if left_join {
        return Ok(written);
    }
    let using: &[String] = only_join.map_or(&[], |join| &join.using);
    let scope = Scope::joined(left, right, false, using);
    let pending = select.filter.as_ref().map(conjuncts_of).unwrap_or_default();
    Ok(both(written, answerable_in(&pending, &scope, left.0)))
}

/// The conjuncts a two-table scope can answer that are not about the **outer** side alone, as one
/// `AND`.
///
/// That test, rather than "mentions the inner table", because a *join* condition mentions both
/// sides by definition and `mentions_only` is the predicate this file already has. What it
/// separates is `a.x = b.y`, which turns `outer x inner` rows into the ones that match, from
/// `a.x = 1`, which belongs above the loop where it costs one comparison per outer row rather than
/// one per pair.
fn answerable_in(pending: &[&Expr], scope: &Scope<'_>, outer: &TableDef) -> Option<Expr> {
    let mut ready: Vec<&Expr> = Vec::new();
    for conjunct in pending {
        if !is_pushable(conjunct) || mentions_only(conjunct, outer) {
            continue;
        }
        if resolve(conjunct, scope).is_ok() {
            ready.push(conjunct);
        }
    }
    let (first, rest) = ready.split_first()?;
    let mut predicate = (*first).clone();
    for next in rest {
        predicate = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(predicate),
            right: Box::new((*next).clone()),
        };
    }
    Some(predicate)
}

/// `a AND b`, or whichever of the two exists.
fn both(left: Option<Expr>, right: Option<Expr>) -> Option<Expr> {
    match (left, right) {
        (Some(left), Some(right)) => Some(Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        }),
        (only, None) | (None, only) => only,
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
            join.kind,
            width_of(&entries[..=at]),
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
            Expr::Subquery(sub) => sub.correlated || sub.operands.iter().any(in_expr),
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
    // **Identity, not the referable name** — the same distinction the chain above makes, and the
    // reason it matters here too: `s1.things` and `s2.things` are both referable as `things` and
    // are two entries a real server allows. What it refuses is the bare *reference*, which
    // `Scope::entry` does with `42P09`.
    let left_identity = select
        .from
        .as_ref()
        .map_or("", crate::plan::TableRef::identity);
    let right_identity = select
        .joins
        .first()
        .map_or("", |join| join.table.identity());
    if !right_identity.is_empty() && left_identity == right_identity {
        return Err(SqlError::DuplicateTableName(
            crate::catalog::split_qualified(left_identity).1.to_owned(),
        ));
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
/// One step of a join chain: the node to this point, joined to the entry on its right.
///
/// Lifted out of `plan_chain` for its length, and it is a clean seam: everything it needs is the
/// step's index into `entries` and whether the chain was reordered.
/// Where one step of a chain is: which entry, whether the chain was reordered, and the two scopes
/// it resolves against.
///
/// A struct because the argument list reached eight, and these four are one thing — *the position
/// in the chain* — where `node` and the condition are the step's inputs. A call whose seventh and
/// eighth arguments are a `bool` and an `Option` is a call whose arguments get swapped one day.
struct Step<'a> {
    at: usize,
    reordered: bool,
    enclosing: Option<&'a Scope<'a>>,
    scope: &'a Scope<'a>,
}

fn chain_step(
    node: Node,
    select: &Select,
    entries: &[(&TableDef, String)],
    step: &Step<'_>,
    answerable: Option<Expr>,
) -> Result<Node> {
    let Step {
        at,
        reordered,
        enclosing,
        scope,
    } = *step;
    let inner = entries[at + 1].0;
    let outer_columns = width_of(&entries[..=at]);
    if reordered {
        return join_node(
            node,
            answerable.as_ref(),
            crate::plan::JoinKind::Inner,
            outer_columns,
            scope,
            inner,
            None,
        );
    }
    let join = &select.joins[at];
    // The written `ON` **and** whatever the `WHERE` contributes to this step. For an inner join
    // the two are the same thing said in two places, which is why they may be joined with `AND`;
    // for an outer join nothing is contributed, because the caller does not offer any.
    let on = both(join.on.clone(), answerable);
    // Implicitly `LATERAL`: the entries strictly to this one's left, which at step `at` is
    // everything up to and including the outer side of this join.
    let left = Scope::chain(&entries[..=at]).under(enclosing);
    let inner_function = source_function(Some(&join.table), inner, &left)?;
    join_node(
        node,
        on.as_ref(),
        join.kind,
        outer_columns,
        scope,
        inner,
        inner_function
            .as_ref()
            .or_else(|| join.table.derived_plan()),
    )
}

/// How wide the rows to a join's left are — what an inner row kept by a `FULL JOIN` is extended
/// with, and the one thing a full join cannot learn from an outer row because it may have none.
fn width_of(entries: &[(&TableDef, String)]) -> usize {
    entries.iter().map(|(def, _)| def.row_schema().len()).sum()
}

fn join_node(
    outer: Node,
    on: Option<&Expr>,
    kind: crate::plan::JoinKind,
    outer_columns: usize,
    scope: &Scope<'_>,
    inner: &TableDef,
    inner_plan: Option<&Node>,
) -> Result<Node> {
    // **A full join reads its inner side into memory whatever the `ON` says.** A probe answers
    // "which inner row matches this outer row" and nothing else; a full join also has to answer
    // "which inner rows matched *nobody*", and that question is only answerable against a set the
    // node holds. So the probe is given up here rather than in `probe_for`, which is about cost.
    let full = kind == crate::plan::JoinKind::Full;
    // A computed relation has no primary key and no index, so there is nothing to probe with and
    // the inner side is read once into memory like any other unindexed join. Decided here rather
    // than left to `probe_for`, so that a view can never be reached through a key. A **derived
    // table** is the same case for the same reason: its rows come from a plan.
    let inner_view = pg_catalog::view_of(inner);
    let probe = on
        .filter(|_| !full && inner_view.is_none() && inner_plan.is_none())
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
        left_join: kind != crate::plan::JoinKind::Inner,
        keep_right: full,
        outer_columns,
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
///
/// **This used to refuse a subquery here**, naming it — `docs/plans/phase-12-subquery.md` §4
/// deferred the whole write path and the refusal was that promise kept. The `WHERE` half landed
/// with `tests/write_in_subquery.rs` and the `SET` half is `tests/update_set_subquery.rs`; what
/// makes it work is that the assignment's subqueries are planned and the uncorrelated ones run
/// **once, before the first row** (`exec::dml::plan_assignment_subqueries`), so by the time this
/// runs there is a plan behind each one. A `RETURNING` list still refuses, and still through
/// `subquery::refuse_in`.
pub(super) fn resolve_against_scope(expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
    resolve(expr, scope)
}

/// `ORDER BY x` where `x` is an output alias means the expression that alias names.
/// [`dealias`], and then PostgreSQL's other half: an output column named by a **derived** name
/// beats an input column of that name too.
///
/// Measured on 19beta1, and the two spellings differ by the alias alone:
///
/// ```text
/// SELECT m::text      FROM t ORDER BY m   -- sorts by the TEXT: m::text is *named* m
/// SELECT m::text AS x FROM t ORDER BY m   -- sorts by the enum, the input column
/// ```
///
/// **Only a projection whose derived name is not its own column's** is substituted here, which is
/// the whole of what differs: a target-list entry that *is* the bare column produces the same
/// expression either way, so leaving it alone changes no answer and keeps every aggregated query
/// on the path it was already taking — `SELECT g, count(*) … GROUP BY g ORDER BY g` resolves `g`
/// as the grouping key, which is what the rewrite below expects to see.
pub(super) fn deshadow(expr: &Expr, select: &Select) -> Expr {
    let aliased = dealias(expr, select);
    if aliased != *expr {
        return aliased;
    }
    let Expr::Column { table: None, name } = expr else {
        return aliased;
    };
    for item in &select.projection {
        if let SelectItem::Expr {
            expr: projected,
            alias: None,
            ..
        } = item
            && !matches!(projected, Expr::Column { name: own, .. } if own == name)
            && figure_column_name(projected) == *name
        {
            return projected.clone();
        }
    }
    aliased
}

pub(super) fn dealias(expr: &Expr, select: &Select) -> Expr {
    let Expr::Column { table: None, name } = expr else {
        return expr.clone();
    };
    for item in &select.projection {
        if let SelectItem::Expr {
            expr: aliased,
            alias: Some(alias),
            ..
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
        // **A type lowering already settled is kept; the rest is settled here.** A constructor
        // over constants now reaches this arm too — it keeps its node so that a real server's
        // `ARRAY[1, 2]` prints as one (`debts-v1.1.md` #42) — and what it carries is what was
        // *written*, which a recomputation cannot get back: every element of
        // `ARRAY['a'::name, 'b'::name]` is a `Datum::Text` by the time it is an expression, so
        // widening over them answered `text[]` where a real server says `name[]`. Passing `None`
        // here was correct exactly while this arm could not see a folded constructor.
        //
        // An element whose type needs a scope still settles here, which is what `None` means.
        Expr::Array { elements, element } => {
            let mut resolved = Vec::with_capacity(elements.len());
            for expr in elements {
                resolved.push(resolve(expr, scope)?);
            }
            let element = array_element_type(&resolved, *element, scope)?;
            // **`void` has no array type**, and a subscript and an `unnest` both need an array
            // built first, so three of the four array shapes are gated right here. Measured over
            // the probe list's 100 spellings: `void` is the only one 19beta1 refuses
            // (`tests/captures/pg19_array_of_void.txt`), so the test is the type and not
            // `ArrayValue::array_of(ty).is_none()` — that would also refuse `lquery`,
            // `int2vector` and `oidvector`, which a real server builds arrays of.
            if element == Some(ColumnType::Void) {
                return Err(SqlError::NoArrayType(ColumnType::Void.name()));
            }
            Expr::Array {
                elements: resolved,
                element,
            }
        }
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
        // Every one of these reads its argument as `text` except `octet_length`, which reports
        // the storage — `octet_length(c)` is `4` where `length(c)` is `1`, measured.
        Expr::Scalar { func, operand } => {
            let operand = resolve(operand, scope)?;
            // **A scalar function has an overload list and this read everything as `text`.**
            // Measured over the wire v3 probe list's 100 spellings
            // (`tests/captures/pg19_length_overloads.txt`): `length` answers eleven of them and
            // this node answered ninety-nine, because `read_as_text` gave every operand `text`'s
            // reading — the same shape `||` had. Thirteen of the seventeen diverging rows were the
            // *worse* direction, a value where a real server raises: `length(daterange)` came back
            // as the range's **upper bound**.
            if let Ok(ty) = expr_type(&operand, scope)
                && !scalar_accepts(*func, ty)
            {
                return Err(SqlError::UndefinedFunctionTypes(format!(
                    "{}({})",
                    func.name(),
                    ty.name()
                )));
            }
            Expr::Scalar {
                func: *func,
                operand: Box::new(if *func == crate::plan::ScalarFunc::OctetLength {
                    operand
                } else {
                    read_as_text(operand, scope)
                }),
            }
        }
        // **Permission is checked here and not at lowering**, because the operand's type is not
        // known until it is resolved against a scope: `'2020-01-01'::date::int` folds and is
        // `42846` already, and `d::int` over a `timestamptz` column has to reach the same answer.
        // `pg_cast` is the table, so the two paths cannot disagree.
        Expr::Cast {
            operand,
            to,
            typmod,
        } => {
            let operand = resolve(operand, scope)?;
            // **A cast to the type the operand already has is not a node.** PostgreSQL builds no
            // `RelabelType` for it, so `pg_get_expr` has nothing to print: a generated column
            // written `((upper(t))::text)` is stored `upper(t)`, and `((t)::text)` is stored `t`.
            // Measured across the whole family, `tests/corpus/pg19_deparse_census.txt`'s group E —
            // and it is not a printing rule, which is why it lives here and not in the deparser:
            // an index key that resolves to a bare column stops being an expression key, and
            // `pg_index.indkey`, `indexprs` and `pg_get_indexdef` all move with it.
            //
            // **The modifier is part of the type.** `(nn)::numeric(10,2)` over a `numeric(10,2)`
            // is elided and `(nn)::numeric` over the same column is *kept* — dropping a typmod is
            // a coercion, not a no-op — as are `(v)::character varying(5)` over a `varchar(10)`
            // and `(n)::numeric(10,2)` over a bare `numeric`. Measured, all four.
            //
            // **An unknown literal has no type of its own**, so the cast that gives it one is the
            // node: `('a')::text` is stored `'a'::text` and `(NULL)::text` is `NULL::text`. The
            // exception is [`is_unknown_literal`] and it is PostgreSQL's own.
            //
            // Before the coercion below, not after: [`read_as_text`] turns a `bpchar` operand into
            // something of type `text`, and `(b)::text` over a `character(4)` is a cast a real
            // server keeps. Asking after the wrap would elide exactly the one that must stay.
            if !is_unknown_literal(&operand)
                && expr_type(&operand, scope).is_ok_and(|from| from == *to)
                && typmod_of(&operand, scope) == *typmod
            {
                return Ok(operand);
            }
            // **A cast *out of* `bpchar` strips the padding first, and a cast into one does not.**
            // Measured with `octet_length`, which is the only reader that can tell:
            // `c::varchar`, `c::name` and `c::text` over a `character(4)` holding `x` are 1 byte,
            // `c::char(2)` is 2 and `c::char(6)` is 6 — truncated or padded to the target's own
            // width — and `c::bpchar` with no width is 4, the value unchanged. So the strip is
            // exactly "leaving the type", which is what [`read_as_text`] inserts.
            // **And `xml` is the second target that does not strip**, measured on 19beta1,
            // 2026-09-10: `'<' || 'ab'::character(4)::xml || '>'` is `<ab  >` and
            // `octet_length('ab'::character(4)::xml::text)` is **4**. The original measurement
            // covered `varchar`, `name`, `text`, `char(n)` and bare `bpchar` — every target that
            // *is* a character type — and read the rule off them as "everything but `bpchar`".
            // `xml` is not a character type and keeps the value it was given.
            //
            // **Named rather than generalised.** The honest statement of what is measured is
            // "`bpchar` and `xml` do not strip"; whether a cast to a `date` or an `int4` should
            // see the padding has not been put to the oracle, and those targets' parsers tolerate
            // trailing blanks either way, so nothing here turns on a guess
            // (`debts-v1.1.md` #44, group 4).
            let operand = if matches!(*to, ColumnType::Bpchar | ColumnType::Xml) {
                operand
            } else {
                read_as_text(operand, scope)
            };
            if let Ok(from) = expr_type(&operand, scope)
                && !is_already_of_type(&operand, *to)
                && !pg_catalog::casts_to(from, *to)
            {
                return Err(SqlError::CannotCast {
                    from: from.name(),
                    to: to.name(),
                });
            }
            Expr::Cast {
                operand: Box::new(operand),
                to: *to,
                typmod: *typmod,
            }
        }
        Expr::ToText { operand, .. } => {
            let operand = resolve(operand, scope)?;
            // **`::text` over something already `text` is the same no-op the `Cast` arm elides**,
            // and it arrives here rather than there because a cast whose target is `text` lowers
            // to this node. `((upper(t))::text)` is stored `upper(t)` on a real server and
            // `(((t)::text))` is a *column* index, not an expression one. Measured, group E.
            //
            // `bpchar` is the operand that must not take this exit — `(b)::text` over a
            // `character(4)` is a cast a real server keeps, and it is what `strip_blanks` below
            // exists for — and so is an unknown literal, whose type this cast is what gives it.
            if !is_unknown_literal(&operand)
                && expr_type(&operand, scope).is_ok_and(|from| from == ColumnType::Text)
            {
                return Ok(operand);
            }
            let strip_blanks = matches!(expr_type(&operand, scope), Ok(ColumnType::Bpchar));
            // The operand's output function, where the operand is an enum column: the label, not
            // the ordinal the row holds.
            let enum_labels = match &operand {
                Expr::Ordinal { at, .. } => match scope.enum_at(*at).map(|def| &def.kind) {
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
            // **An aggregate is reconciled here too, and it has to be here**: `reconcile` below
            // is given two resolved expressions and no scope, and an aggregate's *argument* is
            // still an unresolved column at that point — `sum(salary)` can only be typed where
            // `salary` can, which is here.
            if let Some((left, right)) = reconcile_aggregate(*op, &left, &right, scope)? {
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
            // **An array's `=` is its element's, and the element must have a *btree* equality.**
            // Measured for six element types at once — `point`, `line`, `path`, `xml`, `json` and
            // `circle` — and the sentence names the **element**, not the array:
            // `'{…}'::circle[] = '{…}'::circle[]` is
            // `42883 could not identify an equality operator for type circle` while the scalar
            // `'…'::circle = '…'::circle` is `t`. `'{a}'::text[] = '{a}'::text[]` is the control
            // and answers. The same list `SELECT DISTINCT` and `count(DISTINCT)` read, so a type
            // cannot be refused by one and answered by another.
            if op.is_comparison()
                && let Ok(ty) = expr_type(&left, scope)
                && let Some(element) = esker_keys::array::ArrayValue::element_of(ty)
                && !crate::value::has_equality_operator(ty)
            {
                return Err(SqlError::NoEqualityOperator(element.name()));
            }
            // **A `jsonb` comparison becomes `jsonb_compare(a, b) <op> 0`.** All six operators are
            // then one implementation and the ordinary `int4` comparison does the rest. It is
            // rewritten here and not at lowering because the parser sees a *cast* and this sees a
            // *type*, so a `jsonb` column compares like a `jsonb` literal — the provenance split
            // that has cost this lane four units.
            if op.is_comparison()
                && let Ok(ty) = expr_type(&left, scope)
                && ty == ColumnType::Jsonb
            {
                return Ok(Expr::Binary {
                    op: *op,
                    left: Box::new(Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                        func: CatalogFunc::JsonbCompare,
                        args: vec![left, right],
                    }))),
                    right: Box::new(Expr::Literal(Literal::Typed(Box::new(Datum::Int4(0))))),
                });
            }
            // **And a scalar whose `=` does not exist at all**, which is a different list from
            // the one above and from `same_family`: `same_family(polygon, polygon)` is true —
            // they are the same type — and there is still no `polygon = polygon` on a real
            // server. Four types, and `polygon` is the one that reads like an oversight because
            // every other geometric shape has the operator.
            // **The measured table decides, and `missing_symbol` names the operator being asked
            // for.** `IS DISTINCT FROM` needs `=`, so it is refused where `=` is; `<>` asks for
            // `<>`, and `point` has one where it has no `=` — which is the cell a single
            // "these types have no comparisons" predicate got wrong in both directions.
            if op.is_comparison()
                && let Ok(ty) = expr_type(&left, scope)
                && !crate::value::operator_exists(op.missing_symbol(), ty)
            {
                return Err(SqlError::UndefinedOperator {
                    left: ty.name().to_owned(),
                    // **`IS DISTINCT FROM` names `=`**, because `=` is the operator it is missing:
                    // a real server says `operator does not exist: json = json` for it and not the
                    // spelling the user wrote. `IS NOT DISTINCT FROM` the same. Measured.
                    op: match op {
                        BinaryOp::Distinct | BinaryOp::NotDistinct => BinaryOp::Eq.symbol(),
                        other => other.symbol(),
                    },
                    right: expr_type(&right, scope).unwrap_or(ty).name().to_owned(),
                });
            }
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
            any,
        } => resolve_in_list(operand, list, *negated, *any, scope)?,
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: Box::new(resolve(operand, scope)?),
            negated: *negated,
        },
        // Both sides resolve, and **neither is reconciled against the other**: an array's elements
        // have no type of their own here — they are text in the catalog — so what gives them one
        // is the operand, at evaluation, exactly as the plan-time form types its list against the
        // operand it is compared with.
        Expr::QuantifiedArray {
            operand,
            op,
            all,
            array,
        } => {
            let operand = resolve(operand, scope)?;
            let array = resolve(array, scope)?;
            // **A right-hand side that cannot hold more than one value**, which is PostgreSQL's
            // `42809` and not a missing operator: the operator exists and the *shape* is wrong,
            // and the sentence it uses names both quantifiers at once. Measured, `1 = ALL (1)`.
            if let Ok(right) = expr_type(&array, scope)
                && !holds_many(right)
            {
                return Err(SqlError::QuantifierNeedsArray);
            }
            // **An array that knows its element type is not `unknown`.** `'{1,2}'` is an unknown
            // literal and takes the operand's type at evaluation, which is the arm below; an
            // `ARRAY['1','2']` is a `text[]` **value** — `pg_typeof` says so on a real server —
            // and `id = ANY(...)` of one is `42883 operator does not exist: bigint = text`,
            // naming the *element* type. Without this the evaluator read each element at the
            // operand's type and answered a row, which is a wrong row set rather than a wrong
            // error.
            if let Some(element) = quantified_element_type(&array, scope)
                && let Ok(left) = expr_type(&operand, scope)
                // **An unknown literal on the left takes the element's type**, which is the same
                // coercion a real server applies and the same one this arm's own comment claims
                // for the *right*. A bare `NULL` is `unknown` on 19beta1 and `text` here, so
                // without this `NULL = ALL (ARRAY[1, 2])` raised
                // `42883 operator does not exist: text = integer` where a real server answers
                // NULL — the check written for `id = ANY(ARRAY['1','2'])` firing in the opposite
                // direction. The measured refusal is still refused: `1 = ALL (ARRAY['a'])` has an
                // *integer* literal on the left and stays `42883`.
                && !is_unknown_literal(&operand)
                && !same_family(left, element)
            {
                return Err(SqlError::UndefinedOperator {
                    left: left.name().to_owned(),
                    // **The operator that is missing, not the one this arm used to assume.**
                    // `1 = ALL (ARRAY['a'])` is `operator does not exist: integer = text` on
                    // 19beta1 and `1 > ALL (ARRAY['a'])` names `>`; hard-coding `=` was right
                    // only while `=` was the sole spelling that reached here.
                    op: op.symbol(),
                    right: element.name().to_owned(),
                });
            }
            Expr::QuantifiedArray {
                operand: Box::new(operand),
                op: *op,
                all: *all,
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
            operand,
            branches,
            otherwise,
        } => resolve_case(operand.as_deref(), branches, otherwise.as_deref(), scope)?,
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
                let arg = resolve(arg, scope)?;
                // **A function reads a `bpchar` argument as `text` exactly when its result *is*
                // `text`.** `upper(c)` is `X` and `substr(c, 1, 4)` is `x`; `greatest(d, d)` is a
                // `character(2)` and comes back `y ` **padded**, because a production that keeps
                // its argument's type never coerced it. That is the same fact as their typmod
                // surviving (`typmod_of`, `debts-v1.1.md` #28) seen from the value side, and it
                // is why the exclusions below are a list of type-preserving functions rather than
                // a list of names.
                //
                // `concat` is the one that breaks the pattern and it is measured: its result is
                // `text` and it still pads, because it takes `"any"` and goes through the output
                // function — `concat(c, 'z')` is `x   z`. The type guard inside [`read_as_text`]
                // is what leaves `->`, `||` over `jsonb` and every non-text argument alone.
                args.push(
                    if matches!(
                        call.func,
                        CatalogFunc::Concat
                            | CatalogFunc::NullIf
                            | CatalogFunc::Greatest
                            | CatalogFunc::Least
                            // **`pg_typeof` does not read its argument at all**, which is the
                            // property this list is about: the exclusions are the functions whose
                            // result is *not* `text`, and `pg_typeof`'s is a `regtype`. Reading a
                            // `bpchar` argument as `text` on the way in made it answer `text`,
                            // which is a report about the coercion this function inserted rather
                            // than about the expression the user wrote — `debts-v1.1.md` #33.
                            //
                            // It is the same seam the three instances before it were, one layer
                            // over: `output_columns` describes the column from the expression and
                            // `pg_typeof` described it from the expression *plus a cast of its
                            // own*, so one function answered two things. The wire half was right
                            // the whole time, which is why no corpus row caught it — a corpus
                            // compares what a column says, and both readers are only visible
                            // together.
                            | CatalogFunc::PgTypeof
                    ) {
                        arg
                    } else {
                        read_as_text(arg, scope)
                    },
                );
            }
            // **An operator this crate spells as a function still has to exist for its operand.**
            // `&&` and `@>` are lowered to catalog functions, so the binary-comparison check above
            // never sees them, and by the time `exec::cursor` has them the type is a `Datum` and
            // gone — which is why the refusals read `text && text` for a `json` operand and
            // `@> over point` for a `point` one.
            //
            // The question is per operator **and** per type, because no two of these operators
            // agree about which types have them: `polygon @> polygon` answers where
            // `point @> point` does not, and `polygon && polygon` answers where `jsonb && jsonb`
            // does not (`value::operator_exists`, measured cell by cell).
            // **The spelling the user wrote is recoverable here**, though `CatalogFunc::name`
            // collapses both containments to `@>` for the evaluator's dispatch: `parse::lower`
            // sends a written `@>` to `HstoreContains` and a written `<@` to `RangeContains`
            // **with its arguments flipped**, and each of the two has exactly one origin. So the
            // refusal can say `json <@ json`, which is what a real server says, rather than naming
            // the operator this crate rewrote it into.
            let containment = match call.func {
                CatalogFunc::RangeContains => Some(("<@", true)),
                CatalogFunc::HstoreContains => Some(("@>", false)),
                CatalogFunc::RangeOverlaps | CatalogFunc::SameAs => Some((call.func.name(), false)),
                _ => None,
            };
            if let Some((symbol, flipped)) = containment {
                let (written_left, written_right) = if flipped {
                    (args.get(1), args.first())
                } else {
                    (args.first(), args.get(1))
                };
                // **A document containment is the document's**, and `@>` alone cannot say which of
                // the three types it was written for — an hstore, a range or a `jsonb` — because
                // by evaluation a `jsonb` is a `Datum::Text`. Rewritten here, where the operand's
                // type is known, exactly as the six comparisons are. `args` is already in
                // `@>` order, so the flip is undone once, here, and never again.
                // The polygon half of the same seam: `@>`, `<@` and `&&` all reach here as one
                // of the three shared catalog functions, and a polygon is canonical text by
                // evaluation just as a `jsonb` is.
                // **Only the three containment spellings**: `SameAs` reaches this block too, and
                // without this guard `~=` was rewritten into `polygon_contains`, which answers `t`
                // for the same points in any order — a shuffled square compared *same* where
                // PostgreSQL says it is a different polygon.
                if matches!(symbol, "@>" | "<@" | "&&")
                    && let Some(operand) = written_left
                    && let Ok(ColumnType::Polygon) = expr_type(operand, scope)
                {
                    let (left, right) = if flipped {
                        (args[1].clone(), args[0].clone())
                    } else {
                        (args[0].clone(), args[1].clone())
                    };
                    return Ok(Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                        func: if symbol == "&&" {
                            CatalogFunc::PolygonOverlaps
                        } else {
                            CatalogFunc::PolygonContains
                        },
                        args: vec![left, right],
                    })));
                }
                if matches!(symbol, "@>" | "<@")
                    && let Some(operand) = written_left
                    && let Ok(ColumnType::Jsonb) = expr_type(operand, scope)
                {
                    let (left, right) = if flipped {
                        (args[1].clone(), args[0].clone())
                    } else {
                        (args[0].clone(), args[1].clone())
                    };
                    return Ok(Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                        func: CatalogFunc::JsonbContains,
                        args: vec![left, right],
                    })));
                }
                if let Some(operand) = written_left
                    && let Ok(left) = expr_type(operand, scope)
                    && !crate::value::operator_exists(symbol, left)
                {
                    let right = written_right
                        .and_then(|arg| expr_type(arg, scope).ok())
                        .unwrap_or(left);
                    return Err(SqlError::UndefinedOperator {
                        left: left.name().to_owned(),
                        op: symbol,
                        right: right.name().to_owned(),
                    });
                }
            }
            // **`pg_typeof` is answered here, from the argument's *declared* type, always.**
            //
            // It used to read the datum with three exceptions carved out of it — an enum column,
            // whose storage is an `int2` and whose name is the one thing a client must not be told
            // wrong (ADR 0050); and `hstore[]`, `json` and `jsonb`, which are canonical
            // `Datum::Text`s. Every exception was the same fact and the list was never going to
            // stop growing: `name`, `varchar` and `bpchar` are that `Datum::Text` too, a `cidr` and
            // an `inet` are one `Datum::Inet`, a `void` is an empty string. Four units of this
            // queue each declared a slice of it before the shape was one rule.
            //
            // **The argument stays an argument**, which the folded exceptions did not manage:
            // `pg_typeof(unnest(ARRAY['a','b']))` is **two** rows on a real server, measured, and
            // folding the call to a constant would answer one. So the resolved type rides along as
            // a second argument and the evaluator answers that, leaving the first to be evaluated
            // exactly as it was.
            // **`NULLIF` is a comparison, so it is reconciled like one.** Its two arguments meet
            // under `=` and everything `reconcile` does for `c = 'x'` has to happen here too —
            // the one that shows is a `character(n)`: a `bpchar` comparison ignores trailing
            // blanks, which this crate implements by padding the *other* side to the column's
            // width (`blank_pad`), so `nullif(c, 'x')` over a `character(4)` holding `x` is NULL
            // on a real server and was `x   ` here. One rule, called from its second caller,
            // rather than a second copy of it (`debts-v1.1.md` #31).
            if call.func == CatalogFunc::NullIf && args.len() == 2 {
                let right = args.pop().unwrap_or(Expr::Literal(Literal::Null));
                let left = args.pop().unwrap_or(Expr::Literal(Literal::Null));
                let (left, right) = reconcile(BinaryOp::Eq, left, right)?;
                args.push(left);
                args.push(right);
            }
            // **`GREATEST`/`LEAST` needs a comparison function, and eleven types have none.**
            // Checked here rather than in the evaluator because PostgreSQL's is at *executor
            // init* (`ExecInitExprRec`, `execExpr.c`), so `WHERE false` refuses too — an
            // evaluator-side gate would leave a row-less query answering.
            //
            // **Not `min`/`max`'s list, and it must never become one.** That one is a `pg_proc`
            // lookup in the parser; this is a `btree` opclass lookup. Measured, they disagree in
            // both directions: `min(point[])` is answered and `GREATEST(point[], point[])` is
            // refused; `min(hstore)` is refused and `GREATEST(hstore)` is answered
            // (`tests/captures/pg19_min_max_greatest.txt`).
            //
            // The list was measured **whole** — `GREATEST` and `LEAST` asked of all 166 type
            // spellings the wire v3 probe list has, of which exactly these twenty refuse — rather
            // than extended a type at a time, which is how the arm above it came to carry a
            // scalar's rule on its array (`debts-v1.1.md`, wire v3 families F1b and F2).
            if matches!(call.func, CatalogFunc::Greatest | CatalogFunc::Least) {
                for arg in &args {
                    let ty = expr_type(arg, scope)?;
                    if !comparable_by_btree(ty) {
                        return Err(SqlError::NoComparisonFunction(ty.name()));
                    }
                }
            }
            // **And a pair PostgreSQL has no `||` for is `42883`.** The chain this replaced ended
            // in a `text` fallback, so every pair concatenated: measured over the probe list's 100
            // spellings asked three ways, 91 of the 300 shape-rows answered `text` where a real
            // server has no operator at all (`tests/captures/pg19_concat_operator.txt`).
            //
            // **Before the match, not inside it**, because the arm that wraps an element beside an
            // array would otherwise consume the call first: `'{"x"}'::"char"[] || 'a'::text` was
            // wrapped into an array concatenation and answered `{x,a}` where 19beta1 has no
            // operator for the pair. A gate placed after the rewrite is a gate on the rewrite.
            //
            // **Decided from the declared types and not from the datums**, because the evaluator's
            // `text_concat` cannot: a `json`, an `xml` and a `void` are all `Datum::Text` here, so
            // it asks `column_type()`, is told `text`, and concatenates. That is the borrowed
            // representation reaching a second decision, and the plan is the only place that still
            // knows what the operand is.
            if call.func == CatalogFunc::HstoreConcat && args.len() == 2 {
                let left = concat_operand_type(&args[0], scope)?;
                let right = concat_operand_type(&args[1], scope)?;
                if left != Some(ColumnType::Char)
                    && right != Some(ColumnType::Char)
                    && concat_answer(left, right).is_none()
                {
                    return Err(SqlError::UndefinedOperator {
                        left: left.map_or("unknown", ColumnType::name).to_owned(),
                        op: "||",
                        right: right.map_or("unknown", ColumnType::name).to_owned(),
                    });
                }
                // **An operand the pair settled on `text` for is coerced here**, which is what a
                // real server's parser does and what the evaluator cannot: `'a=>1'::hstore ||
                // 'x'::text` is string concatenation on 19beta1 (`"a"=>"1"x`) while
                // `'a=>1'::hstore || 'b=>2'` is an hstore merge, and the two reach the evaluator
                // as the same pair of `Datum`s — an `unknown` has no mark on it once it is a
                // value. Only the operands with a `||` of their own are wrapped: the rest already
                // render as themselves.
                if concat_answer(left, right) == Some(ColumnType::Text) {
                    for (at, ty) in [(0, left), (1, right)] {
                        if let Some(ty) = ty
                            && !concat_stringy(ty)
                            && concat_pair(ty, ty).is_some()
                        {
                            args[at] = Expr::Cast {
                                operand: Box::new(args[at].clone()),
                                to: ColumnType::Text,
                                typmod: crate::value::NO_TYPMOD,
                            };
                        }
                    }
                }
            }
            match (call.func, args.first()) {
                (CatalogFunc::PgTypeof, Some(arg)) if args.len() == 1 => {
                    let named = pg_typeof_of(arg, scope)?;
                    Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                        func: CatalogFunc::PgTypeof,
                        args: vec![
                            args.swap_remove(0),
                            Expr::Literal(Literal::Typed(Box::new(named))),
                        ],
                    }))
                }
                // **A `->` over a json or jsonb column**, which lowered to the hstore spelling
                // because only the plan knows the type — a `jsonb` is a canonical `Datum::Text`
                // here and so is a string, so the values cannot decide it. Rewritten to the
                // document operator *here* rather than dispatched in the evaluator, so that the
                // declared type follows from the same decision: `pg_typeof(payload->'b')` is
                // `jsonb` and `pg_typeof(doc->'a')` is `json`, measured, and an evaluator-only
                // dispatch answered `text` for both.
                (CatalogFunc::HstoreFetch, Some(_)) => {
                    let func = arrow_fetch(call.func, &args, scope);
                    Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall { func, args }))
                }
                // **A `"char"` operand makes `||` ambiguous rather than missing.** A real server
                // has a candidate at every string width and category `Z` picks none of them, so it
                // is `42725` and not the `42883` a wrong type gets — measured. Decided here
                // because the evaluator sees a `Datum::Text` for a `"char"` and cannot tell.
                // **An element beside an array is wrapped into a one-element array**, which is
                // what makes the two NULL rules one rule. Measured: `ARRAY[1,2] || NULL::int4` is
                // `{1,2,NULL}` and `NULL::int4[] || ARRAY[1]` is `{1}` — the same NULL is a value
                // on one side of the operator and an absence on the other, and the evaluator
                // cannot tell them apart, because a NULL datum carries no type. Decided here,
                // where the declared types are, so that the evaluator has one rule: a NULL array
                // contributes nothing.
                (CatalogFunc::HstoreConcat, Some(_))
                    if args.len() == 2 && concat_element_side(&args, scope).is_some() =>
                {
                    let at = concat_element_side(&args, scope).unwrap_or(0);
                    // **The element the pair *answers* with, not the one the array happens to
                    // hold.** `'a'::text || '{x}'::varchar[]` is `text[]` on 19beta1 — the scalar
                    // instantiates the polymorphic pair when it comes first — so wrapping the
                    // scalar in the array's own element type declared `character varying[]` and
                    // was measured wrong. [`concat_pair`] is the only thing that knows which.
                    let element = expr_type(&args[0], scope)
                        .ok()
                        .zip(expr_type(&args[1], scope).ok())
                        .and_then(|(left, right)| concat_pair(left, right))
                        .or_else(|| expr_type(&args[1 - at], scope).ok())
                        .and_then(esker_keys::array::ArrayValue::element_of);
                    let mut args = args;
                    let operand = args[at].clone();
                    args[at] = Expr::Array {
                        elements: vec![operand],
                        element,
                    };
                    Expr::CatalogFunc(Box::new(crate::plan::CatalogFuncCall {
                        func: call.func,
                        args,
                    }))
                }
                (CatalogFunc::HstoreConcat, Some(_))
                    if args
                        .iter()
                        .any(|arg| expr_type(arg, scope) == Ok(ColumnType::Char)) =>
                {
                    return Err(SqlError::AmbiguousConcat {
                        left: concat_operand_name(args.first(), scope),
                        right: concat_operand_name(args.get(1), scope),
                    });
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
        // **The operand is resolved and the clause kept.** This function's tail is
        // `other => other.clone()`, so a node with a child that is *not* listed here keeps an
        // unresolved `Expr::Column` inside it and reaches the row evaluator as an internal error
        // — which is what a new one-child variant costs, and the compiler cannot say so.
        Expr::Collate { operand, collation } => Expr::Collate {
            operand: Box::new(resolve(operand, scope)?),
            collation: collation.clone(),
        },
        Expr::Subquery(sub) => {
            let mut resolved = sub.clone();
            resolved.operands = sub
                .operands
                .iter()
                .map(|operand| subquery_operand(operand, sub, scope))
                .collect::<Result<Vec<_>>>()?;
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
    any: bool,
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
        && scope.enum_at(*at).is_some()
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
            any,
        });
    }
    // **A list of two or more follows the assignment rule; a list of one is an `=`.**
    // Measured on 19beta1 over all three `reg*` types (`tests/captures/pg19_reg_comparison.txt`):
    //
    //     c IN ('ra','rb')   2 rows      c IN ('ra')   22P02 invalid input syntax for type oid
    //     c IN ('ra', 1)     1 row       c = 'ra'      22P02
    //
    // A multi-item `IN` becomes a `ScalarArrayOpExpr` whose array is built through the **type's
    // input function**, so the names resolve; a single-item one is rewritten to `=` and goes
    // through `oideq`, whose operand is an `oid`. Both halves are one rule and this had only the
    // first — `c IN ('ra')` answered where a real server refuses — and only for `regclass`, so
    // `regproc IN ('int4in','int8in')` and the `regtype` pair took the *comparison's* rule and
    // refused where a real server answers (wire v3 family F8, `debts-v1.1.md` #41).
    //
    // Decided **before** the common-type coercion below: that one gives every `unknown` the
    // list's type, which for a `reg*` operand means reading the name as an oid — the comparison's
    // rule, arriving one step too early.
    if !any
        && items.len() >= 2
        && let Ok(reg @ (ColumnType::RegClass | ColumnType::RegProc | ColumnType::RegType)) =
            expr_type(&operand, scope)
    {
        let mut cast = Vec::with_capacity(items.len());
        for item in items {
            cast.push(match item {
                Expr::Literal(Literal::String(_)) => Expr::Cast {
                    operand: Box::new(item),
                    to: reg,
                    typmod: crate::value::NO_TYPMOD,
                },
                other => other,
            });
        }
        return Ok(Expr::InList {
            operand: Box::new(operand),
            list: cast,
            negated,
            any,
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
    // **`IN` is `=`, so a type with no `=` cannot be on either side of one.** The written
    // comparison asks this in `resolve`; a list is the same question with the operator implied,
    // and asking it in only one of the two places is how `polygon IN (polygon)` answered while
    // `polygon = polygon` refused, in the same build.
    if let Ok(ty) = expr_type(&operand, scope)
        && !crate::value::operator_exists(BinaryOp::Eq.symbol(), ty)
    {
        return Err(SqlError::UndefinedOperator {
            left: ty.name().to_owned(),
            op: BinaryOp::Eq.symbol(),
            right: ty.name().to_owned(),
        });
    }
    Ok(Expr::InList {
        operand: Box::new(operand),
        list: resolved,
        negated,
        any,
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
    operand: Option<&Expr>,
    branches: &[crate::plan::CaseBranch],
    otherwise: Option<&Expr>,
    scope: &Scope<'_>,
) -> Result<Expr> {
    // **The simple form's operand decides what a `WHEN` *is*.** With one, a `WHEN` is a value
    // compared against it and takes the operand's type the way a comparison's other side does —
    // `CASE t WHEN 'x' THEN …` over a `text` column stores `WHEN 'x'::text`, measured. Without
    // one, a `WHEN` is a condition and must be boolean. The two are checked apart below, because
    // running the boolean check over a simple form's values would refuse every one of them.
    let operand = match operand {
        Some(expr) => Some(Box::new(resolve(expr, scope)?)),
        None => None,
    };
    let operand_type = operand
        .as_deref()
        .and_then(|expr| expr_type(expr, scope).ok());
    let mut otherwise = match otherwise {
        Some(expr) => Some(Box::new(resolve(expr, scope)?)),
        None => None,
    };
    let mut resolved = Vec::with_capacity(branches.len());
    for branch in branches {
        let mut when = resolve(&branch.when, scope)?;
        // A condition that is not boolean is `42804` here rather than a row that quietly
        // never matches. An **`unknown`** condition is left alone, and that is not a
        // detail: `CASE WHEN NULL THEN 'a' ELSE 'b' END` is `b` on a real server, because
        // a bare NULL takes the type it is used at — and `expr_type` calls a NULL `text`,
        // which would refuse it here. `WHEN 'x'` is left for the same reason, and reaches
        // the evaluator's own `42804`.
        if let Some(ty) = operand_type {
            // A value, not a condition. An **unknown** literal takes the operand's type, which is
            // what makes `WHEN 'x'` store `'x'::text` and what raises
            // `22P02 invalid input syntax for type integer: "x"` when it will not convert —
            // measured, both.
            give_type(&mut when, ty)?;
            // And a value that has a type of its own must be one `=` is defined between, because
            // that is the operator the branch is: `CASE t WHEN 1 THEN …` over a `text` column is
            // `42883 operator does not exist: text = integer` on a real server, measured. The same
            // question `reconcile`'s caller asks for a written comparison, asked here because the
            // comparison is implied rather than written.
            if let Ok(value) = expr_type(&when, scope)
                && !same_family(ty, value)
            {
                return Err(SqlError::UndefinedOperator {
                    left: ty.name().to_owned(),
                    op: BinaryOp::Eq.symbol(),
                    right: value.name().to_owned(),
                });
            }
        } else if !matches!(when, Expr::Literal(Literal::Null | Literal::String(_)))
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
    let types: Vec<ColumnType> = otherwise
        .iter()
        .map(AsRef::as_ref)
        .chain(resolved.iter().map(|branch| &branch.then))
        .filter_map(|expr| branch_type(expr, scope))
        .collect();
    // **`select_common_type` over the results, the `ELSE` first**, which is [`common_of`] and is
    // the same function a `COALESCE` and a set operation ask.
    //
    // Two things this fold used to get wrong and one it got right. It kept whatever the head of
    // the list carried and only checked that the rest were in its **family** — a *comparison*
    // predicate, which separates every array type from every other, so `CASE … "char"[] …
    // text[]` was `42804` where 19beta1 answers `text[]`. And it had no second pass, so a pair in
    // one category with no cast between them got the `42804` that belongs to two categories.
    // What it got right is the order: the `ELSE` is the head of the list, which is why
    // `THEN id ELSE name` is `CASE types text and bigint`.
    let common = if types.is_empty() {
        None
    } else {
        Some(common_of(&types, Unifying::Case)?)
    };
    if let Some(ty) = common {
        if let Some(expr) = &mut otherwise {
            give_branch_type(expr, ty)?;
        }
        for branch in &mut resolved {
            give_branch_type(&mut branch.then, ty)?;
        }
    }
    Ok(Expr::Case {
        operand,
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
    // **`select_common_type` over the whole list, left to right**, which is [`common_of`] and is
    // the same function a set operation and a `CASE` ask. This folded pairwise through
    // `arith::result_type(Add, …)` instead and kept the left operand when that had no answer, so
    // `COALESCE(citext, text)` was `citext` where 19beta1 says `text`, `COALESCE("char", text)`
    // answered where it refuses, and a pair in one category with no cast between them got the
    // `42804` that belongs to two categories rather than the `42846` that belongs to this.
    //
    // The `unknown`s are skipped and coerced afterwards, which is what `branch_type` answering
    // `None` means.
    let types: Vec<ColumnType> = resolved
        .iter()
        .filter_map(|arg| branch_type(arg, scope))
        .collect();
    let common = if types.is_empty() {
        None
    } else {
        Some(common_of(&types, Unifying::Coalesce)?)
    };
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

/// Whether a type can stand on the right of a quantifier — that is, whether it holds **more than
/// one value**.
///
/// Three groups, and the third is the one a shorter rule gets wrong:
///
/// * the `*Array` family, which is what `ArrayValue::element_of` answers for;
/// * the two **vector** types, `int2vector` and `oidvector`. They are not in the array family and
///   they are exactly what this node's quantified-array variant was built for —
///   `a.attnum = ANY(i.indkey)`, which the schema dump sends and which
///   `tests/indkey_any.rs` pins. A first version of this check listed the text types and not
///   these, and refused six of that corpus's rows;
/// * **text**, because this node has no `unknown`: `'{1,2}'` is a `text` literal that a real
///   server coerces to the operand's array type, and an array reaches some readers as its own text
///   form (`crate::value::vector`).
///
/// Everything else — an integer, a date, a boolean — is one value, and a quantifier over one value
/// is the `42809` above. Written as an allow-list because the cheap mistake is to *accept*: a type
/// wrongly listed here reaches the evaluator, which reads it as an array's text and answers
/// something, where a type wrongly left out is a refusal a reader can see and report.
fn holds_many(ty: ColumnType) -> bool {
    esker_keys::array::ArrayValue::element_of(ty).is_some()
        || matches!(
            ty,
            ColumnType::Int2Vector
                | ColumnType::OidVector
                | ColumnType::Text
                | ColumnType::Varchar
                | ColumnType::Bpchar
                | ColumnType::Name
                | ColumnType::Char
        )
}

/// The element type a quantifier's right-hand side compares against, as PostgreSQL resolves it.
///
/// **An `unknown` element takes the type the typed ones settle, and only an array of nothing but
/// unknowns is `text`.** That is one sentence and it decides three cases the same way a real
/// server does:
///
/// ```text
/// 1 = ALL (ARRAY[1, NULL])     integer[]   the NULL takes the integer's type; answers NULL
/// 1 = ALL (ARRAY['a'])         text[]      nothing typed to take; 42883 integer = text
/// id = ANY (ARRAY['1','2'])    text[]      the same, and the case this check was written for
/// ```
///
/// A plain widening over *every* element gets the first one wrong: `wider_element(integer, text)`
/// is `text`, so `ARRAY[1, NULL]` resolved to `text[]` and the comparison was refused where a real
/// server answers NULL. The array's own type resolution is independent of what it is compared
/// with, which is why PostgreSQL still refuses `ARRAY['a']` against an integer even though the
/// operand would have given it a type.
///
/// `None` means "not an array whose elements are decidable here" — a column of a vector type, or
/// text holding an array's own form — and the caller then leaves the comparison to the evaluator,
/// where each element is read as the operand's type.
fn quantified_element_type(array: &Expr, scope: &Scope<'_>) -> Option<ColumnType> {
    if let Expr::Array { elements, element } = array {
        let typed: Vec<Expr> = elements
            .iter()
            .filter(|element| !is_unknown_literal(element))
            .cloned()
            .collect();
        // **The node's own settled type is not consulted when an element is unknown**, and that
        // ordering is the whole fix: `resolve` sets it by widening over *every* element, and
        // `wider_element(integer, text)` is `text`, so `ARRAY[1, NULL]` arrives here already
        // labelled `text[]`. Recomputing over the typed elements is what PostgreSQL does.
        if !typed.is_empty() && typed.len() < elements.len() {
            return array_element_type(&typed, None, scope).ok().flatten();
        }
        if let Some(settled) = element {
            return Some(*settled);
        }
        return array_element_type(elements, None, scope).ok().flatten();
    }
    expr_type(array, scope)
        .ok()
        .and_then(esker_keys::array::ArrayValue::element_of)
}

/// Whether an expression is a literal PostgreSQL would call **`unknown`** — a bare `NULL` or a
/// quoted string with no cast — and so takes its type from what it is compared against.
///
/// This node has no `unknown`: a `NULL` literal is `text` here and so is `'1'`, which is why a
/// type check that reads those as *decided* rejects comparisons a real server coerces. Used by the
/// quantified-array arm; the plan-time `IN` form has never needed it, because its list is retyped
/// against the operand rather than checked against it.
fn is_unknown_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(Literal::Null | Literal::String(_)))
}

/// Whether an operator exists between two types, as coarsely as this node's type surface allows.
///
/// PostgreSQL's answer comes out of `pg_operator` and its implicit casts; ours is the same
/// grouping [`crate::plan::Literal::comparable_with`] already uses for a literal against a column,
/// lifted to two columns. Coarse in the safe direction: it refuses only pairs that no cast in
/// PostgreSQL relates either, so it cannot turn a comparison a real server runs into an error.
#[expect(
    clippy::too_many_lines,
    reason = "one match over the whole type vocabulary, and it is a list of names \
              rather than of rules; splitting it would put half the vocabulary somewhere \
              else and let a type be added to one half without the other"
)]
pub(crate) fn same_family(left: ColumnType, right: ColumnType) -> bool {
    // **`json` compares with nothing, including another `json`.** Measured:
    // `'{"a":1}'::json = '{"a":1}'::json` is `42883 operator does not exist: json = json` -- the
    // type has no equality operator at all, which is a property of it rather than a gap, and is
    // why `json` cannot be a key, `DISTINCT`ed or grouped either. So this is checked before the
    // families, because a family test says "the same type compares with itself" and here that is
    // the case PostgreSQL refuses.
    fn family(ty: ColumnType) -> u8 {
        match ty {
            // **A family of one that is checked out below anyway.** A `void` has no comparison at
            // all on a real server — it is a pseudo-type, and the only thing a client does with
            // one is read the zero characters it prints.
            ColumnType::Void => 92,
            // **A `regtype` is in the numbers' family**, with `oid`: measured,
            // `'text'::regtype = 25` is true against an uncast integer, so the two compare and a
            // family of its own would make that `42883`. Its array is its own, like every array.
            // A `regclass` is in the numbers' family with them, and it is what makes
            // `WHERE attrelid = 'iv'::regclass` compare at all: measured,
            // `'pg_class'::regclass = 1259` is true against an uncast integer.
            ColumnType::RegType | ColumnType::RegProc | ColumnType::RegClass => family(ColumnType::Oid),
            // **Text's family, because text is what they are here.** They compare as the
            // strings they print as, which is what `attnum = ANY(indkey)` already relies on.
            // **They share `text`'s representation and compare with nothing but themselves.**
            // Being in `text`'s family answered nine pairs a real server refuses — the borrowed
            // representation reaching a second decision, as it does everywhere in this crate.
            ColumnType::Int2Vector => 96,
            ColumnType::OidVector => 97,
            ColumnType::RegTypeArray => 200,
            ColumnType::RegProcArray => 201,
            ColumnType::RegClassArray => 202,
            // **A family of one each.** `'{1}'::int[] = '{1}'::int8[]` is `42883` on a real
            // server — an array's comparison is its element type's, and two element types are two
            // operators — so no two of these share a family and none shares one with a scalar.
            ColumnType::Int8Array => 20,
            ColumnType::Int4Array => 21,
            ColumnType::Int2Array => 25,
            ColumnType::NumericArray => 22,
            ColumnType::TextArray => 23,
            ColumnType::HstoreArray => 26,
            ColumnType::TsVectorArray => 81,
            ColumnType::TsQueryArray => 82,
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
            // **`citext` is in here**, measured: `'a'::citext = 'a'::text` answers, and so does it
            // against `name`, `varchar`, `bpchar` and `"char"`. It had a family of its own and
            // refused all ten pairs.
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Name
            | ColumnType::Char
            | ColumnType::Bpchar
            | ColumnType::Citext => 1,
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
            ColumnType::TsVector => 83,
            ColumnType::TsQuery => 84,
            // Its own family: a citext compares only with a citext and with an `unknown`.
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
            // **`name[]`'s own family, not `text[]`'s**, which is the same rule as every other
            // array: measured, `'{a,b}'::name[] = '{a,b}'::text[]` is `42883` on a real server
            // even though `'x'::text = ANY('{x,y}'::name[])` is `t`. An array's comparison is its
            // element type's and two element types are two operators.
            ColumnType::NameArray => 86,
            // **`"char"[]`'s own family**, like every array's; the scalar is in text's family
            // below, because `'r'::"char" = 'r'::text` is `t`.
            ColumnType::CharArray => 93,
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
            // A family each, for the reason every range and every array already has one: a
            // range compares only with a range of the same subtype, and an array's
            // comparison is its element type's.
            ColumnType::DateRange => 49,
            ColumnType::NumRange => 50,
            ColumnType::Int8Range => 51,
            ColumnType::TstzRangeArray => 52,
            ColumnType::Int4RangeArray => 53,
            ColumnType::DateRangeArray => 54,
            ColumnType::NumRangeArray => 55,
            ColumnType::Int8RangeArray => 56,
            // **A family of one, and it compares with nothing — not even itself.**
            // `point = point` is `42883` on a real server, so `Point` joins `Json` in the
            // early return below; this rank exists so the match stays total.
            ColumnType::Point => 57,
            ColumnType::PointArray => 58,
            // Its own family: an array compares with an array of the same element, and 85 is the
            // next number nothing else uses — 59 is `floatrange`'s and 77 `xml`'s.
            ColumnType::BoxArray => 85,
            // **Five families, one per element type**, which is every array's rule: measured,
            // `'{…}'::circle[] = '{…}'::circle[]` is `42883 could not identify an equality
            // operator for type circle` on a real server even though the scalar `=` answers, so
            // sharing a family with anything would answer where PostgreSQL refuses.
            ColumnType::LsegArray => 87,
            ColumnType::PathArray => 88,
            ColumnType::PolygonArray => 89,
            ColumnType::CircleArray => 90,
            ColumnType::LineArray => 91,
            // A family each, like every other range: a `floatrange` compares with a
            // `floatrange` and `float_range = '[0.5,0.7]'::numrange` is `42883`, measured.
            ColumnType::FloatRange => 59,
            ColumnType::VarcharRange => 60,
            // **A family of its own, and the point of the type.** `money = numeric`,
            // `money = bigint` and `money + 1` are each `42883` on a real server: cents in an
            // `i64` compare with cents and with nothing else.
            ColumnType::Money => 61,
            ColumnType::MoneyArray => 62,
            // **One family for `inet` and `cidr`**, which is the whole point of them sharing a
            // representation: `'192.168.1.1'::inet = '192.168.1.1'::cidr` is `t` on a real server.
            // A `macaddr` is its own — no operator relates it to an address.
            ColumnType::Inet | ColumnType::Cidr => 63,
            ColumnType::MacAddr => 64,
            ColumnType::InetArray => 65,
            ColumnType::CidrArray => 66,
            ColumnType::MacAddrArray => 67,
            // **One family for the two**, which is the point of them sharing a representation:
            // `B'101'::bit varying = B'101'::bit(3)` is `t` on a real server.
            ColumnType::Bit | ColumnType::VarBit => 68,
            ColumnType::BitArray => 69,
            ColumnType::VarBitArray => 70,
            // A family each: no operator relates one shape to another.
            ColumnType::Lseg => 71,
            ColumnType::Box => 72,
            ColumnType::Path => 73,
            ColumnType::Polygon => 74,
            ColumnType::Circle => 75,
            ColumnType::Line => 76,
            // **A family of one, and not the datetime family.** A `date` joins `timestamp`
            // because `date = timestamp` is a real operator; a `time` does not, because
            // `time = timestamp` and `time = date` are both `42883 operator does not exist` on
            // 19beta1 — measured, because putting it in family 4 by analogy would answer where a
            // real server raises, which is ADR 0031's worst class.
            // **A `time` and an `interval` are one family**, through the implicit cast a real
            // server has between them: `'1 min'::interval = '00:01:00'::time` answers. Measured.
            ColumnType::Time | ColumnType::Interval => 6,
            // Its own family too: `uuid = text` and `uuid = integer` are both `42883` on a real
            // server, and its only comparisons are with another uuid.
            ColumnType::Uuid => 7,
            // Its own family: `interval = integer` is `42883` on a real server, and an interval
            // compares with another interval and with nothing else here.
            // Unreachable: returned above, and kept as an arm rather than a `_` so that the next
            // type added here is a compile error rather than a silent family 9.
            ColumnType::Json => 9,
            // Unreachable for the same reason, both of them: `xml` is `json`'s shape and has no
            // equality operator either.
            ColumnType::Xml | ColumnType::XmlArray => 77,
            // **Its own family**: `ltree = text` is `42883` on a real server, and an ltree
            // compares with another ltree and with nothing else.
            ColumnType::Ltree => 78,
            ColumnType::LtreeArray | ColumnType::LQueryArray => 79,
            // A pattern compares with nothing, including another pattern.
            ColumnType::LQuery => 80,
        }
    }
    // **And `json[]` with it.** `ARRAY['{"a":1}'::json] = ARRAY['{"a":1}'::json]` is
    // `42883 could not identify an equality operator for type json` — a different sentence
    // from the one above and the same reason: an array's equality is its element's, and
    // `json` has none to lend.
    // **And `point` with them.** `'(1,2)'::point = '(1,2)'::point` is
    // `42883 operator does not exist: point = point` — a type with no equality even with
    // itself, which is why `CREATE INDEX` on one is `42704` and why it is not a key here.
    // **And `xml` with `json`.** `'<a/>'::xml = '<a/>'::xml` is
    // `42883 operator does not exist: xml = xml`, measured — the same sentence `json` gets, from
    // a type with the same shape.
    // **Not [`crate::value::has_equality_operator`], and the geometric corpus is what proves the
    // two are different questions.** That one asks whether a btree family exists, which is what
    // `DISTINCT` needs; this one asks whether `=` answers at all. An `lseg` splits them —
    // `'…'::lseg = '…'::lseg` is `t` and `count(DISTINCT lseg)` raises — so the shapes are on
    // that list and not on this one.
    // **`point` is not on this list, and that is the point of the list.** It was, back when a
    // `point` had no comparison at all; it has exactly one — `point_ne`, and no `point_eq` — so
    // "comparable to each other" and "which operators exist" are two questions, and only the
    // second can answer for `<>` without answering the same way for `=`.
    // `crate::value::operator_exists` is that second question and refuses `point =` where this
    // now lets it through.
    //
    // `json`, `xml` and `json[]` stay because they have no operator at all, so the two questions
    // agree about them and a second reader costs nothing. If one of them ever grows an operator,
    // it leaves this list the way `point` did.
    if matches!(
        left,
        ColumnType::Json | ColumnType::JsonArray | ColumnType::Xml
    ) || matches!(
        right,
        ColumnType::Json | ColumnType::JsonArray | ColumnType::Xml
    ) {
        return false;
    }
    // **An `oid` compares with the integers and with the other oid-ish types, and with nothing
    // else** — `26::oid = 26` answers and `1::numeric = 1::oid` is `42883`, both measured. A flat
    // family tag cannot say that: the integers, the floats and `numeric` are one family because
    // they all compare with each other, and `oid` overlaps only part of it. So this pair is asked
    // before the tags, the way `json`'s is above.
    let oid_ish = |ty| {
        matches!(
            ty,
            ColumnType::Oid | ColumnType::RegType | ColumnType::RegProc | ColumnType::RegClass
        )
    };
    let integer = |ty| matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8);
    if oid_ish(left) != oid_ish(right) {
        return if oid_ish(left) {
            integer(right)
        } else {
            integer(left)
        };
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
        Expr::Ordinal { at, .. } => scope.enum_at(*at),
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

/// Gives a literal beside an **aggregate** the aggregate's type, the way a column beside one does.
///
/// `None` for every pair that is not that, so the ordinary rules run untouched.
///
/// Separate from [`reconcile`] because of what each is given: that one takes two resolved
/// expressions and no scope, and an aggregate's argument is *not* resolved by then — `resolve` has
/// no arm for a call, so `sum(salary)` still holds a bare `Expr::Column`. Typing it needs the
/// scope, which exists only here.
///
/// Both halves of the rule were wrong before, and both were wrong the same way — a silent `false`:
///
/// * `sum(salary) > 'x'` is `22P02 invalid input syntax for type bigint: "x"` on a real server,
///   the unknown literal read by the aggregate's own input function;
/// * `sum(salary) > 'x'::text` is `42883 operator does not exist: bigint > text`, which is byte
///   for byte what this node already answered for `80000::int8 > 'x'::text`. The aggregate was the
///   hole in a check that was otherwise right.
///
/// No `blank_pad`: an aggregate's output carries no typmod — `Aggregation::rewrite` says so, and
/// `min(character(3))` is a `bpchar` with none on a real server too.
fn reconcile_aggregate(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    scope: &Scope<'_>,
) -> Result<Option<(Expr, Expr)>> {
    if !op.is_comparison() {
        return Ok(None);
    }
    Ok(match (left, right) {
        (Expr::Aggregate(_), Expr::Literal(literal)) => Some((
            left.clone(),
            Expr::Literal(retype(expr_type(left, scope)?, literal, op, false)?),
        )),
        (Expr::Literal(literal), Expr::Aggregate(_)) => Some((
            Expr::Literal(retype(expr_type(right, scope)?, literal, op, true)?),
            right.clone(),
        )),
        _ => None,
    })
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
        // **A cast types the other side exactly as a column does**, and it has to say so out loud
        // now that a cast naming a modifier keeps its node (`parse::lower`, `debts-v1.1.md` #28).
        // Before that, `'101'::bit(3)` folded to a typed literal and `retype` read `bit` off it —
        // `'101'::bit(3) = '101'::text` was `42883 operator does not exist: bit = text`, which is
        // what a real server answers. With the node kept, the pair matched no arm here, nothing
        // typed the other side, and the comparison quietly answered `f`. One arm, and it restores
        // the sentence the fold used to carry.
        //
        // **And refuses it exactly as a column does**, which is the half of that sentence this arm
        // was missing. Retyping is what an `unknown` needs; a literal that already *has* a type
        // needs the family check the two-column and two-literal arms below both make, and got
        // neither — the pair matched here first. So `'x'::text = '<a/>'::xml` answered `f` where a
        // real server says `42883 operator does not exist: text = xml`, and with it twenty-five
        // more pairs in `tests/captures/pg19_comparison_matrix.txt`: every type whose value cannot
        // carry its own name — `xml`, `jsonb`, `name`, `"char"`, `bit`, `int2vector`, `oidvector`,
        // `lquery`, `void` — keeps a `Cast` node out of the fold and so arrives on this side of
        // the pattern, against a `tsrange` or a `text` that folded to a literal on the other
        // (`debts-v1.1.md` #43's second mechanism).
        (Expr::Cast { to, typmod, .. }, Expr::Literal(literal)) => {
            refuse_across_families(*to, literal, op, false)?;
            (
                left.clone(),
                Expr::Literal(blank_pad(retype(*to, literal, op, false)?, *to, *typmod)),
            )
        }
        (Expr::Literal(literal), Expr::Cast { to, typmod, .. }) => {
            refuse_across_families(*to, literal, op, true)?;
            (
                Expr::Literal(blank_pad(retype(*to, literal, op, true)?, *to, *typmod)),
                right.clone(),
            )
        }
        // **A constructor gives the other side its array type**, which is the mirror of the
        // subscript rule below and the same failure if it is missing: `ARRAY[t] = '{x}'` compared
        // a `Datum::Array` against a `Datum::Text` and answered **`f` for every row**, including
        // the rows that match — a wrong answer that looks like an empty result rather than an
        // error. The element type is settled by then, which is what makes the array type
        // answerable here without a scope.
        //
        // **Only an `unknown` string**, which is the doctrine this whole function is about: a
        // folded `ARRAY[7]` is a `bigint[]` here and an `integer[]` on a real server, so retyping
        // it against the constructor turned a working comparison into
        // `42883 integer[] = bigint[]`. A literal that already has a type is left to the pair
        // rules below, exactly as it is beside a column.
        (
            Expr::Array {
                element: Some(ty), ..
            },
            Expr::Literal(literal @ Literal::String(_)),
        ) => {
            let array =
                esker_keys::array::ArrayValue::array_over(*ty).unwrap_or(ColumnType::TextArray);
            (
                left.clone(),
                Expr::Literal(retype(array, literal, op, false)?),
            )
        }
        (
            Expr::Literal(literal @ Literal::String(_)),
            Expr::Array {
                element: Some(ty), ..
            },
        ) => {
            let array =
                esker_keys::array::ArrayValue::array_over(*ty).unwrap_or(ColumnType::TextArray);
            (
                Expr::Literal(retype(array, literal, op, true)?),
                right.clone(),
            )
        }
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
        // **The two sites below name `missing_symbol`, not the spelling.** `IS DISTINCT FROM` is
        // built on `=` and a real server refusing it says `operator does not exist: json = json`.
        // Found by marking every `UndefinedOperator` in this crate with its own line and running
        // the census once — three readings of the code had blamed three other sites, and these two
        // are the pair that fires.
        (Expr::Literal(left_literal), Expr::Literal(right_literal)) => {
            match (literal_type(left_literal), literal_type(right_literal)) {
                (Some(a), Some(b)) if !same_family(a, b) => {
                    return Err(SqlError::UndefinedOperator {
                        left: a.name().to_owned(),
                        op: op.missing_symbol(),
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
                op: op.missing_symbol(),
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
    if let Ok(ty) = expr_type(operand, scope) {
        if let Some(element) = esker_keys::array::ArrayValue::element_of(ty) {
            return Some(element);
        }
        // **A vector's element is a property of the type**, which is what `pg_type.typelem` says:
        // 21 for an `int2vector` and 26 for an `oidvector`, measured. Asked before the column
        // names below, because a vector that is not a catalog column has the same element —
        // `('23 25'::oidvector)[0]` is an `oid` on a real server and was `text` here.
        match ty {
            ColumnType::Int2Vector => return Some(ColumnType::Int2),
            ColumnType::OidVector => return Some(ColumnType::Oid),
            _ => {}
        }
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
        // The type the cast named, which is the whole point of carrying it.
        Expr::Cast { to, .. } => Some(*to),
        // A `COLLATE` carries whatever it was written on: it changes an ordering, never a type.
        Expr::Collate { operand, .. } => carried_type(operand),
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
        // **A `CASE`'s type is its branches' and never its operand's**, so the simple
        // form answers exactly as the searched one does: `CASE a WHEN 1 THEN 'x' END`
        // is `text` however `a` is typed.
        Expr::Case {
            operand: _,
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

fn branch_common_type<'a>(branches: impl Iterator<Item = &'a Expr>) -> ColumnType {
    // **The same `select_common_type` `resolve_case` and `resolve_coalesce` ask**, because this is
    // the answer a client is *told* and those two are what actually happens — and they were two
    // different rules. Measured: `pg_typeof(COALESCE('{"x"}'::"char"[], '{"x"}'::text[]))` read
    // this one and said `"char"[]` while the resolution settled on `text[]`, which is 19beta1's
    // answer. A declared type that disagrees with the plan is the `->` bug's shape.
    //
    // Infallible here on purpose: a refusal is `resolve`'s to raise, and this is asked of
    // expressions that have not been resolved yet (`output_columns` on a raw projection). `text`
    // is the fallback `select_common_type` itself uses when nothing carries a type.
    let types: Vec<ColumnType> = branches
        .filter_map(|branch| {
            carried_type(branch).or_else(|| match branch {
                Expr::Literal(literal) => literal_type(literal),
                _ => None,
            })
        })
        .collect();
    common_of(&types, Unifying::Coalesce).unwrap_or(ColumnType::Text)
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
/// and needs none. The other four are what PostgreSQL calls them, and there is no divergence left
/// in the list: the `int4` rung (ADR 0087) closed the integer's width and a bare decimal is a
/// `numeric` here as it is there.
pub(super) fn literal_type(literal: &Literal) -> Option<ColumnType> {
    match literal {
        // **The one NULL that has a type**, which is why the variant exists: everything that asks
        // this question — a subquery's column type, an operator's two sides, a `COALESCE`'s
        // unification — gets the cast's answer instead of `None`.
        Literal::TypedNull(ty) => Some(*ty),
        Literal::Null | Literal::String(_) => None,
        // **The `int4` rung.** An unadorned integer takes the smallest type that holds it:
        // `pg_typeof(1)` is `integer`, `pg_typeof(2147483648)` is `bigint`, and past `int8` the
        // parser has already made it a `numeric` before this is asked
        // (`tests/integer_literal_type.rs` carries the ladder in both signs).
        Literal::Integer(value) => Some(if i32::try_from(*value).is_ok() {
            ColumnType::Int4
        } else {
            ColumnType::Int8
        }),
        // **A bare decimal is a `numeric`.** It was a `float8`, which was a wrong *value* and not
        // only a wrong type — `1.10` printed `1.1`, `0.1 + 0.2` printed `0.30000000000000004` —
        // and a `float8` beside it still wins, because the promotion table is unchanged and only
        // the literal's own type moved.
        Literal::Decimal(_) => Some(ColumnType::Numeric),
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
    // **A `numeric` literal against an integer column is compared, not narrowed.** PostgreSQL
    // resolves `bigint = numeric` by promoting the *column* — measured, `9223372036854775808 =
    // 1::bigint` is `f` and not an error — so `id = 9223372036854775808` is false for every row.
    // Narrowing is what an assignment does, and an assignment of this literal is `22003 bigint out
    // of range`, which is the right answer to `INSERT` and the wrong one to `WHERE`. The
    // comparison itself is exact: `Datum`'s ordering has a `numeric`-against-`int8` arm.
    if matches!(literal, Literal::Typed(value) if matches!(**value, Datum::Numeric(_)))
        && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    {
        return Ok(literal.clone());
    }
    // **And an integer literal against an integer column, which is the same sentence at every
    // width.** PostgreSQL picks an operator — `int24gt` for `i2 > 1` — and leaves the constant an
    // `integer`; narrowing it here built a `smallint` node, and the printed tree said so:
    // `(i2 > (1)::smallint)` where a real server prints `(i2 > 1)`
    // (`tests/captures/pg19_numeric_literal_deparse.txt`, `debts-v1.1.md` #23). The **values** are
    // unaffected — `Datum`'s ordering compares the integer widths against each other — which is
    // why the only place it showed was a deparse.
    //
    // `int4` and `int8` were already right *by accident*: the datum stays an `i64` whatever width
    // the literal is declared (ADR 0087), so narrowing to either is a no-op and only `smallint`
    // had a distinct one. Written as the rule rather than as the width, because a fix aimed at
    // `smallint` would be a fix to the symptom.
    //
    // **And a literal that already *carries* an integer type keeps it**, which is the same
    // sentence again and the half that was missing: `i8 > 1::bigint` folds the written cast into
    // the constant, so the literal arrives here as `Literal::Typed(Int8(1))`, fell through to
    // `assign` and came back `Literal::Integer(1)` — the `Datum::Int8(value) =>
    // Literal::Integer(value)` line below, which is where the declared width went. A real server
    // prints `(i8 > (1)::bigint)` and this node printed `(i8 > 1)`
    // (`debts-v1.1.md` #24's `c_i8_cast` row). Measured over both signs and three widths in
    // `tests/corpus/pg19_negative_constant.txt`: `(i2 > (1)::smallint)`,
    // `(i2 > ('-1'::integer)::smallint)`, `(nm > ('-1'::integer)::numeric)`.
    let carries_an_integer_type = matches!(literal, Literal::Integer(_))
        || matches!(literal, Literal::Typed(value)
            if matches!(**value, Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)));
    if carries_an_integer_type
        && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    {
        return Ok(literal.clone());
    }
    // **A literal that was written with a cast is compared, not narrowed** — the arms above are
    // this same sentence for the three spellings that carry *no* cast, and this is the general
    // one. `assign` is the wrong question for a comparison, and its failure says so out loud:
    // `1::oid = 1::int8` is `42804 column "?column?" is of type bigint but expression is of type
    // oid` and `regproc_col = 1::oid` is the same sentence one type over, both for comparisons a
    // real server answers. Nothing here needs narrowing: the pair has already passed
    // `comparable_with`, which is `same_family` now, and `Datum::pg_cmp` has an arm for every
    // pair a family holds — the integer widths, `date` against `timestamp`, `inet` against
    // `cidr`, `citext` against `text`, an `oid` against each integer width.
    //
    // **Only `Literal::Typed`**, and that boundary is the point: a bare `1`, `1.5` or `'x'` has no
    // type of its own and *should* take the column's, which is what the arms above and
    // `literal.assign` below are for. A cast keeps what it was given (ADR 0086).
    //
    // Measured across all 2,756 column-against-literal pairs
    // (`tests/captures/pg19_comparison_matrix_column.txt`): eleven of them were this `42804`, and
    // they became visible only once `comparable_with` stopped refusing them one gate earlier.
    if let Literal::Typed(value) = literal
        && let Some(held) = value.column_type()
        && held != ty
    {
        return Ok(literal.clone());
    }
    // **And an integer literal wider than the column is compared, not narrowed**, which is the
    // arm above with the widths one step in: PostgreSQL has an `int4 > int8` operator, so
    // `i4 > 9223372036854775807` is answered — `f` for every row — where narrowing the literal to
    // the column's type is `22003 integer out of range`, a refusal for a statement a real server
    // runs. Measured on 19beta1 while building `tests/corpus/pg19_numeric_literal_deparse.txt`,
    // which is a corpus about *printing* and found this because the row would not build.
    //
    // The same sentence as the `numeric` arm applies unchanged: narrowing is what an assignment
    // does, and `22003` is the right answer to an `INSERT` and the wrong one to a `WHERE`.
    if let Some(value) = integer_literal_value(literal)
        && let Some((low, high)) = integer_span(ty)
        && !(low..=high).contains(&value)
    {
        return Ok(literal.clone());
    }
    // **A comparison against a `regproc` reads the literal as an `oid`, not as a function name.**
    // Measured: `typinput = 'array_in'` is `22P02 invalid input syntax for type oid: "array_in"`
    // on a real server, while `typinput = 'array_in'::regproc` answers and so does
    // `typinput::text = 'array_in'`. The reason is the operator: `=` over a `regproc` is `oideq`,
    // whose right operand is an `oid`, so the `unknown` literal is handed to `oidin`. An
    // *assignment* is the other way — `regprocin` resolves a name — which is why this arm is here
    // and not in `Literal::assign` (ADR 0098).
    if matches!(ty, ColumnType::RegProc)
        && let Literal::String(text) = literal
    {
        let oid = crate::value::oid::from_text(text)?;
        return Ok(Literal::Typed(Box::new(Datum::RegProc {
            oid,
            name: crate::value::reg_proc::to_text(oid).into_boxed_str(),
        })));
    }
    // **And a `regclass` the same way, for the same reason one type over** (`debts-v1.1.md` #41).
    // Measured: `WHERE r = 'ra'` is `22P02 invalid input syntax for type oid: "ra"` on a real
    // server — `=` over a `regclass` is `oideq`, so the `unknown` literal goes to `oidin` — while
    // `WHERE r = 'ra'::regclass` answers, and so does an *assignment* of the bare name, which
    // resolves through `regclassin`. That asymmetry is the whole row: this half needs no catalog
    // at all, and the `22P02` falls out of the oid reader rather than being written here.
    //
    // **`IN` is the exception and is not reproduced here.** `r IN ('ra','rb')` answers on a real
    // server, because the list is coerced through the *type's* input function rather than through
    // the operator's operand type — and this crate reconciles each item of an `IN` with the
    // operand through this same function, so the two cannot be told apart until the name half of
    // #41 gives them a catalog. Declared in `tests/regclass_literal.rs`.
    if matches!(ty, ColumnType::RegClass)
        && let Literal::String(text) = literal
    {
        let oid = crate::value::oid::from_text(text)?;
        return Ok(Literal::Typed(Box::new(crate::value::regclass_of_oid(
            i64::from(oid),
        ))));
    }
    // **And a `regtype`, which is the third of three and was the one this rule never reached.**
    // Measured on a `regtype` column holding `int4`: `c = 'int4'`, `c > 'int4'`, `'int4' = c` and
    // `c IN ('int4')` are all `22P02 invalid input syntax for type oid: "int4"` on 19beta1, and
    // this node answered every one of them — `Datum::from_text(RegType, …)` resolves the *name*,
    // which is `regtypein`'s rule and the **input function's** question, not the operator's.
    // `pg_operator` has no `=` for `regtype` at all: the comparison is `oid`'s, so the `unknown`
    // goes to `oidin` (wire v3 family F8).
    if matches!(ty, ColumnType::RegType)
        && let Literal::String(text) = literal
    {
        let oid = crate::value::oid::from_text(text)?;
        return Ok(Literal::Typed(Box::new(crate::value::regtype_of_oid(oid))));
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

/// The value of an integer literal, whichever of the two spellings it is in.
///
/// `Literal::Integer` is what the parser makes and `Literal::Typed(Int2|Int4|Int8)` is what a cast
/// or an earlier retype leaves; both are the same value and the fit question is the same question.
fn integer_literal_value(literal: &Literal) -> Option<i64> {
    match literal {
        Literal::Integer(value) => Some(*value),
        Literal::Typed(value) => match value.as_ref() {
            Datum::Int2(value) => Some(i64::from(*value)),
            Datum::Int4(value) => Some(i64::from(*value)),
            Datum::Int8(value) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

/// The inclusive range an integer column type holds, or `None` for a type that is not one.
fn integer_span(ty: ColumnType) -> Option<(i64, i64)> {
    match ty {
        ColumnType::Int2 => Some((i64::from(i16::MIN), i64::from(i16::MAX))),
        ColumnType::Int4 => Some((i64::from(i32::MIN), i64::from(i32::MAX))),
        ColumnType::Int8 => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// Refuses a cast against a literal that **already carries a type**, where no operator relates
/// the two.
///
/// The check the `Ordinal` arms get from [`Literal::comparable_with`] and the two-literal arm
/// makes for itself; a cast beside a literal reached neither. `None` is an `unknown` — a quoted
/// string or a bare NULL — which is exactly the case the cast is there to give a type to, so it
/// passes through to [`retype`] untouched.
/// Whether a constant operand **already is** a value of `ty`, in the one case where its own
/// `column_type` cannot say so.
///
/// A [`Datum::Range`] carries its *subtype*, and an `int4range` and an `int8range` are both ranges
/// of an `int8` in this crate — so `column_type` answers the first as a **representative**, which
/// is what it documents itself as doing. `expr_type` reads that representative, and the cast the
/// fold keeps over a range constant then looked like one between two different types:
/// `'[1,3)'::int8range` refused *itself* with `42846 cannot cast type int4range to int8range`, a
/// pair `pg_cast` rightly has no row for, for a statement whose operand is already exactly the
/// type named. Found by the comparison matrix, which is where the row is measured.
///
/// [`Datum::fits`] is the same question asked from the column's side, where the answer is single
/// valued — and it is asked *only* for a range, because a range is the only datum whose type is a
/// set. The node stays either way: it is what makes `int4range = int8range` the `42883` a real
/// server gives, which folding the constant would answer instead.
fn is_already_of_type(expr: &Expr, ty: ColumnType) -> bool {
    matches!(
        expr,
        Expr::Literal(Literal::Typed(value))
            if matches!(**value, Datum::Range { .. }) && value.fits(ty)
    )
}

fn refuse_across_families(
    ty: ColumnType,
    literal: &Literal,
    op: BinaryOp,
    literal_on_the_left: bool,
) -> Result<()> {
    if let Some(carried) = literal_type(literal)
        && !same_family(carried, ty)
    {
        return Err(undefined_operator(ty, literal, op, literal_on_the_left));
    }
    Ok(())
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
        op: op.missing_symbol(),
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
        | Expr::QuantifiedArray { .. }
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
        // **A boolean is a predicate whatever shape it arrived in**, so the last word belongs to
        // the type and not to the list above — which is the shapes whose type needs no lookup.
        // PostgreSQL's rule is `coerce_to_boolean` over the resolved type and nothing else, and
        // `Aggregation::check_boolean` already writes the same rule for `HAVING`.
        //
        // Without the lookup this arm refused every boolean-valued **catalog function** — twelve
        // of them, `@@`, `&&`, `@>` and `?` among the spellings — with a sentence that is its own
        // disproof: `argument of WHERE must be type boolean, not type boolean`. A server cannot
        // refuse a boolean for not being one.
        //
        // PostgreSQL names the type it got, and a user reading "must be type boolean" without it
        // has to work out which of their columns was the problem. Measured, both clauses:
        // `argument of WHERE must be type boolean, not type bigint`.
        other => match expr_type(other, scope) {
            Ok(ColumnType::Bool) => Ok(()),
            ty => Err(SqlError::DatatypeMismatch(format!(
                "argument of {clause} must be type boolean, not type {}",
                ty.as_ref().map_or("unknown", |ty| ty.name())
            ))),
        },
    }
}

/// The name and type of every output column.
///
/// What a target-list entry with no `AS` is called — PostgreSQL's `FigureColname`, measured.
///
/// **A function is named after itself**, unqualified: `area(box '…')` is `area` and
/// `pg_catalog.length('x')` is `length`. Run 75 stopped a Rails test on exactly that, because the
/// test reads its value back **by the column's name**. The outermost call wins, so `upper(lower(…))`
/// is `upper`. A column keeps its own name, an alias beats everything, and everything else — a
/// literal, an operator, a comparison, a subscript, a negation — is `?column?`.
///
/// **What is not recoverable here, and is declared rather than guessed**: a cast over a *literal*.
/// PostgreSQL names `1::text` after the type — `text`, and `1::numeric(5,2)` is `numeric` without
/// the modifier — but this crate folds such a cast at plan time, so by now there is no cast left to
/// read, only the folded literal. A cast that survives to run time is [`Expr::ToText`] and is named
/// the way a real server names it: the operand's name when it has one (`a::text` is `a`), and the
/// target type when it does not (`(a + 1)::text` is `text`).
fn figure_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Aggregate(call) => call.func.name().to_owned(),
        Expr::Scalar { func, .. } => func.name().to_owned(),
        Expr::Uuid(func) => func.name().to_owned(),
        // **An operator expression is `?column?`, a function call is its own name.** Six of
        // these variants are spelled as symbols — `||`, `->`, `?`, `@>`, `&&`, `@@` — and a real
        // server names none of them after the symbol: measured, every one is `?column?` while
        // `abs(-1)` is `abs`. Told apart by the spelling rather than by a list, so a seventh
        // operator-shaped variant is named right the day it is added.
        //
        // Only reachable for `||` since `||` over text was built; before that the statement was
        // refused and the rule was never asked. The other five have the same latent answer.
        Expr::CatalogFunc(call) => {
            let name = call.func.name();
            if name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_') {
                name.to_owned()
            } else {
                "?column?".to_owned()
            }
        }
        Expr::Sequence(call) => call.func.name().to_owned(),
        Expr::SetFunc(call) => call.name.clone(),
        // The keyword-shaped calls, named after the keyword and lower-cased — measured, all of
        // them: `coalesce`, `case`.
        Expr::Coalesce(_) => "coalesce".to_owned(),
        Expr::Case { .. } => "case".to_owned(),
        // **A `COLLATE` is transparent to naming**: `SELECT t COLLATE "C"` is a column called
        // `t` on a real server, measured, exactly as `SELECT (t)` is.
        Expr::Collate { operand, .. } => figure_column_name(operand),
        // A cast that reaches run time: the operand's name, or the type it casts to.
        Expr::ToText { operand, .. } => match figure_column_name(operand) {
            unnamed if unnamed == "?column?" => "text".to_owned(),
            named => named,
        },
        // A scalar subquery takes the **subquery's own** column name and `EXISTS` is called
        // `exists`; everything else about a subquery is `?column?`. Measured with `psql`, which a
        // corpus of types and rows cannot record.
        Expr::Subquery(sub) => sub.output_name().unwrap_or("?column?").to_owned(),
        // **`'x'::regclass` is a column called `regclass`**, and it reaches here as a *literal*
        // because `Executor::bound` resolves the cast against the catalog before the plan is
        // built — so by the time a name is figured there is no cast left to read, which is the
        // gap the doc above declares for a folded literal cast. It is closed for this one type
        // because the value itself says which type it is: nothing but that cast produces a
        // `regclass` datum. Measured — a real server names an unaliased cast after its target
        // type (`tests/captures/pg19_regclass.txt`), and an alias still wins.
        Expr::Literal(Literal::Typed(datum)) if matches!(**datum, Datum::RegClass { .. }) => {
            "regclass".to_owned()
        }
        _ => "?column?".to_owned(),
    }
}

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
                        // A `*` expands to columns, and a column's type is one the catalog holds.
                        pseudo: None,
                    }
                }));
            }
            SelectItem::Expr {
                expr,
                alias,
                user_type: declared,
                pseudo,
            } => {
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
                // **A cast names its column after the type**, which a folded user cast cannot
                // say for itself: by here `'good'::feeling` is the literal `good` and would be
                // `?column?`. `test_reload_type_map_for_newly_defined_types` reads the result by
                // that name, so the name is as load-bearing as the oid below it.
                let name = alias.clone().unwrap_or_else(|| match (declared, pseudo) {
                    (Some(def), _) => crate::catalog::display_name(&def.name),
                    (None, Some(pseudo)) => pseudo.name.to_owned(),
                    (None, None) => figure_column_name(expr),
                });
                // A typmod travels only with a **plain column reference**, which is
                // PostgreSQL's rule and the corpus's: `c || '|'` is `text` with none and
                // `min(c)` is `bpchar` with none, where a bare `c` is `character(3)`.
                // Which expressions carry one is [`typmod_of`]'s subject, measured over every
                // shape in `tests/corpus/pg19_expression_typmod.txt`. Not under an aggregation:
                // the expression here is the pre-rewrite one, and `min(c)` is `bpchar` with none
                // on a real server anyway.
                let typmod = if aggregation.is_none() {
                    typmod_of(expr, scope)
                } else {
                    crate::value::NO_TYPMOD
                };
                // **The type a client is told, for a column declared as a user-defined one.**
                // A plain column reference and an aggregate over one both keep it — `min(mood)` is
                // `mood` on a real server — and everything else loses it, because an expression
                // over an enum is an expression over its ordinal and has no name to give back.
                let user_type = match expr {
                    // **A cast to a user type declared one outright**, and it outranks anything
                    // read off the expression: the expression is the folded value.
                    _ if declared.is_some() => declared.clone(),
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
                    pseudo: *pseudo,
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
/// A `bpchar` operand **read as `text`**, which is where its trailing blanks stop existing.
///
/// A `character(n)` is stored blank-padded and printed padded — `SELECT c` is `x   ` on a real
/// server too — and the padding disappears the moment the value is coerced to `text`, which
/// PostgreSQL does by inserting a cast node in front of every text operator and text function.
/// This inserts the same node, and the cast this crate already had is what trims: `c::text` agreed
/// with a real server before this and `upper(c)` did not.
///
/// **Three readers keep the padding and are not given one**, measured in
/// `tests/corpus/pg19_bpchar_padding.txt`: `octet_length` (it reports the storage, `4`, where
/// `length` reports the value, `1`), `concat` (it takes `"any"` and goes through the output
/// function, so `concat(c, 'z')` is `x   z`), and `LIKE` (`c LIKE 'x'` is **false** while
/// `c = 'x'` is true — the sharpest pair in that file). A comparison keeps them too and needs
/// nothing here: it pads the *other* side instead (`blank_pad`), which is the same answer from the
/// other direction.
fn read_as_text(expr: Expr, scope: &Scope<'_>) -> Expr {
    if matches!(expr_type(&expr, scope), Ok(ColumnType::Bpchar)) {
        return Expr::ToText {
            operand: Box::new(expr),
            // The one operand type it is true for, which is the whole of this function.
            strip_blanks: true,
            enum_labels: None,
        };
    }
    expr
}

pub(super) fn typmod_of(expr: &Expr, scope: &Scope<'_>) -> i32 {
    let none = crate::value::NO_TYPMOD;
    match expr {
        Expr::Column { table, name } => scope
            .resolve_column(table.as_deref(), name)
            .map_or(none, |(_, column)| column.typmod),
        // **Two shapes carry their modifier in a field of their own, and it is the answer.**
        //
        // A **cast** names the one it was written with: `c::char(2)` is `character(2)` and
        // `1.5::numeric(10,2)` is `numeric(10,2)`, measured. A **resolved column** carries the
        // column's — this is asked on both sides of resolution, a `Column` before and the
        // `Ordinal` it becomes after, and having only the `Column` arm answered `-1` for every
        // resolved column reference. Group E of the deparse census found it:
        // `(nn)::numeric(10,2)` over a `numeric(10,2)` compared its cast's modifier against a
        // column's and was told the column had none.
        Expr::Ordinal { typmod, .. } | Expr::Cast { typmod, .. } => *typmod,
        // **`NULLIF` is the identity on its left argument**, so the modifier travels with it —
        // `nullif(c, 'x')` over a `character(4)` is `character(4)` — but only while the *type* is
        // also the left's: a `varchar` is compared as `text` (`nullif_type`), and a modifier does
        // not follow a type change. Measured, both halves.
        Expr::CatalogFunc(call) if call.func == CatalogFunc::NullIf && call.args.len() == 2 => {
            let left = expr_type(&call.args[0], scope);
            match (left, expr_type(expr, scope)) {
                (Ok(left), Ok(whole)) if left == whole => typmod_of(&call.args[0], scope),
                _ => none,
            }
        }
        // **`GREATEST` and `LEAST` keep a modifier only when every argument has the same one** —
        // `greatest(d, d)` is `character(2)` and `greatest(c, 'x')` is `bpchar`, because an
        // untyped literal has none and one input without it settles the answer. Measured.
        Expr::CatalogFunc(call)
            if matches!(call.func, CatalogFunc::Greatest | CatalogFunc::Least) =>
        {
            shared_typmod(&call.args, scope)
        }
        // `COALESCE` is the same rule, one node over: `coalesce(c, c)` is `character(4)` and
        // `coalesce(c, d)` is `bpchar`.
        Expr::Coalesce(args) => shared_typmod(args, scope),
        // **Everything else has none, and `CASE` is the one worth naming.** Two oracles disagree
        // about it: `CREATE TABLE AS` gives the created column the *first branch's* modifier while
        // `\gdesc` on the same expression gives none. The `RowDescription` is what a client reads
        // and what this function answers, so `CASE` is none — which is also what it already was.
        // `tests/corpus/pg19_expression_typmod.txt` records both readings.
        _ => none,
    }
}

/// The modifier every one of these expressions carries, or none if they do not all carry the same.
///
/// PostgreSQL's rule for the productions that pick a common type: one input without a modifier —
/// an untyped literal, a function's result — settles the answer at none, and so does two inputs
/// that disagree. Measured over `character(n)` and `numeric(p,s)` in both directions.
fn shared_typmod(args: &[Expr], scope: &Scope<'_>) -> i32 {
    let mut shared = None;
    for arg in args {
        let typmod = typmod_of(arg, scope);
        if typmod == crate::value::NO_TYPMOD || shared.is_some_and(|seen| seen != typmod) {
            return crate::value::NO_TYPMOD;
        }
        shared = Some(typmod);
    }
    shared.unwrap_or(crate::value::NO_TYPMOD)
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
        // **Two constants, or a constant beside a NULL: the ladder decides.** This read `int8`
        // for both and said so in its own comment. Now `1 + 1` is `integer + integer`, and
        // `2147483647 + 1` is `22003 integer out of range` rather than a silent widening —
        // PostgreSQL does not widen to avoid an overflow, measured.
        (Operand::Integer(a), Operand::Integer(b)) => {
            let width = if fits(a, ColumnType::Int4) && fits(b, ColumnType::Int4) {
                ColumnType::Int4
            } else {
                ColumnType::Int8
            };
            (width, width)
        }
        (Operand::Integer(value), _) | (_, Operand::Integer(value)) => {
            let width = if fits(value, ColumnType::Int4) {
                ColumnType::Int4
            } else {
                ColumnType::Int8
            };
            (width, width)
        }
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

/// The type an `ARRAY[…]` takes when two of its elements disagree, which is **not** order
/// sensitive: `ARRAY[bigint, integer]` and `ARRAY[integer, bigint]` are both `bigint[]`. Measured
/// on 19beta1 along with `ARRAY[numeric, integer]` being `numeric[]` and any string making the
/// whole array `text[]`.
///
/// A rank rather than a pair table: PostgreSQL resolves the constructor by finding the type every
/// element can be converted to, and among these that is simply the widest.
fn wider_element(left: ColumnType, right: ColumnType) -> ColumnType {
    fn rank(ty: ColumnType) -> u8 {
        match ty {
            ColumnType::Int2 => 1,
            ColumnType::Int4 => 2,
            ColumnType::Int8 | ColumnType::Oid => 3,
            ColumnType::Real => 4,
            ColumnType::Double => 5,
            ColumnType::Numeric => 6,
            // A string makes the whole array `text[]`, which is the top of this order and the
            // reason it is a rank at all: every other type has a text form and none of them is
            // reachable from one.
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => 7,
            // Two elements of one unranked type are that type; two different ones cannot both
            // convert, and the last one written is what this answered before the rank existed.
            _ => 0,
        }
    }
    if left == right {
        return left;
    }
    match (rank(left), rank(right)) {
        (0, _) | (_, 0) => right,
        (l, r) if l >= r => left,
        _ => right,
    }
}

/// The type `||` answers, which is **five types told apart by their operands**.
///
/// Every rule here is `any` except the jsonb one, which is `all` — a jsonb column beside a `text`
/// one is `text || text` on a real server, because no `jsonb || text` operator exists. That
/// quantifier is what the evaluator uses, and the two must agree or the rows and the declared
/// type part company. They had: `a || b` over two jsonb columns merged correctly and said `text`.
/// The type `greatest`/`least` answer: the arguments' common type.
///
/// **Folded through the arithmetic promotion rather than written a second time.** A rule here that
/// disagreed with `value::arith::result_type` would be a column whose declared type is not the
/// type of its values, which that module's own header calls worse than either being wrong alone.
/// Every measurement agrees with it: `int2` beside `int8` is `bigint`, an integer beside a decimal
/// is `numeric`, and one beside a `float8` is `double precision`
/// (`tests/captures/pg19_greatest_trim.txt`).
///
/// Arguments that share a type keep it — which is how `greatest('a', 'b')` is `text` — and a pair
/// the promotion has no rule for keeps the first argument's, because the *rows* are still that
/// type and a refusal here would be this node raising where a real server answers.
/// The type `date_trunc` answers, which is the type of the value it cut.
///
/// **Two of the four arms are not the type they look like.** A `date` argument resolves to the
/// `timestamptz` overload rather than the unzoned one, and the three-argument form is zoned even
/// when its value is a plain `timestamp` — a real server casts it before doing anything else.
/// Measured (`tests/captures/pg19_date_trunc.txt`).
fn date_trunc_type(args: &[Expr], scope: &Scope<'_>) -> ColumnType {
    if args.len() > 2 {
        return ColumnType::TimestampTz;
    }
    match args.get(1).map(|value| expr_type(value, scope)) {
        Some(Ok(ColumnType::Interval)) => ColumnType::Interval,
        Some(Ok(ColumnType::TimestampTz | ColumnType::Date)) => ColumnType::TimestampTz,
        _ => ColumnType::Timestamp,
    }
}

fn greatest_type(args: &[Expr], scope: &Scope<'_>) -> ColumnType {
    let mut found: Option<ColumnType> = None;
    for arg in args {
        // **An unadorned literal does not vote.** It is `unknown` to a real server's resolver and
        // takes whatever the known arguments settle on: `greatest(c, 'x')` over a `character(4)`
        // column is a `bpchar` there, and letting the literal in as the `text` this crate resolves
        // it to made it a `text` — the preferred type winning a vote it should not have had.
        if matches!(arg, Expr::Literal(Literal::String(_))) {
            continue;
        }
        let Ok(ty) = expr_type(arg, scope) else {
            continue;
        };
        found = Some(match found {
            None => ty,
            Some(sofar) if sofar == ty => sofar,
            // **The common type, not arithmetic's.** These took `+`'s promotion, which is a
            // different ladder and a right one for a different question: `int2 + float4` really is
            // a `double precision`, because adding them needs the wider float. `GREATEST` picks
            // one of the values, so it takes the type the pair *resolves* to —
            // `GREATEST(int2, float4)` is `real` on a real server, measured, and the same
            // `unify` a `UNION` over the two arms uses.
            Some(sofar) => unify(sofar, ty).unwrap_or(sofar),
        });
    }
    found.unwrap_or(ColumnType::Text)
}

/// The type `NULLIF(a, b)` answers, which is **the comparison's left input type** and not the
/// common type of the pair.
///
/// The pair that says so is `nullif(int4, int8)`, `integer` -- where `GREATEST(int4, int8)` is
/// `bigint`. PostgreSQL resolves `=` between the two arguments and the *left* side of the operator
/// it finds is the answer, so:
///
/// ```text
/// nullif(int2, int8)   smallint          nullif(int8, int2)   bigint
/// nullif(float4, float8) real            nullif(date, timestamptz) date
/// nullif(int4, numeric) numeric          nullif(int4, float8) double precision
/// nullif(varchar, text) text             nullif(char, text)   character
/// ```
///
/// Two rules cover all of it, and both are measured rather than reasoned from the type lattice.
/// **A `varchar` has no `=` of its own** — `varchar = varchar` resolves to `texteq` — so it is
/// asked as `text` and answers `text`; `bpchar` does have one and answers `character`. **And a
/// pair inside one comparison family keeps the left type**, because the family has a cross-type
/// operator to resolve to (`int48eq`, `date_lt_timestamptz`); a pair across families has none, so
/// both sides coerce and the answer is the common type after all.
///
/// `tests/nullif.rs` holds the twelve measurements. The families are the three PostgreSQL gives
/// cross-type comparison operators to; anything else is either the same type on both sides or a
/// coercion, and both of those fall out of the two rules above.
fn nullif_type(args: &[Expr], scope: &Scope<'_>) -> ColumnType {
    let compared = |at: usize| {
        args.get(at)
            .and_then(|arg| expr_type(arg, scope).ok())
            .map(compared_as)
    };
    let (Some(left), Some(right)) = (compared(0), compared(1)) else {
        return compared(0).unwrap_or(ColumnType::Text);
    };
    if left == right || same_comparison_family(left, right) {
        return left;
    }
    crate::value::arith::result_type(crate::plan::ArithOp::Add, left, right).unwrap_or(left)
}

/// [`nullif_type`]'s rule over two values rather than two expressions, for the evaluator.
///
/// The same two rules read off the datums' own types, which is how the `GREATEST` arm in
/// `exec::cursor` recomputes its promotion: the resolved type is not carried into evaluation, and
/// recomputing it there is what keeps one rule in one place.
pub(crate) fn nullif_datum_type(left: &Datum, right: &Datum) -> Option<ColumnType> {
    let left = compared_as(left.column_type()?);
    let right = compared_as(right.column_type()?);
    if left == right || same_comparison_family(left, right) {
        return Some(left);
    }
    Some(crate::value::arith::result_type(crate::plan::ArithOp::Add, left, right).unwrap_or(left))
}

/// The type an operand is *compared* at, which is its own for everything but `varchar`.
///
/// `varchar` has no equality operator of its own and resolves through `texteq`, so a comparison
/// involving one happens at `text` — measured, and visible in the printed form too:
/// `NULLIF((v)::text, 'x'::text)` on a `varchar(10)` column, where a `bpchar` column shows no cast
/// because `bpchareq` exists.
fn compared_as(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::Varchar => ColumnType::Text,
        other => other,
    }
}

/// Whether the two types have a **cross-type** comparison operator, and so resolve without
/// coercing either side.
///
/// Three families on a real server: the integers, the two floats, and the date/timestamp trio.
/// `numeric` is its own — `int4 = numeric` does not exist, which is why `nullif(int4, numeric)` is
/// `numeric` where `nullif(int4, int8)` is `integer`.
fn same_comparison_family(left: ColumnType, right: ColumnType) -> bool {
    let family = |ty: ColumnType| match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => Some(0_u8),
        ColumnType::Real | ColumnType::Double => Some(1),
        ColumnType::Date | ColumnType::Timestamp | ColumnType::TimestampTz => Some(2),
        _ => None,
    };
    matches!((family(left), family(right)), (Some(a), Some(b)) if a == b)
}

/// The type one of the counting functions answers, or `None` for the rest.
///
/// **All of them are `integer` except one**: `length` over an `lseg` or a `path` is a
/// `double precision`, the geometric length, which is the one overload of the eight whose answer
/// is not a count. Measured: `pg_typeof(length('[(0,0),(3,4)]'::lseg))` is `double precision`.
fn counting_type(
    func: crate::plan::ScalarFunc,
    operand: &Expr,
    scope: &Scope<'_>,
) -> Option<ColumnType> {
    use crate::plan::ScalarFunc;
    match func {
        ScalarFunc::Length
            if matches!(
                expr_type(operand, scope),
                Ok(ColumnType::Lseg | ColumnType::Path)
            ) =>
        {
            Some(ColumnType::Double)
        }
        ScalarFunc::Length
        | ScalarFunc::CharLength
        | ScalarFunc::CharacterLength
        | ScalarFunc::OctetLength
        | ScalarFunc::BitLength
        | ScalarFunc::Ascii => Some(ColumnType::Int4),
        _ => None,
    }
}

/// **Which types a scalar function has an overload for** — `pg_proc`, measured rather than
/// assumed.
///
/// Eight rows over the four counting names on 19beta1, and they do **not** agree with each other:
///
/// ```text
///                 length          char_length   octet_length   bit_length
/// text/character  characters      characters    bytes          bytes x 8
/// bit / varbit    **bits**        -             bytes          bits
/// bytea           bytes           -             bytes          bytes x 8
/// tsvector        **lexemes**     -             -              -
/// lseg / path     **float8**      -             -              -
/// ```
///
/// **`bit_length` has no `(character)` row** where `octet_length` does, so a `character(5)` reaches
/// it through the coercion to `text` and the trailing blanks go: `bit_length('ab'::character(5))`
/// is 16 and `octet_length` of the same value is 5. Measured.
///
/// `lower` and `upper` have a **range** overload beside the string one — they are the bounds — and
/// that is the pair this crate already told apart by the operand. Everything else here is the same
/// rule for the other five names.
///
/// `abs` is not in the table: it is the numeric one and its path is untouched.
fn scalar_accepts(func: crate::plan::ScalarFunc, ty: ColumnType) -> bool {
    use crate::plan::ScalarFunc;
    let stringy = concat_stringy(ty) || ty == ColumnType::Char;
    match func {
        // The bounds of a range, or the case of a string.
        ScalarFunc::Lower | ScalarFunc::Upper => stringy || esker_keys::row::is_range(ty),
        ScalarFunc::Reverse
        | ScalarFunc::Ascii
        | ScalarFunc::CharLength
        | ScalarFunc::CharacterLength => stringy,
        ScalarFunc::Length => {
            stringy
                || matches!(
                    ty,
                    ColumnType::Bit
                        | ColumnType::VarBit
                        | ColumnType::Bytea
                        | ColumnType::TsVector
                        | ColumnType::Lseg
                        | ColumnType::Path
                )
        }
        // **The same set, measured and not assumed.** `bit_length`'s three `pg_proc` rows are
        // `(bit)`, `(bytea)` and `(text)`, and asking `pg_typeof(bit_length(NULL::<t>))` for each
        // of the probe list's 100 spellings on 19beta1 answers for exactly eight of them —
        // `"char"`, `bit`, `bytea`, `character`, `character varying`, `citext`, `name`, `text` —
        // plus `bit varying`, which that list does not carry. That is `octet_length`'s set.
        ScalarFunc::OctetLength | ScalarFunc::BitLength => {
            stringy || matches!(ty, ColumnType::Bit | ColumnType::VarBit | ColumnType::Bytea)
        }
        // Untouched: the numeric one, whose operand the evaluator has always decided.
        ScalarFunc::Abs => true,
    }
}

/// The types that reach `text || text` through an implicit cast, which is why
/// `citext || citext` answers `text` and not `citext`.
fn concat_stringy(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Citext
            | ColumnType::Name
    )
}

/// One operand's type for `||`, where **`None` means `unknown`** — an unquoted string literal or a
/// bare `NULL`, neither of which has a type until an operator gives it one.
///
/// `expr_type` answers `text` for both, which is right almost everywhere and wrong here: `unknown`
/// is what makes `'a=>1'::hstore || 'b=>2'` an **hstore** on a real server while
/// `'a=>1'::hstore || 'x'::text` is `text`. The two differ only in whether the second operand has
/// a type, and nothing below the plan can tell.
fn concat_operand_type(expr: &Expr, scope: &Scope<'_>) -> Result<Option<ColumnType>> {
    if matches!(expr, Expr::Literal(Literal::String(_) | Literal::Null)) {
        return Ok(None);
    }
    expr_type(expr, scope).map(Some)
}

/// The type `a || b` answers, `unknown` operands included, or `None` for a pair with no operator.
///
/// **An `unknown` takes the other operand's type when that type has a `||` with itself**, and the
/// pair is `text` otherwise. Measured, and it is not "the unknown takes the other side's type":
///
/// ```text
/// 'a=>1'::hstore || 'b=>2'   hstore      '1'::bit  || '0'    bit varying
/// 'a'::tsquery   || 'b'      tsquery     'a.b'::ltree || 'c' ltree
/// 1              || 'x'      text        '2020-01-01'::date || 'x'  text
/// ```
///
/// `hstore` has an `hstore || hstore`, so the unknown becomes one; `integer` has no
/// `integer || integer`, so the pair resolves through `anynonarray || text` instead.
fn concat_answer(left: Option<ColumnType>, right: Option<ColumnType>) -> Option<ColumnType> {
    match (left, right) {
        (Some(left), Some(right)) => concat_pair(left, right),
        (Some(known), None) | (None, Some(known)) => {
            concat_pair(known, known).or(Some(ColumnType::Text))
        }
        (None, None) => Some(ColumnType::Text),
    }
}

/// **Which pairs `||` has an operator for, and the type it answers** — `pg_operator`'s entries for
/// the symbol, measured rather than accumulated.
///
/// `None` means PostgreSQL has no `||` for this pair and raises `42883`. The chain of "any operand
/// of type X" arms this replaced had a `text` fallback, so **everything** concatenated: measured
/// over all 100 type spellings of the wire v3 probe list asked three ways
/// (`tests/captures/pg19_concat_operator.txt`), 91 of the 300 shape-rows answered `text` where a
/// real server has no operator at all.
///
/// **`||` is not one operator**, which is why one probe shape could not find this. `pg_operator`
/// carries `text || text`, `anynonarray || text`, `text || anynonarray`, `anyarray || anyarray`,
/// `anyarray || anyelement`, `anyelement || anyarray`, and one each for `bytea`, `bit`,
/// `tsvector`, `tsquery`, `jsonb`, `hstore` and `ltree`.
///
/// Two widenings that an arm written by hand gets wrong, both measured: **`bit || bit` is
/// `bit varying`**, and **`citext || citext` is `text`**.
///
/// `"char"` is not here: an operand of it makes the call *ambiguous* rather than missing —
/// `42725`, decided in `resolve` before this is asked, because a real server has a candidate at
/// every string width and category `Z` prefers none of them.
fn concat_pair(left: ColumnType, right: ColumnType) -> Option<ColumnType> {
    use esker_keys::array::ArrayValue;
    let element_of = ArrayValue::element_of;
    let stringy = concat_stringy;
    match (element_of(left), element_of(right)) {
        // **`anyarray || anyarray` is `unify` on the elements**, which is what `unify` already
        // does — measured on seven pairs the probe list cannot ask, because its rows carry one
        // type: `text[] || varchar[]` is `text[]` and `varchar[] || text[]` is
        // `character varying[]`, `integer[] || bigint[]` is `bigint[]`, `citext[] || text[]` and
        // `text[] || citext[]` are both `text[]`, `name[] || text[]` is `name[]`. The asymmetry in
        // the first two is the same one `unify` documents for `name` beside `text`: where both
        // directions are implicit, the operand that came first keeps the answer.
        (Some(le), Some(re)) if stringy(le) && stringy(re) => {
            // **`citext` reaches `text` and nothing reaches back**, which is why it is the one
            // element that displaces the operand that came first: `citext[] || text[]` and
            // `text[] || citext[]` are both `text[]`, while `citext[] || citext[]` stays
            // `citext[]`. `unify` cannot see this — the cast that makes it true is created by
            // `CREATE EXTENSION citext` and is not in `pg_catalog::CASTS`, which is a dump of a
            // stock server.
            let element = if le == re {
                le
            } else if le == ColumnType::Citext || re == ColumnType::Citext {
                ColumnType::Text
            } else {
                le
            };
            ArrayValue::array_of(element)
        }
        (Some(_), Some(_)) => unify(left, right).ok(),
        // `anyarray || anyelement`, and **the side decides which array type comes out**: measured,
        // `varchar[] || text` is `character varying[]` while `text || varchar[]` is `text[]`,
        // because the first operand instantiates the polymorphic pair. A `citext[]` gives `text[]`
        // on both sides, because `citext -> text` is implicit and the other direction is not.
        (Some(element), None) if stringy(right) => {
            if matches!(
                element,
                ColumnType::Varchar | ColumnType::Bpchar | ColumnType::Name
            ) {
                Some(left)
            } else {
                stringy(element).then_some(ColumnType::TextArray)
            }
        }
        (Some(element), None) => (element == right).then_some(left),
        (None, Some(element)) if stringy(left) => stringy(element).then_some(ColumnType::TextArray),
        (None, Some(element)) => (element == left).then_some(right),
        (None, None) => concat_scalars(left, right, stringy),
    }
    .or_else(|| {
        // An array of an element the pair agrees on, for the `ARRAY[…] || element` rewrite that
        // wrapped one side already.
        (element_of(left).is_some() && left == right)
            .then(|| ArrayValue::array_of(element_of(left)?))
            .flatten()
    })
}

/// The scalar half of [`concat_pair`], split out so neither is a wall of arms.
fn concat_scalars(
    left: ColumnType,
    right: ColumnType,
    stringy: fn(ColumnType) -> bool,
) -> Option<ColumnType> {
    // **An `ltree` beside a string is an `ltree`**, which is a real `ltree || text` operator and
    // not the `anynonarray || text` one — so it is asked before the string rule that would make it
    // `text`.
    if (left == ColumnType::Ltree && stringy(right))
        || (stringy(left) && right == ColumnType::Ltree)
    {
        return Some(ColumnType::Ltree);
    }
    if stringy(left) && stringy(right) {
        return Some(ColumnType::Text);
    }
    // **The two vectors keep the answer they have**, which is neither server's. PostgreSQL's
    // `int2vector` and `oidvector` really are arrays — `int2vector || int2vector` is `smallint[]`
    // there, measured — and here they borrow `text`'s representation, so answering the array type
    // would mean *parsing* the value and not only declaring a type. That is family **F6**. Left
    // answering `text` on purpose: turning a wrong type into a refusal would break the queries
    // that concatenate one today, and neither answer is right until F6 lands.
    //
    // **Only beside itself.** `int2vector || text` is `42883` on 19beta1 — a vector is in the
    // array category there and `text` is not its element — and this node answered `1 2a`. Written
    // as one rule for both operands first, which kept that row answering; measured, and narrowed.
    if left == right && matches!(left, ColumnType::Int2Vector | ColumnType::OidVector) {
        return Some(ColumnType::Text);
    }
    // **The widening, and it does not need the two spellings to match**: `bit(3) || bit(2)`,
    // `varbit || varbit` and `varbit || bit(2)` are all `bit varying` on 19beta1
    // (`tests/corpus/pg19_varbit.txt`). Written first as `left == right` only, which left
    // `varbit || bit(2)` refused — a declared divergence that stayed declared because the pair
    // was never asked with two spellings.
    if matches!(left, ColumnType::Bit | ColumnType::VarBit)
        && matches!(right, ColumnType::Bit | ColumnType::VarBit)
    {
        return Some(ColumnType::VarBit);
    }
    if left == right {
        return match left {
            ColumnType::Bytea
            | ColumnType::TsVector
            | ColumnType::TsQuery
            | ColumnType::Jsonb
            | ColumnType::Hstore
            | ColumnType::Ltree => Some(left),
            _ => None,
        };
    }
    // **The two vectors are not `anynonarray`.** They are in PostgreSQL's *array* category, so
    // `int2vector || text` is `42883` there — `text` is not an `int2` — where every other
    // non-array scalar beside a string is `text`. Here they are `Datum::Text`, so the rule below
    // would have answered `1 2a`; measured, and excluded.
    if matches!(left, ColumnType::Int2Vector | ColumnType::OidVector)
        || matches!(right, ColumnType::Int2Vector | ColumnType::OidVector)
    {
        return None;
    }
    // `anynonarray || text` and `text || anynonarray`, the pair that makes `id || '-'` work.
    (stringy(left) || stringy(right)).then_some(ColumnType::Text)
}

fn concat_type(call: &crate::plan::CatalogFuncCall, scope: &Scope<'_>) -> ColumnType {
    // **Folded pairwise, left to right, because that is how `||` associates.** `a || b || c` is
    // `(a || b) || c` on a real server and the middle type is what the third operand meets.
    //
    // A pair [`concat_pair`] has no operator for cannot be reached: `resolve` refuses it with
    // `42883` before anything asks for a type. `text` is the total answer for that unreachable
    // case and for the `unknown` literals, which is what they resolve to.
    let mut folded: Option<Option<ColumnType>> = None;
    for arg in &call.args {
        let Ok(ty) = concat_operand_type(arg, scope) else {
            return ColumnType::Text;
        };
        folded = Some(match folded {
            None => ty,
            Some(left) => concat_answer(left, ty),
        });
    }
    folded.flatten().unwrap_or(ColumnType::Text)
}

/// Which fetch a `->` is, from the **declared type of its operand**.
///
/// `->` means an hstore's fetch and a document's, and a `jsonb` is a canonical `Datum::Text` here
/// — so nothing about the values decides it and the plan's own types have to. Asked in two places
/// that must not disagree: `resolve`, which rewrites the call so the evaluator does the right
/// fetch, and `expr_type`, which is what a client is **told**.
///
/// **Both, because the second was missing and the first alone is a wrong answer.** With only the
/// rewrite, `payload->'a'` over a column returned the right `{}` and described it as `text`
/// (oid 25) where a real server says `jsonb` (3802) — and `ActiveRecord` decodes by that oid, so
/// it handed back the string `"{}"` instead of a Hash. `pg_typeof` could not see it (it folds at
/// resolution, off the rewritten call) and neither could a rendered value, because `text` and
/// `json` print identically. Only `ftype()` off the wire can, which is what the test asserts.
/// The element type of an `ARRAY[…]`, from the type it was resolved with or from its elements.
///
/// **Two askers, and the second is why this is a function.** `resolve` settles the type and stores
/// it on the node; `expr_type` is asked by `output_columns` on the **unresolved** projection, where
/// the stored type is still `None`. Reading `None` as `text[]` there is the same wrong-declaration
/// bug `->` had — the rows would be an `integer[]` and the client would be told `text[]`, and
/// `ActiveRecord` decodes an array by that oid exactly as it decodes a document by `->`'s.
fn array_element_type(
    elements: &[Expr],
    settled: Option<ColumnType>,
    scope: &Scope<'_>,
) -> Result<Option<ColumnType>> {
    if settled.is_some() {
        return Ok(settled);
    }
    let mut widest = None;
    for element in elements {
        let ty = expr_type(element, scope)?;
        widest = Some(match widest {
            None => ty,
            Some(so_far) => wider_element(so_far, ty),
        });
    }
    Ok(widest)
}

/// The type `lower` or `upper` answers when its operand is a **range**, or `None` for the rest.
///
/// The two names are overloaded on a real server exactly as they are here: over a string they fold
/// case and answer `text`, over a range they are the bounds and answer the **subtype** —
/// `lower(ts_range)` is a `timestamp`, measured. The evaluator has told them apart by the operand
/// since the range unit; this is the same rule on the side that says what a client is told.
///
/// **The eight range types are written out** rather than asked of `crate::value::range_subtype`,
/// which answers `timestamp` for everything it does not know: a guard written as "its subtype is
/// not `text`" made `lower('MiXeD')` a `timestamp`, which is the second time in this queue that a
/// helper's fallback has been read as an answer.
fn range_bound_type(
    func: crate::plan::ScalarFunc,
    operand: &Expr,
    scope: &Scope<'_>,
) -> Result<Option<ColumnType>> {
    if !matches!(
        func,
        crate::plan::ScalarFunc::Lower | crate::plan::ScalarFunc::Upper
    ) {
        return Ok(None);
    }
    let ty = expr_type(operand, scope)?;
    Ok(matches!(
        ty,
        ColumnType::TsRange
            | ColumnType::TstzRange
            | ColumnType::Int4Range
            | ColumnType::Int8Range
            | ColumnType::DateRange
            | ColumnType::NumRange
            | ColumnType::FloatRange
            | ColumnType::VarcharRange
    )
    .then(|| crate::value::range_subtype(ty)))
}

/// How PostgreSQL names one `||` operand in its `42725`.
///
/// **An unadorned literal is `unknown` there**, which this crate has no type for — it resolves one
/// to `text` before anything asks — so the name comes from the *expression* rather than from its
/// resolved type. That is the one place in this message where the two differ, and it is why
/// `'r'::"char" || 'x'` reads `"char" || unknown` on both.
/// Which side of an array `||` is the **element**, or `None` when both or neither is an array.
///
/// The wrapping this decides is what makes the operator's two NULL rules one rule; see the arm in
/// [`resolve`] that calls it.
fn concat_element_side(args: &[Expr], scope: &Scope<'_>) -> Option<usize> {
    let is_array = |at: usize| {
        matches!(expr_type(&args[at], scope), Ok(ty)
            if esker_keys::array::ArrayValue::element_of(ty).is_some())
    };
    match (is_array(0), is_array(1)) {
        (true, false) => Some(1),
        (false, true) => Some(0),
        _ => None,
    }
}

fn concat_operand_name(expr: Option<&Expr>, scope: &Scope<'_>) -> String {
    match expr {
        None | Some(Expr::Literal(Literal::String(_) | Literal::Null)) => "unknown".to_owned(),
        Some(expr) => {
            expr_type(expr, scope).map_or_else(|_| "unknown".to_owned(), |ty| ty.name().to_owned())
        }
    }
}

/// The `regtype` `pg_typeof` answers for one argument.
///
/// **A user-defined type names itself**, which the declared type alone cannot give: an enum's
/// storage is an `int2` (ADR 0050) and a `floatrange`'s is a range representation, and `pg_typeof`
/// reports what the column was *declared* as. `Scope::user_type_at` is the same lookup
/// `OutputColumn::user_type` uses, so this function and the `RowDescription` beside it cannot
/// disagree — which is the property the datum-reading version could not have.
fn pg_typeof_of(expr: &Expr, scope: &Scope<'_>) -> Result<Datum> {
    if let Expr::Ordinal { at, .. } = expr
        && let Some(def) = scope.user_type_at(*at)
    {
        return Ok(Datum::RegType {
            oid: u32::try_from(def.oid).unwrap_or(0),
            // **The stored name as a sentence spells it**, which is a NUL apart from what it
            // holds: a type in a schema is `schema ++ NUL ++ name` on disk, and `pg_typeof` was
            // handing that byte to a client — `s\0dom_probe` where a real server says
            // `s.dom_probe`. `format_type` had it right one screen away, which is how it was
            // found — and it is `information_schema.sql_identifier` that needs it, since every
            // column of those views is a domain now (ADR 0103).
            //
            // **Qualified for anything outside `public`**, which is `qualify`'s rule read back
            // and not quite PostgreSQL's: a real server qualifies a type that is not in the
            // *session's* search path, so a schema a client has put on its path prints bare there
            // and qualified here. `exec::cursor`'s `type_qualified_for` asks the session and is
            // the right answer; this function has no `Settings` to ask, and the narrower
            // divergence is not the one that was putting a NUL on the wire.
            name: crate::catalog::display_name(&def.name).into(),
        });
    }
    Ok(crate::value::regtype_of_oid(expr_type(expr, scope)?.oid()))
}

/// **Whether a type has a `btree` comparison function**, which is what `GREATEST`/`LEAST` needs.
///
/// The eleven that do not, and the arrays of them this node has — measured on 19beta1 over every
/// type spelling the wire v3 probe list carries, 166 of them, of which exactly twenty refuse
/// (`tests/captures/pg19_min_max_greatest.txt`). `lquery[]` refuses there too and is absent here
/// because this node has no such type; `void` has no array on either server.
///
/// **Everything else has one**, `int2vector`, `oidvector`, `hstore`, `tsvector`, `tsquery`,
/// `money`, `macaddr`, `bit`, `citext`, `ltree`, `jsonb`, `uuid` and every range included — which
/// is why this is a refusal list rather than an allow list, and why it was measured whole.
fn comparable_by_btree(ty: ColumnType) -> bool {
    !matches!(
        ty,
        ColumnType::Json
            | ColumnType::JsonArray
            | ColumnType::Xml
            | ColumnType::XmlArray
            | ColumnType::Point
            | ColumnType::PointArray
            | ColumnType::Lseg
            | ColumnType::LsegArray
            | ColumnType::Box
            | ColumnType::BoxArray
            | ColumnType::Path
            | ColumnType::PathArray
            | ColumnType::Polygon
            | ColumnType::PolygonArray
            | ColumnType::Circle
            | ColumnType::CircleArray
            | ColumnType::Line
            | ColumnType::LineArray
            | ColumnType::LQuery
            | ColumnType::Void
    )
}

fn arrow_fetch(func: CatalogFunc, args: &[Expr], scope: &Scope<'_>) -> CatalogFunc {
    if func != CatalogFunc::HstoreFetch {
        return func;
    }
    match args.first().map(|operand| expr_type(operand, scope)) {
        Some(Ok(ColumnType::Jsonb)) => CatalogFunc::JsonbFetch,
        Some(Ok(ColumnType::Json)) => CatalogFunc::JsonFetch,
        _ => func,
    }
}

/// The type a catalog function answers, for the four that cannot answer without their arguments.
///
/// Split out of [`expr_type`] so that neither is over a hundred lines; the arms are in the order
/// they were written and each says why it is not `CatalogFunc::result_type`.
fn catalog_func_type(call: &crate::plan::CatalogFuncCall, scope: &Scope<'_>) -> ColumnType {
    match call.func {
        CatalogFunc::HstoreConcat => concat_type(call, scope),
        // **`substring` over a bit string answers a bit string**, and a plain `bit` whichever of
        // the two it was given — measured, `pg_typeof(substring('10110'::varbit from 2 for 3))` is
        // `bit`. One name over two families, told apart by the operand, which is the same shape as
        // `||` and `->` above; `text` is the answer for everything else.
        CatalogFunc::Substr | CatalogFunc::Substring
            if matches!(
                call.args.first().map(|arg| expr_type(arg, scope)),
                Some(Ok(ColumnType::Bit | ColumnType::VarBit))
            ) =>
        {
            ColumnType::Bit
        }
        // **`->` is the same shape as `||` above** — one symbol over several types, told apart by
        // the operand — and it needs the same arm here for the same reason that one gives: the
        // rows were already right and it was the *declared* type that said `text`, which a client
        // binds against.
        CatalogFunc::HstoreFetch => arrow_fetch(call.func, &call.args, scope).result_type(),
        // **`date_trunc` answers the type of the value it cut**, and two of the four arms are not
        // the type they look like: a `date` argument resolves to the `timestamptz` overload, and
        // so does the three-argument form even when its value is an unzoned `timestamp`. This is
        // the half of the function that `timestamp_test.rb` reads — `assert_kind_of Time` over the
        // grouped keys is a String and a failure if the column is described as `text`.
        CatalogFunc::DateTrunc => date_trunc_type(&call.args, scope),
        // **The common type of the arguments**, which is what a real server resolves and what
        // `CatalogFunc::result_type` cannot answer without them. Measured: `int2` beside `int8`
        // is `bigint`, an integer beside a decimal is `numeric`, and one beside a `float8` is
        // `double precision` — the same promotion arithmetic makes, so it is folded through that
        // rule rather than written a second time.
        CatalogFunc::Greatest | CatalogFunc::Least => greatest_type(&call.args, scope),
        CatalogFunc::NullIf => nullif_type(&call.args, scope),
        // The operator's own rule, since it is the operator's own implementation.
        CatalogFunc::Mod => match (
            call.args.first().map(|a| expr_type(a, scope)),
            call.args.get(1).map(|a| expr_type(a, scope)),
        ) {
            (Some(Ok(left)), Some(Ok(right))) => {
                crate::value::arith::result_type(crate::plan::ArithOp::Modulo, left, right)
                    .unwrap_or(left)
            }
            _ => ColumnType::Int8,
        },
        _ => call.func.result_type(),
    }
}

/// The type a `COALESCE`'s arguments or a `CASE`'s results settle on, asked of the one
/// [`common_of`] the resolution asks.
///
/// `branch_type` rather than `expr_type` is the whole of the filter: an `unknown` and a bare NULL
/// carry no type and take one from the branch that has one, so they are skipped here and coerced
/// afterwards.
fn branch_result_type<'a>(
    results: impl Iterator<Item = &'a Expr>,
    kind: Unifying,
    scope: &Scope<'_>,
) -> Result<ColumnType> {
    let types: Vec<ColumnType> = results
        .filter_map(|expr| branch_type(expr, scope))
        .collect();
    common_of(&types, kind)
}

pub(super) fn expr_type(expr: &Expr, scope: &Scope<'_>) -> Result<ColumnType> {
    Ok(match expr {
        // The type the cast named. Settled at lowering, where the permission was checked too.
        Expr::Cast { to, .. } => *to,
        // Both operands, then the promotion table — the same table the evaluator uses, so the
        // type a client is told matches the values it is sent.
        Expr::Negate(operand) => crate::value::arith::negate_type(expr_type(operand, scope)?)?,
        // Settled at resolution and carried, for the reason `Arithmetic::ty` is: a client is told
        // the column's type before any row is read. **And computed from the elements when it is
        // not settled**, because `output_columns` asks this of the *unresolved* projection —
        // answering `text[]` there is a right value under a wrong declared type, which is what
        // `->` was doing one arm below.
        Expr::Array { elements, element } => array_element_type(elements, *element, scope)?
            .and_then(esker_keys::array::ArrayValue::array_over)
            .unwrap_or(ColumnType::TextArray),
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
        // A sequence function answers `bigint` on a real server, all four of them — and it no
        // longer shares this arm with an integer literal, which is a different question with a
        // different answer since the ladder gained its `int4` rung.
        Expr::Sequence(_) => ColumnType::Int8,
        Expr::Literal(Literal::Integer(value)) => {
            if i32::try_from(*value).is_ok() {
                ColumnType::Int4
            } else {
                ColumnType::Int8
            }
        }
        // Every catalog function returns `text`, which is what makes them one variant.
        // **`||` is spelled the same for three types**, and its result is its operands': an
        // hstore concatenation is an hstore and everything else is `text` — measured,
        // `pg_typeof('x'::citext || 'y')` is `text`. This works only because a folded
        // `'a=>b'::hstore` constant is a `Datum::Hstore` and not a `Datum::Text`; while it was the
        // latter, the type was gone by the time anything could ask, and the two concatenations
        // were indistinguishable.
        Expr::CatalogFunc(call) => catalog_func_type(call, scope),
        // **`lower` and `upper` are overloaded on a range**, which is how a real server spells them
        // too: over a string they fold case and answer `text`, over a range they are the bounds and
        // answer the **subtype** — `lower(ts_range)` is a `timestamp`, measured. The evaluator has
        // told them apart by the operand since the range unit; this is the same rule, on the side
        // that says what a client is told.
        // **A bare decimal is a `numeric`**, which `literal_type` also says — the two must agree or
        // a client is told one type and sent another's characters.
        Expr::Literal(Literal::Decimal(_)) => ColumnType::Numeric,

        // `abs` is the one scalar function that answers its argument's type rather than `text`.
        //
        // **Including an aggregate argument**, which used to be `text` here: `abs(min(n))` is
        // typed while the aggregates are still un-rewritten, and the arm below answered the
        // internal "reached `expr_type`" for it, so `text` was the lesser of two wrong answers.
        // The arm answers now, so this asks it like any other operand — and measured on 19beta1,
        // `abs(min(int4))` is `integer`, `abs(sum(int4))` is `bigint`, `abs(avg(int4))` is
        // `numeric` and `abs(count(*))` is `bigint`, which is exactly its argument's type.
        // **And a `COLLATE`, which has the type it was written on**: it changes an ordering
        // and never a representation, so it is transparent here exactly as `abs` is over its
        // argument (ADR 0096).
        Expr::Scalar {
            func: crate::plan::ScalarFunc::Abs,
            operand,
        }
        | Expr::Collate { operand, .. } => expr_type(operand, scope)?,
        // **The three counting functions answer `integer`, whatever they count.** Measured on
        // 19beta1: `pg_typeof(length('abc'))`, `char_length` and `octet_length` are all `integer`,
        // and this node declared `text` for every one of them — the *values* were always right, so
        // only a client that binds by the declared type could see it, which is exactly what
        // `ActiveRecord` does. Found while wiring `length(tsvector)`, which is the same function
        // over a fourth operand.
        Expr::Scalar { func, operand } if let Some(ty) = counting_type(*func, operand, scope) => ty,
        // **The session functions answer `name`, and the plural answers `name[]`.** Measured:
        // `current_schema()`, `current_database()` and `current_user` are `name` on a real server
        // and `current_schemas(bool)` is `name[]`, where `current_setting()` is a `text` and stays
        // below with the rest. They were all `text` here while the node had no array of `name` to
        // name — `n.nspname = ANY (current_schemas(false))` is the predicate every catalog query
        // `ActiveRecord` sends is built on (`tests/captures/pg19_name_array.txt`).
        // **A `void` for the three that cannot fail, a `boolean` for the four that can** — the
        // split PostgreSQL's own `pg_proc.prorettype` makes, measured for all eleven of its
        // advisory functions. This node folded every one of them to an empty string, so a client
        // was told `text` for all seven.
        Expr::Advisory { call, .. } => {
            if call.is_void() {
                ColumnType::Void
            } else {
                ColumnType::Bool
            }
        }
        Expr::CurrentSchema { all: Some(_) } => ColumnType::NameArray,
        Expr::CurrentSchema { all: None } | Expr::CurrentDatabase | Expr::CurrentUser => {
            ColumnType::Name
        }
        // Whatever the operand is, a cast to `text` answers `text` — that is what it is for.
        // The two text functions take text and answer text.
        // **`lower` and `upper` over a range answer the subtype**, and every other scalar
        // function that reaches here answers `text`. Placed after the arms that name a
        // function, so those keep deciding first.
        Expr::Scalar { func, operand } => {
            range_bound_type(*func, operand, scope)?.unwrap_or(ColumnType::Text)
        }
        Expr::ToText { .. }
        | Expr::CurrentSetting { .. }
        | Expr::Literal(Literal::String(_) | Literal::Null) => ColumnType::Text,
        Expr::Literal(Literal::Typed(value)) => value.column_type().unwrap_or(ColumnType::Text),
        Expr::Literal(Literal::Bool(_))
        | Expr::Like { .. }
        | Expr::RegexMatch { .. }
        | Expr::Binary { .. }
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InList { .. }
        | Expr::QuantifiedArray { .. } => ColumnType::Bool,
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
        // **One reader for a subscript's element type, not two.** This asked the operand's array
        // element and fell back to the `element` the node carries; `resolve` set that field from
        // [`attnum_vector_element`], which also knows the catalog's text vectors. So the two
        // agreed only *after* resolution — and `output_columns` types a projection **before** it,
        // which is why `indkey[0]` was described `text` (25) while `pg_typeof(indkey[0])` answered
        // `smallint`. The wire and the function disagreed about the same expression, and only a
        // `Describe` could see it.
        Expr::Subscript {
            operand, element, ..
        } => attnum_vector_element(operand, scope).unwrap_or(*element),
        Expr::Uuid(_) => ColumnType::Uuid,
        // Resolution has already given every argument the common type, so the first one that
        // **carries** a type is the answer. `branch_type` rather than `expr_type` is the whole of
        // it: a bare NULL answers `text` from the second and nothing from the first, and
        // `COALESCE(NULL, 2)` typed as `text` made `COALESCE(1, NULL) + COALESCE(NULL, 2)` the
        // `42883 operator does not exist: bigint + text` that a real server adds without blinking.
        // One generated **value**, not the set: the column a client is described is the element
        // type. `unnest` answers its array's element type and the two generators answer their own.
        Expr::SetFunc(call) => super::table_function::result_type(call, scope),
        // **The branches are unified, not raced.** `COALESCE(bigint_column, 0)` is a `bigint` on
        // a real server, and so is `CASE WHEN flag THEN id ELSE 0 END`. This took the first branch
        // that carried a type, with the `ELSE` read first — and while every integer literal was
        // an `int8` the two rules agreed on every statement in every corpus here. The `int4` rung
        // is what made the difference observable, and the two cannot land apart: unifying alone
        // makes `COALESCE(NULL::integer, 0)` a `bigint`, and the rung alone makes the `ELSE` win.
        //
        // **And it is `common_of`, the one `resolve_coalesce` uses**, not a fold of its own. There
        // were three readers of this question — this one, `branch_common_type` above it, and the
        // resolution — and they were three rules: `pg_typeof(COALESCE("char"[], text[]))` read one
        // of them and said `"char"[]` where the resolution settled on `text[]`, which is 19beta1's
        // answer.
        Expr::Coalesce(args) => branch_result_type(args.iter(), Unifying::Coalesce, scope)?,
        // **A `CASE`'s type is its branches' and never its operand's**, so the simple
        // form answers exactly as the searched one does: `CASE a WHEN 1 THEN 'x' END`
        // is `text` however `a` is typed.
        Expr::Case {
            operand: _,
            branches,
            otherwise,
        } => branch_result_type(
            otherwise
                .iter()
                .map(AsRef::as_ref)
                .chain(branches.iter().map(|branch| &branch.then)),
            Unifying::Case,
            scope,
        )?,
        Expr::Parameter(number) => return Err(SqlError::UndefinedParameter(*number)),
        // **An aggregate is typed from its argument**, by the same table the aggregation itself
        // uses — because this is asked *before* the aggregation exists.
        //
        // By the time a plan is executed every aggregate has been rewritten into an `Ordinal`
        // carrying the answer, and this arm used to say so with an `XX000`. Three things asked
        // anyway, and each was wrong in its own way: `SELECT (sum(salary) + 0) > $1` reached the
        // internal error, `sum(salary) > 'x'::text` slipped past an operator check that already
        // answered `42883` for `80000::int8 > 'x'::text`, and a parameter beside an aggregate
        // fell back to `text` and compared as one — zero rows for `HAVING sum(salary) > $1`
        // where a real server answers three (`tests/having_bind.rs`).
        //
        // An aggregate whose argument has no aggregate for it — `sum(text)` — has no type either,
        // and the refusal is the aggregation's own `42883`, raised here rather than invented.
        Expr::Aggregate(call) => {
            let arg = match call.arg() {
                Some(arg) => Some(expr_type(arg, scope)?),
                // `count(*)`, which reads no value.
                None => None,
            };
            aggregate::Aggregation::result_type(call.func, arg)?
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
