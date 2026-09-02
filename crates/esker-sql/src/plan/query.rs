//! `SELECT`, lowered, and the physical plan it becomes.
//!
//! Two shapes live here and the difference between them is the planner. [`Select`] is the
//! statement as written — names, not positions; a `WHERE` clause, not an access path. [`Node`] is
//! what the executor pulls rows through, and every column reference in it has been resolved to a
//! position and every access path chosen.
//!
//! # What the planner is allowed to decide
//!
//! Rule-based, and the rules are few enough to name:
//!
//! 1. A `WHERE` that pins the **whole primary key** to constants is a point read, not a scan.
//! 2. A `WHERE` that bounds the **first primary key column** narrows the scanned range instead of
//!    filtering every row out of it.
//! 3. A `WHERE` that pins the whole key of a **unique index** is a lookup in that index followed by
//!    a point read of the row it names.
//!
//! Anything else is a sequential scan with a filter over it, which is always correct and sometimes
//! slow. `EXPLAIN` prints which one was chosen, because a plan a user cannot see is a plan they
//! cannot fix.

use crate::catalog::pg_catalog::CatalogView;
use crate::plan::{AggregateFunc, Expr, Literal};
use crate::row::RowSchema;
use crate::value::PgDatum;
use crate::value::{ColumnType, Datum};

/// Which rows a join keeps when nothing on the right matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `JOIN` / `INNER JOIN`: the pair, or nothing.
    Inner,
    /// `LEFT [OUTER] JOIN`: the pair, or the left row with **every** right column NULL.
    ///
    /// The one that must not drop rows, and the reason the `ON` and the `WHERE` cannot be merged:
    /// `ON l.id = r.id AND r.flag` keeps a left row with NULLs where `WHERE r.flag` removes it
    /// entirely. Measured, both (`tests/corpus/pg19_join.txt`).
    Left,
}

/// One entry in a `FROM` clause: a table, and the name the query refers to it by.
///
/// An **alias replaces the name**, which is the whole of what one is and the half a scope keyed by
/// the table's own name gets wrong: after `FROM pg_type AS t`, `t.oid` resolves and `pg_type.oid`
/// is `42P01` — measured, with a `HINT` naming the alias
/// (`tests/corpus/pg19_alias.txt`). So [`TableRef::referred_as`] is the *only* name resolution may
/// match, and the table's own name is kept for the catalog lookup and for `EXPLAIN`.
///
/// Not `Eq`, since [`TableRef::derived`] carries a whole `SELECT` and an expression in one can hold
/// a float literal — the same reason [`Select`] is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// The relation, as the catalog knows it.
    ///
    /// **Empty for a derived table with no alias**, which PostgreSQL 19 allows (measured;
    /// `tests/corpus/pg19_subquery_from.txt`). Empty is the honest name for a relation there is no
    /// way to qualify: no identifier folds to it, so no qualifier can match, and generating one
    /// would be inventing a name a user could collide with.
    pub name: String,
    /// `AS x`, or `x` — the name a qualifier must use once it is there.
    pub alias: Option<String>,
    /// `FROM (SELECT …) AS t` — the sub-select this entry is, or `None` for a real relation.
    pub derived: Option<Box<crate::plan::Derived>>,
    /// Set when this name is a `WITH` item **this part of the query cannot see** — a forward
    /// reference, or a CTE referring to itself.
    ///
    /// It changes the message and nothing else, and only when the lookup fails: a later CTE does
    /// **not** hide a real table of the same name from an earlier body (measured), so the flag is
    /// read after the catalog has been asked and answered `42P01`. Then it becomes PostgreSQL's
    /// three-part answer, whose `DETAIL` is the half that matters — a bare `relation "a" does not
    /// exist` sends a reader looking for a missing table when what is wrong is the order of two
    /// things they wrote.
    pub hidden_cte: bool,
}

impl TableRef {
    /// A table under its own name.
    #[must_use]
    pub fn bare(name: String) -> Self {
        TableRef {
            name,
            alias: None,
            derived: None,
            hidden_cte: false,
        }
    }

