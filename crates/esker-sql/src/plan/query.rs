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

use std::fmt::Write as _;

use crate::catalog::pg_catalog::CatalogView;
use crate::plan::{AggregateFunc, Expr, Literal};
use crate::row::RowSchema;
use crate::value::PgDatum;
use crate::value::PgType as _;
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
    /// `FROM (VALUES (1),(2)) AS t(x)` — a relation made of **constant rows**, or `None` for
    /// anything else.
    ///
    /// A fourth kind of `FROM` entry beside a relation, a derived table and a set-returning
    /// function. It is not a derived table even though it is written like one: a derived table's
    /// rows come from a sub-*select*, and no select with no `FROM` produces more than one row.
    pub values: Option<Box<ValuesList>>,
    /// `FROM generate_subscripts(a, 1) AS i` — a **set-returning function** standing where a
    /// relation would, or `None` for anything else.
    ///
    /// A third kind of `FROM` entry beside a relation and a derived table: it has no key range,
    /// no statistics and no rows on disk, and its shape is one column of the alias's name.
    pub function: Option<Box<TableFunction>>,
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
    /// The name **as the user wrote it**, when that is not the name the catalog is asked for.
    ///
    /// One case, and it is the only schema that has it: `public` is stored as no qualifier at all
    /// (`catalog::SCHEMA_SEPARATOR`), so `FROM public.nosuch` looks up `nosuch` and a `42P01` built
    /// from the stored name says `relation "nosuch" does not exist` where PostgreSQL says
    /// `relation "public.nosuch" does not exist` — the qualifier inside the quotes, whole, and not
    /// `"public"."nosuch"`. Measured.
    ///
    /// Like [`TableRef::hidden_cte`] it changes the message and nothing else, and only when the
    /// lookup fails: resolution never reads it, because a name that resolves is never quoted back.
    pub written: Option<String>,
}

impl TableRef {
    /// A table under its own name.
    #[must_use]
    pub fn bare(name: String) -> Self {
        TableRef {
            values: None,
            name,
            alias: None,
            derived: None,
            function: None,
            hidden_cte: false,
            written: None,
        }
    }

    /// The plan a derived table's rows come from, or `None` for a real relation.
    #[must_use]
    pub fn derived_plan(&self) -> Option<&Node> {
        self.derived
            .as_ref()
            .and_then(|derived| derived.plan.as_deref())
    }

    /// What makes two `FROM` entries the *same* entry, which is not what makes them referable.
    ///
    /// The alias where there is one, and otherwise the **qualified** name — so `s1.things` twice is
    /// `42712 table name "things" specified more than once` and `s1.things, s2.things` is allowed,
    /// measured both ways. Those two share an implicit alias and are still two relations a query
    /// can tell apart, by writing the qualifier or by aliasing them.
    ///
    /// [`Self::referred_as`] is the other half and deliberately not this: one decides identity,
    /// the other decides reference.
    #[must_use]
    pub fn identity(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    /// The name a qualifier in this query has to write: the alias if there is one, and the table's
    /// **unqualified** name otherwise.
    ///
    /// A `FROM` item's implicit alias is the relation name without its schema — measured, both
    /// halves: `SELECT things.name FROM test_schema.things` resolves, and so does the fully
    /// qualified `test_schema.things.name`. Returning the stored name here answered `42P01
    /// missing FROM-clause entry for table "things"` for the first, which is the shape
    /// `schema_test.rb` writes and the one `corpus/pg19_tsvector.txt` part 3 needs.
    ///
    /// The name is stored `schema ++ NUL ++ relation` outside `public`
    /// ([`crate::catalog::qualify`]), so the split is on a byte a relation name cannot contain
    /// rather than on a dot a quoted one could.
    #[must_use]
    pub fn referred_as(&self) -> &str {
        self.alias
            .as_deref()
            .unwrap_or_else(|| crate::catalog::split_qualified(&self.name).1)
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

/// How strongly `FOR UPDATE` / `FOR SHARE` asks a row to be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    /// `FOR UPDATE`.
    Update,
    /// `FOR SHARE`.
    Share,
}

impl LockStrength {
    /// The clause as PostgreSQL spells it in its own errors — **the one the user wrote**, which is
    /// what makes `SELECT DISTINCT … FOR SHARE` say `FOR SHARE` and not `FOR UPDATE`.
    #[must_use]
    pub fn clause(self) -> &'static str {
        match self {
            LockStrength::Update => "FOR UPDATE",
            LockStrength::Share => "FOR SHARE",
        }
    }
}

/// What a locking clause does when the row is already held by somebody else.
///
/// The three are one decision made three ways, and the difference between them is the whole
/// content of the modifiers: **wait** for the holder, **refuse** at once, or **leave the row out
/// of the answer**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWait {
    /// The bare clause: block until the holder's transaction ends
    /// ([ADR 0057](../../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)).
    Wait,
    /// `NOWAIT`: `55P03 could not obtain lock on row in relation "x"`, immediately.
    NoWait,
    /// `SKIP LOCKED`: the row is not in the answer, and nothing is said about it.
    SkipLocked,
}

