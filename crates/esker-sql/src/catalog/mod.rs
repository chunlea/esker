//! What tables exist, and a cache that cannot serve a stale one.
//!
//! Definitions live in the `'m'` space (`record`) and are read through a transaction like any
//! other data, which is what makes DDL and DML consistent with each other: a `CREATE TABLE` that
//! has not committed is invisible, and one that has is visible to every transaction that starts
//! after it and to none that started before.
//!
//! # The cache, and the one thing it must never do
//!
//! Every node caches definitions, because resolving a name on every statement would put a network
//! round trip in front of every query. A cache that can serve a *stale* definition is not a
//! performance problem, it is a correctness one: writing a row against an old column list produces
//! bytes that decode wrong, and writing it against an old index list produces a row that is
//! missing from an index that exists.
//!
//! So there is a monotone `catalog_version` in the store, every DDL statement bumps it, and each
//! transaction reads it **once** — [`Catalog::view`]. Every lookup a transaction makes is then
//! answered at that one version, so a statement cannot see two different shapes of the same table
//! and cannot see a table that appeared halfway through it. The cache is keyed by that version and
//! is discarded, not repaired, when the version moves.
//!
//! A transaction whose snapshot is *older* than what the cache holds reads through to the store
//! instead: its snapshot is real, the cache is just newer than it, and serving from the cache
//! would show it a definition from its own future.
//!
//! # And the second thing it must never do: cache a definition nobody has committed
//!
//! A transaction that has run DDL reads its *own* catalog writes back, version bump included, so
//! its view sits at a version no committed transaction has reached yet. Caching what it reads
//! would publish an uncommitted definition to every session on the node — and version numbers are
//! reused after a rollback, because the bump is `read + 1` inside the transaction, so the next DDL
//! that really does commit lands on the same number and finds the abandoned entry waiting for it.
//! The failure is a `SELECT` against a table that was never created, or worse, a row written
//! against a column list that was rolled back.
//!
//! So a transaction that has written the catalog gets [`Catalog::view_uncached`]: it reads through
//! for every lookup, which is also what makes its own uncommitted DDL visible to itself, and it
//! puts nothing back. DDL is rare and this is the statement that just paid for a round trip
//! anyway.

pub mod def_functions;
pub mod information_schema;
pub mod pg_attribute;
pub mod pg_catalog;
pub mod pg_constraint;
pub mod pg_index;
pub mod pg_relations;
mod record;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::backend::Txn;
use crate::error::{Result, SqlError};
use crate::value::{self, ColumnType, Datum};

/// The version byte on every catalog record. An unknown one is an error, never a guess
/// (`CLAUDE.md` invariant 2).
pub const CATALOG_FORMAT_VERSION: u8 = record::CATALOG_FORMAT_VERSION;

/// How long old MVCC versions are kept when nothing says otherwise: **one hour**.
///
/// This number is two things at once and they pull in opposite directions
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md)). It is the depth of the *time machine* — a
/// read `AS OF` an instant older than this has nothing left to read — and it is the depth of every
/// version chain the storage engine has to walk past to answer an ordinary read. An hour is chosen
/// to be long enough that "what did this look like before the last run" is answerable out of the
/// box and short enough that a hot-rewritten key does not accumulate a chain nobody wanted.
pub const DEFAULT_RETENTION_MS: u64 = 60 * 60 * 1000;

/// A retention that never collects. Reserved rather than derived, because every other value is
/// subtracted from a timestamp and this one cannot be.
pub const RETENTION_FOREVER: u64 = u64::MAX;

/// The longest identifier PostgreSQL keeps. Longer ones are truncated with a `42622` notice, not
/// refused — measured against the server, which truncated a 70-character name to 63.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// One column of one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// As the user wrote it, already folded (see [`fold_identifier`]).
    pub name: String,
    /// One of the types this node stores ([ADR 0033](../../../docs/adr/0033-tier-1-of-the-type-surface.md)).
    pub ty: ColumnType,
    /// PostgreSQL's `pg_attribute.atttypmod`, **verbatim**, or `-1` for a type given no number.
    ///
    /// The length of a `varchar(n)` or `character(n)` and the precision of a `timestamp(p)` are
    /// properties of the *column*, not of the type — which is why there is one `varchar` row in
    /// `pg_type` and a number here, exactly as a real server arranges it.
    ///
    /// **Stored in PostgreSQL's own encoding rather than as a plain `n`**, measured against
    /// 19beta1: `varchar(5)` is `9` and `character(3)` is `7` — the length plus the four bytes of
    /// a varlena header — while `timestamp(3)` is `3`. That looks like a trap and is the opposite
    /// of one: the two things that read this field are `RowDescription`'s type-modifier column and
    /// `pg_attribute.atttypmod`, and **both are defined as this number**, so storing anything else
    /// would mean converting on the way out to two places and getting it wrong in one of them.
    /// Everything inside this crate asks [`ColumnDef::length`] or [`ColumnDef::precision`] instead
    /// and never does the arithmetic itself.
    pub typmod: i32,
    /// Whether a NULL is refused. Primary key columns are always `NOT NULL`.
    pub not_null: bool,
    /// A default that stays an **expression**, evaluated per row — the text `pg_get_expr` prints.
    ///
    /// PostgreSQL stores every default as a parse tree and folds exactly the ones its coercion
    /// machinery folds: a literal, with the cast to the column's type applied. Everything else —
    /// `now()`, `CURRENT_DATE`, `concat('a', 'b')`, `gen_random_uuid()` — stays a tree and is
    /// evaluated when a row is written. This field is that tree, stored the way a `CHECK` and a
    /// generation expression are, and [`ColumnDef::default`] is the folded case. **Exactly one of
    /// the two is set.**
    ///
    /// It replaced a closed set of three tags, and the widening is the point: there is no
    /// volatility test here and no constant test, because PostgreSQL has neither. What it forbids
    /// in a default is three things — a column reference, a subquery, a set-returning function —
    /// and those are refused where the column is lowered, by the messages a real server uses.
    ///
    /// **Per row, not per statement**: two rows of one `INSERT` get two UUIDs and two `random()`
    /// draws, which is what makes `id uuid DEFAULT gen_random_uuid() PRIMARY KEY` a workable key
    /// and what folding once would get wrong.
    ///
    /// **The spelling the user wrote survives**, because this is text: a column declared
    /// `DEFAULT CURRENT_TIMESTAMP` prints `CURRENT_TIMESTAMP` and one declared `DEFAULT now()`
    /// prints `now()`, which is what a real server does and what the tag could not express.
    pub default_expr: Option<String>,
    /// What an `INSERT` that omits this column writes. `None` is NULL.
    ///
    /// A **constant**, not an expression: `DEFAULT 7` and `DEFAULT 'x'` are stored, `DEFAULT
    /// random()` is refused by name (`docs/plans/phase-6e.md` §5 unit 1). PostgreSQL folds a
    /// constant expression like `(1+1)` to `2` before storing it; there is no folder here, so a
    /// parenthesised expression is refused rather than half-read.
    pub default: Option<Datum>,
    /// What a **row narrower than this column** reads as. `None` is NULL.
    ///
    /// PostgreSQL 11's `attmissingval`, and the reason `ALTER TABLE ... ADD COLUMN ... DEFAULT` is
    /// instant on a populated table: rather than rewriting every row to carry the value, the
    /// *catalog* remembers it and the decoder pads with it. ADR 0019's pad rule generalised — that
    /// rule pads with NULL, which is this field's `None`.
    ///
    /// **Separate from [`ColumnDef::default`], and the two really do diverge.** Measured on
    /// PostgreSQL 19beta1: after `ADD COLUMN c text DEFAULT 'old'`, a later
    /// `ALTER COLUMN c SET DEFAULT 'new'` leaves `attmissingval` at `old` — rows that predate the
    /// column still read `old` and new rows get `new`. Storing one field for both would rewrite
    /// history the first time somebody changed a default.
    pub missing: Option<Datum>,
    /// `GENERATED ALWAYS AS (expr) STORED`: the expression, as text.
    ///
    /// Text and re-lowered per write, the trade [`CheckDef`] and an index expression already make
    /// — and for the extra reason that `pg_get_expr` has to print it back anyway.
    ///
    /// **A generated column is not a defaulted one**, and the catalog says so twice: it is
    /// `pg_attrdef`'s expression *and* `information_schema.columns.column_default` is NULL for it,
    /// with `generation_expression` carrying the text instead. Measured. So this is its own field
    /// rather than a flag beside [`ColumnDef::default`] — the two are read by different columns of
    /// different views, and a writer may not supply a value for this one at all.
    pub generated: Option<String>,
    /// `COMMENT ON COLUMN t.c IS '…'`, or `None` for a column that has none.
    ///
    /// **`None` and the empty string are the same thing**, which is not a shortcut: PostgreSQL
    /// deletes the `pg_description` row for `IS ''` rather than storing an empty comment, so a
    /// comment that *is* the empty string cannot exist there and needs no representation here.
    ///
    /// It lives on the column rather than in a table of its own so that it moves with the column:
    /// a `RENAME COLUMN` keeps it and a `DROP COLUMN` takes it away, both without a line of code
    /// (ADR 0049).
    pub comment: Option<String>,
}

/// What kind of `UNIQUE` constraint an index belongs to, when it belongs to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniqueKind {
    /// `UNIQUE (c)` — checked at the statement, and no `DEFERRABLE` in its printed definition.
    Immediate,
    /// `UNIQUE (c) DEFERRABLE [INITIALLY IMMEDIATE]` — **also** checked at the statement.
    ///
    /// It is checked at the statement, and the flag reaches two readers that a plain `UNIQUE`
    /// does not give: `pg_constraint.condeferrable` and `pg_get_constraintdef`, which keeps the
    /// word `DEFERRABLE` and drops the `INITIALLY IMMEDIATE` half.
    ///
    /// **Its index still stores suffixed entries**, as [`UniqueKind::Deferred`]'s does, because
    /// `SET CONSTRAINTS <name> DEFERRED` may defer it for a transaction and the two colliding rows
    /// then have to coexist. The shape follows *deferrability*, which is fixed at declaration; the
    /// mode only decides when the scan runs (`crate::exec::deferred`).
    Deferrable,
    /// `UNIQUE (c) DEFERRABLE INITIALLY DEFERRED` — checked at `COMMIT`, not at the statement.
    ///
    /// The form that really waits, and the reason the mechanism exists: a transaction may break
    /// the constraint in the middle and repair it before the end, and a real server commits that.
    Deferred,
}

/// Where an index or a column is in a staged schema change (ADR 0020).
///
/// Four states, moved one at a time, and each exists because the pair on either side of it is safe
/// together and the pair you would get by skipping it is not. The variants carry that argument;
/// `tests/schema_change.rs` carries the repro for each, written so that it **fails** if the state
/// it guards is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaState {
    /// Nobody reads it, nobody writes it, nobody removes from it. What an index is before a
    /// `CREATE INDEX` starts and after a `DROP INDEX` finishes.
    Absent,
    /// A **delete** removes an entry if one is there; nothing reads and nothing inserts.
    ///
    /// Exists so that *every* node removes entries before *any* node creates them. Skipping it
    /// leaves an entry pointing at a row a node that did not know about the index deleted — and
    /// the moment the index goes public, a scan through it returns a row the table does not
    /// contain (ADR 0020, "skip delete-only").
    DeleteOnly,
    /// Inserts, updates and deletes all maintain it; nothing reads it.
    ///
    /// Exists so that *every* node maintains the index before *any* node trusts it. Skipping it
    /// lets a node behind insert a row and write no entry, while a node ahead answers a query
    /// from the index and does not find it (ADR 0020, "skip write-only").
    WriteOnly,
    /// Read, written and removed from: an ordinary index.
    ///
    /// Reached only after the **backfill**, which is what makes it complete. Skipping that leaves
    /// every row written before write-only invisible to an index scan — the same wrong answer with
    /// a wider blast radius (ADR 0020, "skip the backfill").
    Public,
}

impl SchemaState {
    /// Whether a query may answer *from* this index.
    #[must_use]
    pub fn readable(self) -> bool {
        self == SchemaState::Public
    }

    /// Whether an insert or an update writes an entry into it.
    #[must_use]
    pub fn written(self) -> bool {
        matches!(self, SchemaState::WriteOnly | SchemaState::Public)
    }

    /// Whether a delete removes an entry from it.
    ///
    /// True one state *earlier* than [`SchemaState::written`], and that asymmetry is the whole
    /// design: removal has to lead creation, or an entry outlives the row it names.
    #[must_use]
    pub fn maintained(self) -> bool {
        matches!(
            self,
            SchemaState::DeleteOnly | SchemaState::WriteOnly | SchemaState::Public
        )
    }

    /// The next state towards `Absent`, or `None` at it — the direction a **removal** runs, and
    /// the direction a failed change unwinds in.
    #[must_use]
    pub fn backward(self) -> Option<SchemaState> {
        match self {
            SchemaState::Public => Some(SchemaState::WriteOnly),
            SchemaState::WriteOnly => Some(SchemaState::DeleteOnly),
            SchemaState::DeleteOnly => Some(SchemaState::Absent),
            SchemaState::Absent => None,
        }
    }

    /// The next state towards `Public`, or `None` at it.
    #[must_use]
    pub fn forward(self) -> Option<SchemaState> {
        match self {
            SchemaState::Absent => Some(SchemaState::DeleteOnly),
            SchemaState::DeleteOnly => Some(SchemaState::WriteOnly),
            SchemaState::WriteOnly => Some(SchemaState::Public),
            SchemaState::Public => None,
        }
    }

    /// How it is stored, and how it reads in `esker_schema_jobs()`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SchemaState::Absent => "absent",
            SchemaState::DeleteOnly => "delete-only",
            SchemaState::WriteOnly => "write-only",
            SchemaState::Public => "public",
        }
    }
}

/// One part of an index's key: a column of the table, or an expression over the row.
///
/// PostgreSQL keeps the same distinction and a client reads it: `pg_index.indkey` holds an
/// attribute number for a column part and **`0`** for an expression one, and `ActiveRecord`'s
/// `indexes()` branches on exactly that (`indkey.include?(0)`) to decide whether to believe the
/// column list or re-read the definition text. So the two are different shapes here rather than a
/// column position with a sentinel — a sentinel is what `0` is on the wire, and it is only safe
/// there because attribute numbers are one-based and positions here are not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPart {
    /// A position into [`TableDef::columns`]. Renaming the column cannot orphan the index.
    Column(usize),
    /// An expression over the row — `CREATE UNIQUE INDEX … ON t ((lower(b)))`.
    ///
    /// Stored as text and lowered per row, the same trade [`CheckDef`] and
    /// [`IndexDef::predicate`] make, and for the same two reasons: `pg_get_indexdef` needs the
    /// text anyway, and one string is an encoding the table record already writes.
    Expression {
        /// The expression's own text, with no parentheses of its own: `lower(b)`, `b IS NULL`,
        /// `1`. The parentheses PostgreSQL prints are a function of the [`ExprShape`] beside it,
        /// and there are three different ones — which is why they are added where they are read
        /// rather than baked in here.
        expr: String,
        /// Which of PostgreSQL's three deparse shapes this expression is.
        shape: ExprShape,
        /// The type the expression evaluates to, resolved once when the index was created.
        ///
        /// Stored rather than re-derived because the index **relation** has a `pg_attribute` row
        /// per key part and that row has to declare a type: `pg_attribute` is in this module and
        /// re-lowering the text to ask would put the executor's resolver under the catalog. It is
        /// also the honest place for it — the type an index key has is the type it had when the
        /// index was built, and an expression that would resolve differently today is a rebuild,
        /// not a re-read.
        ty: ColumnType,
    },
}

/// How PostgreSQL prints one index expression, in the three places it prints one.
///
/// Not a property of the text and not recoverable from it — `'x)'::text` and `f(x)` end the same
/// way — so it is decided where the expression is lowered and stored beside it. Measured on
/// PostgreSQL 19 (`tests/corpus/pg19_expression_index.txt`):
///
/// | written | `pg_get_expr(indexprs)` | `pg_get_indexdef(i, n, t)` | in the key list |
/// |---|---|---|---|
/// | `lower(b)` | `lower(b)` | `lower(b)` | `lower(b)` |
/// | `b IS NULL` | `(b IS NULL)` | `(b IS NULL)` | `((b IS NULL))` |
/// | `1` | `1` | `(1)` | `(1)` |
/// | `CASE …` | `CASE …` | `(CASE …)` | `(CASE …)` |
///
/// Three columns and no two rows alike, which is the whole reason this is an enum and not a flag.
///
/// The fourth row is a `CASE`, and it is [`ExprShape::Value`] rather than a variant of its own:
/// measured, its parentheses are a value's in all three columns. What is unlike a value about it
/// is its **text** — five lines with the implicit `ELSE` materialised — and that is decided where
/// the expression is deparsed (`crate::exec::ddl`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprShape {
    /// A bare function call. Printed and listed unparenthesised, the only shape that is.
    Call,
    /// A value with no operator at its top — a constant, a cast. Printed bare and listed in one
    /// pair, and the **one** shape whose per-column form is not its printed form.
    Value,
    /// An operator or a connective. Printed in one pair already, and listed in two.
    Operator,
}

impl ExprShape {
    /// What `pg_get_expr(indexprs, indrelid)` answers, and what a `23505` `DETAIL` names.
    #[must_use]
    pub fn printed(self, expr: &str) -> String {
        match self {
            ExprShape::Call | ExprShape::Value => expr.to_owned(),
            ExprShape::Operator => format!("({expr})"),
        }
    }

    /// What the key list inside `pg_get_indexdef`'s `USING btree (…)` holds.
    ///
    /// One pair more than [`ExprShape::printed`] for everything that is not a call — so an
    /// operator, already printed in one pair, is listed in **two**: `((b IS NULL))`.
    #[must_use]
    pub fn listed(self, expr: &str) -> String {
        match self {
            ExprShape::Call => expr.to_owned(),
            ExprShape::Value | ExprShape::Operator => format!("({})", self.printed(expr)),
        }
    }

    /// What `pg_get_indexdef(oid, n, pretty)` answers for this key part alone.
    ///
    /// [`ExprShape::listed`] for a value and [`ExprShape::printed`] for the other two, which is
    /// the row of the table above where the last two columns disagree.
    #[must_use]
    pub fn per_column(self, expr: &str) -> String {
        match self {
            ExprShape::Call | ExprShape::Operator => self.printed(expr),
            ExprShape::Value => format!("({expr})"),
        }
    }
}

/// One part of an index's key, in the order it is stored in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexKey {
    /// What the key part is over.
    pub part: KeyPart,
    /// Which way it is stored, and where its NULLs go.
    pub order: KeyOrder,
}

impl IndexKey {
    /// An ascending column part, which is what a primary key and a `UNIQUE` constraint are made
    /// of: neither takes a direction, because `UNIQUE (a DESC)` is a **syntax error** on a real
    /// server — measured — and a key nothing can order differently has one spelling.
    #[must_use]
    pub fn column(at: usize) -> Self {
        IndexKey {
            part: KeyPart::Column(at),
            order: KeyOrder::ASCENDING,
        }
    }

    /// The column this part is over, or `None` for an expression.
    #[must_use]
    pub fn position(&self) -> Option<usize> {
        match self.part {
            KeyPart::Column(at) => Some(at),
            KeyPart::Expression { .. } => None,
        }
    }

    /// The name the index **relation**'s own column carries: the table's column name for a column
    /// part, and for an expression the function it calls, or `expr`.
    ///
    /// PostgreSQL's `ChooseIndexColumnNames`, measured: `CREATE INDEX xa_e ON xa (a, (lower(b)))`
    /// has `pg_attribute` rows `a` and **`lower`** on the index. It is the same rule that names a
    /// derived index (`crate::plan::index_name`), which is not a coincidence — a derived name is
    /// this joined with `_idx`.
    #[must_use]
    pub fn attname<'a>(&'a self, table: &'a TableDef) -> &'a str {
        match &self.part {
            KeyPart::Column(at) => table
                .columns
                .get(*at)
                .map_or(INTERNAL_ROW_ID_NAME, |column| column.name.as_str()),
            KeyPart::Expression {
                expr,
                shape: ExprShape::Call,
                ..
            } => expr.split_once('(').map_or(expr.as_str(), |(name, _)| name),
            KeyPart::Expression { .. } => "expr",
        }
    }
}