    /// The plan a derived table's rows come from, or `None` for a real relation.
    #[must_use]
    pub fn derived_plan(&self) -> Option<&Node> {
        self.derived
            .as_ref()
            .and_then(|derived| derived.plan.as_deref())
    }

    /// The name a qualifier in this query has to write: the alias if there is one, the table's own
    /// name otherwise.
    #[must_use]
    pub fn referred_as(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// One `JOIN`, as written.
///
/// A statement may have a **chain** of them, executed left-deep in the order written — which is
/// what the SQL means, not a planner choice: `A LEFT JOIN B ON … JOIN C ON …` is
/// `((A LJ B) JOIN C)`, and the inner join filters back out the rows the left join NULL-extended.
/// Measured, `tests/corpus/pg19_join_chain.txt`.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// The right-hand table, under the name the query refers to it by.
    pub table: TableRef,
    /// Whether an unmatched left row survives.
    pub kind: JoinKind,
    /// `ON`, or `None` for a `CROSS JOIN` and for `USING`, which becomes one.
    pub on: Option<Expr>,
    /// `USING (a, b, …)`, the columns as written.
    ///
    /// Kept alongside the `ON` it lowers to rather than only as that `ON`, because `USING` does a
    /// second thing an equality cannot: it **merges** the named columns, so `SELECT *` returns each
    /// one once and at the front, and a bare reference to one is no longer ambiguous.
    pub using: Vec<String>,
}

/// `SELECT`, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// The table, or `None` for `SELECT 1` — a single row of no table at all, which drivers use to
    /// check a connection. With any [`Select::joins`] it is the left-most one.
    pub from: Option<TableRef>,
    /// The `WITH` list, lowered, as the derived tables they are inlined as.
    ///
    /// Every CTE is here whether anything referenced it or not, and that is the whole reason the
    /// field exists: **an unreferenced CTE is still analysed** — `WITH t AS (SELECT nope FROM a)
    /// SELECT 1` is `42703` on a real server, measured — and inlining alone would never look at
    /// one nobody references. The planner plans each of these for its errors and throws the plan
    /// away (`crate::plan::cte`).
    pub ctes: Vec<TableRef>,
    /// The joins, in the order written. Empty for a statement with none.
    ///
    /// A chain rather than one, because `ActiveRecord`'s `indexes()` sends four tables and three
    /// joins — `pg_class` twice under two aliases — and rung 3 of the ladder stops on it.
    pub joins: Vec<Join>,
    /// What to return.
    pub projection: Vec<SelectItem>,
    /// `WHERE`.
    pub filter: Option<Expr>,
    /// `SELECT DISTINCT`. `DISTINCT ON` is a different clause and is `0A000` naming itself.
    pub distinct: bool,
    /// `GROUP BY`, in the order written. An integer literal here is a **position** in the target
    /// list, which is PostgreSQL's rule and not a constant to group by.
    pub group_by: Vec<Expr>,
    /// `HAVING`.
    ///
    /// Its own field rather than a second `filter`, because it is evaluated over a *group* and not
    /// over a row — and because the two clauses do not share a name scope: `GROUP BY` may name an
    /// output alias and `HAVING` may not (measured; `tests/corpus/pg19_aggregate.txt`).
    pub having: Option<Expr>,
    /// `ORDER BY`, in significance order.
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`.
    pub limit: Option<Expr>,
    /// `OFFSET`.
    pub offset: Option<Expr>,
}

/// One item in a target list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// `t.*` — every column of one table in the query. Its own variant rather than a flag on
    /// [`SelectItem::Wildcard`], because a qualifier that names no table in the query is `42P01`
    /// and an absent one never can be.
    QualifiedWildcard(String),
    /// An expression, with the name it will be reported under. `None` means PostgreSQL's own
    /// default: a bare column keeps its name and anything else is `?column?`.
    Expr {
        /// What to evaluate.
        expr: Expr,
        /// `AS name`.
        alias: Option<String>,
    },
}

/// How the inner side of a nested-loop join produces rows for one outer row.
///
/// The three that are not [`Probe::Materialize`] are the reason a join is worth planning at all:
/// each turns "read the inner table again" into "read one key". They are the *same* two access
/// paths the planner already knows for a `WHERE` (this module's rules 1 and 3), reached from
/// an outer row's value instead of from a constant — a join is a `WHERE` whose right-hand side
/// changes per row, and nothing more.
#[derive(Debug, Clone)]
pub enum Probe {
    /// Read the inner table once into memory and pair every outer row with all of it, filtering by
    /// the join condition. Always correct, and what a join with no usable index gets.
    Materialize,
    /// The inner table's whole primary key is one outer column: one point read per outer row.
    PrimaryKey {
        /// Position of the value in the outer row.
        outer: usize,
    },
    /// A unique index's whole key is one outer column: one index read and one point read.
    UniqueIndex {
        /// The index.
        index_id: u64,
        /// Its name, for `EXPLAIN`.
        index_name: String,
        /// Position of the value in the outer row.
        outer: usize,
        /// The primary key columns, for decoding what the entry points at. A key is exactly its
        /// own width, so it pads nothing.
        primary_key_types: RowSchema,
    },
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// What to sort on.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST` / `NULLS LAST` when the user said so. `None` means PostgreSQL's default,
    /// which is last for ascending and first for descending — the two are not the same rule
    /// written twice, they are one rule about where NULL sits in the *value* order.
    pub nulls_first: Option<bool>,
}

