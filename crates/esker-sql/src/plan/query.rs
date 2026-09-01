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

use crate::plan::{AggregateFunc, Expr, Literal};
use crate::row::RowSchema;
use crate::value::PgDatum;
use crate::value::{ColumnType, Datum};

/// One `JOIN`, as written.
///
/// Exactly one, and inner only. A second join, an outer join and a `USING` clause are each refused
/// by name (contract C2) rather than approximated: an outer join that silently behaved like an
/// inner one would drop rows, which is the worst thing a join can do.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// The right-hand table.
    pub table: String,
    /// `ON`, or `None` for `CROSS JOIN` — every pair of rows.
    pub on: Option<Expr>,
}

/// `SELECT`, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// The table, or `None` for `SELECT 1` — a single row of no table at all, which drivers use to
    /// check a connection. With a [`Select::join`] it is the left-hand one.
    pub from: Option<String>,
    /// The one join this crate runs, if the statement has one.
    pub join: Option<Join>,
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
    /// `FROM a JOIN b` gives them and therefore the order `SELECT *` returns.
    NestedLoop {
        /// The left side, pulled once.
        outer: Box<Node>,
        /// The inner table.
        inner_table_id: u64,
        /// Its name, for `EXPLAIN`.
        inner_table: String,
        /// How the inner table's rows decode.
        inner_columns: RowSchema,
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
    pub fn explain(&self, table: &str, columns: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        self.explain_into(table, columns, 0, &mut lines);
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

    fn explain_into(&self, table: &str, columns: &[String], depth: usize, lines: &mut Vec<String>) {
        let indent = "  ".repeat(depth);
        let (line, child, extra) = self.describe(table, columns, &indent);
        lines.push(format!("{indent}{line}"));
        if let Some(extra) = extra {
            // One `extra` may carry more than one line: a join prints its inner access path and
            // its residual condition, and both belong to the node rather than to a child of it.
            lines.extend(extra.split('\n').map(|line| format!("{indent}  {line}")));
        }
        if let Some(child) = child {
            child.explain_into(table, columns, depth + 1, lines);
        }
    }

    /// One node's own line, its child, and the extra lines that belong to it — split out of
    /// [`Node::explain_into`] so that the walk and the per-node description are two readable
    /// things rather than one long one.
    fn describe(
        &self,
        table: &str,
        columns: &[String],
        indent: &str,
    ) -> (String, Option<&Node>, Option<String>) {
        // The names to render *this* node's expressions against. `columns` stays the table's own
        // names all the way down, because every node works out its own row space from there --
        // narrowing it for the child instead would hand a `Project`'s one output column to the
        // three-column `Sort` beneath it.
        let names = &self.input_names(columns)[..];
        match self {
            Node::OneRow => ("Result".to_owned(), None, None),
            Node::SeqScan { narrowed, .. } => (
                format!("Seq Scan on {table}"),
                None,
                narrowed.then(|| "Range: narrowed by the primary key".to_owned()),
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
        }
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
        Expr::Binary { op, left, right } => format!(
            "({} {} {})",
            render(left, columns),
            op.symbol(),
            render(right, columns)
        ),
        Expr::Not(operand) => format!("NOT {}", render(operand, columns)),
        Expr::IsNull { operand, negated } => format!(
            "{} IS {}NULL",
            render(operand, columns),
            if *negated { "NOT " } else { "" }
        ),
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