/// Which way one key part is stored, and where its NULLs go.
///
/// **Recorded and printed, and it changes nothing else.** An index here is read in exactly two
/// ways — a `UNIQUE` check and a lookup with the whole key pinned to constants — and neither
/// depends on the order the entries are in. Nothing chooses an index to satisfy an `ORDER BY`, so
/// there is no plan for a direction to be wrong in; when something does, this is the field it
/// will read. Storing it is what makes `pg_get_indexdef` reproduce the statement, which is how
/// `ActiveRecord` gets `order: :desc` back out of a schema dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOrder {
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST`. **The default depends on the direction** — ascending sorts NULLs last and
    /// descending sorts them first — which is the whole reason both are stored rather than one.
    pub nulls_first: bool,
}

impl KeyOrder {
    /// The order a key part written with no direction and no null placement has.
    pub const ASCENDING: KeyOrder = KeyOrder {
        descending: false,
        nulls_first: false,
    };

    /// The order for a direction, with PostgreSQL's default null placement for it.
    #[must_use]
    pub fn of(descending: bool) -> Self {
        KeyOrder {
            descending,
            nulls_first: descending,
        }
    }

    /// What `pg_get_indexdef` prints after a key part — **only what differs from the default**,
    /// and the default depends on the direction.
    ///
    /// Measured on PostgreSQL 19, and it is why the text cannot be round-tripped from what was
    /// written (`tests/corpus/pg19_desc_index.txt`):
    ///
    /// | written | printed |
    /// |---|---|
    /// | `a` / `a ASC` / `a ASC NULLS LAST` | `a` |
    /// | `a ASC NULLS FIRST` | `a NULLS FIRST` |
    /// | `a DESC` / `a DESC NULLS FIRST` | `a DESC` |
    /// | `a DESC NULLS LAST` | `a DESC NULLS LAST` |
    ///
    /// `ASC` never prints, because it is the default; `NULLS FIRST` prints under `ASC` and not
    /// under `DESC`, and `NULLS LAST` the other way round.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match (self.descending, self.nulls_first) {
            (false, false) => "",
            (false, true) => " NULLS FIRST",
            (true, true) => " DESC",
            (true, false) => " DESC NULLS LAST",
        }
    }

    /// `pg_index.indoption`'s bitmask for this part: `1` for `DESC`, `2` for `NULLS FIRST`.
    ///
    /// PostgreSQL's own `INDOPTION_DESC` and `INDOPTION_NULLS_FIRST`, measured across all four
    /// combinations — an ascending part is `0` and a plain `DESC` one is `3`, because descending
    /// carries its own default null placement in the same word.
    #[must_use]
    pub fn indoption(self) -> u16 {
        u16::from(self.descending) | (u16::from(self.nulls_first) << 1)
    }
}

/// One index on one table. Its column parts are positions into the table's column list, so
/// renaming a column cannot orphan an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// From the tenant's relation-id sequence, like a table's.
    pub id: u64,
    /// Unique across the tenant, in the same namespace as table names.
    pub name: String,
    /// Whether a duplicate is refused with `23505`.
    pub unique: bool,
    /// The key, in key order: a column of the table or an expression over the row.
    pub keys: Vec<IndexKey>,
    /// Where this index is in a staged schema change (ADR 0020).
    ///
    /// [`SchemaState::Public`] for an index that was built the old way — one statement, one
    /// transaction — and for every index a version 2 catalog holds, which is what a table that has
    /// never staged a change means.
    pub state: SchemaState,
    /// The [`TableDef::schema_version`] this index entered [`IndexDef::state`] at.
    ///
    /// The number ADR 0020's two-version invariant is stated over: the step clock may advance only
    /// when no node can still be acting on a state two behind this one, and this is what "behind"
    /// is measured in. It is written on every transition and read by the job, never by a read or a
    /// write of a row.
    pub state_since: u64,
    /// `INCLUDE (…)` — the **non-key payload** columns, by position, in the order written.
    ///
    /// **They are in `indkey` and only a count separates them from the key**: the suite's index
    /// has `indnatts = 4`, `indnkeyatts = 2` and `indkey = "2 3 4 5"`, one vector holding both
    /// halves. A client that reconstructs an index from `indkey` alone reports a four-column
    /// index, and `ActiveRecord`'s schema dumper is such a client.
    ///
    /// **Uniqueness is over [`IndexDef::keys`] only.** The payload is recorded and never compared,
    /// which is what makes `("firm_id") INCLUDE ("name")` refuse a second row with the same
    /// `firm_id` and a different `name`. An included column may also **repeat** a key column —
    /// `("firm_id") INCLUDE ("firm_id")` is accepted, `indkey = "2 2"`, not deduplicated.
    pub include: Vec<usize>,
    /// `WHERE …` — a **partial** index, whose entries exist only for rows the predicate admits.
    ///
    /// Stored as text and lowered per row, the same trade [`CheckDef`] makes and for the same two
    /// reasons: `pg_get_indexdef` needs the text anyway, and one string is an encoding the table
    /// record already writes.
    ///
    /// A partial index is **maintained and never read**. Choosing one for a scan is only correct
    /// when the query's own predicate implies the index's, and this crate has no implication
    /// prover — a planner that picked it anyway would answer a correct-looking query with the
    /// rows the index happens to hold, which is ADR 0020's "skip the backfill" anomaly arriving
    /// by a different road. So it enforces its `UNIQUE` and never narrows a read.
    pub predicate: Option<String>,
    /// `NULLS NOT DISTINCT`, which makes **two NULLs collide** in a unique index.
    ///
    /// PostgreSQL admits any number of NULLs in a `UNIQUE` column by default — two unknowns are
    /// not known to be equal — and this is the clause that says to treat them as one value
    /// instead. It is the only thing in an index definition that changes which rows are *refused*
    /// rather than how they are stored or printed, which is why it reaches
    /// `crate::exec::index::entry` and the direction and the predicate do not.
    ///
    /// Stored and printed on a **non-unique** index too, where it can refuse nothing: a real
    /// server accepts `CREATE INDEX … NULLS NOT DISTINCT` and prints it back. Measured.
    pub nulls_not_distinct: bool,
    /// Whether a `UNIQUE` **constraint** made this index, and whether that constraint is
    /// `DEFERRABLE` — `None` for an index that is not a constraint at all.
    ///
    /// **Three states, not two.** `CREATE UNIQUE INDEX` and `UNIQUE (c)` build the same index and
    /// PostgreSQL tells them apart: only the second has a `pg_constraint` row, so a node storing
    /// one bit would either invent a constraint for every unique index or report none for any.
    /// The third state carries `DEFERRABLE`, which is not deferred — it checks at the statement
    /// like any other, and only `condeferrable` and `pg_get_constraintdef` can see it
    /// (`crate::plan::UniqueConstraint::deferrable`).
    pub constraint: Option<UniqueKind>,
    /// `COMMENT ON INDEX i IS '…'`, or `None`. `ActiveRecord`'s schema dump reads it for every
    /// index of every table — boot statement 32.
    ///
    /// On the index for the reason a column's is on the column: it dies with the index, and an
    /// index dropped and recreated under the same name has none, which is what a real server
    /// answers because it is a different index.
    pub comment: Option<String>,
}

impl IndexDef {
    /// Whether this index's constraint may be deferred — in either initial mode.
    ///
    /// The question the *writer* asks, because it decides the entry's shape
    /// (`crate::exec::index`), and it is about the declaration rather than about the transaction:
    /// a `DEFERRABLE INITIALLY IMMEDIATE` constraint can be deferred by `SET CONSTRAINTS` and so
    /// needs the same room a deferred one does.
    #[must_use]
    pub fn deferrable(&self) -> bool {
        matches!(
            self.constraint,
            Some(UniqueKind::Deferrable | UniqueKind::Deferred)
        )
    }

    /// Whether it **starts** deferred, which is what a transaction that has said nothing gets.
    #[must_use]
    pub fn initially_deferred(&self) -> bool {
        self.constraint == Some(UniqueKind::Deferred)
    }

    /// The key's column positions, or `None` for an index with an expression in its key.
    ///
    /// `None` is what stops a read from choosing one. The planner narrows a scan by pinning every
    /// key column to a constant from the `WHERE`, and there is no constant to pin an expression
    /// to: proving `WHERE lower(b) = 'x'` reaches the same entries as an index on `lower(b)`
    /// needs the equivalence this crate has no prover for, and picking it without one would
    /// answer a correct-looking query with the rows the index happens to hold — the same anomaly
    /// [`IndexDef::predicate`] is kept out of a read for.
    #[must_use]
    pub fn key_columns(&self) -> Option<Vec<usize>> {
        self.keys.iter().map(IndexKey::position).collect()
    }
}

/// The name the internal row id column carries: **no name at all**.
///
/// It cannot collide with anything a user writes, and that is a fact about PostgreSQL rather than
/// a convention of ours: a zero-length delimited identifier is `42601 zero-length delimited
/// identifier`, measured against the server, so `""` is a name no statement can ever mention. That
/// makes the column unnameable by construction instead of by a reserved word somebody could
/// legitimately want — no `_rowid` that a user is then forbidden to call their own column.
pub const INTERNAL_ROW_ID_NAME: &str = "";

/// How many row ids a session reserves at a time (see [`allocate_row_ids`]).
pub const ROW_ID_BATCH: u64 = 256;

/// How many sequence values a session reserves at a time (see [`allocate_sequence_values`]).
///
/// PostgreSQL's own default is `CACHE 1`, so its gaps come only from rolled-back statements and
/// ours come from those *and* from a session ending mid-batch. That is a declared divergence and
/// not a different kind of thing: `CACHE n` is a sequence option PostgreSQL has, with exactly this
/// behaviour, and neither server offers gap-freeness. The reason to take it is the one
/// [`allocate_row_ids`] gives — the counter is one key, and one durable bump per row would make
/// every concurrent insert into a table contend on it.
pub const SEQUENCE_BATCH: u64 = 32;

/// The relation id every **derived table** wears: `FROM (SELECT …) AS t`.
///
/// A tenant's relation ids come from a sequence that starts at 1, so nothing a user creates
/// reaches here, and it sits just below `pg_catalog`'s reserved block for the same reason that one
/// exists: a [`TableDef`] has an id, and a relation nothing stores still needs one to be shaped
/// like a table. **No key is ever built from it** — `crate::exec::query::plan` puts the
/// sub-select's own plan where an access path would go, and `crate::exec::cursor` reads a derived
/// join's inner side from that plan rather than from a range.
///
/// One id for all of them rather than one each: nothing compares two relation ids in a plan, and a
/// counter would be a number in the output of `EXPLAIN` that changed with the statement around it.
pub const DERIVED_TABLE_ID: u64 = u64::MAX - 1024;

/// Where a **sequence read as a relation** carries its sequence id.
///
/// `SELECT last_value, is_called FROM s` is a three-column relation on a real server, and the
/// relation is the sequence itself — there is no table behind it. So the synthetic `TableDef` the
/// planner is given carries the sequence's id *in* its own, above every id a tenant can allocate,
/// and `crate::exec::query` reads it back rather than seeking a key range that does not exist. The
/// same trick `PRIMARY_KEY_OID_BASE` uses one catalog over.
pub const SEQUENCE_RELATION_ID_BASE: u64 = u64::MAX - 1_048_576;

/// The three columns PostgreSQL shows for a sequence, in its own order.
///
/// `log_cnt` is how many values are left in the WAL-logged batch on a real server — an
/// implementation detail of *its* crash safety, which this node reaches differently — and it
/// reports **0**, which is what a freshly written sequence shows there too.
#[must_use]
pub fn sequence_relation_def(name: &str, sequence_id: u64) -> Arc<TableDef> {
    let column = |name: &str, ty: ColumnType| ColumnDef {
        name: name.to_owned(),
        ty,
        typmod: value::NO_TYPMOD,
        not_null: false,
        default_expr: None,
        default: None,
        missing: None,
        generated: None,
        comment: None,
    };
    Arc::new(TableDef {
        id: SEQUENCE_RELATION_ID_BASE.wrapping_add(sequence_id),
        name: name.to_owned(),
        columns: vec![
            column("last_value", ColumnType::Int8),
            column("log_cnt", ColumnType::Int8),
            column("is_called", ColumnType::Bool),
        ],
        persistence: Persistence::Permanent,
        primary_key: Vec::new(),
        indexes: Vec::new(),
        primary_key_name: String::new(),
        schema_version: 1,
        sequences: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        triggers_disabled: false,
        parents: Vec::new(),
        children: Vec::new(),
        triggers: Vec::new(),
        child_scans: Vec::new(),
        excludes: Vec::new(),
        partition_by: None,
        partition_bound: None,
        comment: None,
        primary_key_comment: None,
    })
}

/// The sequence a synthetic relation id names, or `None` for an ordinary table.
#[must_use]
pub fn sequence_of_relation(id: u64) -> Option<u64> {
    id.checked_sub(SEQUENCE_RELATION_ID_BASE)
}

/// A user-defined type: what `CREATE TYPE` made, by name.
///
/// **Its oid comes from the tenant's relation-id sequence**, the same counter tables and indexes
/// draw from, so a type and a relation can never share one. That is PostgreSQL's arrangement too —
/// `pg_class` and `pg_type` are two catalogs over one oid space — and it is what lets
/// `'floatrange'::regtype` and `'people'::regclass` be numbers a client can compare.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeDef {
    /// As the user wrote it, folded like every other identifier.
    pub name: String,
    /// From the tenant's relation-id sequence.
    pub oid: u64,
    /// Which of the three `CREATE TYPE` shapes it is.
    pub kind: TypeKind,
}

/// One field of a composite type.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeField {
    /// The field's name.
    pub name: String,
    /// Its type.
    pub ty: ColumnType,
    /// Its typmod, so `format_type` prints `character varying(90)` and not `character varying`.
    pub typmod: i32,
}

/// The three shapes `CREATE TYPE` takes, and the three `typtype` codes they answer.
///
/// **`typtype` and `typcategory` are different one-letter codes and both matter**: a range is
/// `r`/`R`, a composite `c`/`C` and an enum `e`/`E`. Measured; an implementation that answered one
/// of them for both would pass a `typtype` probe and fail a `typcategory` one.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeKind {
    /// `CREATE TYPE r AS RANGE (subtype = …)`.
    Range {
        /// The element type the range is over, which `pg_range.rngsubtype` reports.
        subtype: ColumnType,
        /// The `subtype_diff` function's name, verbatim — `pg_range.rngsubdiff` prints it back and
        /// nothing here calls it. `None` when the clause was not written.
        subtype_diff: Option<String>,
    },
    /// `CREATE TYPE c AS (field type, …)`.
    Composite {
        /// The fields, in declaration order, which is the order `pg_attribute` reports them in.
        fields: Vec<TypeField>,
    },
    /// `CREATE TYPE e AS ENUM ('a', 'b')` — and **an empty label list is legal**, measured.
    Enum {
        /// The labels, in declaration order. That order **is** the sort order: `'past' < 'future'`
        /// is true for `('past','present','future')` because of where they were declared, not
        /// because of the alphabet.
        labels: Vec<String>,
    },
}

impl TypeKind {
    /// `pg_type.typtype`: `r`, `c` or `e`.
    #[must_use]
    pub fn typtype(&self) -> &'static str {
        match self {
            TypeKind::Range { .. } => "r",
            TypeKind::Composite { .. } => "c",
            TypeKind::Enum { .. } => "e",
        }
    }

    /// `pg_type.typcategory`: `R`, `C` or `E`. **Not the upper case of `typtype` by accident** —
    /// they are two different columns of one-letter codes, and only these three pairs happen to
    /// look alike.
    #[must_use]
    pub fn typcategory(&self) -> &'static str {
        match self {
            TypeKind::Range { .. } => "R",
            TypeKind::Composite { .. } => "C",
            TypeKind::Enum { .. } => "E",
        }
    }
}

/// A table, its columns, its primary key and its indexes — everything needed to write a row.
#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    /// From the tenant's relation-id sequence. Part of every key of every row.
    pub id: u64,
    /// Whether the table was created `UNLOGGED`.
    ///
    /// **Stored on the table and nowhere else.** A real server marks the sequence a `bigserial`
    /// owns and every index — the primary key's included — with the table's persistence, and
    /// `ALTER TABLE … SET LOGGED` moves all of them together. Deriving each relation's answer from
    /// its table (`crate::catalog::pg_catalog`) is what makes both of those true at once, and it
    /// is the only arrangement in which they cannot disagree.
    pub persistence: Persistence,
    /// Unique across the tenant.
    pub name: String,
    /// In declaration order, which is the order a row's values are encoded in.
    pub columns: Vec<ColumnDef>,
    /// Positions into [`TableDef::columns`], in key order.
    pub primary_key: Vec<usize>,
    /// Every index, including the unique ones a `UNIQUE` constraint creates.
    pub indexes: Vec<IndexDef>,
    /// The primary key constraint's name, which is a relation name like any other even though it
    /// has no index behind it. It is what a `23505` on the key quotes back.
    ///
    /// **Empty means the user declared no primary key**, in which case [`TableDef::row_id`] is
    /// `Some` and the key is an internal row id. Empty is unambiguous rather than convenient: a
    /// zero-length identifier is a syntax error in PostgreSQL (see [`INTERNAL_ROW_ID_NAME`]), so
    /// no constraint a user names can be spelled this way.
    pub primary_key_name: String,
    /// How many times this table's *shape* has changed. `1` for a table as `CREATE TABLE` left
    /// it; each `ALTER TABLE ADD COLUMN` adds one.
    ///
    /// Nothing needs it to read a row — a row carries its own column count (`crate::row`) — so it
    /// is not load-bearing yet. It is here because a schema change is the table's own event and
    /// the cluster-wide `catalog_version` cannot say which table moved, which is what the staged
    /// online-DDL design in `docs/adr/0020-online-schema-change.md` needs to attach per-column
    /// states to.
    pub schema_version: u64,
    /// The sequences that fill this table's columns — `bigserial` and identity columns.
    ///
    /// **Not part of the table record**, and that is deliberate. A sequence is keyed by the column
    /// it fills (`crate::catalog::record`), so a table's sequences are one prefix scan, and the
    /// scan happens where the table is loaded and cached. The table record has a format version
    /// and readers on both sides of it; a feature that can be added without touching it is a
    /// feature that cannot break one.
    ///
    /// In column order, which is the order the scan returns them in.
    pub sequences: Vec<SequenceDef>,
    /// `CHECK` constraints, in the order `pg_constraint` lists them — by name.
    ///
    /// Each holds its predicate as **text**, not as a parsed tree, and is re-lowered when the
    /// table is loaded. That is two things at once and both are wanted: `pg_get_constraintdef`
    /// needs the text anyway, and one string is an encoding this record already knows how to
    /// write, where a serialised expression tree would be a second format to version.
    pub checks: Vec<CheckDef>,
    /// `FOREIGN KEY` constraints **this table is the child of**, in name order.
    ///
    /// The other direction is not here and cannot be: a table does not know who references it
    /// without reading every other table. That question — which a `DELETE` on a parent row asks
    /// once per statement — is answered by a key space instead
    /// ([`foreign_key_backref_range`]), so a parent's delete costs a short prefix
    /// scan rather than a scan of the whole catalog.
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// Whether `ALTER TABLE … DISABLE TRIGGER ALL` has suspended this table's referential checks.
    ///
    /// **Stored, because PostgreSQL's is stored**: `pg_trigger.tgenabled` outlives the transaction
    /// that set it and every session sees it, so a flag kept beside the connection would leave a
    /// second client enforcing what the first one turned off.
    ///
    /// It suspends the checks *attached to this table*, which is not the same as the checks this
    /// table is named in. A foreign key has two internal triggers, one on the child and one on the
    /// parent; disabling the child's lets a row in with no parent, disabling the parent's lets a
    /// referenced row be deleted, and neither does the other's job. Measured on PostgreSQL 19, all
    /// four combinations ([`crate::plan::AlterTableAction::SetTriggersDisabled`]).
    pub triggers_disabled: bool,
    /// The tables this one **inherits from**, in the order written, and the tables that inherit
    /// **from** it.
    ///
    /// Both directions are stored because both are asked in O(1) and by different callers: a scan
    /// of a parent needs its children, and `pg_inherits` and the column merge need a child's
    /// parents. They are one edge written twice, always in the same transaction — a child's
    /// `CREATE TABLE` appends to its parents' lists, and `DROP TABLE` removes it from them.
    ///
    /// **Inheritance is a read rule, not only a DDL one.** A `SELECT`, an `UPDATE` and a `DELETE`
    /// on a parent all reach the rows in `children`; a node that copied the columns and stopped
    /// would answer about half a table.
    pub parents: Vec<u64>,
    /// See [`TableDef::parents`].
    pub children: Vec<u64>,
    /// The `EXCLUDE` constraints on this table.
    pub excludes: Vec<ExcludeDef>,
    /// The triggers registered on this table, in creation order.
    ///
    /// **Stored and never fired.** `ALTER TABLE … DISABLE TRIGGER ALL` is a separate flag
    /// ([`TableDef::triggers_disabled`]) that predates these and means something else: it turns
    /// off the *internal* triggers a foreign key is made of.
    pub triggers: Vec<TriggerDef>,
    /// `PARTITION BY LIST (…)` — the key this table's rows are routed on, or `None` for a table
    /// that is not partitioned.
    ///
    /// A partitioned table **stores no rows of its own**: `SELECT count(*) FROM ONLY m` is `0` on
    /// a real server however many rows the partitions hold. Its own key range stays empty here for
    /// the same reason, and the rows come from [`TableDef::children`] — the edge declarative
    /// partitioning shares with `INHERITS`, which is why a partition appears in `pg_inherits` too.
    pub partition_by: Option<PartitionKey>,
    /// `FOR VALUES IN (…)` or `DEFAULT` — which rows this table takes, for a partition.
    ///
    /// `None` for everything that is not a partition, including a table that merely *inherits*:
    /// the two share an edge and not this.
    pub partition_bound: Option<PartitionBound>,
    /// How to read each child's rows **as this table's**, filled where the table is loaded.
    ///
    /// Derived rather than stored, exactly as [`TableDef::sequences`] is: a scan of a parent
    /// returns its children's rows too, and the planner has no catalog in reach to work out how.
    /// A record decoded straight from bytes therefore has none.
    pub child_scans: Vec<ChildScan>,
    /// `COMMENT ON TABLE t IS '…'`, or `None`.
    ///
    /// Separate from the columns' comments, exactly as `pg_description` keeps them separate:
    /// `COMMENT ON TABLE t IS NULL` removes this one and leaves every column's. Measured.
    pub comment: Option<String>,
    /// `COMMENT ON INDEX t_pkey IS '…'`, or `None`.
    ///
    /// Here rather than on an [`IndexDef`] because **there is no index behind a primary key** in
    /// this node — the row key *is* the key — and yet `t_pkey` is a relation a client can name
    /// (`RelKind::PrimaryKey`), so `COMMENT ON INDEX t_pkey` is a statement a real server accepts
    /// and `obj_description('t_pkey'::regclass, 'pg_class')` answers. Measured; it is the one
    /// comment that has nowhere else to live.
    pub primary_key_comment: Option<String>,
}