/// A physical plan node: rows come out of it one at a time.
#[derive(Debug, Clone)]
pub enum Node {
    /// One row of no table, for `SELECT 1`.
    OneRow,
    /// Every row of a `pg_catalog` relation, which is **computed rather than stored**.
    ///
    /// It has no key range, no index and no statistics, so there is one access path and it is all
    /// of the rows. That is the cost of the shape `docs/plans/phase-9-rails.md` §Unit 5 chose —
    /// views over the records rather than catalog tables kept in step by every DDL statement — and
    /// for a schema dump over a handful of relations it is the right trade. Materialising is a
    /// change behind this same variant if it ever is not.
    CatalogView {
        /// Which relation.
        view: CatalogView,
        /// How its rows are shaped, so everything above it reads a row like any other.
        columns: RowSchema,
    },
    /// Every row of a table, in primary key order, over a key range.
    SeqScan {
        /// The table.
        table_id: u64,
        /// How the table's rows decode: types, and what an absent column reads as.
        columns: RowSchema,
        /// Inclusive start of the scanned range.
        start: Vec<u8>,
        /// Exclusive end.
        end: Vec<u8>,
        /// What the range came from, for `EXPLAIN` — a narrowed range is the interesting case.
        narrowed: bool,
    },
    /// One row, by its primary key.
    PointGet {
        /// The table.
        table_id: u64,
        /// How the table's rows decode: types, and what an absent column reads as.
        columns: RowSchema,
        /// The key values, in key order.
        key: Vec<Datum>,
    },
    /// One row, found through a unique index.
    IndexLookup {
        /// The table.
        table_id: u64,
        /// How the table's rows decode.
        columns: RowSchema,
        /// The index.
        index_id: u64,
        /// Its name, for `EXPLAIN`.
        index_name: String,
        /// The indexed values, in index order.
        key: Vec<Datum>,
        /// The primary key columns, for decoding what the entry points at. A key is exactly its
        /// own width, so it pads nothing.
        primary_key_types: RowSchema,
    },
    /// Keeps the rows its predicate is true for. NULL is not true.
    Filter {
        /// Where the rows come from.
        input: Box<Node>,
        /// The predicate, with columns already resolved to positions.
        predicate: Expr,
    },
    /// Rebuilds each row as the target list.
    Project {
        /// Where the rows come from.
        input: Box<Node>,
        /// One expression per output column.
        exprs: Vec<Expr>,
    },
    /// Buffers its input and orders it. The one node that cannot stream: the last row of the input
    /// can be the first row of the output.
    Sort {
        /// Where the rows come from.
        input: Box<Node>,
        /// Keys, in significance order.
        keys: Vec<SortKey>,
    },
    /// A nested-loop join: for every row of `outer`, the rows of the inner table that match.
    ///
    /// The output row is the outer row's columns followed by the inner row's, which is the order
    /// `FROM a JOIN b` gives them and therefore the order `SELECT *` returns. A `USING` clause
    /// changes what the *projection* does with that row and not the row itself, so the merged
    /// column lives in the executor's scope rather than here.
    NestedLoop {
        /// Whether an outer row with no match survives, with every inner column NULL.
        ///
        /// **Applied after the `ON`, never after the `WHERE`.** That ordering is the whole of the
        /// difference the capture exists to pin: a condition in the `ON` decides whether a *pair*
        /// is kept, and an outer row that kept none is NULL-extended; a condition in the `WHERE`
        /// runs over the already-extended row and can remove it.
        left_join: bool,
        /// The left side, pulled once.
        outer: Box<Node>,
        /// The inner table, or a reserved id when the inner side is a catalog view.
        inner_table_id: u64,
        /// Set when the inner side is a `pg_catalog` relation, which has no key range to scan.
        /// Always paired with [`Probe::Materialize`]: a computed relation has no index to seek in.
        inner_view: Option<CatalogView>,
        /// Its name, for `EXPLAIN`.
        inner_table: String,
        /// How the inner table's rows decode.
        inner_columns: RowSchema,
        /// Set when the inner side is a **derived table**, whose rows come from this plan rather
        /// than from a key range. Always paired with [`Probe::Materialize`], for the same reason
        /// [`Node::NestedLoop::inner_view`] is: a relation with no key has nothing to seek in.
        inner_plan: Option<Box<Node>>,
        /// How one outer row produces inner rows.
        probe: Probe,
        /// What is left of `ON` after the probe, over the **combined** row. A probe answers an
        /// equality exactly, so this is `None` for a probe and the whole condition for a
        /// materialised inner side.
        residual: Option<Expr>,
    },
    /// `LIMIT` and `OFFSET`.
    Limit {
        /// Where the rows come from.
        input: Box<Node>,
        /// How many to skip.
        offset: usize,
        /// How many to return, or `None` for all of them.
        limit: Option<usize>,
    },
    /// `GROUP BY` and the aggregates over it: folds every group of input rows down to one row.
    ///
    /// The output row is **the grouping keys followed by the aggregate values**, in the order
    /// [`Node::Aggregate::keys`] and [`Node::Aggregate::aggregates`] give them, which is the row
    /// every expression above this node has been rewritten against.
    ///
    /// Like [`Node::Sort`] it cannot stream — the last input row can change the first output row —
    /// so it is bounded and answers `53400` past the bound rather than allocating without limit.
    Aggregate {
        /// Where the rows come from.
        input: Box<Node>,
        /// The grouping keys, resolved against the *input* row. Empty for an ungrouped aggregate,
        /// which is one group and not none.
        keys: Vec<Expr>,
        /// One per aggregate call, in the order the output row carries them.
        aggregates: Vec<AggregateSpec>,
        /// `HAVING`, resolved against the **output** row.
        having: Option<Expr>,
        /// Whether the statement wrote a `GROUP BY`.
        ///
        /// It decides the empty-input rule and nothing else, and the rule is two rules rather than
        /// one: over no rows an **ungrouped** aggregate is one row (`count` 0, everything else
        /// NULL) and a **grouped** one is no rows at all. Measured, both ways.
        grouped: bool,
    },
    /// An aggregate evaluated on columnar replicas, one fragment per region, finished here.
    ///
    /// **It stands exactly where a [`Node::Aggregate`] stood**, and produces exactly the row that
    /// one produces — the grouping keys followed by the aggregate values — which is what lets
    /// everything above it be untouched by the routing decision
    /// ([`crate::plan::routing`], ADR 0022 milestone 4). It carries the row plan it falls back to,
    /// so a refusal is answered by a field rather than by a branch somebody remembers.
    Columnar(Box<crate::plan::routing::Columnar>),
    /// A **derived table**: its input's rows, under the name and column names the `FROM` entry
    /// gave them.
    ///
    /// It computes nothing — [`crate::exec::cursor`] opens the input and hands its rows straight
    /// through — and it exists for `EXPLAIN`, which is not a small reason. The plan text threads
    /// **one** table name and **one** list of column names down the whole tree, so without a node
    /// to switch them at, a scan of `dt_a` inside `FROM (SELECT … FROM dt_a) AS t` prints
    /// `Seq Scan on t`: the wrong relation, named confidently. PostgreSQL calls this node
    /// `Subquery Scan on t` and so does this one.
    Derived {
        /// The sub-select's plan.
        input: Box<Node>,
        /// The name the outer query refers to it by, or empty for a derived table with no alias —
        /// which PostgreSQL 19 allows.
        alias: String,
        /// The relation the sub-select reads, and the column names its expressions were resolved
        /// against — the pair `EXPLAIN` threads down, replaced here for the subtree.
        input_table: String,
        /// The sub-select's own column names, for the same reason.
        input_columns: Vec<String>,
    },
    /// `SELECT DISTINCT`: the first row of each distinct value, in the order the input gave them.
    ///
    /// Distinctness is [`crate::value::PgDatum::pg_cmp`] equality, the same rule grouping uses, so
    /// `-0.0` and `0.0` are one value and so are two `NaN`s — which is what a real server does and
    /// what `Datum`'s own bitwise `PartialEq` deliberately does not.
    Distinct {
        /// Where the rows come from — always a [`Node::Project`], because `DISTINCT` is over the
        /// target list and not over the table.
        input: Box<Node>,
    },
}