/// One `FOR UPDATE` / `FOR SHARE` clause. A statement may carry more than one.
///
/// **The rows it names are locked as the statement returns them**
/// ([ADR 0057](../../../../docs/adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md)
/// §5): a `SELECT … FOR UPDATE` takes the same row lock a write takes, so a writer behind it waits
/// rather than racing it to the commit. Without that, `lock!`-then-`UPDATE` — the whole of
/// `ActiveRecord`'s pessimistic API — is two sessions proceeding in parallel and one of them
/// losing at commit with `40001`.
///
/// `FOR SHARE` is served as `FOR UPDATE`: stricter than the standard asks for, which costs
/// concurrency and never correctness, and declared as a divergence rather than approximated in
/// silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locking {
    /// `FOR UPDATE` or `FOR SHARE`.
    pub strength: LockStrength,
    /// `OF x` — the relation to lock, **as the query refers to it**, or `None` for all of them.
    pub of: Option<String>,
    /// `NOWAIT`, `SKIP LOCKED`, or neither.
    pub wait: LockWait,
}

/// One arm of a set operation after the first, and the operator that joins it on.
///
/// The operator belongs to the *arm* rather than to the set, because a chain may mix them:
/// `a UNION ALL b EXCEPT c` is `(a UNION ALL b) EXCEPT c`, left-associative, so each arm carries
/// how it joins the result of everything before it.
#[derive(Debug, Clone, PartialEq)]
pub struct SetArm {
    /// How this arm joins what precedes it.
    pub op: SetOp,
    /// Whether duplicates are kept. `UNION ALL` keeps them; `UNION` does not.
    pub all: bool,
    /// The arm itself, lowered as its own select.
    pub select: Select,
}

/// The three set operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`: the rows of both.
    Union,
    /// `INTERSECT`: the rows in both.
    Intersect,
    /// `EXCEPT`: the rows of the first that are not in the second.
    Except,
}

impl SetOp {
    /// The keyword, for a message that names what was not supported.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        }
    }
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
    /// `FOR UPDATE` / `FOR SHARE`, in the order written. Empty for a statement with none.
    ///
    /// **What the executor locks.** `crate::exec::query::finish_plan` turns each entry into the
    /// key columns of the relation it names — carried through the projection as junk columns the
    /// client never sees — and the executor takes a row lock per row before it answers. See
    /// [`Locking`].
    pub locking: Vec<Locking>,
    /// The **other arms of a set operation**, when this select is the first of one.
    ///
    /// Empty for an ordinary `SELECT`, which is every statement that is not a `UNION`. A set
    /// operation is this select with more arms after it rather than a wrapper around them, and
    /// that is not only economy: `ORDER BY`, `LIMIT` and `OFFSET` written after the last arm
    /// belong to the **whole set** on a real server, and they are already this struct's fields.
    /// So the first arm carries the set's clauses, exactly as the grammar does.
    ///
    /// The names a client is told come from the first arm and the types from unifying every arm —
    /// `SELECT i FROM t UNION ALL SELECT n FROM t` is a column called `i` of type `numeric`
    /// ([`tests/captures/pg19_set_operations.txt`]). Both halves of that are the planner's, in
    /// `exec::query`, because the types need a scope and lowering has none.
    ///
    /// [`tests/captures/pg19_set_operations.txt`]: ../../../tests/captures/pg19_set_operations.txt
    pub set_arms: Vec<SetArm>,
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