/// Whether a relation's contents survive a crash: `pg_class.relpersistence`.
///
/// Three values on a real server and two here — `t`, temporary, is a different feature and
/// `CREATE TEMPORARY TABLE` is refused by name, so a value for it would describe a table this node
/// cannot make. **`u` is not a kind of `t`**: an unlogged table is permanent and shared, and only
/// its *contents* are expendable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persistence {
    /// `p`. Written to the log, and what every table is unless it says otherwise.
    #[default]
    Permanent,
    /// `u`. **Recorded and not yet acted on**: this node logs an unlogged table's writes like any
    /// other, so the only difference a client can see is the catalog column. The saving an
    /// unlogged table exists for is an engine decision — which column family a write goes to and
    /// whether it is truncated on recovery — and skipping the WAL for one would be a durability
    /// change (`CLAUDE.md` invariant 1) rather than a catalog one. What the suite needs is the
    /// statement to work and the column to be right; what it does not need is the data loss.
    Unlogged,
}

impl Persistence {
    /// The one character `pg_class.relpersistence` carries.
    #[must_use]
    pub fn relpersistence(self) -> &'static str {
        match self {
            Persistence::Permanent => "p",
            Persistence::Unlogged => "u",
        }
    }
}

/// A stored function — **defined and never executed**.
///
/// The schema load reaches `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql` twice (statements 762
/// and 790) and inserts nothing through it; the capture proves the table is empty immediately
/// after. So what a suite needs from this node is a catalog that can *hold* a function, not a
/// procedural-language runtime — and exactly one test in the whole suite ever fires a trigger.
/// Calling one is refused where the statement is lowered — before any catalog is in reach, so the
/// message names the function rather than saying what a real server says, which is that trigger
/// functions can only be called as triggers. A declared divergence, and the load never calls one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    /// Its own relation id, from the same counter tables draw from.
    pub id: u64,
    /// Its name, folded. This node holds one function per name: every function it can store takes
    /// no arguments, so there is nothing to overload on.
    pub name: String,
    /// The body **verbatim**, without the dollar quotes — which is what `pg_proc.prosrc` holds and
    /// what its `length()` counts.
    pub body: String,
    /// `LANGUAGE plpgsql`, folded. The only one this node accepts; anything else is `42704`.
    pub language: String,
}

/// One trigger on a table — **registered and never fired**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDef {
    /// Its name, unique per table rather than per database: `pg_trigger` is keyed by both, and
    /// a second `CREATE TRIGGER` of the same name on the same table is `42710`.
    pub name: String,
    /// `BEFORE` or `AFTER`.
    pub before: bool,
    /// The events, as PostgreSQL's own `tgtype` bits: `4` INSERT, `8` DELETE, `16` UPDATE.
    pub events: i16,
    /// `FOR EACH ROW` rather than `FOR EACH STATEMENT`.
    pub for_each_row: bool,
    /// The function it names — by name, because the function is a separate record.
    pub function: String,
    /// `tgenabled`: **`O` when enabled and `D` when disabled**, a letter and not a boolean.
    pub enabled: bool,
}

impl TriggerDef {
    /// `tgtype`, the bitmask a client reads: `1` ROW, `2` BEFORE, then the events.
    ///
    /// `7` for `BEFORE INSERT … FOR EACH ROW`, which is what the capture pins.
    #[must_use]
    pub fn tgtype(&self) -> i16 {
        i16::from(self.for_each_row) | (i16::from(self.before) << 1) | self.events
    }

    /// `tgenabled`.
    #[must_use]
    pub fn tgenabled(&self) -> &'static str {
        if self.enabled { "O" } else { "D" }
    }
}

/// `PARTITION BY LIST (col, …)` — how a partitioned table routes a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionKey {
    /// Which strategy. `LIST` is the one this node builds; the others are refused by name where
    /// the statement is lowered, because a bound it cannot compare is a row it would misroute.
    pub strategy: PartitionStrategy,
    /// The key columns, by position, in the order written.
    pub columns: Vec<usize>,
}

/// The strategy letter `pg_partitioned_table.partstrat` reports.
///
/// **A one-letter code, not the word in the DDL** — `l`, never `LIST`. Measured.
///
/// `HASH` is not here: nothing captured it, and a strategy whose routing nobody measured is a row
/// this node would put in the wrong partition. It is refused by name where the statement is
/// lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStrategy {
    /// `LIST`, reported as `l`.
    List,
    /// `RANGE`, reported as `r`.
    Range,
}

impl PartitionStrategy {
    /// `partstrat`.
    ///
    /// `l` is measured. `r` is PostgreSQL's own code for `RANGE` and this capture never asked for
    /// it — `pg_partitioned_table` is probed only over the `LIST` table — so it is the one letter
    /// here that is taken from PostgreSQL rather than from a row.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            PartitionStrategy::List => "l",
            PartitionStrategy::Range => "r",
        }
    }

    /// The word `pg_get_partkeydef` prints.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            PartitionStrategy::List => "LIST",
            PartitionStrategy::Range => "RANGE",
        }
    }
}

/// Which rows a partition takes.
#[derive(Debug, Clone, PartialEq)]
pub enum PartitionBound {
    /// `FOR VALUES IN (…)`, **already coerced to the key columns' types**.
    ///
    /// That coercion is visible: the suite writes the integer `1` against a `character varying`
    /// key and a real server prints the bound back as `FOR VALUES IN ('1')`, quoted. Storing the
    /// literal as written would diverge the moment `ActiveRecord` dumps the schema.
    Values(Vec<Datum>),
    /// `FOR VALUES FROM (…) TO (…)` — **half-open**, `from` included and `to` excluded.
    ///
    /// Measured: `FROM (MINVALUE) TO (10)` takes `9` and `FROM (10) TO (MAXVALUE)` takes `10`. A
    /// closed upper bound would put `10` in both partitions, which is why the two ranges in the
    /// capture do not overlap despite sharing the number.
    Range {
        /// The lower bound, one entry per key column.
        from: Vec<RangeBound>,
        /// The upper bound, one entry per key column.
        to: Vec<RangeBound>,
    },
    /// `DEFAULT` — everything no other partition takes, and its bound prints as the bare word.
    Default,
}

/// One end of a `RANGE` bound: a value, or an infinity.
///
/// **`MINVALUE` and `MAXVALUE` print back verbatim** — they are not a very small and a very large
/// number, and a node that stored them as `i64::MIN` and `i64::MAX` would print numbers where a
/// real server prints the words.
#[derive(Debug, Clone, PartialEq)]
pub enum RangeBound {
    /// `MINVALUE`: below every value, NULL included.
    MinValue,
    /// A value, **already coerced to the key column's type**.
    Value(Datum),
    /// `MAXVALUE`: above every value.
    MaxValue,
}

impl RangeBound {
    /// Where a key value sits relative to this end.
    ///
    /// `MINVALUE` is below everything and `MAXVALUE` above it, which makes the half-open test one
    /// comparison either side rather than four cases.
    #[must_use]
    pub fn cmp_value(&self, value: &Datum) -> core::cmp::Ordering {
        match self {
            RangeBound::MinValue => core::cmp::Ordering::Less,
            RangeBound::MaxValue => core::cmp::Ordering::Greater,
            RangeBound::Value(bound) => value::PgDatum::pg_cmp(bound, value),
        }
    }

    /// How two ends of the same kind order against each other.
    #[must_use]
    pub fn cmp_bound(&self, other: &RangeBound) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        match (self, other) {
            (RangeBound::MinValue, RangeBound::MinValue)
            | (RangeBound::MaxValue, RangeBound::MaxValue) => Ordering::Equal,
            (RangeBound::MinValue, _) | (_, RangeBound::MaxValue) => Ordering::Less,
            (RangeBound::MaxValue, _) | (_, RangeBound::MinValue) => Ordering::Greater,
            (RangeBound::Value(ours), RangeBound::Value(theirs)) => {
                value::PgDatum::pg_cmp(ours, theirs)
            }
        }
    }

    /// The text `pg_get_expr(relpartbound, oid)` prints for this end.
    #[must_use]
    pub fn printed(&self) -> String {
        match self {
            RangeBound::MinValue => "MINVALUE".to_owned(),
            RangeBound::MaxValue => "MAXVALUE".to_owned(),
            RangeBound::Value(value) => partition_literal(value),
        }
    }
}

/// A bound value as PostgreSQL deparses it: **a number bare and a string quoted**.
///
/// Both spellings are in one capture. The `LIST` bound is on a `character varying` key and comes
/// back `FOR VALUES IN ('1')`; the `RANGE` bound is on an `int4` key and comes back
/// `FOR VALUES FROM (MINVALUE) TO (10)` — no quotes. The type decides, which is why the value is
/// coerced before it is stored rather than printed as it was typed.
fn partition_literal(value: &Datum) -> String {
    let text = value::PgDatum::to_text(value).unwrap_or_else(|| "NULL".to_owned());
    match value {
        Datum::Int2(_)
        | Datum::Int4(_)
        | Datum::Int8(_)
        | Datum::Numeric(_)
        | Datum::Double(_)
        | Datum::Real(_)
        | Datum::Oid(_) => text,
        Datum::Bool(flag) => (if *flag { "true" } else { "false" }).to_owned(),
        _ => format!("'{}'", text.replace('\'', "''")),
    }
}

/// `pg_get_partkeydef(oid)` — `LIST (city_id)`, or NULL for a relation that is not partitioned.
///
/// The **word**, not the code: `partstrat` is `l` and this is `LIST`, and the two are read from
/// the same field. A client that guessed one from the other would be wrong in both directions.
#[must_use]
pub fn partition_key_definition(relations: &pg_relations::Relations, oid: Option<i64>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    let Some(relation) = relations.by_oid(oid) else {
        return Datum::Null;
    };
    let Some(table) = relations.table(relation) else {
        return Datum::Null;
    };
    let Some(key) = &table.partition_by else {
        return Datum::Null;
    };
    let columns: Vec<&str> = key
        .columns
        .iter()
        .filter_map(|&at| table.columns.get(at).map(|column| column.name.as_str()))
        .collect();
    Datum::Text(format!("{} ({})", key.strategy.word(), columns.join(", ")))
}

/// `FOR VALUES IN ('1')`, `FOR VALUES FROM (MINVALUE) TO (10)` or `DEFAULT` — the bound as
/// `pg_get_expr(relpartbound, oid)` prints it.
///
/// The bound stored here has already been coerced to the key columns' types, so this prints what
/// the column holds and not what was typed — which is the whole reason the suite's
/// `FOR VALUES IN (1)` against a `character varying` key comes back quoted: a number prints bare
/// and a string prints quoted, and the value knows which it is.
#[must_use]
pub fn partition_bound_definition(bound: &PartitionBound) -> String {
    let listed = |values: &[Datum]| {
        values
            .iter()
            .map(partition_literal)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ends = |bounds: &[RangeBound]| {
        bounds
            .iter()
            .map(RangeBound::printed)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match bound {
        PartitionBound::Default => "DEFAULT".to_owned(),
        PartitionBound::Values(values) => format!("FOR VALUES IN ({})", listed(values)),
        PartitionBound::Range { from, to } => {
            format!("FOR VALUES FROM ({}) TO ({})", ends(from), ends(to))
        }
    }
}

/// One child table, seen from its parent: where its rows are and how they line up.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildScan {
    /// The child's own id, for the key range its rows are in.
    pub table_id: u64,
    /// The child's own row schema — what its bytes decode as, which is **not** the parent's: a
    /// child with no primary key carries an internal row id the parent may not have.
    pub schema: crate::row::RowSchema,
    /// For each of the parent's columns in order, where that column sits in a child's row.
    ///
    /// By name rather than by position, because the two differ the moment a child has a row id
    /// the parent does not, or a column of its own.
    pub project: Vec<usize>,
}

/// One `EXCLUDE` constraint: a key expression, an operator, and the rows it applies to.
///
/// **No index behind it.** PostgreSQL builds a `GiST` index and this node scans the table instead —
/// `USING gist` is recorded because `pg_get_indexdef` prints it and `ActiveRecord` reads it, and
/// the access method is the one thing about an exclusion constraint that is a *performance*
/// decision rather than an answer. What is not negotiable is the answer: a row that overlaps an
/// existing one is refused with the same `23P01` either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludeDef {
    /// Its name, as written or as PostgreSQL derives one.
    pub name: String,
    /// The key expression, stored as text the way a `CHECK` is — `daterange(start_date, end_date)`.
    pub key: String,
    /// The operator two keys are compared with. `&&` is the one this node has.
    pub operator: String,
    /// The access method named, recorded and not used: `gist`.
    pub method: String,
    /// `WHERE (…)` — the partial predicate, or `None`.
    ///
    /// **It is what makes NULLs legal, and duplicates too**: a row the predicate rejects is not in
    /// the index at all, so two identical ones both insert. Four rows of `(NULL, NULL)` are four
    /// conflicts without it.
    pub predicate: Option<String>,
    /// `DEFERRABLE`.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED`: whether the check **starts** held to `COMMIT`.
    ///
    /// The default this and [`ExcludeDef::deferrable`] give
    /// `crate::exec::deferred::Constraints::deferred`, which a `SET CONSTRAINTS` in the
    /// transaction overrides. Deferring is not a delay for its own sake — it is what lets a
    /// transaction break the constraint in the middle and repair it before the end, and the check
    /// is re-examined at `COMMIT` rather than replayed.
    pub deferred: bool,
}

/// Each operand of a top-level `AND`/`OR` chain in its own parentheses — PostgreSQL's rule for
/// re-printing a boolean expression.
///
/// `a IS NOT NULL AND b IS NOT NULL` comes back `(a IS NOT NULL) AND (b IS NOT NULL)`; a predicate
/// that is a single comparison is returned unchanged, which is why `CHECK ((p > 0))` has only the
/// two pairs `pg_get_constraintdef` adds around it. Measured, both.
///
/// Split on the **top level only**: a keyword inside parentheses or inside a string literal is
/// part of an operand, not a separator.
pub(crate) fn parenthesised_operands(predicate: &str) -> String {
    let bytes = predicate.as_bytes();
    let upper = predicate.to_ascii_uppercase();
    let upper = upper.as_bytes();
    let mut operands = Vec::new();
    let mut separators = Vec::new();
    let (mut depth, mut quoted, mut start, mut at) = (0_i32, false, 0, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            _ if quoted || depth != 0 => {}
            _ => {
                for keyword in [" AND ", " OR "] {
                    if upper[at..].starts_with(keyword.as_bytes()) {
                        operands.push(predicate[start..at].trim());
                        separators.push(keyword.trim());
                        start = at + keyword.len();
                        at += keyword.len() - 1;
                        break;
                    }
                }
            }
        }
        at += 1;
    }
    if operands.is_empty() {
        return predicate.to_owned();
    }
    operands.push(predicate[start..].trim());
    let mut out = format!("({})", operands[0]);
    for (operand, separator) in operands[1..].iter().zip(&separators) {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!(" {separator} ({operand})"));
    }
    out
}

/// One `FOREIGN KEY` constraint, held by the **child** — the table whose rows must point at
/// something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    /// Its name — given, or derived as `<table>_<column>_fkey` the way PostgreSQL derives one.
    pub name: String,
    /// Positions into this table's columns: `conkey`.
    pub columns: Vec<usize>,
    /// The referenced table's id, which is also its oid: `confrelid`.
    ///
    /// An id rather than a name, for the reason an index's key parts are positions — renaming the
    /// parent cannot orphan the constraint, and the name in a message is read back out of the
    /// parent's own record.
    pub parent: u64,
    /// Positions into the **parent's** columns, in the order they pair with [`Self::columns`]:
    /// `confkey`.
    pub parent_columns: Vec<usize>,
    /// `ON UPDATE …`: `confupdtype`.
    pub on_update: ReferentialAction,
    /// `ON DELETE …`: `confdeltype`.
    pub on_delete: ReferentialAction,
    /// `DEFERRABLE`, which is recorded and **changes nothing**: every check here is immediate,
    /// and `DEFERRABLE INITIALLY IMMEDIATE` — the only deferrable form `ActiveRecord` writes — is
    /// immediate on a real server too. `INITIALLY DEFERRED` is `0A000` naming itself, because
    /// accepting it and checking immediately would refuse a transaction PostgreSQL commits.
    pub deferrable: bool,
}

/// What a `FOREIGN KEY` does when the row it points at is deleted or its key is changed.
///
/// `SET NULL` and `SET DEFAULT` are PostgreSQL's other two and are `0A000` naming themselves:
/// nothing `ActiveRecord` writes uses them, and each would need a rule about which columns it
/// touches that this crate has nowhere to put yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferentialAction {
    /// `NO ACTION`, the default — refuse. On a real server this one is deferrable to the end of
    /// the statement and [`ReferentialAction::Restrict`] is not; with every check immediate here
    /// the two behave identically and differ only in what they print and store, which is exactly
    /// what the capture shows for an immediate constraint.
    NoAction,
    /// `RESTRICT` — refuse.
    Restrict,
    /// `CASCADE` — delete the referencing rows, or rewrite their key to follow the parent's.
    Cascade,
}

impl ReferentialAction {
    /// `confupdtype` / `confdeltype`: PostgreSQL's one-character code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            ReferentialAction::NoAction => "a",
            ReferentialAction::Restrict => "r",
            ReferentialAction::Cascade => "c",
        }
    }

    /// What `pg_get_constraintdef` prints for it — **nothing at all** for the default, which is
    /// why `ON DELETE NO ACTION` written out comes back absent.
    #[must_use]
    pub fn clause(self) -> &'static str {
        match self {
            ReferentialAction::NoAction => "",
            ReferentialAction::Restrict => "RESTRICT",
            ReferentialAction::Cascade => "CASCADE",
        }
    }

    /// Whether a parent row this action guards may be removed or re-keyed at all.
    #[must_use]
    pub fn refuses(self) -> bool {
        matches!(
            self,
            ReferentialAction::NoAction | ReferentialAction::Restrict
        )
    }
}

/// One `CHECK` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDef {
    /// Its name — given, or derived as `<table>_<column>_check` the way PostgreSQL derives one.
    pub name: String,
    /// The predicate, as the user wrote it. Re-parsed on load and printed by
    /// `pg_get_constraintdef`.
    pub expr: String,
}

impl ColumnDef {
    /// The declared length of a `varchar(n)` or `character(n)`, or `None` for a column given no
    /// number — which for `character varying` means unlimited and for nothing else means anything.
    ///
    /// A `character` with no number is **not** one of those: PostgreSQL reads a bare `character` as
    /// `character(1)`, so the parser gives it a typmod and this answers `Some(1)`.
    #[must_use]
    pub fn length(&self) -> Option<u32> {
        match self.ty {
            ColumnType::Varchar | ColumnType::Bpchar => value::length_of_typmod(self.typmod),
            _ => None,
        }
    }

    /// The declared precision of a `timestamp(p)`, or `None` for one given no number — which means
    /// the full six digits PostgreSQL stores.
    #[must_use]
    pub fn precision(&self) -> Option<u32> {
        match self.ty {
            ColumnType::Timestamp | ColumnType::TimestampTz => {
                value::precision_of_typmod(self.typmod)
            }
            _ => None,
        }
    }
}