/// One aggregate, resolved: what to compute, over which value, at which type.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateSpec {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// The argument, resolved against the input row, or `None` for `count(*)`.
    pub arg: Option<Expr>,
    /// `DISTINCT` inside the parentheses.
    pub distinct: bool,
    /// The argument's type — what decides the accumulator. `None` for `count(*)`.
    pub arg_type: Option<ColumnType>,
}

/// One resolved sort key.
#[derive(Debug, Clone)]
pub struct SortKey {
    /// What to sort on.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// Whether NULLs come first, already resolved from the default.
    pub nulls_first: bool,
}

impl Node {
    /// The `EXPLAIN` tree, one line per node, indented by depth.
    ///
    /// PostgreSQL's own output carries cost estimates; there is no cost model here, so there are no
    /// costs. What it does carry is the access path and the condition, which is the part a user
    /// changes their schema over — and it is written for a *user*, so `columns` is needed to turn
    /// the positions the executor works in back into the names they typed.
    #[must_use]
    pub fn explain(
        &self,
        table: &str,
        columns: &[String],
        engine: Option<&crate::plan::routing::Decision>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        self.explain_into(table, columns, engine, 0, &mut lines);
        lines
    }

    /// The names of the columns of the row **this node produces**, for a reader.
    ///
    /// Most nodes hand their input's row through unchanged, and two do not: a `Project` rebuilds
    /// the row as its target list, and an `Aggregate` replaces it entirely with the grouping keys
    /// followed by the aggregate values. Without this an `EXPLAIN` above an aggregate prints the
    /// *table's* first column for position 0 — a name that is not merely unhelpful but wrong, and
    /// wrong in the direction a reader would believe.
    fn row_names(&self, columns: &[String]) -> Vec<String> {
        match self {
            Node::Aggregate {
                input,
                keys,
                aggregates,
                ..
            } => {
                let inner = input.row_names(columns);
                keys.iter()
                    .map(|key| render(key, &inner))
                    .chain(aggregates.iter().map(|spec| render_aggregate(spec, &inner)))
                    .collect()
            }
            Node::Project { input, exprs } => {
                let inner = input.row_names(columns);
                exprs.iter().map(|expr| render(expr, &inner)).collect()
            }
            Node::Filter { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input } => input.row_names(columns),
            // The row space of the aggregate it replaced, which is the whole point of the
            // substitution: ask the plan it falls back to, because that *is* that aggregate.
            Node::Columnar(columnar) => columnar.fallback.row_names(columns),
            _ => columns.to_vec(),
        }
    }