/// The constant rows of a `VALUES` list, and the names they are read under.
///
/// **Every row has the same number of expressions**, checked where this is built: PostgreSQL calls
/// a ragged list `42601 VALUES lists must all be the same length`, which is a syntax error and not
/// a type one, so it is raised at lowering rather than carried into a plan.
///
/// **The types are not here.** A column's type is its *first row's*, and reading an expression's
/// type needs a scope — so it is decided in `crate::exec::values`, once, and the rows are read as
/// it there too. Carrying a half-answer from lowering would mean two places deciding one thing.
#[derive(Debug, Clone, PartialEq)]
pub struct ValuesList {
    /// The rows, in the order written. `ORDER BY` may reorder them; nothing else does.
    pub rows: Vec<Vec<Expr>>,
    /// One name per column: `column1`, `column2`, … unless an alias list renames them.
    ///
    /// A name a statement can use — `VALUES (1),(2) ORDER BY column1 DESC` sorts by it — which is
    /// why they are carried rather than generated when a row description is built.
    pub columns: Vec<String>,
}

/// A set-returning function used as a `FROM` entry.
///
/// Only `generate_subscripts`, which is what `ActiveRecord`'s schema dump reads every foreign key
/// and unique constraint through — it turns `pg_constraint.conkey`, an array of attribute
/// numbers, into the column *names* in the order the constraint declares them.
#[derive(Debug, Clone, PartialEq)]
pub struct TableFunction {
    /// The function's name, folded — for the message when something else is asked for.
    pub name: String,
    /// Its arguments, lowered.
    pub args: Vec<Expr>,
    /// The one-column relation it stands for, filled in by the planner.
    pub def: Option<std::sync::Arc<crate::catalog::TableDef>>,
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
    /// Rows written into the statement: `FROM (VALUES (1),(2))`, and `VALUES …` on its own.
    ///
    /// Its own node beside [`Node::OneRow`] for the reason that one exists: a source of rows the
    /// planner can put anything above. Nothing is read to produce them, so there is no key range,
    /// no statistics and no order but the one they were written in.
    Values {
        /// The rows and the names they are read under.
        list: Box<ValuesList>,
        /// How a row is shaped, so everything above it reads one like any other.
        columns: RowSchema,
    },
    /// The one row a **sequence read as a relation** has: `SELECT last_value, is_called FROM s`.
    ///
    /// Computed when the cursor opens, like a catalog view and for the same reason — the values are
    /// a counter in the catalog rather than a key range, and there is exactly one row.
    SequenceRead {
        /// Which sequence's counter to read.
        sequence_id: u64,
        /// `(last_value, is_called)`, filled by the executor **outside the statement's
        /// transaction**.
        ///
        /// A sequence is not transactional: `nextval` and `setval` commit on their own, so a
        /// session that read one through its own snapshot would see the value as of `BEGIN` and
        /// report a sequence nobody has. Measured the hard way — a `setval(s, 3, true)` followed by
        /// a read in the same block answered the sequence's starting state.
        state: Option<(i64, bool)>,
        /// The three columns PostgreSQL shows: `last_value`, `log_cnt`, `is_called`.
        columns: RowSchema,
    },
    /// The rows a set-returning function in `FROM` yields.
    ///
    /// Computed when the cursor opens, like a catalog view and for the same reason: there is no
    /// key range to seek in and the row count is the length of one array.
    TableFunction {
        /// The call, whose arguments are evaluated at open.
        call: Box<TableFunction>,
        /// One column, of the alias's name.
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
        /// The children whose rows this scan also returns, read after the table's own.
        ///
        /// **Inheritance is a read rule**: `SELECT … FROM parent` returns a child's rows too, and
        /// so does `UPDATE` and `DELETE` on it. Empty for every table that is nobody's parent,
        /// which is all of them until `INHERITS` runs.
        inherited: Vec<crate::catalog::ChildScan>,
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
    /// **Every arm's rows, in the order the arms were written** — a `UNION ALL`.
    ///
    /// It streams: an arm is drained and then the next one is opened, so a set operation over two
    /// scans costs what the two scans cost and nothing is materialised. That is what `UNION ALL`
    /// is; the deduplicating forms need a [`Node::Distinct`] above this one, which is where the
    /// memory bound already lives.
    ///
    /// The arms' output columns are unified before the plan is built (`exec::query::append`), so
    /// every arm here produces a row of the same width and the same types.
    Append {
        /// The arms, in the order written. At least two, or the planner would not have built one.
        arms: Vec<Node>,
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
    /// It computes nothing — `crate::exec::cursor` opens the input and hands its rows straight
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
        /// Where the rows come from.
        ///
        /// A [`Node::Project`] for `SELECT DISTINCT`, because that clause is over the target list
        /// and not over the table — and a [`Node::Append`] for a `UNION`, whose deduplication is
        /// over the arms' rows and is the same operation on the same whole row. **`NULL` is equal
        /// to `NULL` for it**, which it is nowhere else in SQL: measured, two `NULL` arms of a
        /// `UNION` come back as one row.
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
    /// `ORDER BY` **inside the parentheses**: `array_agg(x ORDER BY y DESC)`.
    ///
    /// Not the query's `ORDER BY` and not `SortKey`'s usual home — this one sorts the values *of
    /// one aggregate within one group*, by expressions the aggregate does not return. It is
    /// resolved against the input row like the argument beside it, and it is empty for every call
    /// that does not write the clause, which is all of them but `array_agg`'s.
    pub order_by: Vec<SortKey>,
}

/// One resolved sort key.
#[derive(Debug, Clone, PartialEq)]
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

    /// Every node directly under this one, so a pass over the plan needs no match of its own.
    ///
    /// Written for the sequence read, whose value is taken **outside** the statement's transaction
    /// and so has to be found before the cursor opens. Total by construction: a node with children
    /// that forgot to list them here would silently hide them from every such pass.
    #[must_use]
    pub fn children_mut(&mut self) -> Vec<&mut Node> {
        match self {
            Node::Filter { input, .. }
            | Node::Project { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input }
            | Node::Aggregate { input, .. }
            | Node::Derived { input, .. } => vec![input],
            Node::NestedLoop {
                outer, inner_plan, ..
            } => match inner_plan {
                Some(inner) => vec![outer, inner],
                None => vec![outer],
            },
            Node::Columnar(columnar) => vec![&mut columnar.fallback],
            // **Every arm**, and this is the node the doc above is about: a pass that missed one
            // would hide a sequence read in the second arm of a `UNION ALL`.
            Node::Append { arms } => arms.iter_mut().collect(),
            Node::OneRow
            | Node::Values { .. }
            | Node::SequenceRead { .. }
            | Node::TableFunction { .. }
            | Node::CatalogView { .. }
            | Node::SeqScan { .. }
            | Node::PointGet { .. }
            | Node::IndexLookup { .. } => Vec::new(),
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

    /// The same plan as a tree of named fields, for the three structured `FORMAT`s.
    ///
    /// **It reads the same `describe` the text form does**, which is the point: two walks over one
    /// description can disagree about layout and can never disagree about the plan. See
    /// [`crate::plan::explain`] for what the fields are and what a real server has that they do
    /// not.
    #[must_use]
    pub fn plan_tree(
        &self,
        table: &str,
        columns: &[String],
        engine: Option<&crate::plan::routing::Decision>,
    ) -> crate::plan::PlanNode {
        // A derived table changes both names on the way down, exactly as in `explain_into`, and
        // for the same reason: below it the relation is the sub-select's.
        if let Node::Derived {
            input,
            alias,
            input_table,
            input_columns,
        } = self
        {
            let line = match alias.as_str() {
                "" => "Subquery Scan".to_owned(),
                name => format!("Subquery Scan on {name}"),
            };
            let mut node = crate::plan::PlanNode::new(&line, None);
            node.children
                .push(input.plan_tree(input_table, input_columns, None));
            return node;
        }
        let (line, child, extra) = self.describe(table, columns, engine, "");
        let mut node = crate::plan::PlanNode::new(&line, extra.as_deref());
        if let Some(child) = child {
            // A columnar node's subtree is the *fallback* and is not given the decision, which is
            // the rule `explain_into` prints by.
            let below = if matches!(self, Node::Columnar(_)) {
                None
            } else {
                engine
            };
            node.children.push(child.plan_tree(table, columns, below));
        }
        node
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
            // PostgreSQL's own name for it, `*VALUES*` included: the rows are a relation with no
            // relation behind them, and the quoted star is what it calls that relation.
            Node::Values { .. } => ("Values Scan on \"*VALUES*\"".to_owned(), None, None),
            // PostgreSQL's own name for it: a sequence is a relation and the scan is its one row.
            Node::SequenceRead { .. } => (format!("Seq Scan on {table}"), None, None),
            // Named for what it is: one access path, no costs, and a row count that is the length
            // of an array nobody has read yet.
            Node::TableFunction { call, .. } => {
                (format!("Function Scan on {}", call.name), None, None)
            }
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
            // **`Append`, with the first arm below it and the rest counted.** PostgreSQL prints
            // every arm indented under an `Append`; this renderer returns *one* child node and one
            // block of extra text, which is the shape a join needed and no more. Showing the
            // arms in full means giving it N children, which is a change to every node's arm and
            // is not this unit's — so the count is printed rather than a plan that looks like one
            // arm is the whole set.
            Node::Append { arms } => (
                "Append".to_owned(),
                arms.first(),
                (arms.len() > 1).then(|| format!("Arms: {}", arms.len())),
            ),
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
#[allow(
    clippy::too_many_lines,
    reason = "one arm per expression shape; splitting it would hide the vocabulary rather than clarify it"
)]
fn render(expr: &Expr, columns: &[String]) -> String {
    match expr {
        // Its own parentheses, as the comparison operators print theirs — `EXPLAIN` shows the
        // grouping the parser chose rather than the one the user typed.
        Expr::Negate(operand) => format!("(- {})", render(operand, columns)),
        Expr::Array { elements, .. } => format!(
            "ARRAY[{}]",
            elements
                .iter()
                .map(|element| render(element, columns))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Arithmetic {
            op, left, right, ..
        } => format!(
            "({} {} {})",
            render(left, columns),
            op.symbol(),
            render(right, columns)
        ),
        Expr::Literal(literal) => render_literal(literal),
        // As the user wrote it: `EXPLAIN` prints a cast the way SQL spells one.
        Expr::ToText { operand, .. } => format!("{}::text", render(operand, columns)),
        Expr::Cast { operand, to, .. } => {
            format!("{}::{}", render(operand, columns), to.name())
        }
        Expr::Scalar { func, operand } => format!("{}({})", func.name(), render(operand, columns)),
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            ..
        } => format!(
            "{} {}{} {}",
            render(operand, columns),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            render(pattern, columns)
        ),
        Expr::RegexMatch {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "{} {} {}",
            render(operand, columns),
            crate::plan::regex_operator(*negated, *case_insensitive),
            render(pattern, columns)
        ),
        Expr::Parameter(number) => format!("${number}"),
        Expr::CurrentSchema { all: None } => "current_schema()".to_owned(),
        Expr::CurrentDatabase | Expr::Advisory { .. } => "current_database()".to_owned(),
        Expr::CurrentUser => "CURRENT_USER".to_owned(),
        Expr::CurrentSchema {
            all: Some(implicit),
        } => format!("current_schemas({implicit})"),
        Expr::CurrentSetting { name, missing_ok } => {
            crate::plan::current_setting_text(name, *missing_ok)
        }
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
        // As the user wrote it: the name and its arguments.
        Expr::CatalogFunc(call) => format!(
            "{}({})",
            call.func.name(),
            call.args
                .iter()
                .map(|arg| render(arg, columns))
                .collect::<Vec<_>>()
                .join(", ")
        ),
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
        Expr::AnyArray { operand, array } => format!(
            "{} = ANY ({})",
            render(operand, columns),
            render(array, columns)
        ),
        Expr::Subscript { operand, index, .. } => {
            format!("{}[{}]", render(operand, columns), render(index, columns))
        }
        Expr::Uuid(func) => format!("{}()", func.name()),
        // On one line, the way `EXPLAIN` prints everything else — `pg_get_indexdef`'s five-line
        // layout is for a stored definition and is built where that is written
        // (`crate::exec::ddl`), not here.
        Expr::SetFunc(call) => format!(
            "{}({})",
            call.name,
            call.args
                .iter()
                .map(|arg| render(arg, columns))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Coalesce(args) => format!(
            "COALESCE({})",
            args.iter()
                .map(|arg| render(arg, columns))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Case {
            branches,
            otherwise,
        } => {
            let mut text = "CASE".to_owned();
            for branch in branches {
                let _ = write!(
                    text,
                    " WHEN {} THEN {}",
                    render(&branch.when, columns),
                    render(&branch.then, columns)
                );
            }
            if let Some(otherwise) = otherwise {
                let _ = write!(text, " ELSE {}", render(otherwise, columns));
            }
            text + " END"
        }
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
        Literal::TypedNull(ty) => format!("NULL::{}", crate::value::PgType::name(*ty)),
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