impl TableDef {
    /// The sequence that fills column `at`, if one does.
    #[must_use]
    pub fn sequence_for(&self, at: usize) -> Option<&SequenceDef> {
        self.sequences
            .iter()
            .find(|sequence| sequence.column == Some(at))
    }

    /// The position of a column by name, or `None`.
    ///
    /// A user's name never finds the internal row id, because that column's name is one no
    /// statement can contain ([`INTERNAL_ROW_ID_NAME`]).
    #[must_use]
    pub fn column(&self, name: &str) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        self.columns.iter().position(|column| column.name == name)
    }

    /// The position of the internal row id, or `None` for a table whose user declared a primary
    /// key.
    ///
    /// It is always column 0 when it is there, so a later `ALTER TABLE ADD COLUMN` appends after
    /// the user's columns and cannot move it.
    #[must_use]
    pub fn row_id(&self) -> Option<usize> {
        // A `pg_catalog` view has no key **and no row id**: nothing stores its rows, so there is
        // no identity to hide in column 0. Without this it reads as a keyless table and
        // [`TableDef::user_columns`] hides its first column — which is `pg_type.oid`, so
        // `SELECT * FROM pg_type` came back one column short. A wrong answer rather than a gap,
        // and found by asking the running node a question the corpus could not: a real server's
        // `pg_type` has some thirty columns, so `SELECT *` is not a line two servers can agree on.
        if pg_catalog::view_of(self).is_some() {
            return None;
        }
        // A **derived table** likewise: its rows come from a plan, not from a key, so there is no
        // identity to hide in column 0 — and hiding one would drop the first column of every
        // `SELECT * FROM (SELECT …) AS t`, which is the same wrong answer a catalog view gave
        // before the line above it.
        if self.id == DERIVED_TABLE_ID {
            return None;
        }
        // A **sequence** read as a relation, for the third time and the same reason: its one row
        // is a counter rather than something stored, so there is no identity in column 0 — and
        // hiding one dropped `last_value` from `SELECT * FROM <sequence>`, which is the same wrong
        // answer the two above it gave before their lines existed.
        if sequence_of_relation(self.id).is_some() {
            return None;
        }
        self.primary_key_name.is_empty().then_some(0)
    }

    /// The columns a user can see: every column except the internal row id.
    ///
    /// This is what `SELECT *` expands to and what an `INSERT` with no column list fills, which is
    /// the whole of what makes the row id *hidden* rather than merely unnamed.
    pub fn user_columns(&self) -> impl Iterator<Item = (usize, &ColumnDef)> {
        let skip = usize::from(self.row_id().is_some());
        self.columns.iter().enumerate().skip(skip)
    }

    /// Every column's type, in encoding order — what [`crate::row`] needs.
    #[must_use]
    pub fn column_types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(|column| column.ty).collect()
    }

    /// How this table's rows decode: [`TableDef::column_types`] and [`TableDef::column_missing`]
    /// as the one value [`crate::row::decode_row`] takes.
    #[must_use]
    pub fn row_schema(&self) -> crate::row::RowSchema {
        crate::row::RowSchema::new(self.column_types(), self.column_missing())
    }

    /// What each column reads as in a row too narrow to hold it, in the same order.
    ///
    /// The second half of what [`crate::row::decode_row`] needs for a **table row**, and the reason
    /// `ALTER TABLE ADD COLUMN ... DEFAULT <constant>` rewrites nothing: the value lives here
    /// rather than in every row (`ColumnDef::missing`).
    #[must_use]
    pub fn column_missing(&self) -> Vec<Option<Datum>> {
        self.columns
            .iter()
            .map(|column| column.missing.clone())
            .collect()
    }

    /// The types of the primary key columns, in key order.
    #[must_use]
    pub fn primary_key_types(&self) -> Vec<ColumnType> {
        self.primary_key
            .iter()
            .map(|&ordinal| self.columns[ordinal].ty)
            .collect()
    }
}

/// How a column's sequence answers a value the user wrote for it.
///
/// Three states, and the difference between them is what `INSERT INTO t (id) VALUES (7)` does.
/// Measured on PostgreSQL 19beta1, all three:
///
/// * [`Identity::Default`] — a `bigserial` column. The sequence is the column's `DEFAULT`, so an
///   explicit value is taken and **the sequence is not advanced**; the next insert that omits the
///   column can therefore collide with it, and does, with the `23505` a real server gives.
/// * [`Identity::ByDefault`] — `GENERATED BY DEFAULT AS IDENTITY`. Behaves the same way.
/// * [`Identity::Always`] — `GENERATED ALWAYS AS IDENTITY`. An explicit value is `428C9`, with
///   PostgreSQL's own `DETAIL` and its `HINT` naming `OVERRIDING SYSTEM VALUE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// `bigserial`: the sequence is a `DEFAULT` and nothing more.
    Default,
    /// `GENERATED BY DEFAULT AS IDENTITY`.
    ByDefault,
    /// `GENERATED ALWAYS AS IDENTITY`.
    Always,
}

impl Identity {
    /// The byte it is stored as. Frozen.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Identity::Default => 0,
            Identity::ByDefault => 1,
            Identity::Always => 2,
        }
    }

    /// Reads the byte back. An unknown one is corruption, not a default: a record written by a
    /// newer version means this node cannot say what the column does, and guessing `Default`
    /// would let a value into a column a real server refuses.
    pub fn from_u8(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(Identity::Default),
            1 => Ok(Identity::ByDefault),
            2 => Ok(Identity::Always),
            other => Err(SqlError::DataCorrupted(format!(
                "identity kind byte {other}"
            ))),
        }
    }

    /// Whether a value the user wrote for this column is refused.
    #[must_use]
    pub fn refuses_explicit(self) -> bool {
        matches!(self, Identity::Always)
    }
}

/// One sequence: what fills a `bigserial` or an identity column.
///
/// Every sequence here is **owned by one column**. A standalone `CREATE SEQUENCE` is `0A000` (its
/// options are gap G07 in `docs/plans/phase-6a.md` §9), so there is no unowned case to carry, and
/// the owner is what the record is keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceDef {
    /// Its own id, from the same counter tables and indexes draw from — one relation-id space, as
    /// PostgreSQL has one `pg_class`.
    pub id: u64,
    /// `<table>_<column>_seq`, folded, and a name in the same namespace as tables and indexes.
    pub name: String,
    /// The table whose record range holds it: the table that **owns** it, or
    /// [`STANDALONE_SEQUENCE_OWNER`] for one that no column does.
    pub table_id: u64,
    /// The column this sequence **fills** — its `nextval` is that column's default.
    ///
    /// `None` for a sequence that fills nothing, which is every sequence `CREATE SEQUENCE` makes:
    /// creating one leaves the column's default alone, measured on `pg_attrdef`, and it takes an
    /// `ALTER COLUMN … SET DEFAULT` to point a column at it.
    pub column: Option<usize>,
    /// The column that **owns** it: dropping that column drops the sequence.
    ///
    /// A different fact from [`SequenceDef::column`], and PostgreSQL keeps them apart — a
    /// `bigserial` sets both to the same column, and `OWNED BY` sets only this one. Conflating
    /// them is what the single ordinal in the key did before catalog version 15, and it could not
    /// hold the statement this exists for: a column may own more than one sequence.
    pub owner_column: Option<usize>,
    /// What it does with a value the user wrote.
    pub identity: Identity,
    /// `START n`: the **first** value `nextval` answers, not the one before it. Measured —
    /// `START 101` gives `101` and then `102`.
    pub start: i64,
    /// `INCREMENT BY n`, `1` unless one was written.
    pub increment: i64,
}

/// The owner id a sequence no column owns is filed under.
///
/// A real relation id is never `0` — they come from a counter that starts above it — so this
/// cannot collide with a table, and it keeps every sequence in one key space with one scan.
pub const STANDALONE_SEQUENCE_OWNER: u64 = 0;

/// What a name resolves to. Tables, indexes and sequences share one namespace, as they do in
/// PostgreSQL's `pg_class`: creating an index over a table's name answers `42P07`, confirmed
/// against a server, and so does a table named after a `bigserial` column's sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// A table.
    Table {
        /// Its id.
        table_id: u64,
    },
    /// An index, and the table it is on.
    Index {
        /// The table the index is defined on.
        table_id: u64,
        /// The index's own id.
        index_id: u64,
    },
    /// A sequence, by the column it fills — which is what its record is keyed by.
    Sequence {
        /// The table that owns it, or [`STANDALONE_SEQUENCE_OWNER`].
        table_id: u64,
        /// Its own id.
        sequence_id: u64,
    },
    /// A primary key constraint's name — `<table>_pkey`.
    ///
    /// It points at no index because there is none to point at: the row key *is* the primary key
    /// (`crate::row`), so the constraint is enforced by the key space itself. The name is still
    /// taken, though, and has to be: PostgreSQL answers `42P07` to `CREATE INDEX t_pkey ON t (a)`,
    /// and it is the name a `23505` on the primary key quotes back.
    PrimaryKey {
        /// The table whose primary key it is.
        table_id: u64,
    },
}

/// An identifier as PostgreSQL stores it: folded when it was written unquoted, and truncated to
/// [`MAX_IDENTIFIER_BYTES`].
///
/// **Only ASCII `A`–`Z` fold.** A UTF-8 server leaves `Ébc` alone — measured, and the reason this
/// is not `str::to_lowercase`, which would have quietly renamed every non-ASCII identifier.
/// The second return value is whether truncation happened, which PostgreSQL reports as a `42622`
/// notice rather than an error.
#[must_use]
pub fn fold_identifier(name: &str, quoted: bool) -> (String, bool) {
    let folded = if quoted {
        name.to_owned()
    } else {
        name.chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c
                }
            })
            .collect()
    };
    if folded.len() <= MAX_IDENTIFIER_BYTES {
        return (folded, false);
    }
    // Truncate on a character boundary: cutting a multi-byte character in half would leave a name
    // that is not UTF-8, which the catalog would then refuse to read back.
    let mut end = MAX_IDENTIFIER_BYTES;
    while !folded.is_char_boundary(end) {
        end -= 1;
    }
    (folded[..end].to_owned(), true)
}

/// The per-node cache of definitions. One per SQL node, shared by every session.
#[derive(Debug, Default)]
pub struct Catalog {
    cached: Mutex<Cache>,
}

#[derive(Debug, Default)]
struct Cache {
    /// The catalog version everything below was read at.
    version: u64,
    /// `(tenant, name)` to what it resolves to, `None` for a name that is known not to exist.
    names: BTreeMap<(u64, String), Option<Relation>>,
    /// `(tenant, table_id)` to the definition.
    tables: BTreeMap<(u64, u64), Arc<TableDef>>,
}

impl Catalog {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Catalog::default()
    }

    /// Reads the catalog version once and returns the view every lookup in this transaction goes
    /// through.
    ///
    /// This is the whole of the consistency mechanism: one read, at the transaction's snapshot,
    /// and every definition it then sees belongs to that version.
    ///
    /// Only for a transaction that has **not** written the catalog itself — see
    /// [`Catalog::view_uncached`], which is what a transaction that has run DDL must use.
    pub fn view<'a>(&'a self, txn: &'a dyn Txn, tenant: u64) -> Result<View<'a>> {
        self.view_at(txn, tenant, true)
    }

    /// The same view, reading through the cache and never filling it.
    ///
    /// This is what a transaction that has run DDL gets. Its reads answer with its own
    /// uncommitted definitions, which are exactly the ones that must not be published to the
    /// other sessions sharing this node (see the module docs).
    pub fn view_uncached<'a>(&'a self, txn: &'a dyn Txn, tenant: u64) -> Result<View<'a>> {
        self.view_at(txn, tenant, false)
    }

    fn view_at<'a>(&'a self, txn: &'a dyn Txn, tenant: u64, cached: bool) -> Result<View<'a>> {
        let version = match txn.get(&record::version_key())? {
            Some(bytes) => record::decode_counter(&bytes)?,
            // No DDL has ever run. Version 0 is the empty catalog.
            None => 0,
        };
        Ok(View {
            catalog: cached.then_some(self),
            txn,
            tenant,
            version,
        })
    }

    /// A poisoned lock means a thread panicked holding it; the maps behind it are still sound and
    /// taking them back beats propagating a panic into a session (`CLAUDE.md` invariant 9).
    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops everything older than `version`, and reports whether the cache may now be used for
    /// it. A view older than the cache is told no: its snapshot is real and the cache is ahead of
    /// it.
    fn usable_at(&self, version: u64) -> bool {
        let mut cache = self.lock();
        if version > cache.version {
            *cache = Cache {
                version,
                ..Cache::default()
            };
        }
        cache.version == version
    }
}

/// One transaction's consistent view of the catalog.
#[derive(Debug)]
pub struct View<'a> {
    /// The cache to answer from and fill, or `None` for a transaction that has written the
    /// catalog and whose reads are therefore its own uncommitted DDL.
    catalog: Option<&'a Catalog>,
    txn: &'a dyn Txn,
    tenant: u64,
    version: u64,
}

impl View<'_> {
    /// The catalog version this view is pinned to.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// What a name is, or `None` when nothing of that name exists.
    pub fn relation(&self, name: &str) -> Result<Option<Relation>> {
        let cache = self.cache();
        let key = (self.tenant, name.to_owned());
        if let Some(cache) = cache
            && let Some(hit) = cache.lock().names.get(&key)
        {
            return Ok(*hit);
        }
        let found = match self.txn.get(&record::name_key(self.tenant, name))? {
            Some(bytes) => Some(record::decode_relation(&bytes)?),
            None => None,
        };
        if let Some(cache) = cache {
            cache.lock().names.insert(key, found);
        }
        Ok(found)
    }

    /// A table by name, or `None`. An index's name resolves to no table: `SELECT * FROM an_index`
    /// is `42P01` in PostgreSQL too.
    pub fn table(&self, name: &str) -> Result<Option<Arc<TableDef>>> {
        // A `pg_catalog` relation is computed rather than stored, so it is answered before the
        // `'m'` space is consulted and cannot be shadowed by a record. This node has no schemas,
        // so `pg_type` is one name — where a real server would have resolved `pg_catalog.pg_type`
        // ahead of `public.pg_type` and let both exist (`pg_catalog::refuse_write` is what stops
        // the second from being created here, and `tests/pg_catalog.rs` declares the difference).
        if let Some(view) = pg_catalog::view(name) {
            return Ok(Some(view.table_def()));
        }
        match self.relation(name)? {
            Some(Relation::Table { table_id }) => self.table_by_id(table_id),
            Some(
                Relation::Index { .. } | Relation::PrimaryKey { .. } | Relation::Sequence { .. },
            )
            | None => Ok(None),
        }
    }

    /// A table by id.
    pub fn table_by_id(&self, table_id: u64) -> Result<Option<Arc<TableDef>>> {
        let cache = self.cache();
        let key = (self.tenant, table_id);
        if let Some(cache) = cache
            && let Some(hit) = cache.lock().tables.get(&key)
        {
            return Ok(Some(Arc::clone(hit)));
        }
        let Some(bytes) = self.txn.get(&record::table_key(self.tenant, table_id))? else {
            return Ok(None);
        };
        let mut table = record::decode_table(&bytes)?;
        // The sequences are a second read, and this is the one place it happens: a `TableDef` in
        // anybody's hands has them, so nothing above the catalog has to remember to ask.
        table.sequences = table_sequences(self.txn, self.tenant, table_id)?;
        // And the parents' — see `inherited_sequences` for why the record stays theirs.
        let inherited = inherited_sequences(self.txn, self.tenant, &table, &table.parents.clone())?;
        table.sequences.extend(inherited);
        table.child_scans = child_scans(self.txn, self.tenant, &table)?;
        let table = Arc::new(table);
        if let Some(cache) = cache {
            cache.lock().tables.insert(key, Arc::clone(&table));
        }
        Ok(Some(table))
    }

    /// The cache this view may use, or `None` — either because the transaction has written the
    /// catalog, or because the cache holds a version this view cannot be answered from.
    fn cache(&self) -> Option<&Catalog> {
        self.catalog
            .filter(|catalog| catalog.usable_at(self.version))
    }

    /// A table by name, or `42P01` — the shape almost every statement wants.
    ///
    /// A **sequence's** name is the one case that is not `42P01`, because the relation is there:
    /// PostgreSQL reads a sequence as a three-column relation (`last_value`, `log_cnt`,
    /// `is_called`) and this node does not, so it is `0A000` naming the construct. Answering
    /// "does not exist" for something that does is the wrong-answer shape contract C2 exists to
    /// prevent — it sends a user looking for a missing table.
    pub fn require_table(&self, name: &str) -> Result<Arc<TableDef>> {
        if let Some(table) = self.table(name)? {
            return Ok(table);
        }
        // **A sequence is a three-column relation**, which is what PostgreSQL shows for one, and
        // the relation *is* the sequence — there is no table behind it. The def carries the
        // sequence's id in its own so that the planner can read the counter rather than seek a key
        // range that does not exist (`sequence_relation_def`).
        if let Some(Relation::Sequence { sequence_id, .. }) = self.relation(name)? {
            return Ok(sequence_relation_def(name, sequence_id));
        }
        Err(SqlError::UndefinedTable(name.to_owned()))
    }
}

/// The key that records "`child` has a `FOREIGN KEY` into `parent`".
///
/// Written when the constraint is made and removed when either table is dropped. See
/// Records an extension as installed, at the version its build offers.
///
/// A catalog write like any other, so it commits and rolls back with the transaction that ran the
/// `CREATE EXTENSION` — which is what a real server does, and what makes the statement safe inside
/// the schema-load transaction `ActiveRecord` wraps everything in.
pub fn install_extension(txn: &mut dyn Txn, tenant: u64, name: &str, version: &str) {
    txn.put(
        &record::extension_key(tenant, name),
        &record::encode_extension(version),
    );
}

/// Writes a user-defined type. The caller has already checked that the name is free.
pub fn put_type(txn: &mut dyn Txn, tenant: u64, def: &TypeDef) {
    txn.put(
        &record::type_key(tenant, &def.name),
        &record::encode_type(def),
    );
}

/// One user-defined type by name, or `None`.
pub fn type_by_name(txn: &dyn Txn, tenant: u64, name: &str) -> Result<Option<TypeDef>> {
    match txn.get(&record::type_key(tenant, name))? {
        Some(bytes) => record::decode_type(name, &bytes).map(Some),
        None => Ok(None),
    }
}

/// Every user-defined type of one tenant, in name order — which is the order the key space returns
/// them in, and the order `pg_type` lists them.
pub fn user_types(txn: &dyn Txn, tenant: u64) -> Result<Vec<TypeDef>> {
    let (start, end) = record::type_range(tenant);
    txn.scan(&start, &end, 0)?
        .into_iter()
        .map(|(key, value)| {
            let name = record::type_name_of(tenant, &key)?;
            record::decode_type(&name, &value)
        })
        .collect()
}

/// Removes one. The caller has already checked that nothing depends on it.
pub fn drop_type(txn: &mut dyn Txn, tenant: u64, name: &str) {
    txn.delete(&record::type_key(tenant, name));
}

/// Every extension this tenant has installed, by name, in name order.
///
/// One prefix scan. The **available** set is a property of the build and is not stored — which of
/// them is installed is the only part that is state.
pub fn installed_extensions(txn: &dyn Txn, tenant: u64) -> Result<Vec<(String, String)>> {
    let (start, end) = record::extension_range(tenant);
    let mut installed = Vec::new();
    for (key, value) in txn.scan(&start, &end, 0)? {
        installed.push((
            record::extension_name_of(tenant, &key)?,
            record::decode_extension(&value)?,
        ));
    }
    Ok(installed)
}

/// `record::fk_backref_key`: the parent comes first so that "who references me" is a prefix scan
/// rather than a scan of every table in the catalog.
#[must_use]
pub fn foreign_key_backref_key(tenant: u64, parent: u64, child: u64) -> Vec<u8> {
    record::fk_backref_key(tenant, parent, child)
}

/// Every child of one parent: the range [`foreign_key_backref_key`] writes into.
#[must_use]
pub fn foreign_key_backref_range(tenant: u64, parent: u64) -> (Vec<u8>, Vec<u8>) {
    record::fk_backref_range(tenant, parent)
}

/// The child id out of a key [`foreign_key_backref_key`] wrote.
pub fn foreign_key_backref_child(tenant: u64, parent: u64, key: &[u8]) -> Result<u64> {
    record::fk_backref_child(tenant, parent, key)
}