    /// The names of the row this node *reads*, which is what every expression on it is written
    /// against — except an `Aggregate`'s `HAVING`, which is written against what it produces.
    fn input_names(&self, columns: &[String]) -> Vec<String> {
        match self {
            Node::Filter { input, .. }
            | Node::Project { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input }
            | Node::Aggregate { input, .. } => input.row_names(columns),
            _ => columns.to_vec(),
        }
    }

    fn explain_into(
        &self,
        table: &str,
        columns: &[String],
        engine: Option<&crate::plan::routing::Decision>,
        depth: usize,
        lines: &mut Vec<String>,
    ) {
        let indent = "  ".repeat(depth);
        // A derived table is where the two names this walk threads down have to **change**: below
        // it the relation is the sub-select's, not the outer query's, and so are the column names
        // every expression is rendered against. The engine is not handed down either — the
        // decision above is about a different scan, and a derived table is never routed.
        if let Node::Derived {
            input,
            alias,
            input_table,
            input_columns,
        } = self
        {
            lines.push(match alias.as_str() {
                "" => format!("{indent}Subquery Scan"),
                name => format!("{indent}Subquery Scan on {name}"),
            });
            input.explain_into(input_table, input_columns, None, depth + 1, lines);
            return;
        }
        let (line, child, extra) = self.describe(table, columns, engine, &indent);
        lines.push(format!("{indent}{line}"));
        if let Some(extra) = extra {
            // One `extra` may carry more than one line: a join prints its inner access path and
            // its residual condition, and both belong to the node rather than to a child of it.
            lines.extend(extra.split('\n').map(|line| format!("{indent}  {line}")));
        }
        if let Some(child) = child {
            // **A columnar node's subtree is not given the engine.** It prints its own engine line
            // and the subtree beneath it is the *fallback*; handing the decision down would print
            // it twice, once about the node and once about the scan it fell back to.
            let below = if matches!(self, Node::Columnar(_)) {
                None
            } else {
                engine
            };
            child.explain_into(table, columns, below, depth + 1, lines);
        }
    }