/// Writes a new table, its name, and its indexes' names, and bumps the catalog version.
///
/// The duplicate check is the same composition `crate::backend` documents for a unique index: read
/// the name inside the transaction and require it absent, then write it. That catches a name that
/// is already committed; a *concurrent* `CREATE TABLE` of the same name is caught by the write
/// conflict on the same key, and one of the two loses at commit.
pub fn create_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    let names = [&table.name, &table.primary_key_name]
        .into_iter()
        .chain(table.indexes.iter().map(|index| &index.name));
    for name in names {
        if txn.get(&record::name_key(tenant, name))?.is_some() {
            return Err(SqlError::DuplicateTable(name.clone()));
        }
    }
    write_table(txn, tenant, table)?;
    bump_version(txn)
}

/// Rewrites a table that already exists — `CREATE INDEX` and `DROP INDEX` — and bumps the version.
///
/// `previous` is what the table looked like before, so names that are no longer in use are
/// removed. Passing the wrong one would leave a name pointing at an index that is gone.
pub fn replace_table(
    txn: &mut dyn Txn,
    tenant: u64,
    previous: &TableDef,
    table: &TableDef,
) -> Result<()> {
    for index in &previous.indexes {
        if !table.indexes.iter().any(|kept| kept.id == index.id) {
            txn.delete(&record::name_key(tenant, &index.name));
        }
    }
    for index in &table.indexes {
        if !previous.indexes.iter().any(|had| had.id == index.id)
            && txn.get(&record::name_key(tenant, &index.name))?.is_some()
        {
            return Err(SqlError::DuplicateTable(index.name.clone()));
        }
    }
    write_table(txn, tenant, table)?;
    bump_version(txn)
}

/// Removes a table, its name and its indexes' names, and bumps the version. The table's *rows* are
/// the caller's to delete; this is the catalog only.
pub fn drop_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    txn.delete(&record::table_key(tenant, table.id));
    // A sequence owned by a column goes with the column. PostgreSQL does the same and says so:
    // `DROP SEQUENCE` on an owned one is `2BP01` naming the table that depends on it, and a
    // `DROP TABLE` takes it without being asked.
    drop_sequences(txn, tenant, table);
    // A relation id is never reused, so an orphan override could not be mistaken for another
    // table's -- but it would sit in the collector's scan of every override for ever.
    clear_table_retention(txn, tenant, table.id);
    txn.delete(&record::row_id_key(tenant, table.id));
    txn.delete(&record::name_key(tenant, &table.name));
    if !table.primary_key_name.is_empty() {
        txn.delete(&record::name_key(tenant, &table.primary_key_name));
    }
    for index in &table.indexes {
        txn.delete(&record::name_key(tenant, &index.name));
    }
    bump_version(txn)
}

fn write_table(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    txn.put(
        &record::table_key(tenant, table.id),
        &record::encode_table(table)?,
    );
    txn.put(
        &record::name_key(tenant, &table.name),
        &record::encode_relation(&Relation::Table { table_id: table.id }),
    );
    // The primary key constraint's name is a relation name and has to be taken, even though there
    // is no index behind it -- the row key is the primary key. PostgreSQL answers `42P07` to
    // `CREATE INDEX t_pkey ON t (a)` and so must this.
    // A table with no declared key has no constraint and so takes no name: `CREATE TABLE t (a
    // int8); CREATE INDEX t_pkey ON t (a);` succeeds on a real server.
    if !table.primary_key_name.is_empty() {
        txn.put(
            &record::name_key(tenant, &table.primary_key_name),
            &record::encode_relation(&Relation::PrimaryKey { table_id: table.id }),
        );
    }
    for index in &table.indexes {
        txn.put(
            &record::name_key(tenant, &index.name),
            &record::encode_relation(&Relation::Index {
                table_id: table.id,
                index_id: index.id,
            }),
        );
    }
    Ok(())
}

/// Moves one index to a new state, bumping the table's schema version.
///
/// **One step is all a caller may take**, and the check is here rather than in the caller because
/// this is the primitive every path goes through — the job, and a test staging a state by hand.
/// Two steps at once is exactly the two-version invariant broken (ADR 0020): a node whose snapshot
/// predates the write would be two states behind a node whose snapshot follows it, and no pair of
/// states two apart is safe together.
///
/// It bumps `schema_version`, which is what makes the transition a *table* event that
/// `IndexDef::state_since` can be compared against — the cluster-wide catalog version says only
/// that something moved.
pub fn advance_index_state(
    txn: &mut dyn Txn,
    tenant: u64,
    table: &TableDef,
    index_id: u64,
    to: SchemaState,
) -> Result<TableDef> {
    let next_version = table.schema_version + 1;
    let mut updated = table.clone();
    updated.schema_version = next_version;
    let index = updated
        .indexes
        .iter_mut()
        .find(|index| index.id == index_id)
        .ok_or_else(|| {
            SqlError::Internal(format!(
                "index {index_id} is not on table \"{}\"",
                table.name
            ))
        })?;
    // **One step, in either direction.** Forwards is a change being added and backwards is one
    // being removed — or one being unwound after it failed — and both need the same rule: a node
    // one step behind must never be two, whichever way the cluster is moving.
    //
    // Idempotent for the state it is already in, so a job that crashed after writing and before
    // recording resumes rather than failing.
    let one_step = index.state == to
        || index.state.forward() == Some(to)
        || index.state.backward() == Some(to);
    if !one_step {
        return Err(SqlError::Internal(format!(
            "index \"{}\" cannot go from {} to {} in one step",
            index.name,
            index.state.name(),
            to.name()
        )));
    }
    index.state = to;
    index.state_since = next_version;
    replace_table(txn, tenant, table, &updated)?;
    Ok(updated)
}

/// A schema-change job in flight: which table, how far its backfill got, and whether it finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// The index being built.
    pub index_id: u64,
    /// The table it is on.
    pub table_id: u64,
    /// The row key the next batch starts at. Empty means "from the beginning".
    ///
    /// **Durable, and that is the point.** A backfill is many small transactions, and a node that
    /// dies mid-job must resume rather than restart — on a large table, restarting is how a job
    /// never finishes.
    pub cursor: Vec<u8>,
    /// Whether the backfill has reached the end of the table.
    pub done: bool,
    /// Whether this change is **removing** the index rather than adding it.
    ///
    /// The states run backwards for a removal — `public → write-only → delete-only → absent` — and
    /// a removing step waits the retention window on top of the ordinary interval, because what it
    /// has to outlast is a *reader* rather than a writer.
    pub removing: bool,
}

/// Records a job, or moves its cursor on.
pub fn put_job(txn: &mut dyn Txn, tenant: u64, job: &JobRecord) {
    txn.put(
        &record::job_key(tenant, job.index_id),
        &record::encode_job(job.table_id, &job.cursor, job.done, job.removing),
    );
}

/// One job, or `None`.
pub fn job(txn: &dyn Txn, tenant: u64, index_id: u64) -> Result<Option<JobRecord>> {
    let Some(bytes) = txn.get(&record::job_key(tenant, index_id))? else {
        return Ok(None);
    };
    let (table_id, cursor, done, removing) = record::decode_job(&bytes)?;
    Ok(Some(JobRecord {
        index_id,
        table_id,
        cursor,
        done,
        removing,
    }))
}

/// Forgets a job. Called when it reaches `public`, or when it unwinds.
pub fn drop_job(txn: &mut dyn Txn, tenant: u64, index_id: u64) {
    txn.delete(&record::job_key(tenant, index_id));
}

/// The `[start, end)` key range holding one tenant's jobs, in index-id order.
#[must_use]
pub fn job_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    record::job_range(tenant)
}

/// One listed job, out of its key and its value.
pub fn decode_job(tenant: u64, key: &[u8], value: &[u8]) -> Result<JobRecord> {
    let index_id = record::job_index_id(tenant, key)?;
    let (table_id, cursor, done, removing) = record::decode_job(value)?;
    Ok(JobRecord {
        index_id,
        table_id,
        cursor,
        done,
        removing,
    })
}

/// A flashback in progress: where it is putting the table back to, and how far it has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashbackRecord {
    /// The table being put back.
    pub table_id: u64,
    /// The snapshot it is being put back to.
    pub target_ts: u64,
    /// The row key the next batch starts at. Empty means "from the beginning".
    pub cursor: Vec<u8>,
    /// How many rows have been changed so far, for the answer the statement gives.
    pub changed: u64,
}

/// Records a flashback, or moves its cursor on.
pub fn put_flashback(txn: &mut dyn Txn, tenant: u64, record: &FlashbackRecord) {
    txn.put(
        &record::flashback_key(tenant, record.table_id),
        &record::encode_flashback(record.target_ts, &record.cursor, record.changed),
    );
}