    /// One node's own line, its child, and the extra lines that belong to it — split out of
    /// [`Node::explain_into`] so that the walk and the per-node description are two readable
    /// things rather than one long one.
    fn describe(
        &self,
        table: &str,
        columns: &[String],
        engine: Option<&crate::plan::routing::Decision>,
        indent: &str,
    ) -> (String, Option<&Node>, Option<String>) {
        // The names to render *this* node's expressions against. `columns` stays the table's own
        // names all the way down, because every node works out its own row space from there --
        // narrowing it for the child instead would hand a `Project`'s one output column to the
        // three-column `Sort` beneath it.
        let names = &self.input_names(columns)[..];
        match self {
            Node::OneRow => ("Result".to_owned(), None, None),
            // No costs and no alternative: a computed relation has one access path. The name says
            // what it is rather than implying a choice that was not made -- the same rule the
            // aggregate and access-path plans already print by.
            Node::CatalogView { view, .. } => {
                (format!("Catalog Scan on {}", view.name()), None, None)
            }
            // Unreachable: `explain_into` prints this node itself, because it is the one place the
            // table and column names change on the way down. Kept total rather than `unreachable!`
            // so that a plan is never a panic.
            Node::Derived { input, .. } => ("Subquery Scan".to_owned(), Some(input), None),
            // **The engine goes on the scan**, which is the node the decision is about: ADR 0022
            // Decision 2 asks `EXPLAIN` to name the engine it chose, and a plan that was
            // *considered* for columns and left on rows has to say so as loudly as one that was
            // not considered at all — the two look identical without it, and "it was fast
            // yesterday" is what that costs.
            Node::SeqScan { narrowed, .. } => (
                format!("Seq Scan on {table}"),
                None,
                Self::scan_extras(*narrowed, engine),
            ),
            Node::PointGet { .. } => (format!("Point Get on {table}"), None, None),
            Node::IndexLookup { index_name, .. } => (
                format!("Index Lookup on {table}"),
                None,
                Some(format!("Index: {index_name}")),
            ),
            Node::Filter { input, predicate } => (
                "Filter".to_owned(),
                Some(input),
                Some(format!("Condition: {}", render(predicate, names))),
            ),
            Node::Project { input, exprs } => (
                format!("Project ({} columns)", exprs.len()),
                Some(input),
                None,
            ),
            Node::Sort { input, keys } => (
                "Sort".to_owned(),
                Some(input),
                Some(format!(
                    "Keys: {}",
                    keys.iter()
                        .map(|key| format!(
                            "{}{}",
                            render(&key.expr, names),
                            if key.descending { " DESC" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            ),
            // The one node with two sides. The outer subtree is printed as a child; the inner
            // side has no subtree of its own, so its access path is the `extra` line -- which is
            // exactly the thing a user reads an `EXPLAIN` of a join to find out.
            Node::NestedLoop {
                outer,
                inner_table,
                probe,
                residual,
                ..
            } => {
                let access = match probe {
                    Probe::Materialize => format!("Materialize on {inner_table}"),
                    Probe::PrimaryKey { .. } => format!("Point Get on {inner_table}"),
                    Probe::UniqueIndex { index_name, .. } => {
                        format!("Index Lookup on {inner_table} using {index_name}")
                    }
                };
                let extra = match residual {
                    Some(condition) => {
                        format!(
                            "Inner: {access}\n{indent}  Join Filter: {}",
                            render(condition, names)
                        )
                    }
                    None => format!("Inner: {access}"),
                };
                ("Nested Loop".to_owned(), Some(outer), Some(extra))
            }
            Node::Limit {
                input,
                offset,
                limit,
            } => (
                match limit {
                    Some(limit) => format!("Limit {limit} Offset {offset}"),
                    None => format!("Offset {offset}"),
                },
                Some(input),
                None,
            ),
            Node::Aggregate { input, .. } => {
                let (name, extra) = self.describe_aggregate(columns, names);
                (name, Some(input), Some(extra))
            }
            Node::Distinct { input } => ("Unique".to_owned(), Some(input), None),
            // Its own line and its own extras, in `crate::exec::explain`, which is the one place
            // that knows how to say what a routing decision was and what it cost.
            Node::Columnar(columnar) => {
                let (name, extra) = crate::exec::explain::describe(columnar, columns);
                (name, crate::exec::explain::child(columnar), Some(extra))
            }
        }
    }

    /// A sequential scan's extra lines: how the range was reached, and which engine will read it.
    ///
    /// The engine line is this milestone's and the range line is not, and they are together
    /// because both are facts about *this scan* rather than about the tree. Joined with a
    /// newline, which [`Node::explain_into`] splits and indents.
    fn scan_extras(
        narrowed: bool,
        engine: Option<&crate::plan::routing::Decision>,
    ) -> Option<String> {
        let mut extra: Vec<String> = Vec::new();
        if narrowed {
            extra.push("Range: narrowed by the primary key".to_owned());
        }
        if let Some(decision) = engine.filter(|decision| decision.reason.worth_printing()) {
            extra.push(format!(
                "Engine: {}  ({})",
                decision.engine.name(),
                decision.reason.describe()
            ));
        }
        (!extra.is_empty()).then(|| extra.join("\n"))
    }

    /// The aggregate's own two or three lines: the grouping keys, the calls, and the `HAVING`.
    ///
    /// PostgreSQL prints `HashAggregate` or `GroupAggregate` depending on the strategy it chose;
    /// there is one strategy here, so the name says what it is rather than implying a choice that
    /// was not made. An ungrouped aggregate prints as `Aggregate`, which is what a real server
    /// calls the same node.
    fn describe_aggregate(&self, columns: &[String], names: &[String]) -> (String, String) {
        let Node::Aggregate {
            keys,
            aggregates,
            having,
            ..
        } = self
        else {
            return ("Aggregate".to_owned(), String::new());
        };
        let mut extra = Vec::new();
        if !keys.is_empty() {
            extra.push(format!(
                "Group Key: {}",
                keys.iter()
                    .map(|key| render(key, names))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        extra.push(format!(
            "Aggregates: {}",
            aggregates
                .iter()
                .map(|spec| render_aggregate(spec, names))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(having) = having {
            // Against the *output* row -- the grouping keys and the aggregates -- which is the one
            // place in a plan where an expression is not written against what its node reads.
            extra.push(format!(
                "Filter: {}",
                render(having, &self.row_names(columns))
            ));
        }
        let name = if keys.is_empty() {
            "Aggregate"
        } else {
            "Group Aggregate"
        };
        (name.to_owned(), extra.join("\n"))
    }
}

/// An expression as `EXPLAIN` shows it.
///
/// Written for a person: a column is its **name**, not the position the executor works in, and a
/// literal is the value, not a `Debug` rendering of the node holding it. Not SQL that would
/// re-parse — PostgreSQL's own `EXPLAIN` output is not either — but every token in it is one the
/// user wrote or could have written.
fn render(expr: &Expr, columns: &[String]) -> String {
    match expr {
        Expr::Literal(literal) => render_literal(literal),
        Expr::Parameter(number) => format!("${number}"),
        Expr::Column { name, .. } => name.clone(),
        // Resolved to a position by the planner; put the name back for the reader. A position with
        // no name behind it can only be a bug, and saying so beats printing a number.
        Expr::Ordinal { at, .. } => columns
            .get(*at)
            .cloned()
            .unwrap_or_else(|| format!("<column {at}>")),
        // A column of a row this plan does not have, so `columns` cannot name it: the outer
        // query's own plan text does, one level up, and printing a name from the wrong row would
        // be worse than printing none.
        Expr::Outer { level, at, .. } => format!("<outer {level}.{at}>"),
        Expr::Binary { op, left, right } => format!(
            "({} {} {})",
            render(left, columns),
            op.symbol(),
            render(right, columns)
        ),
        Expr::Not(operand) => format!("NOT {}", render(operand, columns)),
        Expr::InList {
            operand,
            list,
            negated,
        } => format!(
            "{} {}IN ({})",
            render(operand, columns),
            if *negated { "NOT " } else { "" },
            list.iter()
                .map(|item| render(item, columns))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::IsNull { operand, negated } => format!(
            "{} IS {}NULL",
            render(operand, columns),
            if *negated { "NOT " } else { "" }
        ),
        // The sub-plan is **not** printed inside the condition. It is a tree, and a tree rendered
        // on one line is unreadable; what a reader needs here is that there is a subquery and
        // which kind, the way PostgreSQL prints `SubPlan 1` and puts the plan below.
        Expr::Subquery(sub) => match &sub.operand {
            Some(operand) => format!("{} {}", render(operand, columns), sub.kind.describe()),
            None => sub.kind.describe().to_owned(),
        },
        Expr::Default => "DEFAULT".to_owned(),
        Expr::Sequence(call) => match (&call.name, call.value) {
            (Some(name), Some(value)) => format!("{}('{name}', {value})", call.func.name()),
            (Some(name), None) => format!("{}('{name}')", call.func.name()),
            _ => format!("{}()", call.func.name()),
        },
        Expr::Aggregate(call) => format!(
            "{}({}{})",
            call.func.name(),
            if call.distinct { "DISTINCT " } else { "" },
            call.arg()
                .map_or_else(|| "*".to_owned(), |arg| render(arg, columns))
        ),
    }
}

/// One aggregate, as `EXPLAIN` shows it: the call the user wrote, with its argument put back into
/// the name they typed.
fn render_aggregate(spec: &AggregateSpec, columns: &[String]) -> String {
    format!(
        "{}({}{})",
        spec.func.name(),
        if spec.distinct { "DISTINCT " } else { "" },
        spec.arg
            .as_ref()
            .map_or_else(|| "*".to_owned(), |arg| render(arg, columns))
    )
}

/// A literal as it would have been written. A string is quoted, because `WHERE e = c` and
/// `WHERE e = 'c'` mean different things and a plan that cannot tell them apart is a plan that
/// cannot be checked against the query.
fn render_literal(literal: &Literal) -> String {
    match literal {
        Literal::Null => "NULL".to_owned(),
        Literal::Integer(value) => value.to_string(),
        Literal::Decimal(digits) => digits.clone(),
        Literal::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_owned(),
        Literal::String(text) => quoted(text),
        // Already resolved against a column's type, so it prints the way that type prints — the
        // same text a client would see the value as in a result.
        Literal::Typed(value) => match value.as_ref() {
            Datum::Null => "NULL".to_owned(),
            Datum::Text(text) => quoted(text),
            other => other.to_text().unwrap_or_else(|| "NULL".to_owned()),
        },
    }
}

/// SQL's own quoting: a single quote is doubled.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