/// The flashback in progress on one table, or `None`.
pub fn flashback(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Option<FlashbackRecord>> {
    let Some(bytes) = txn.get(&record::flashback_key(tenant, table_id))? else {
        return Ok(None);
    };
    let (target_ts, cursor, changed) = record::decode_flashback(&bytes)?;
    Ok(Some(FlashbackRecord {
        table_id,
        target_ts,
        cursor,
        changed,
    }))
}

/// Forgets a flashback, when it has finished.
pub fn drop_flashback(txn: &mut dyn Txn, tenant: u64, table_id: u64) {
    txn.delete(&record::flashback_key(tenant, table_id));
}

/// How many columnar replicas a table wants, or `None` when nothing has been said about it.
///
/// [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5. `None` and
/// `Some(0)` mean the same thing to a reader — no columnar copy — and are kept apart only so that
/// a table somebody explicitly turned off is distinguishable from one nobody ever turned on.
pub fn table_columnar_replicas(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Option<u8>> {
    match txn.get(&record::columnar_key(tenant, table_id))? {
        Some(bytes) => Ok(Some(record::decode_columnar_replicas(&bytes)?)),
        None => Ok(None),
    }
}

/// The published row schema for a table that wants a columnar copy.
///
/// What a store below this crate needs to turn a committed row into typed columns: the schema
/// version it is as of, and a `(type, missing)` pair per column — the two halves
/// `esker_keys::row::RowSchema` is built from.
///
/// **Types alone would be silently wrong.** `decode_row` pads a row written before a column
/// existed with that column's missing value; a decoder given only types builds
/// `RowSchema::nullable` and reads NULL where the row store reads the default, for exactly the
/// rows that predate the `ALTER`.
pub fn table_published_schema(
    txn: &dyn Txn,
    tenant: u64,
    table_id: u64,
) -> Result<Option<PublishedSchema>> {
    match txn.get(&record::columnar_key(tenant, table_id))? {
        Some(bytes) => Ok(Some(record::decode_columnar(&bytes)?.1)),
        None => Ok(None),
    }
}

/// How a table's rows decode, published for a layer that cannot ask this crate.
///
/// The type and its codec are [`esker_keys::columnar::Published`]: a store holding a columnar
/// learner reads this record and cannot link `esker-sql`
/// ([ADR 0030](../../../docs/adr/0030-the-row-codec-moves-down.md)). Re-exported rather than
/// wrapped, so there is one definition of what the bytes mean and not two.
pub use esker_keys::columnar::Published as PublishedSchema;

/// Sets how many columnar replicas a table wants.
///
/// **This does not bump the catalog version**, for the same reason a retention override does not:
/// it changes nothing about how a row is written or read, so no node caching a `TableDef` is
/// stale because of it, and ADR 0020's two-version invariant has nothing to say about it. What
/// acts on it is the placement driver, which is not a reader of rows.
pub fn set_table_columnar_replicas(
    txn: &mut dyn Txn,
    tenant: u64,
    table: &TableDef,
    replicas: u8,
) -> Result<()> {
    txn.put(
        &record::columnar_key(tenant, table.id),
        &record::encode_columnar(replicas, Some(table))?,
    );
    Ok(())
}

/// Rewrites the published schema of a table that has one, leaving its replica count alone.
///
/// Called by every `ALTER` that changes what a row decodes to, **in that `ALTER`'s own
/// transaction**. That is the whole ordering guarantee a learner gets: the schema it needs to
/// read rows written after the `ALTER` is committed by the same transaction that made those rows
/// possible, so it can never be published later than the first row that needs it.
///
/// A no-op for a table nobody has asked for a columnar copy of, which is why every DDL path can
/// call it unconditionally.
pub fn refresh_published_schema(txn: &mut dyn Txn, tenant: u64, table: &TableDef) -> Result<()> {
    let key = record::columnar_key(tenant, table.id);
    let Some(bytes) = txn.get(&key)? else {
        return Ok(());
    };
    let replicas = record::decode_columnar_replicas(&bytes)?;
    txn.put(&key, &record::encode_columnar(replicas, Some(table))?);
    Ok(())
}

/// Forgets the setting entirely, which reads back the same as zero.
pub fn clear_table_columnar_replicas(txn: &mut dyn Txn, tenant: u64, table_id: u64) {
    txn.delete(&record::columnar_key(tenant, table_id));
}

/// The `[start, end)` key range holding one tenant's columnar settings, in table-id order.
///
/// One scan of it is the whole map, which is what a placement driver wants: it reads every
/// table's wish once rather than asking per table.
#[must_use]
pub fn columnar_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    record::columnar_range(tenant)
}

/// One listed setting, out of its key and its value.
pub fn decode_columnar(tenant: u64, key: &[u8], value: &[u8]) -> Result<(u64, u8)> {
    Ok((
        record::columnar_table_id(tenant, key)?,
        record::decode_columnar_replicas(value)?,
    ))
}

/// Sets the retention override for one table, in milliseconds.
///
/// It does **not** bump the catalog version, and that is deliberate. Retention changes nothing
/// about how a row is written or read — it is a housekeeping bound the collector reads directly
/// from the store — so bumping the version would make every node in the cluster throw away its
/// table cache to learn a number none of them uses. The collector picks the change up on its next
/// pass, which is the only place it matters.
pub fn set_table_retention(txn: &mut dyn Txn, tenant: u64, table_id: u64, retention_ms: u64) {
    txn.put(
        &record::retention_key(tenant, table_id),
        &record::encode_retention(retention_ms),
    );
}

/// Removes a table's override, putting it back on the cluster default.
pub fn clear_table_retention(txn: &mut dyn Txn, tenant: u64, table_id: u64) {
    txn.delete(&record::retention_key(tenant, table_id));
}

/// One table's override, or `None` when it takes the cluster default.
pub fn table_retention(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Option<u64>> {
    match txn.get(&record::retention_key(tenant, table_id))? {
        Some(bytes) => Ok(Some(record::decode_retention(&bytes)?)),
        None => Ok(None),
    }
}

/// Sets the cluster's default retention, in milliseconds.
pub fn set_default_retention(txn: &mut dyn Txn, retention_ms: u64) {
    txn.put(
        &record::default_retention_key(),
        &record::encode_retention(retention_ms),
    );
}

/// The cluster's default retention, or [`DEFAULT_RETENTION_MS`] when nobody has set one.
pub fn default_retention(txn: &dyn Txn) -> Result<u64> {
    match txn.get(&record::default_retention_key())? {
        Some(bytes) => record::decode_retention(&bytes),
        None => Ok(DEFAULT_RETENTION_MS),
    }
}

/// Names a timestamp: one record, and nothing else.
///
/// A checkpoint is **free** ([ADR 0021](../../../docs/adr/0021-time-machine.md) Decision 3) — no
/// snapshot, no copy, no flush — because the data it refers to is kept by retention whether
/// anybody named it or not. Re-using a name replaces it, which is what `pg_export_snapshot()`'s
/// named variant should do: a name is a label a user moves, not a unique key they have to free.
pub fn set_checkpoint(txn: &mut dyn Txn, tenant: u64, name: &str, start_ts: u64) {
    txn.put(
        &record::checkpoint_key(tenant, name),
        &record::encode_checkpoint(start_ts),
    );
}

/// The timestamp a checkpoint names, or `None` if there is no such checkpoint.
pub fn checkpoint_at(txn: &dyn Txn, tenant: u64, name: &str) -> Result<Option<u64>> {
    match txn.get(&record::checkpoint_key(tenant, name))? {
        Some(bytes) => Ok(Some(record::decode_checkpoint(&bytes)?)),
        None => Ok(None),
    }
}

/// Forgets a checkpoint. The versions it named are retention's business and are not touched.
pub fn drop_checkpoint(txn: &mut dyn Txn, tenant: u64, name: &str) {
    txn.delete(&record::checkpoint_key(tenant, name));
}

/// The `[start, end)` key range holding one tenant's checkpoints, in name order.
#[must_use]
pub fn checkpoint_range(tenant: u64) -> (Vec<u8>, Vec<u8>) {
    record::checkpoint_range(tenant)
}

/// One listed checkpoint: the name out of its key, and the timestamp out of its value.
pub fn decode_checkpoint(tenant: u64, key: &[u8], value: &[u8]) -> Result<(String, u64)> {
    Ok((
        record::checkpoint_name(tenant, key)?,
        record::decode_checkpoint(value)?,
    ))
}

/// Writes one sequence and takes its name, which lives in the same namespace as tables and
/// indexes — `CREATE TABLE t_id_seq` after a `bigserial` is `42P07` on a real server.
pub fn create_sequence(txn: &mut dyn Txn, tenant: u64, sequence: &SequenceDef) -> Result<()> {
    if txn
        .get(&record::name_key(tenant, &sequence.name))?
        .is_some()
    {
        return Err(SqlError::DuplicateTable(sequence.name.clone()));
    }
    txn.put(
        &record::sequence_key(tenant, sequence.table_id, sequence.id),
        &record::encode_sequence(sequence),
    );
    txn.put(
        &record::name_key(tenant, &sequence.name),
        &record::encode_relation(&Relation::Sequence {
            table_id: sequence.table_id,
            sequence_id: sequence.id,
        }),
    );
    // **The catalog version, without which the name is invisible.** Every node caches what names
    // resolve to and the cache is keyed by this counter; a `CREATE TABLE` bumped it on the way out
    // through `write_table`, and a sequence no column owns writes no table record at all, so
    // nothing bumped it and `nextval` on the sequence just created answered `42P01`.
    bump_version(txn)
}

/// Overwrites one sequence's record in place, keeping its name entry as it is.
///
/// The name is not rewritten because it did not change: this exists for moving which column a
/// sequence fills, which is what `ALTER COLUMN … SET DEFAULT nextval(…)` does to two sequences at
/// once — the one that filled the column stops, and the named one starts.
pub fn replace_sequence(txn: &mut dyn Txn, tenant: u64, sequence: &SequenceDef) {
    txn.put(
        &record::sequence_key(tenant, sequence.table_id, sequence.id),
        &record::encode_sequence(sequence),
    );
}

/// `pg_get_triggerdef(oid)` — a trigger's `CREATE TRIGGER`, re-printed.
///
/// **`EXECUTE PROCEDURE` normalises to `EXECUTE FUNCTION`.** The two are one clause and only the
/// second is ever printed, so a client that round-trips a schema gets the newer spelling whichever
/// it wrote — which matters because statement 762 writes the older one.
#[must_use]
pub fn trigger_definition(relations: &pg_relations::Relations, oid: Option<i64>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    for relation in relations.rows() {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        for (at, trigger) in table.triggers.iter().enumerate() {
            if pg_catalog::trigger_oid(table.id, at) != oid {
                continue;
            }
            let mut events = Vec::new();
            if trigger.events & 4 != 0 {
                events.push("INSERT");
            }
            if trigger.events & 8 != 0 {
                events.push("DELETE");
            }
            if trigger.events & 16 != 0 {
                events.push("UPDATE");
            }
            return Datum::Text(format!(
                "CREATE TRIGGER {} {} {} ON public.{} FOR EACH {} EXECUTE FUNCTION {}()",
                trigger.name,
                if trigger.before { "BEFORE" } else { "AFTER" },
                events.join(" OR "),
                table.name,
                if trigger.for_each_row {
                    "ROW"
                } else {
                    "STATEMENT"
                },
                trigger.function
            ));
        }
    }
    Datum::Null
}

/// Stores a function, replacing one of the same name.
///
/// **`CREATE OR REPLACE FUNCTION` run twice is a plain success**, not `42710` — measured, and
/// unlike a second `CREATE TRIGGER` of the same name, which *is* a duplicate. `OR REPLACE` exists
/// for the function and not for the trigger.
pub fn write_function(txn: &mut dyn Txn, tenant: u64, function: &FunctionDef) -> Result<()> {
    txn.put(
        &record::function_key(tenant, &function.name),
        &record::encode_function(function),
    );
    bump_version(txn)
}

/// One function by name, or `None`.
pub fn function(txn: &dyn Txn, tenant: u64, name: &str) -> Result<Option<FunctionDef>> {
    let Some(bytes) = txn.get(&record::function_key(tenant, name))? else {
        return Ok(None);
    };
    record::decode_function(&bytes, name.to_owned()).map(Some)
}
/// Every schema this tenant has **created**, in name order.
///
/// `public` is not among them: it is a property of the build, the way the available extensions
/// are, and a tenant that has created nothing still has it. Callers that want the whole list ask
/// [`schema_names`].
pub fn schemas(txn: &dyn Txn, tenant: u64) -> Result<Vec<(String, u64)>> {
    let (start, end) = record::schema_range(tenant);
    let mut out = Vec::new();
    for (key, value) in txn.scan(&start, &end, u32::MAX)? {
        let name = record::schema_name_of(tenant, &key)?;
        out.push((name, record::decode_schema(&value)?));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Every schema a name can resolve in, `public` first — which is the order `pg_namespace` and
/// `schema_names` report.
pub fn schema_names(txn: &dyn Txn, tenant: u64) -> Result<Vec<(String, u64)>> {
    let mut out = vec![(PUBLIC_SCHEMA.to_owned(), PUBLIC_SCHEMA_ID)];
    out.extend(schemas(txn, tenant)?);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Whether a schema exists, `public` included.
pub fn schema_exists(txn: &dyn Txn, tenant: u64, name: &str) -> Result<bool> {
    if name == PUBLIC_SCHEMA {
        return Ok(true);
    }
    Ok(txn.get(&record::schema_key(tenant, name))?.is_some())
}

/// Records a schema. The caller has already decided it is not there.
pub fn create_schema(txn: &mut dyn Txn, tenant: u64, name: &str, id: u64) -> Result<()> {
    txn.put(
        &record::schema_key(tenant, name),
        &record::encode_schema(id),
    );
    bump_version(txn)
}

/// Removes one. The caller has already decided what depends on it.
pub fn drop_schema(txn: &mut dyn Txn, tenant: u64, name: &str) -> Result<()> {
    txn.delete(&record::schema_key(tenant, name));
    bump_version(txn)
}

/// The one schema every tenant has.
pub const PUBLIC_SCHEMA: &str = "public";

/// What separates a schema from a relation name **as stored**.
///
/// **A NUL, not a dot.** A dot is ambiguous: `schema_test.rb`'s own `setup` creates
/// `test_schema."things.table"`, so `"a.b.c"` could be the relation `b.c` in schema `a` or the
/// relation `c` in schema `a.b`. A NUL cannot appear in a PostgreSQL identifier at all, so it
/// separates the two without a length prefix — which is what lets the name record's key keep its
/// shape, where "the name is the whole tail" is the property the scan relies on.
///
/// **A relation in `public` is stored with no separator at all**, so every key and every record
/// written before schemas existed reads back unchanged and every answer about `public` is
/// byte-identical to what it was.
pub const SCHEMA_SEPARATOR: char = '\0';

/// The stored name of a relation: bare in `public`, `schema ++ NUL ++ name` anywhere else.
#[must_use]
pub fn qualify(schema: &str, name: &str) -> String {
    if schema == PUBLIC_SCHEMA {
        return name.to_owned();
    }
    format!("{schema}{SCHEMA_SEPARATOR}{name}")
}

/// The schema and the bare name out of a stored one.
#[must_use]
pub fn split_qualified(stored: &str) -> (&str, &str) {
    match stored.split_once(SCHEMA_SEPARATOR) {
        Some((schema, name)) => (schema, name),
        None => (PUBLIC_SCHEMA, stored),
    }
}

/// A stored name as a **message** spells it: `schema.name`, or the bare name in `public`.
///
/// The separator is a NUL on disk and a dot in a sentence, because that is what PostgreSQL quotes
/// back: `42P01 relation "nosuchschema.t" does not exist`, with the schema **inside** the quotes.
#[must_use]
pub fn display_name(stored: &str) -> String {
    match stored.split_once(SCHEMA_SEPARATOR) {
        Some((schema, name)) => format!("{schema}.{name}"),
        None => stored.to_owned(),
    }
}

/// A name **as a user wrote it** — `schema.relation` — turned into the stored form.
///
/// This is `::regclass`'s input and nothing else: everywhere else a qualified name arrives already
/// split by the parser, which knows which halves were quoted. Here it is one string, so the rule is
/// PostgreSQL's own for an unquoted one — the first dot separates — and the two schemas this node
/// spells *into* a name keep theirs:
///
/// * `pg_catalog.x` is the relation `x`, which is how it is stored;
/// * `information_schema.x` **is** the stored name, dot and all (`catalog::information_schema`).
///
/// A name carrying a quote is left whole, because splitting `test_schema."things.table"` correctly
/// needs the parser and `::regclass` does not have it. That is a gap in one spelling of one cast,
/// and it answers `42P01` rather than the wrong relation.
#[must_use]
pub fn parse_qualified(written: &str) -> String {
    if written.contains('"') {
        return written.to_owned();
    }
    let Some((schema, name)) = written.split_once('.') else {
        return written.to_owned();
    };
    if schema.eq_ignore_ascii_case("information_schema") {
        return written.to_owned();
    }
    if schema.eq_ignore_ascii_case("pg_catalog") {
        return name.to_owned();
    }
    qualify(schema, name)
}

/// Every relation stored in one schema, as stored names.
///
/// A prefix scan of the name records: `schema ++ NUL` is a prefix no other schema's names share,
/// which is what makes `DROP SCHEMA … CASCADE` a range rather than a filter over everything.
pub fn relations_in_schema(txn: &dyn Txn, tenant: u64, schema: &str) -> Result<Vec<String>> {
    if schema == PUBLIC_SCHEMA {
        // `public`'s names have no prefix to scan for — they are every name without a separator.
        let (start, end) = record::name_range(tenant);
        let mut out = Vec::new();
        for (key, _) in txn.scan(&start, &end, u32::MAX)? {
            let name = record::name_of(tenant, &key)?;
            if !name.contains(SCHEMA_SEPARATOR) {
                out.push(name);
            }
        }
        return Ok(out);
    }
    let prefix = format!("{schema}{SCHEMA_SEPARATOR}");
    let (start, end) = record::name_range(tenant);
    let mut out = Vec::new();
    for (key, _) in txn.scan(&start, &end, u32::MAX)? {
        let name = record::name_of(tenant, &key)?;
        if name.starts_with(&prefix) {
            out.push(name);
        }
    }
    Ok(out)
}

/// `public`'s oid, which a real server also fixes rather than allocating.
pub const PUBLIC_SCHEMA_ID: u64 = 11;

/// Every stored function of one tenant, in name order.
pub fn functions(txn: &dyn Txn, tenant: u64) -> Result<Vec<FunctionDef>> {
    let (start, end) = record::function_range(tenant);
    let mut out = Vec::new();
    for (key, value) in txn.scan(&start, &end, u32::MAX)? {
        let name = record::function_name_of(tenant, &key)?;
        out.push(record::decode_function(&value, name)?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Removes one function.
pub fn drop_function(txn: &mut dyn Txn, tenant: u64, name: &str) -> Result<()> {
    txn.delete(&record::function_key(tenant, name));
    bump_version(txn)
}

/// One sequence by the pair its name resolves to, read straight from its record.
///
/// Not through the table: a sequence no column owns is filed under
/// [`STANDALONE_SEQUENCE_OWNER`], which has no `TableDef` behind it, and going via one would make
/// every unowned sequence unreachable.
pub fn sequence_by_id(
    txn: &dyn Txn,
    tenant: u64,
    table_id: u64,
    sequence_id: u64,
) -> Result<Option<SequenceDef>> {
    let Some(bytes) = txn.get(&record::sequence_key(tenant, table_id, sequence_id))? else {
        return Ok(None);
    };
    // The key ordinal only matters for a record written before version 15, and one of those is
    // keyed by the column it fills — which is the id read back here.
    let key_column = usize::try_from(sequence_id).unwrap_or(0);
    record::decode_sequence(&bytes, table_id, key_column).map(Some)
}

/// How each of this table's children lines up with it, for a scan that has to return both.
///
/// One record read per child and no further: the children are decoded directly rather than
/// through the caching loader, so loading a parent cannot walk into loading its parents again.
pub(super) fn child_scans(txn: &dyn Txn, tenant: u64, table: &TableDef) -> Result<Vec<ChildScan>> {
    let mut scans = Vec::with_capacity(table.children.len());
    for &child_id in &table.children {
        let Some(bytes) = txn.get(&record::table_key(tenant, child_id))? else {
            continue;
        };
        let child = record::decode_table(&bytes)?;
        // Every one of this table's columns, found in the child by name. A child that somehow
        // lacks one is skipped rather than guessed at: a projection with a wrong position in it
        // would return another column's values under this one's name.
        let mut project = Vec::with_capacity(table.columns.len());
        for (at, column) in table.columns.iter().enumerate() {
            // **The internal row id is not found by name**, and deliberately: its name is the
            // empty string and `TableDef::column` refuses that, so a user's identifier can never
            // reach it. Matching it positionally is the only way — and without this a parent
            // with no primary key silently lost every child, because the *first* column it
            // looked for was the one name that can never resolve.
            let found = if Some(at) == table.row_id() {
                child.row_id()
            } else {
                child.column(&column.name)
            };
            let Some(found) = found else {
                project.clear();
                break;
            };
            project.push(found);
        }
        if project.len() != table.columns.len() {
            continue;
        }
        scans.push(ChildScan {
            table_id: child_id,
            schema: child.row_schema(),
            project,
        });
    }
    Ok(scans)
}

/// A child's inherited sequences: the ones filling a parent's column, remapped to this table's
/// own ordinals.
///
/// **The child draws from the parent's counter, not from one of its own.** Measured: `ic`'s `id`
/// defaults to `nextval('ip_id_seq'::regclass)`, the parent's, so rows inserted through either
/// table take numbers from the same sequence and cannot collide. Copying the *record* would give
/// the child a second counter and two rows the same id.
///
/// The record stays the parent's — `SequenceDef::table_id` is untouched — so `DROP SEQUENCE` still
/// reports the parent's column as the dependent and the child cannot drop what it borrowed. Only
/// the in-memory list a `TableDef` carries gains an entry, which is what the writer reads to fill
/// a column and what `pg_attrdef` reads to print the default.
pub(super) fn inherited_sequences(
    txn: &dyn Txn,
    tenant: u64,
    table: &TableDef,
    parents: &[u64],
) -> Result<Vec<SequenceDef>> {
    let mut inherited = Vec::new();
    for &parent_id in parents {
        let Some(bytes) = txn.get(&record::table_key(tenant, parent_id))? else {
            continue;
        };
        let parent = record::decode_table(&bytes)?;
        for sequence in table_sequences(txn, tenant, parent_id)? {
            let Some(at) = sequence.column else {
                continue;
            };
            // By **name**, because the child's ordinals are its own: a child with no primary key
            // carries an internal row id at position 0 and every inherited column sits one later.
            let Some(name) = parent.columns.get(at).map(|column| &column.name) else {
                continue;
            };
            let Some(mine) = table.column(name) else {
                continue;
            };
            inherited.push(SequenceDef {
                column: Some(mine),
                ..sequence
            });
        }
    }
    Ok(inherited)
}

/// Every sequence one table owns, in column order.
///
/// A short scan of one prefix rather than a read per column: a table with no sequences costs one
/// empty read, which is what nearly every table costs.
pub fn table_sequences(txn: &dyn Txn, tenant: u64, table_id: u64) -> Result<Vec<SequenceDef>> {
    let (start, end) = record::table_sequence_range(tenant, table_id);
    let mut out = Vec::new();
    // A table cannot have more columns than a row can hold, so one page is every sequence it has.
    for (key, value) in txn.scan(&start, &end, u32::MAX)? {
        let id = record::sequence_id_of(tenant, table_id, &key)?;
        // A record written before version 15 was keyed by the column it filled, so for those the
        // id read out of the key *is* that ordinal. `decode_sequence` uses it only in that case.
        let key_column = usize::try_from(id).unwrap_or(0);
        out.push(record::decode_sequence(&value, table_id, key_column)?);
    }
    out.sort_by_key(|sequence| sequence.column);
    Ok(out)
}

/// Removes **one** sequence: its record, its name and its counter.
///
/// The column's default goes with it and nothing else does, because the default *is* the sequence
/// — `pg_attrdef` reports one for a column that owns a `nextval` and nothing for a column that
/// does not, so deleting the record removes both facts at once. Measured: after
/// `DROP SEQUENCE … CASCADE` a real server has no `pg_attrdef` row for the table and the column is
/// still there.
pub fn drop_sequence(txn: &mut dyn Txn, tenant: u64, table_id: u64, sequence: &SequenceDef) {
    txn.delete(&record::sequence_key(tenant, table_id, sequence.id));
    txn.delete(&record::name_key(tenant, &sequence.name));
    txn.delete(&record::sequence_value_key(tenant, sequence.id));
}

/// Removes one table's sequences: their records, their names and their counters.
fn drop_sequences(txn: &mut dyn Txn, tenant: u64, table: &TableDef) {
    for sequence in &table.sequences {
        txn.delete(&record::sequence_key(tenant, table.id, sequence.id));
        txn.delete(&record::name_key(tenant, &sequence.name));
        txn.delete(&record::sequence_value_key(tenant, sequence.id));
    }
}

/// Reserves `count` consecutive values of one sequence and answers with the first.
///
/// The same shape and the same trade as [`allocate_row_ids`], and for a user-visible counter
/// rather than a hidden one — which is why the gaps it leaves are documented on
/// [`SEQUENCE_BATCH`] rather than merely tolerated. Called in a transaction of its own, which is
/// also what makes `nextval` **non-transactional**: a rolled-back `INSERT` has still consumed its
/// value, here as there, measured on both.
pub fn allocate_sequence_values(
    txn: &mut dyn Txn,
    tenant: u64,
    sequence_id: u64,
    count: u64,
) -> Result<i64> {
    let key = record::sequence_value_key(tenant, sequence_id);
    let next = match txn.get(&key)? {
        Some(bytes) => record::decode_sequence_counter(&bytes)?.0,
        // A sequence starts at 1, which is PostgreSQL's `START WITH` default.
        None => 1,
    };
    let after = next.checked_add(count).ok_or(SqlError::BigintOutOfRange)?;
    // Handing a value out is what `is_called` means, so it is true from here on whatever it was.
    txn.put(&key, &record::encode_sequence_counter(after, true));
    i64::try_from(next).map_err(|_| SqlError::BigintOutOfRange)
}

/// `setval`: the next value the sequence will hand out.
///
/// Whole values only, and the value written is what `nextval` answers **next** — so `setval(s, 5)`
/// with `is_called` true stores 6 and `setval(s, 5, false)` stores 5. The caller does that sum,
/// because it is the one place the two spellings differ and it belongs with the function that
/// reads them.
pub fn set_sequence_value(
    txn: &mut dyn Txn,
    tenant: u64,
    sequence_id: u64,
    next: u64,
    is_called: bool,
) {
    txn.put(
        &record::sequence_value_key(tenant, sequence_id),
        &record::encode_sequence_counter(next, is_called),
    );
}

/// What `SELECT last_value, is_called FROM <sequence>` answers.
///
/// **`last_value` is not the counter**: the counter is the value that will be handed out *next*, so
/// a sequence that has called reports the one before it and a sequence that has not reports the
/// counter itself. That is the whole of the difference between `setval(s, 5, true)` and
/// `setval(s, 5, false)` — the first reports 5 and hands out 6, the second reports 5 and hands out
/// 5 — and it is why the flag is stored rather than derived.
pub fn sequence_state(txn: &dyn Txn, tenant: u64, sequence_id: u64) -> Result<(i64, bool)> {
    let key = record::sequence_value_key(tenant, sequence_id);
    let (next, is_called) = match txn.get(&key)? {
        Some(bytes) => record::decode_sequence_counter(&bytes)?,
        None => (1, false),
    };
    let last = if is_called {
        next.saturating_sub(1)
    } else {
        next
    };
    Ok((i64::try_from(last).unwrap_or(i64::MAX), is_called))
}

/// Reserves `count` consecutive row ids for one table and answers with the first.
///
/// **This is called in a transaction of its own, not the statement's**, and the reason is the
/// whole design. The counter is one key per table; if every `INSERT` bumped it inside its own
/// transaction then two concurrent inserts into the same table would conflict on that key and one
/// would always lose — a table with no primary key would accept one writer at a time. So a session
/// reserves a batch ([`ROW_ID_BATCH`]) in a short transaction of its own and hands ids out from
/// memory.
///
/// The consequence is **gaps**, and they are not a defect: ids reserved by a statement that rolls
/// back, or by a session that ends, are never handed out. That is exactly what a PostgreSQL
/// sequence does — `nextval` is non-transactional and a rolled-back `INSERT` still consumed its
/// value — so a user who knows PostgreSQL already expects it. Nothing user-visible depends on the
/// ids being dense; they are the row's identity and never its data.
pub fn allocate_row_ids(txn: &mut dyn Txn, tenant: u64, table_id: u64, count: u64) -> Result<u64> {
    let key = record::row_id_key(tenant, table_id);
    let next = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        // Ids start at 1, so 0 is never a row.
        None => 1,
    };
    let after = next
        .checked_add(count)
        .ok_or_else(|| SqlError::Internal("the row id sequence is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(after));
    Ok(next)
}

/// Takes the next relation id for a tenant. Ids start at 1, so 0 is never a real relation.
pub fn allocate_id(txn: &mut dyn Txn, tenant: u64) -> Result<u64> {
    let key = record::next_id_key(tenant);
    let next = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        None => 1,
    };
    let after = next
        .checked_add(1)
        .ok_or_else(|| SqlError::Internal("the relation id sequence is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(after));
    Ok(next)
}

/// Moves the catalog version forward, which is what makes every node's cache notice.
///
/// Every DDL statement writes this one key, so two concurrent DDL statements conflict and one of
/// them is told to retry. That is the intended serialisation and not a bottleneck worth removing:
/// DDL is rare and a catalog that two statements changed at once is a catalog nobody can reason
/// about.
pub fn bump_version(txn: &mut dyn Txn) -> Result<()> {
    let key = record::version_key();
    let current = match txn.get(&key)? {
        Some(bytes) => record::decode_counter(&bytes)?,
        None => 0,
    };
    let next = current
        .checked_add(1)
        .ok_or_else(|| SqlError::Internal("the catalog version is exhausted".into()))?;
    txn.put(&key, &record::encode_counter(next));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Catalog, ColumnDef, DEFAULT_RETENTION_MS, Identity, IndexDef, IndexKey, KeyOrder, KeyPart,
        MAX_IDENTIFIER_BYTES, RETENTION_FOREVER, Relation, SchemaState, SequenceDef, TableDef,
        allocate_id, clear_table_retention, create_table, default_retention, drop_table,
        fold_identifier, record, replace_table, set_default_retention, set_table_retention,
        table_retention,
    };
    use crate::backend::{Backend, MemoryBackend};
    use crate::sqlstate;
    use crate::value::ColumnType;

    /// The other direction, for a golden that is written down rather than produced.
    fn decode_hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    use crate::value::Datum;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    fn accounts(id: u64) -> TableDef {
        TableDef {
            id,
            persistence: super::Persistence::Permanent,
            name: "accounts".into(),
            comment: None,
            primary_key_comment: None,
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: ColumnType::Int8,
                    typmod: crate::value::NO_TYPMOD,
                    default_expr: None,
                    not_null: true,
                    default: None,
                    missing: None,
                    generated: None,
                    comment: None,
                },
                ColumnDef {
                    name: "email".into(),
                    ty: ColumnType::Text,
                    typmod: crate::value::NO_TYPMOD,
                    default_expr: None,
                    not_null: false,
                    default: None,
                    missing: None,
                    generated: None,
                    comment: None,
                },
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef {
                id: id + 1,
                name: "accounts_email_key".into(),
                unique: true,
                keys: vec![IndexKey::column(1)],
                state: SchemaState::Public,
                state_since: 1,
                include: Vec::new(),
                predicate: None,
                nulls_not_distinct: false,
                constraint: None,
                comment: None,
            }],
            primary_key_name: "accounts_pkey".into(),
            schema_version: 1,
            sequences: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            triggers_disabled: false,
            parents: Vec::new(),
            children: Vec::new(),
            triggers: Vec::new(),
            excludes: Vec::new(),
            child_scans: Vec::new(),
            partition_by: None,
            partition_bound: None,
        }
    }

    /// The golden. A catalog record is an on-disk format like any other, and these bytes are it.
    ///
    /// **The column it fills was in the key and is in the body from version 15**, beside the
    /// column that *owns* it — two facts one ordinal used to conflate, and the key is the id now,
    /// because a column may own more than one sequence.
    #[test]
    fn a_sequence_record_is_a_version_an_id_a_name_and_a_kind() {
        let sequence = SequenceDef {
            id: 9,
            name: "accounts_id_seq".into(),
            table_id: 7,
            column: Some(0),
            owner_column: Some(0),
            identity: Identity::Always,
            start: 1,
            increment: 1,
        };
        let encoded = record::encode_sequence(&sequence);
        assert_eq!(
            hex(&encoded),
            concat!(
                "17",               // catalog format version
                "0900000000000000", // the sequence's own relation id
                // varint 15, "accounts_id_seq" -- the name a real server derives, and a relation
                // name like any other: `CREATE TABLE accounts_id_seq` is `42P07` on both servers.
                "0f6163636f756e74735f69645f736571",
                // GENERATED ALWAYS. The one thing that distinguishes the three identity kinds is
                // what an explicit value does, so it is a byte rather than something derived.
                "02",
                // Version 15. The column it fills and the column that owns it, each as the
                // ordinal **plus one** so that `0` is `None` -- a `bigserial` is both.
                "01",
                "01",
                "0100000000000000", // START 1
                "0100000000000000", // INCREMENT BY 1
            )
        );
        assert_eq!(record::decode_sequence(&encoded, 7, 0).unwrap(), sequence);
    }

    /// **A record written before version 15 reads back as the `bigserial` it could only have
    /// been.** Its key carried the column, its body had no ordinals, and the only sequence that
    /// format could hold both filled and owned that column.
    #[test]
    fn a_version_14_sequence_reads_as_a_bigserial() {
        let mut old = vec![14u8];
        old.extend_from_slice(&9u64.to_le_bytes());
        old.push(15);
        old.extend_from_slice(b"accounts_id_seq");
        old.push(Identity::Always.as_u8());
        let decoded = record::decode_sequence(&old, 7, 3).unwrap();
        assert_eq!(
            decoded.column,
            Some(3),
            "the key's ordinal, which it filled"
        );
        assert_eq!(decoded.owner_column, Some(3), "and owned");
        assert_eq!((decoded.start, decoded.increment), (1, 1));
    }

    /// A sequence's key carries the owner, and its own id reads back out of it.
    #[test]
    fn a_sequence_is_keyed_by_its_own_id() {
        let (start, end) = record::table_sequence_range(1, 7);
        let key = record::sequence_key(1, 7, 3);
        assert!(key >= start && key < end, "the key is inside the range");
        assert_eq!(record::sequence_id_of(1, 7, &key).unwrap(), 3);
        // Another table's sequence is outside this table's range, which is what makes the scan
        // one table's and not the tenant's.
        assert!(!(record::sequence_key(1, 8, 0) < end && record::sequence_key(1, 8, 0) >= start));
    }

    /// An identity byte this crate did not write is corruption rather than a default: guessing
    /// [`Identity::Default`] would let a value into a column a newer node refuses.
    #[test]
    fn an_unknown_identity_byte_is_corruption() {
        assert!(Identity::from_u8(3).is_err());
        let mut encoded = record::encode_sequence(&SequenceDef {
            id: 1,
            name: "s".into(),
            table_id: 1,
            column: Some(0),
            owner_column: Some(0),
            identity: Identity::Default,
            start: 1,
            increment: 1,
        });
        // **The identity byte, by position rather than by `last`.** It was the last byte until
        // version 15 put four fields after it, and corrupting the last one now corrupts the
        // increment — which is a `i64` that takes any bit pattern, so the test would have passed
        // by asserting nothing.
        let identity_at = 1 + 8 + 1 + "s".len();
        assert_eq!(encoded[identity_at], Identity::Default.as_u8());
        encoded[identity_at] = 9;
        assert_eq!(
            record::decode_sequence(&encoded, 1, 0)
                .unwrap_err()
                .sqlstate(),
            sqlstate::DATA_CORRUPTED
        );
    }

    /// An installed extension: a version byte and its version string, and the **key** that says
    /// which extension it is.
    ///
    /// The record has a floor of its own (`OLDEST_EXTENSION_VERSION`) rather than the current
    /// catalog version, so a reader newer than 12 still accepts what 12 wrote — the same promise
    /// every other record here makes, asserted rather than assumed.
    #[test]
    fn an_extension_record_is_a_version_and_the_version_it_installed_at() {
        let encoded = record::encode_extension("1.1");
        assert_eq!(
            hex(&encoded),
            concat!(
                "17",       // catalog format version
                "03312e31", // varint 3, "1.1"
            )
        );
        assert_eq!(record::decode_extension(&encoded).unwrap(), "1.1");

        // The name is the key, not the value — an extension has no other identity a client sees.
        let key = record::extension_key(1, "uuid-ossp");
        let (start, end) = record::extension_range(1);
        assert!(key >= start && key < end, "the key is inside the range");
        assert_eq!(record::extension_name_of(1, &key).unwrap(), "uuid-ossp");
        // Another tenant's is outside this one's range, which is what makes the scan per tenant.
        assert!(record::extension_key(2, "uuid-ossp") >= end);
    }

    /// **A record written at version 12 still decodes when the catalog version moves on.**
    ///
    /// The bug this prevents is a one-word one: reading with `CATALOG_FORMAT_VERSION` as the floor
    /// instead of the version the kind was introduced at, which refuses every record the previous
    /// release wrote the day the version is bumped.
    #[test]
    fn an_extension_record_from_version_12_still_decodes() {
        let mut written_at_12 = vec![12u8];
        written_at_12.extend_from_slice(&[3, b'1', b'.', b'1']);
        assert_eq!(record::decode_extension(&written_at_12).unwrap(), "1.1");
    }

    /// **Every record kind owns its own key byte**, and two that share one are two records in one
    /// key range.
    ///
    /// `KIND_FUNCTION` and `KIND_FLASHBACK` were both `b'f'`, so `catalog::functions` — which is a
    /// prefix scan over `'f' ++ tenant` — swept up every flashback record the tenant had and tried
    /// to decode it as a function. `SELECT * FROM pg_proc` during a flashback is the reachable
    /// shape. The assertion is over the *keys* rather than over the behaviour, because a key range
    /// that overlaps is the bug whatever is stored in it.
    #[test]
    fn no_two_record_kinds_share_a_key_range() {
        let (start, end) = record::function_range(7);
        let flashback = record::flashback_key(7, 1);
        assert!(
            flashback < start || flashback >= end,
            "a flashback record sits inside the function range: functions() would decode it"
        );
        // And the other way round, for the range a flashback scan would use if one is added.
        let function = record::function_key(7, "f");
        let (extension_start, extension_end) = record::extension_range(7);
        assert!(
            function < extension_start || function >= extension_end,
            "a function record sits inside the extension range"
        );
    }

    #[test]
    fn a_table_record_is_a_version_and_then_the_definition() {
        let encoded = record::encode_table(&accounts(7)).unwrap();
        assert_eq!(
            hex(&encoded),
            concat!(
                "17",                 // catalog format version
                "0700000000000000",   // table id 7
                "086163636f756e7473", // varint 8, "accounts"
                // varint 13, "accounts_pkey" -- the primary key constraint's name. It is a
                // relation name like any other and has to be reserved, even though there is no
                // index behind it: the row key *is* the primary key.
                "0d6163636f756e74735f706b6579",
                "01", // schema version 1: CREATE TABLE has run and no ALTER has
                "02", // two columns
                "026964",
                "01",
                "01",       // "id", INT8, NOT NULL
                "00",       // no DEFAULT
                "00",       // and no missing value
                "ffffffff", // version 4: no typmod, which is -1 and not 0
                "00",       // version 5, widened at 13: no volatile default
                "05656d61696c",
                "02",
                "00",       // "email", TEXT, nullable
                "00",       // no DEFAULT
                "00",       // and no missing value
                "ffffffff", // no typmod
                "00",       // and no volatile default
                "01",
                "00",                                     // primary key: one column, column 0
                "01",                                     // one index
                "0800000000000000",                       // index id 8
                "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
                "01",                                     // unique
                "03",                                     // state: public
                "01",                                     // entered at schema version 1
                "01",
                "01", // one column, column 1
                "00", // version 6: no CHECK constraints
                "00", // version 7: the one index has no WHERE predicate
                "00", // version 8: its one key part is a column, not an expression
                "00", // version 9: ascending, with its NULLs where ascending puts them
                "00", // version 10: no FOREIGN KEY constraints
                "00", // version 11: the one index is not NULLS NOT DISTINCT
                "00", // version 12: its triggers have not been disabled
                "00", // version 13: neither column is generated
                "00",
                // Version 14. One string per column again, and empty for both: neither default
                // stays an expression, which is what the two `no DEFAULT` bytes above already
                // said. The section is written even so, because a reader takes the sections in
                // version order and a missing one would be read as the next field's bytes.
                "00",
                "00",
                // Version 16. No parents and no children: this table inherits from nothing and
                // nothing inherits from it, which is every table until `INHERITS` runs.
                "00",
                "00",
                // Version 17. One byte per index: this one is a `CREATE UNIQUE INDEX`, not a
                // `UNIQUE` constraint, so it gets no `pg_constraint` row and no `DEFERRABLE`.
                "00",
                // Version 18. No triggers, which is every table until `CREATE TRIGGER` runs.
                "00",
                // Version 19. No partition key and no bound: this table neither partitions
                // anything nor is a partition, which is every table until `PARTITION BY` runs.
                "00",
                "00",
                // Version 19, still: one list per index, and the one index here includes
                // nothing — which is every index until `CREATE INDEX ... INCLUDE` runs. Two
                // sections under one number, because they arrived in one release.
                "00",
                // Version 20. No `EXCLUDE` constraints, which is every table until one parses —
                // and until this version it could not, being a syntax error rather than a refusal.
                "00",
                // Version 21. Five empty comments: the table's, its primary key's, one per column
                // and one for the index. **An empty string is "no comment"** — PostgreSQL deletes
                // the `pg_description` row for `IS ''` rather than storing an empty one — so a
                // table nobody has commented costs one byte per object and no flag.
                "00", // the table's
                "00", // `accounts_pkey`'s
                "00", // `id`'s
                "00", // `email`'s
                "00", // `accounts_email_key`'s
                // Version 23. Permanent: this table was not created `UNLOGGED`. One byte for the
                // whole table, because every relation it owns reads its persistence from here.
                "00",
            )
        );
        assert_eq!(record::decode_table(&encoded).unwrap(), accounts(7));
    }

    /// The **version 2** golden, kept rather than replaced.
    ///
    /// These are the bytes version 2 wrote, and a cluster that ran phase 6a or 6d has them on
    /// disk. A format change that replaced its golden would be a format change nothing could
    /// prove it had survived, so the old bytes stay and this test decodes them: every column comes
    /// back with no default and no missing value, which is what a column nobody gave one means.
    #[test]
    fn a_version_2_table_record_still_decodes() {
        let v2 = decode_hex(concat!(
            "02",                 // catalog format version 2
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01", // "id", INT8, NOT NULL -- and nothing after it
            "05656d61696c",
            "02",
            "00", // "email", TEXT, nullable
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",
            "01",
            "01",
        ));
        assert_eq!(record::decode_table(&v2).unwrap(), accounts(7));
    }

    /// A key part's order survives the record, which is what a schema dump reads back.
    ///
    /// Not the same thing as printing it: the `TableDef` is re-read from the store, and a version
    /// that dropped the two bits would print correctly until the cache was cold.
    #[test]
    fn an_index_key_keeps_its_order_through_a_record() {
        let mut table = accounts(7);
        table.indexes[0].keys = vec![
            IndexKey::column(0),
            IndexKey {
                part: KeyPart::Column(1),
                order: KeyOrder {
                    descending: true,
                    nulls_first: false,
                },
            },
        ];
        let encoded = record::encode_table(&table).unwrap();
        let read = record::decode_table(&encoded).unwrap();
        assert_eq!(read, table);
        assert_eq!(read.indexes[0].keys[0].order.indoption(), 0);
        assert_eq!(read.indexes[0].keys[1].order.indoption(), 1);
        assert_eq!(read.indexes[0].keys[1].order.suffix(), " DESC NULLS LAST");
    }

    /// The **version 10** golden, kept for the same reason the nine before it are.
    ///
    /// These are the bytes version 10 wrote — the record ends at the count of `FOREIGN KEY`
    /// constraints, with no `NULLS NOT DISTINCT` flag after it. Every index reads back without
    /// one, which is what every index a version 10 catalog could hold had: the clause was `0A000`
    /// until version 11.
    #[test]
    fn a_version_10_table_record_still_decodes() {
        let v10 = decode_hex(concat!(
            "0a",                 // catalog format version 10
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",
            "00",
            "00",
            "ffffffff",
            "00",
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
            "00", // no CHECK constraints
            "00", // no WHERE predicate
            "00", // its one key part is a column
            "00", // ascending
            "00", // no FOREIGN KEY constraints -- and nothing after it
        ));
        let table = record::decode_table(&v10).unwrap();
        assert_eq!(table, accounts(7));
        assert!(table.indexes.iter().all(|index| !index.nulls_not_distinct));
    }

    /// The **version 9** golden, kept for the same reason the eight before it are.
    ///
    /// These are the bytes version 9 wrote — the record ends at the key orders, with no count of
    /// `FOREIGN KEY` constraints after it. The table reads back with none, which is what every
    /// table a version 9 catalog could hold had: `FOREIGN KEY` was `0A000` until version 10.
    #[test]
    fn a_version_9_table_record_still_decodes() {
        let v9 = decode_hex(concat!(
            "09",                 // catalog format version 9
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",
            "00",
            "00",
            "ffffffff",
            "00",
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
            "00", // no CHECK constraints
            "00", // no WHERE predicate
            "00", // its one key part is a column
            "00", // ascending -- and nothing after it
        ));
        let table = record::decode_table(&v9).unwrap();
        assert_eq!(table, accounts(7));
        assert!(table.foreign_keys.is_empty());
    }

    /// The **version 8** golden, kept for the same reason the seven before it are.
    ///
    /// These are the bytes version 8 wrote — the record ends at the key expressions, with no key
    /// order after them. Every key part reads back **ascending with its NULLs last**, which is
    /// what every key part a version 8 catalog could hold was: a `DESC` index column was `0A000`
    /// until version 9.
    #[test]
    fn a_version_8_table_record_still_decodes() {
        let v8 = decode_hex(concat!(
            "08",                 // catalog format version 8
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",
            "00",
            "00",
            "ffffffff",
            "00",
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
            "00", // no CHECK constraints
            "00", // no WHERE predicate
            "00", // its one key part is a column -- and nothing after it
        ));
        let table = record::decode_table(&v8).unwrap();
        assert_eq!(table, accounts(7));
        assert!(
            table
                .indexes
                .iter()
                .flat_map(|index| &index.keys)
                .all(|key| key.order == KeyOrder::ASCENDING)
        );
    }

    /// The **version 7** golden, kept for the same reason the six before it are.
    ///
    /// These are the bytes version 7 wrote — the record ends at the index predicates, with no key
    /// expressions after them. Every key part reads back as a **column**, which is what every
    /// index a version 7 catalog could hold had: an expression index was `0A000` until version 8.
    #[test]
    fn a_version_7_table_record_still_decodes() {
        let v7 = decode_hex(concat!(
            "07",                 // catalog format version 7
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",
            "00",
            "00",
            "ffffffff",
            "00",
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
            "00", // no CHECK constraints
            "00", // the one index has no WHERE predicate -- and nothing after it
        ));
        let table = record::decode_table(&v7).unwrap();
        assert_eq!(table, accounts(7));
        assert!(
            table
                .indexes
                .iter()
                .all(|index| index.key_columns().is_some())
        );
    }

    /// The **version 6** golden, kept for the same reason the five before it are.
    ///
    /// These are the bytes version 6 wrote — the record ends at the count of `CHECK` constraints,
    /// with no index predicate after it. Every index reads back with none, which is what every
    /// index a version 6 catalog could hold had: a partial index was `0A000` until version 7.
    #[test]
    fn a_version_6_table_record_still_decodes() {
        let v6 = decode_hex(concat!(
            "06",                 // catalog format version 6
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",
            "00",
            "00",
            "ffffffff",
            "00",
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
            "00", // no CHECK constraints -- and nothing after it
        ));
        let table = record::decode_table(&v6).unwrap();
        assert_eq!(table, accounts(7));
        assert!(table.indexes.iter().all(|index| index.predicate.is_none()));
    }

    /// The **version 5** golden, kept for the same reason the four before it are.
    ///
    /// These are the bytes version 5 wrote — the record ends at the last index, with no count of
    /// `CHECK` constraints after it — and a cluster that ran the `DEFAULT CURRENT_TIMESTAMP` unit
    /// has them. The table reads back with no checks, which is what every table a version 5
    /// catalog could hold had: `CHECK` was `0A000` until version 6.
    #[test]
    fn a_version_5_table_record_still_decodes() {
        let v5 = decode_hex(concat!(
            "05",                 // catalog format version 5
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",       // "id", INT8, NOT NULL
            "00",       // no DEFAULT
            "00",       // and no missing value
            "ffffffff", // no typmod
            "00",       // and not an expression default
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1 -- and nothing after it
        ));
        let table = record::decode_table(&v5).unwrap();
        assert_eq!(table, accounts(7));
        assert!(table.checks.is_empty());
    }

    /// The **version 4** golden, kept for the same reason the three before it are.
    ///
    /// These are the bytes version 4 wrote — every column ends at its typmod, with no
    /// expression-default byte after it — and a cluster that ran the typmod unit has them. Each
    /// column reads back `default_now: false`, which is what every column a version 4 catalog
    /// could hold was: `CURRENT_TIMESTAMP` was `0A000` until version 5.
    #[test]
    fn a_version_4_table_record_still_decodes() {
        let v4 = decode_hex(concat!(
            "04",                 // catalog format version 4
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01",       // "id", INT8, NOT NULL
            "00",       // no DEFAULT
            "00",       // and no missing value
            "ffffffff", // no typmod -- and nothing after it
            "05656d61696c",
            "02",
            "00",
            "00",
            "00",
            "ffffffff",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
        ));
        let table = record::decode_table(&v4).unwrap();
        assert_eq!(table, accounts(7));
        for column in &table.columns {
            assert!(column.default_expr.is_none());
        }
    }

    /// The **version 3** golden, kept for the same reason the version 2 one is.
    ///
    /// These are the bytes version 3 wrote — every column ends at its missing value, with no
    /// typmod after it — and a cluster that ran any phase from 6e to the typmod unit has them.
    /// Each column reads back `NO_TYPMOD`, which is what a column declared without a number means
    /// and what **every** column a version 3 catalog could hold was: not one of the types version
    /// 3 had took a number.
    #[test]
    fn a_version_3_table_record_still_decodes() {
        let v3 = decode_hex(concat!(
            "03",                 // catalog format version 3
            "0700000000000000",   // table id 7
            "086163636f756e7473", // varint 8, "accounts"
            "0d6163636f756e74735f706b6579",
            "01", // schema version 1
            "02", // two columns
            "026964",
            "01",
            "01", // "id", INT8, NOT NULL
            "00", // no DEFAULT
            "00", // and no missing value -- and nothing after it
            "05656d61696c",
            "02",
            "00", // "email", TEXT, nullable
            "00",
            "00",
            "01",
            "00",                                     // primary key: one column, column 0
            "01",                                     // one index
            "0800000000000000",                       // index id 8
            "126163636f756e74735f656d61696c5f6b6579", // "accounts_email_key"
            "01",                                     // unique
            "03",                                     // state: public
            "01",                                     // entered at schema version 1
            "01",
            "01", // one column, column 1
        ));
        let table = record::decode_table(&v3).unwrap();
        assert_eq!(table, accounts(7));
        for column in &table.columns {
            assert_eq!(column.typmod, crate::value::NO_TYPMOD);
            assert_eq!(column.length(), None);
            assert_eq!(column.precision(), None);
        }
    }

    /// A `varchar(5)`, a `character(3)` and a `timestamp(3)` survive the record that version 4
    /// exists for — read back as the numbers PostgreSQL's `atttypmod` holds, `9`, `7` and `3`.
    #[test]
    fn a_version_4_record_carries_every_typmod() {
        let mut table = accounts(7);
        table.columns.push(ColumnDef {
            name: "v".into(),
            ty: ColumnType::Varchar,
            typmod: crate::value::typmod_of_length(5),
            default_expr: None,
            not_null: false,
            default: None,
            missing: None,
            generated: None,
            comment: None,
        });
        table.columns.push(ColumnDef {
            name: "c".into(),
            ty: ColumnType::Bpchar,
            typmod: crate::value::typmod_of_length(3),
            default_expr: None,
            not_null: false,
            default: None,
            missing: None,
            generated: None,
            comment: None,
        });
        table.columns.push(ColumnDef {
            name: "t".into(),
            ty: ColumnType::Timestamp,
            typmod: crate::value::typmod_of_precision(3),
            default_expr: None,
            not_null: false,
            default: None,
            missing: None,
            generated: None,
            comment: None,
        });
        let back = record::decode_table(&record::encode_table(&table).unwrap()).unwrap();
        assert_eq!(back, table);
        // Measured on 19beta1, `pg_attribute.atttypmod`: the string types carry the four bytes of
        // a varlena header and the time types do not.
        assert_eq!(back.columns[2].typmod, 9);
        assert_eq!(back.columns[3].typmod, 7);
        assert_eq!(back.columns[4].typmod, 3);
        assert_eq!(back.columns[2].length(), Some(5));
        assert_eq!(back.columns[3].length(), Some(3));
        assert_eq!(back.columns[4].precision(), Some(3));
        // And the accessors answer for the type, not for the number: an `int8` never has a length
        // however the bytes read.
        assert_eq!(back.columns[0].length(), None);
    }

    /// Version 3 changed the **table** record's layout and nothing else's, so a version 2 record of
    /// any other kind decodes unchanged. Refusing them because the byte moved would break a cluster
    /// that had run phase 6d over a change that does not touch them.
    #[test]
    fn a_version_2_record_of_another_kind_decodes_unchanged() {
        let mut v2 = record::encode_retention(60_000);
        v2[0] = 2;
        assert_eq!(record::decode_retention(&v2).unwrap(), 60_000);

        let mut v2 = record::encode_checkpoint(1_234);
        v2[0] = 2;
        assert_eq!(record::decode_checkpoint(&v2).unwrap(), 1_234);
    }

    /// Invariant 2 again: a record from a version we do not know is an error, and every truncation
    /// of a record we do know is one too (invariant 9).
    #[test]
    fn a_record_that_is_not_ours_is_refused_rather_than_guessed_at() {
        let mut encoded = record::encode_table(&accounts(1)).unwrap();
        encoded[0] = super::CATALOG_FORMAT_VERSION + 1;
        assert_eq!(
            record::decode_table(&encoded).unwrap_err().sqlstate(),
            sqlstate::DATA_CORRUPTED
        );

        let encoded = record::encode_table(&accounts(1)).unwrap();
        for cut in 0..encoded.len() {
            assert!(
                record::decode_table(&encoded[..cut]).is_err(),
                "{cut} bytes decoded as a whole table"
            );
        }
    }

    /// A catalog that points at a column the table does not have would be a wrong column later,
    /// which is worse than an error now.
    #[test]
    fn a_primary_key_pointing_past_the_columns_is_corruption() {
        // Found rather than computed: the one byte that differs between a primary key on column 0
        // and one on column 1 is the ordinal, so the test cannot drift out of the format.
        let mut on_first = accounts(1);
        on_first.primary_key = vec![0];
        let mut on_second = accounts(1);
        on_second.primary_key = vec![1];
        let (a, b) = (
            record::encode_table(&on_first).unwrap(),
            record::encode_table(&on_second).unwrap(),
        );
        let differing: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        assert_eq!(differing.len(), 1, "exactly one byte is the ordinal");

        let mut encoded = a;
        encoded[differing[0]] = 9;
        assert_eq!(
            record::decode_table(&encoded).unwrap_err().sqlstate(),
            sqlstate::DATA_CORRUPTED
        );
    }

    /// Only ASCII folds. `str::to_lowercase` would have renamed every non-ASCII identifier, and a
    /// real server leaves them alone.
    #[test]
    fn identifiers_fold_the_way_a_utf8_server_folds_them() {
        assert_eq!(fold_identifier("Mixed", false).0, "mixed");
        assert_eq!(fold_identifier("Quoted", true).0, "Quoted");
        assert_eq!(fold_identifier("Ébc", false).0, "Ébc", "only A-Z fold");

        let long = "x".repeat(70);
        let (folded, truncated) = fold_identifier(&long, false);
        assert_eq!(folded.len(), MAX_IDENTIFIER_BYTES);
        assert!(truncated, "the caller owes the client a 42622 notice");

        // A multi-byte character straddling the limit is dropped whole, or the name would not be
        // UTF-8 and the catalog could not read it back.
        let wide = format!("{}é", "x".repeat(62));
        let (folded, _) = fold_identifier(&wide, false);
        assert_eq!(folded, "x".repeat(62));
    }

    #[test]
    fn a_table_is_found_by_name_and_by_id_after_it_is_committed() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        let id = allocate_id(&mut *ddl, 1).unwrap();
        let table = accounts(id);
        create_table(&mut *ddl, 1, &table).unwrap();
        ddl.commit().unwrap();

        let txn = backend.begin().unwrap();
        let view = catalog.view(&*txn, 1).unwrap();
        assert_eq!(view.version(), 1, "one DDL statement, one version");
        assert_eq!(*view.table("accounts").unwrap().unwrap(), table);
        assert_eq!(*view.table_by_id(id).unwrap().unwrap(), table);
        assert_eq!(
            view.relation("accounts_email_key").unwrap(),
            Some(Relation::Index {
                table_id: id,
                index_id: id + 1,
            }),
            "an index's name is in the same namespace"
        );
        assert!(
            view.table("accounts_email_key").unwrap().is_none(),
            "and it is not a table"
        );
        assert_eq!(
            view.require_table("nope").unwrap_err().sqlstate(),
            sqlstate::UNDEFINED_TABLE
        );
    }

    /// DDL is invisible until it commits, like any other write. A node that showed uncommitted DDL
    /// would let a query see a table that may never exist.
    #[test]
    fn an_uncommitted_table_is_invisible_to_everyone_else() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        create_table(&mut *ddl, 1, &accounts(1)).unwrap();

        let other = backend.begin().unwrap();
        assert!(
            catalog
                .view(&*other, 1)
                .unwrap()
                .table("accounts")
                .unwrap()
                .is_none()
        );
        // The writer sees its own DDL, because read-your-writes applies to the catalog too.
        assert!(
            catalog
                .view(&*ddl, 1)
                .unwrap()
                .table("accounts")
                .unwrap()
                .is_some()
        );
    }

    /// The cache must never answer with a definition from a version the asking transaction cannot
    /// see. This is the test for that: fill the cache at a new version, then ask an older
    /// transaction, which must read through and get its own snapshot's answer.
    #[test]
    fn a_cache_warmed_by_a_newer_transaction_does_not_leak_into_an_older_one() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut first = backend.begin().unwrap();
        create_table(&mut *first, 1, &accounts(1)).unwrap();
        first.commit().unwrap();

        // Starts before the second table exists.
        let old = backend.begin().unwrap();

        let mut second = backend.begin().unwrap();
        let mut ledger = accounts(3);
        ledger.name = "ledger".into();
        ledger.primary_key_name = "ledger_pkey".into();
        ledger.indexes[0].name = "ledger_email_key".into();
        create_table(&mut *second, 1, &ledger).unwrap();
        second.commit().unwrap();

        // Warm the cache at the new version.
        let new = backend.begin().unwrap();
        let new_view = catalog.view(&*new, 1).unwrap();
        assert_eq!(new_view.version(), 2);
        assert!(new_view.table("ledger").unwrap().is_some());

        let old_view = catalog.view(&*old, 1).unwrap();
        assert_eq!(old_view.version(), 1, "its own snapshot's catalog");
        assert!(
            old_view.table("ledger").unwrap().is_none(),
            "the newer transaction's cache entry must not reach an older snapshot"
        );
        assert!(old_view.table("accounts").unwrap().is_some());
    }

    /// A definition that changed shape must not survive in a cache. Rewriting the table under a
    /// new version has to reach every later reader.
    #[test]
    fn a_changed_table_is_not_served_from_the_cache() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        create_table(&mut *ddl, 1, &accounts(1)).unwrap();
        ddl.commit().unwrap();

        let first = backend.begin().unwrap();
        let before = catalog
            .view(&*first, 1)
            .unwrap()
            .table("accounts")
            .unwrap()
            .unwrap();
        assert_eq!(before.indexes.len(), 1);

        let mut adding = backend.begin().unwrap();
        let mut with_more = accounts(1);
        with_more.indexes.push(IndexDef {
            id: 5,
            name: "accounts_id_idx".into(),
            unique: false,
            keys: vec![IndexKey::column(0)],
            state: SchemaState::Public,
            state_since: 1,
            include: Vec::new(),
            predicate: None,
            nulls_not_distinct: false,
            constraint: None,
            comment: None,
        });
        replace_table(&mut *adding, 1, &accounts(1), &with_more).unwrap();
        adding.commit().unwrap();

        let second = backend.begin().unwrap();
        let after = catalog
            .view(&*second, 1)
            .unwrap()
            .table("accounts")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.indexes.len(),
            2,
            "the cache served a table with a missing index"
        );
    }

    /// Dropping an index frees its name, and dropping a table frees every name it held.
    #[test]
    fn dropping_frees_the_names_it_held() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        let mut ddl = backend.begin().unwrap();
        let table = accounts(1);
        create_table(&mut *ddl, 1, &table).unwrap();
        let mut without = table.clone();
        without.indexes.clear();
        replace_table(&mut *ddl, 1, &table, &without).unwrap();
        drop_table(&mut *ddl, 1, &without).unwrap();
        ddl.commit().unwrap();

        let txn = backend.begin().unwrap();
        let view = catalog.view(&*txn, 1).unwrap();
        assert_eq!(view.relation("accounts").unwrap(), None);
        assert_eq!(view.relation("accounts_email_key").unwrap(), None);
    }

    /// One namespace: an index cannot take a table's name, which is `42P07` in PostgreSQL and was
    /// confirmed against the server.
    #[test]
    fn an_index_cannot_take_a_name_a_table_already_has() {
        let backend = MemoryBackend::new();
        let mut ddl = backend.begin().unwrap();
        let table = accounts(1);
        create_table(&mut *ddl, 1, &table).unwrap();

        let mut clash = table.clone();
        clash.indexes.push(IndexDef {
            id: 9,
            name: "accounts".into(),
            unique: false,
            keys: vec![IndexKey::column(0)],
            state: SchemaState::Public,
            state_since: 1,
            include: Vec::new(),
            predicate: None,
            nulls_not_distinct: false,
            constraint: None,
            comment: None,
        });
        let error = replace_table(&mut *ddl, 1, &table, &clash).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);

        let mut again = accounts(2);
        again.indexes.clear();
        assert_eq!(
            create_table(&mut *ddl, 1, &again).unwrap_err().sqlstate(),
            sqlstate::DUPLICATE_TABLE
        );
    }

    /// Two nodes creating the same table at once both see the name as free, both write it, and one
    /// of them loses at commit -- the same composition that makes a unique index work, applied to
    /// the catalog.
    #[test]
    fn two_concurrent_creates_of_one_name_leave_one_table() {
        let backend = MemoryBackend::new();
        let mut left = backend.begin().unwrap();
        let mut right = backend.begin().unwrap();

        create_table(&mut *left, 1, &accounts(1)).unwrap();
        let mut other = accounts(2);
        other.indexes[0].name = "accounts_email_key2".into();
        create_table(&mut *right, 1, &other).unwrap();

        assert!(left.commit().is_ok());
        let loser = right.commit().expect_err("one of them must lose");
        assert_eq!(loser.sqlstate(), sqlstate::SERIALIZATION_FAILURE);
    }

    /// Ids are unique across tables and indexes alike, so a debug dump can never confuse the two.
    #[test]
    fn relation_ids_come_from_one_sequence_and_start_at_one() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();
        assert_eq!(allocate_id(&mut *txn, 1).unwrap(), 1);
        assert_eq!(allocate_id(&mut *txn, 1).unwrap(), 2);
        assert_eq!(allocate_id(&mut *txn, 2).unwrap(), 1, "per tenant");
        txn.commit().unwrap();

        let mut next = backend.begin().unwrap();
        assert_eq!(
            allocate_id(&mut *next, 1).unwrap(),
            3,
            "and it survives a commit"
        );
    }

    /// A rolled-back DDL must leave nothing behind in the cache every session shares.
    ///
    /// The trap is that catalog versions are *reused*: the bump is `read + 1` inside the
    /// transaction, so a DDL that rolls back leaves its number free and the next DDL that really
    /// commits lands on it — and finds the abandoned entry waiting. Before
    /// [`Catalog::view_uncached`] this returned a table nobody had ever created.
    #[test]
    fn a_rolled_back_ddl_leaves_nothing_in_the_cache() {
        let backend = MemoryBackend::new();
        let catalog = Catalog::new();

        // Session A: BEGIN; CREATE TABLE accounts; look at it; ROLLBACK.
        let mut a = backend.begin().unwrap();
        create_table(&mut *a, 1, &accounts(1)).unwrap();
        {
            let view = catalog.view_uncached(&*a, 1).unwrap();
            assert_eq!(view.version(), 1);
            assert!(view.table("accounts").unwrap().is_some(), "its own writes");
        }
        a.rollback().unwrap();

        // Session B commits a different DDL, which reaches the same catalog version.
        let mut b = backend.begin().unwrap();
        let mut ledger = accounts(3);
        ledger.name = "ledger".into();
        ledger.primary_key_name = "ledger_pkey".into();
        ledger.indexes[0].name = "ledger_email_key".into();
        create_table(&mut *b, 1, &ledger).unwrap();
        b.commit().unwrap();

        let c = backend.begin().unwrap();
        let view = catalog.view(&*c, 1).unwrap();
        assert_eq!(view.version(), 1);
        assert!(
            view.table("accounts").unwrap().is_none(),
            "a table nobody committed came back from the cache"
        );
    }

    /// The golden for a columnar record: the count PD reads, and the row schema a store reads
    /// past it.
    ///
    /// Both audiences are below this crate and neither links it, so the layout is a contract
    /// between three layers rather than an implementation detail of one. **The count is at a
    /// fixed offset on purpose** — a placement driver reads byte 1 and stops, because it wants a
    /// number and cannot read a table definition anyway (`CLAUDE.md` invariant 7).
    ///
    /// It also pins the kind byte that is **not** the one ADR 0022 sketched. The ADR wrote
    /// `'m' ++ "sql" ++ 'c'`; by the time this was built `'c'` was the checkpoint's, and two
    /// kinds sharing a byte is one scan returning the other's records. The collision is asserted
    /// here rather than avoided and then trusted.
    #[test]
    fn a_columnar_record_carries_the_count_and_the_row_schema() {
        let table = accounts(7);
        let encoded = record::encode_columnar(2, Some(&table)).unwrap();
        assert_eq!(
            hex(&encoded),
            concat!(
                // **`esker_keys::columnar`'s** format version, not the catalog record's —
                // `encode_columnar` delegates, so this byte is not `CATALOG_FORMAT_VERSION`. The
                // two were both 3 until the typmod made the catalog record 4, which is the first
                // time anything has told them apart.
                "03", "02", // two columnar replicas -- byte 1, where PD stops
                "01", // schema_version 1
                "02", // two columns
                "01", "00", // int8, no missing value
                "02", "00", // text, no missing value
            )
        );

        // PD's read: two bytes and no schema parsing at all.
        assert_eq!(record::decode_columnar_replicas(&encoded).unwrap(), 2);

        // A store's read: the pair `esker_keys::row::RowSchema` is built from.
        let (replicas, published) = record::decode_columnar(&encoded).unwrap();
        assert_eq!((replicas, published.schema_version), (2, 1));
        assert_eq!(
            published.columns,
            vec![(ColumnType::Int8, None), (ColumnType::Text, None)]
        );

        // A missing value travels with its column, because a decoder given only types pads NULL
        // where the row store pads the default -- silently, and only for rows older than the
        // `ALTER` that added the column.
        let mut widened = accounts(7);
        widened.columns.push(ColumnDef {
            name: "tier".into(),
            ty: ColumnType::Int8,
            typmod: crate::value::NO_TYPMOD,
            default_expr: None,
            not_null: true,
            default: Some(Datum::Int8(42)),
            missing: Some(Datum::Int8(42)),
            generated: None,
            comment: None,
        });
        let (_, published) =
            record::decode_columnar(&record::encode_columnar(1, Some(&widened)).unwrap()).unwrap();
        assert_eq!(
            published.columns[2],
            (ColumnType::Int8, Some(Datum::Int8(42)))
        );
        // And the built `RowSchema` carries it, which is the property the pair exists for: a
        // decoder that assembled one from types alone would pad NULL here.
        assert_eq!(published.row_schema().types().len(), 3);

        let key = record::columnar_key(1, 7);
        assert_eq!(
            hex(&key),
            concat!(
                "6d",               // 'm', the meta space
                "73716c",           // "sql"
                "6c",               // 'l', the learner ADR 0022 Decision 1 calls a columnar copy
                "0000000000000001", // tenant 1, memcomparable
                "0000000000000007", // table 7
            )
        );
        assert_eq!(record::columnar_table_id(1, &key).unwrap(), 7);

        // The collision that was avoided, asserted rather than remembered.
        let (start, end) = record::columnar_range(1);
        let checkpoint = record::checkpoint_key(1, "nightly");
        assert!(
            key >= start && key < end,
            "the record is outside its own range"
        );
        assert!(
            checkpoint < start || checkpoint >= end,
            "a checkpoint falls inside the columnar scan -- the kind bytes collide"
        );
    }

    /// The golden for a retention record. The garbage collector reads these bytes from a crate
    /// that does not link this one (ADR 0021), so the layout is a contract between two layers and
    /// not an implementation detail of either.
    #[test]
    fn a_retention_record_is_a_version_and_a_u64_of_milliseconds() {
        let encoded = record::encode_retention(600_000);
        assert_eq!(
            hex(&encoded),
            concat!(
                "17",               // catalog format version
                "c027090000000000", // 600000 ms -- ten minutes, little-endian
            )
        );
        assert_eq!(record::decode_retention(&encoded).unwrap(), 600_000);

        // The key layout is the other half of the contract: one scan of the kind byte visits every
        // override, and the cluster default is not in it.
        let key = record::retention_key(1, 7);
        let default = record::default_retention_key();
        assert_eq!(
            hex(&key),
            concat!(
                "6d",               // 'm', the meta space
                "73716c",           // "sql"
                "72",               // 'r', a retention override
                "0000000000000001", // tenant 1, memcomparable
                "0000000000000007", // table 7
            )
        );
        assert_eq!(hex(&default), concat!("6d", "73716c", "64"));
        assert!(
            !default.starts_with(&key[..key.len() - 16]),
            "the default must not fall inside a scan of the overrides"
        );

        // Forever is a reserved value, not a very large duration: a reader subtracts every other
        // one from a timestamp and cannot subtract this.
        assert_eq!(
            record::decode_retention(&record::encode_retention(RETENTION_FOREVER)).unwrap(),
            u64::MAX
        );

        // And it fails closed like every other record.
        let mut damaged = encoded.clone();
        damaged[0] = record::CATALOG_FORMAT_VERSION + 1;
        assert!(record::decode_retention(&damaged).is_err());
        for cut in 0..encoded.len() {
            assert!(
                record::decode_retention(&encoded[..cut]).is_err(),
                "{cut} bytes decoded as a retention"
            );
        }
        assert!(
            record::decode_retention(&[&encoded[..], &[0]].concat()).is_err(),
            "a trailing byte is not ignored"
        );
    }

    /// Retention is per table with a cluster fallback, and it survives the table being dropped
    /// only in the sense that it does not: an override goes when its table does.
    #[test]
    fn retention_falls_back_to_the_cluster_default_and_dies_with_its_table() {
        let backend = MemoryBackend::new();
        let mut txn = backend.begin().unwrap();

        assert_eq!(default_retention(&*txn).unwrap(), DEFAULT_RETENTION_MS);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), None);

        set_default_retention(&mut *txn, 5_000);
        set_table_retention(&mut *txn, 1, 7, 600_000);
        assert_eq!(default_retention(&*txn).unwrap(), 5_000);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), Some(600_000));
        assert_eq!(table_retention(&*txn, 1, 8).unwrap(), None, "another table");
        assert_eq!(
            table_retention(&*txn, 2, 7).unwrap(),
            None,
            "another tenant"
        );

        clear_table_retention(&mut *txn, 1, 7);
        assert_eq!(table_retention(&*txn, 1, 7).unwrap(), None);

        let table = accounts(7);
        create_table(&mut *txn, 1, &table).unwrap();
        set_table_retention(&mut *txn, 1, 7, RETENTION_FOREVER);
        drop_table(&mut *txn, 1, &table).unwrap();
        assert_eq!(
            table_retention(&*txn, 1, 7).unwrap(),
            None,
            "an override must not outlive its table in the collector's scan"
        );
    }

    proptest::proptest! {
        /// Invariant 9 for the catalog: arbitrary bytes are an error or a definition, never a
        /// panic and never an allocation the length asked for but the record cannot fill.
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            bytes in proptest::collection::vec(proptest::arbitrary::any::<u8>(), 0..128)
        ) {
            let _ = record::decode_table(&bytes);
            let _ = record::decode_relation(&bytes);
            let _ = record::decode_counter(&bytes);
        }

        /// And the same for bytes that start out looking like a record, which is the shape a
        /// truncated or partly-overwritten one actually has.
        #[test]
        fn decoding_a_damaged_record_never_panics(
            at in 0usize..64,
            byte in proptest::arbitrary::any::<u8>(),
        ) {
            let mut encoded = record::encode_table(&accounts(1)).unwrap();
            if at < encoded.len() {
                encoded[at] = byte;
            }
            let _ = record::decode_table(&encoded);
        }
    }
}
