//! Expressions, and the one rule that decides what a literal means.
//!
//! A literal in SQL has no type of its own until something gives it one. `'2024-01-01'` is a date
//! in a `timestamptz` column and six characters in a `text` one, and PostgreSQL calls that state
//! `unknown` and resolves it from context. This module keeps literals in that state — [`Literal`]
//! is what was *written* — and [`Literal::assign`] is where a column's type resolves it.
//!
//! That is not a simplification of PostgreSQL's rules, it is the useful half of them, and it is
//! what lets the whole value layer be reused: assigning a string literal to any of the six types is
//! [`crate::value::Datum::from_text`], which is already checked against a real server in both
//! directions.
//!
//! # What a literal will and will not become
//!
//! The conversions here were measured. Some are less obvious than they look:
//!
//! * **`true` in a `text` column stores `true`, not `t`.** The output function writes one
//!   character and the *assignment cast* writes the word, and they are different functions —
//!   the same split that made `bool` the one type where capturing a `::text` cast taught the wrong
//!   answer (`crate::value`).
//! * **`1.5` in a `text` column stores `1.5`** — the digits as written. PostgreSQL types a decimal
//!   literal `numeric`, and `numeric`'s text is its own digits, not a float's rendering of them.
//!   So the literal keeps its source text.
//! * **A decimal literal in an `int8` column is refused**, though PostgreSQL rounds it. Getting
//!   `numeric`'s rounding wrong is a silently wrong number: PostgreSQL rounds half away from zero
//!   (`2.5` becomes `3`) where a `float8` would round half to even (`2.5` becomes `2`), and the
//!   only way to be sure is to have `numeric`, which phase 6a does not. Contract C2 names it.

use crate::error::{Result, SqlError};
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// An expression, as far as phase 6a needs one.
///
/// **Arithmetic was deliberately absent and is here now** ([ADR
/// 0046](../../../../docs/adr/0046-arithmetic-is-its-own-node-and-postgresql-s-promotion-table.md)).
/// The reason it was left out still holds — every operator brings its own overflow, division and
/// type-resolution rules, and each of them is a way to return a confidently wrong number — so it
/// arrived the way the rest of this crate does: a capture of what a real server answers first,
/// and one table (`crate::value::arith`) that the planner and the evaluator both read, so the
/// type a client is told cannot drift from the values it is sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A constant as written.
    Literal(Literal),
    /// `$1`, one-based as PostgreSQL writes it.
    Parameter(u32),
    /// `-operand`, which is **not** `0 - operand`.
    ///
    /// It was written as the subtraction at first, and the subtraction is right for every number:
    /// `-((-2147483648)::int4)` overflows at `int4` because the `0` takes the operand's width. It
    /// is wrong for the temporal types, and in both directions — `-'1 day'::interval` is
    /// `-1 days` where `bigint - interval` does not exist, and `-'12:00'::time` is an **interval**
    /// where `bigint - time` does not exist either. So the negation is its own node, with its own
    /// per-type result (`crate::value::temporal::negate_type`) and PostgreSQL's **unary** `42883`
    /// for a type that has none.
    Negate(Box<Expr>),
    /// `left <op> right` where the operator yields a **value**, not a boolean.
    ///
    /// The type it yields is `crate::value::arith::result_type`'s answer and is computed once, at
    /// plan time, because a client is told the column's OID before any row is read.
    Arithmetic {
        /// Which operator.
        op: ArithOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
        /// The type the operator yields, once a scope has been available to work it out — and
        /// `None` until then, which is a **state and not a default**. A `DEFAULT` expression is
        /// evaluated by the DDL path without ever being resolved against a row, so the evaluator
        /// falls back to the operands' own types there; putting a guess here instead would make
        /// that path silently disagree with the one a `SELECT` takes.
        ty: Option<ColumnType>,
    },
    /// A column of the row being evaluated, by name. The planner resolves it to a position.
    Column {
        /// The table it was qualified with — `a` in `a.id` — or `None` for a bare name.
        ///
        /// Carried rather than dropped, which it used to be. With one table in a query a
        /// qualifier adds nothing *when it is right*, and the first version of this threw it away
        /// on that argument; the argument is wrong, because it also throws away the case where it
        /// is **not** right. `SELECT wrong.a FROM t` is `42P01` on a real server and was answered
        /// here as though the user had written `a`.
        table: Option<String>,
        /// The column's name.
        name: String,
    },
    /// A column already resolved to its position, which is what the executor evaluates.
    Ordinal {
        /// Position in the row.
        at: usize,
        /// The column's type, so a comparison against it can resolve a literal.
        ty: ColumnType,
        /// The column's typmod, so a comparison against a `character(n)` can normalise one.
        ///
        /// A `bpchar`'s values are stored padded to `n`, so a literal has to be padded the same
        /// way before a byte comparison means what PostgreSQL means. Carrying the number here is
        /// what lets that happen once, where the literal is typed, rather than in the evaluator —
        /// which sees two `Datum::Text`s and cannot tell a `character(3)` from a `text`.
        typmod: i32,
    },
    /// A comparison or a logical connective.
    Binary {
        /// Which one.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `NOT x`.
    Not(Box<Expr>),
    /// `x IN (a, b, …)`, or `NOT IN` when negated.
    ///
    /// Carried as itself rather than lowered to `x = a OR x = b`, for one reason that is not
    /// taste: the left-hand side would be **evaluated once per item**. `k IN (id + 6, id + 7)` is
    /// cheap written this way and quadratic written the other way for a wide left side, and a
    /// rewrite that duplicated a `nextval` would be worse than slow.
    ///
    /// The three-valued rule is the trap the corpus exists for and it is **not** "NULL means
    /// false": a match wins over a NULL, and a NULL wins over no match. `1 IN (1, NULL)` is true,
    /// `1 IN (2, NULL)` is NULL, and `1 NOT IN (2, NULL)` is NULL — so a `NOT IN` over a list
    /// containing NULL matches nothing at all. Measured, `tests/corpus/pg19_in.txt`.
    InList {
        /// The left-hand side, evaluated once.
        operand: Box<Expr>,
        /// The list, in the order written. PostgreSQL's grammar has no empty one.
        list: Vec<Expr>,
        /// `NOT IN`, which is `NOT (x IN …)` and not "none of them are equal" — the difference is
        /// entirely in what NULL does.
        negated: bool,
        /// Whether this came from `= ANY(…)` / `<> ALL(…)` rather than from a written `IN`.
        ///
        /// **The two are one rule everywhere but one place**, which is why the flag is here rather
        /// than in two variants: a bare literal in the list. `x IN ('ra')` over a `regclass`
        /// resolves the *name* — a list is coerced through the type's own input function — and
        /// `x = ANY('{ra}')` is `22P02`, because that one really is `=` and `=` over a `regclass`
        /// is `oideq`. Measured, both (`debts-v1.1.md` #41). The lowering folds `= ANY` into this
        /// variant so an index seek can use it, and this is what it must not lose on the way.
        any: bool,
    },
    /// `x <op> ANY(<array>)` and `x <op> ALL(<array>)` — a comparison, a quantifier, and an array
    /// that is a **value of the row** rather than a list the lowering could see
    /// (`a.attnum = ANY(i.indkey)`).
    ///
    /// [`Expr::InList`] is the same rule over a list known at plan time for the two spellings that
    /// have one, and the lowering prefers it: `ARRAY[1,2]`, `'{a,b}'` and `current_schemas(false)`
    /// are expanded where they are written, which is what lets an index seek use them. A
    /// **column** cannot be, because its value differs per row — which is why this is a variant
    /// and not a rewrite, and why boot statement 17 was refused by name until it existed.
    ///
    /// **The three-valued rule is the subquery form's, shared rather than copied**
    /// (`crate::exec::subquery`): no elements settles it with no comparison — `ANY` false and
    /// `ALL` true, even for a NULL operand — one definite answer wins past any number of NULLs,
    /// and otherwise a NULL leaves it unknown. Measured on 19beta1 for both right-hand sides in
    /// one session, `tests/corpus/pg19_all_quantifier.txt`.
    QuantifiedArray {
        /// The left-hand side, evaluated once.
        operand: Box<Expr>,
        /// The comparison, which is any of the six and not only `=`.
        op: BinaryOp,
        /// `ALL` rather than `ANY`. The two differ in **one** thing — which answer is decisive —
        /// so they are a flag on one node and not two nodes, exactly as
        /// [`crate::plan::SubqueryKind::Quantified`] holds them.
        all: bool,
        /// The array, evaluated once per row and read from its own text form
        /// (`crate::value::vector`). A NULL array makes the whole comparison NULL — measured,
        /// `1 = ANY(NULL::int[])` is NULL where `1 = ANY('{}')` is false.
        array: Box<Expr>,
    },
    /// `ARRAY[a, b, …]` whose elements are not all constants, so it is built per row.
    ///
    /// **A constructor over constants never reaches here**: it folds to a `Datum::Array` at
    /// lowering, which is where the element type is settled from what was written
    /// (`lower_array_constructor`). This is the other case — `ARRAY[casttarget]`, which
    /// `ActiveRecord`'s case-insensitivity probe sends — where an element is a column and the
    /// value cannot exist until there is a row.
    ///
    /// The element type is settled at **resolution**, from the resolved elements, and carried here
    /// so a client is told the column's type before any row is read. `None` until then, which is a
    /// state and not a default: an unresolved constructor has no type yet, and guessing one would
    /// make the DDL path disagree with the `SELECT` path the way `Arithmetic::ty` documents.
    Array {
        /// The elements, in order, each evaluated per row.
        elements: Vec<Expr>,
        /// The element type, once a scope has settled it.
        element: Option<ColumnType>,
    },
    /// `a[i]` — one element of an array, by **absolute** subscript.
    ///
    /// Not an offset: it follows the array's own lower bound, so `indkey[0]` is an index's first
    /// column and `conkey[1]` is a constraint's, because an `int2vector` starts at 0 and an
    /// `int2[]` starts at 1. Both spellings are what `ActiveRecord` writes.
    ///
    /// **Every way of missing is NULL and none is an error** — out of range at either end, an
    /// empty array, a NULL array, a NULL subscript. That is what lets a caller walk an array
    /// without checking its length.
    Subscript {
        /// The array, read from its own text form (`crate::value::vector`).
        operand: Box<Expr>,
        /// The subscript, evaluated per row.
        index: Box<Expr>,
        /// The type to read the element **as**.
        ///
        /// An array is text here and so are its elements, so an element has no type of its own —
        /// what gives it one is what it is compared against, exactly as an `= ANY`'s elements take
        /// the operand's type. `a.attnum = d.indkey[0]` is an `int2` column against a subscript,
        /// and a node that compared the element as text would find nothing and report an **empty
        /// join** rather than an error. Set where the comparison is reconciled; `text` until then.
        element: ColumnType,
    },
    /// `gen_random_uuid()` and `uuid_generate_v4()`: a fresh version-4 UUID, per call.
    ///
    /// **Volatile**, which is the property that decides where it may go: two calls in one
    /// statement give two values — measured, `gen_random_uuid() = gen_random_uuid()` is `f` — so
    /// it cannot be folded, cached per statement, or used as an index key. It is a variant rather
    /// than a [`CatalogFunc`] for exactly that reason: that family is documented as a function of
    /// its arguments alone, and this is the opposite.
    Uuid(UuidFunc),
    /// `x [NOT] LIKE p [ESCAPE c]` and its case-insensitive twin `ILIKE`.
    ///
    /// Its own variant rather than a [`BinaryOp`], because it is not a comparison: the two sides
    /// are a subject and a *pattern*, `pg_cmp` says nothing about them, and the operator carries
    /// two modifiers a binary op has nowhere to put.
    Like {
        /// The subject.
        operand: Box<Expr>,
        /// The pattern, in which `%` and `_` are wildcards.
        pattern: Box<Expr>,
        /// `NOT LIKE`. **Not the same as `NOT (x LIKE p)` for a NULL** only in appearance: both
        /// are NULL, because the negation of unknown is unknown.
        negated: bool,
        /// `ILIKE`: fold both sides before matching.
        case_insensitive: bool,
        /// The character that quotes a wildcard, `\\` unless `ESCAPE` named another — and
        /// `ESCAPE` **replaces** the backslash rather than adding to it.
        escape: Option<char>,
    },
    /// `x ~ p`, `x ~* p`, `x !~ p`, `x !~* p` — POSIX regular-expression matching.
    ///
    /// [`Expr::Like`]'s shape, for [`Expr::Like`]'s reason: the sides are a subject and a
    /// *pattern*, and the operator carries modifiers a [`BinaryOp`] has nowhere to put. The
    /// matcher is `crate::value::regex`, written in-house because `deny.toml` forbids a regex
    /// crate.
    RegexMatch {
        /// The subject.
        operand: Box<Expr>,
        /// The pattern, a POSIX extended regular expression.
        pattern: Box<Expr>,
        /// `!~` and `!~*`. **Not a rescue for NULL**: the negation of unknown is unknown.
        negated: bool,
        /// The `*` half of the pair: fold while matching.
        case_insensitive: bool,
    },
    /// `current_schema()` and `current_schemas(bool)` — **the session\'s, resolved**.
    ///
    /// Not folded where the statement is lowered, because the answer is the session\'s
    /// `search_path` and a lowering has no session. It is replaced with the value in
    /// `crate::exec::Executor::bound`, once per statement, the way `::regclass` is — so the row
    /// evaluator never meets one.
    ///
    /// **The path is answered as *resolved*, not as set**: an entry naming no schema is dropped,
    /// which is what makes the default `"$user", public` answer `{public}`. `current_schema()` is
    /// the first that resolves, and **NULL when none do**.
    CurrentSchema {
        /// `None` for the scalar `current_schema()`; `Some(implicit)` for `current_schemas`, where
        /// `implicit` is its argument — `true` prepends `pg_catalog` and nothing else.
        all: Option<bool>,
    },
    /// `current_database()` — the database **this session** is connected to.
    ///
    /// Folded in `crate::exec::Executor::bound`, the way [`Expr::CurrentSchema`] is and for the
    /// same reason. It was a constant folded at lowering while the node had one database to fold
    /// to; with a directory behind it (ADR 0052) the answer is a property of the session, and a
    /// lowering has no session — so a constant here would report the *default* database's name to
    /// a client connected to another one, which is a wrong answer rather than a missing feature.
    CurrentDatabase,
    /// `current_user`, `session_user` and the bare `user` — the role **this session** is running
    /// as.
    ///
    /// Folded in `crate::exec::Executor::bound`, for the reason [`Expr::CurrentDatabase`] is: the
    /// answer is a property of the session and a lowering has none, so a constant here would
    /// report whoever the node booted as to a client that had said `SET SESSION AUTHORIZATION`.
    ///
    /// **The two spellings are the same value here**, and that is a declared divergence rather
    /// than an oversight: PostgreSQL separates them at `SET ROLE`, which changes `current_user`
    /// and leaves `session_user` alone. This node has no `SET ROLE`, so nothing can make them
    /// differ — and inventing a difference would be a distinction a client could not produce.
    CurrentUser,
    /// `current_setting(name)` and `current_setting(name, missing_ok)`.
    ///
    /// Folded to a literal in `crate::exec::Executor::bound`, the way [`Expr::CurrentSchema`] is
    /// and for the same reason: the value is a property of the **session**, which the row
    /// evaluator has no handle on — and a parameter cannot change in the middle of a statement, so
    /// resolving it once per statement is not an approximation.
    CurrentSetting {
        /// The parameter's name, as written.
        name: String,
        /// `missing_ok`: the two-argument form's escape hatch. `true` answers NULL for a name the
        /// server does not know where the one-argument form raises `42704` — the documented
        /// difference, and the one shape here that must not error.
        missing_ok: bool,
    },
    /// `pg_try_advisory_lock` and the three siblings this node answers.
    ///
    /// Folded in `crate::exec::Executor::bound` the way [`Expr::CurrentSetting`] is, and for a
    /// reason those two share and this one sharpens: the answer is a property of the **session**,
    /// which the row evaluator has no handle on. Here that also fixes *how many times* it happens
    /// — once per statement — which is why a non-constant argument is refused by name rather than
    /// evaluated per row (`crate::exec::Executor::resolve_advisory`).
    Advisory {
        /// Which of the four.
        call: AdvisoryCall,
        /// The key, as one `bigint` or as two `int4`s. Whatever the arity, it is folded after
        /// parameter binding, so `pg_try_advisory_lock($1)` is a constant by the time it is read.
        args: Vec<Expr>,
    },
    /// `x IS NULL`, or `IS NOT NULL` when negated. Never NULL itself — that is the whole point of
    /// the operator, and the reason `x = NULL` is not a way to write it.
    IsNull {
        /// What is being tested.
        operand: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// `DEFAULT`, written where a value goes: `INSERT INTO t VALUES (DEFAULT, 1)` and
    /// `UPDATE t SET a = DEFAULT`.
    ///
    /// Not a value and not a literal — it is a *reference to the column's own default*, which is
    /// a constant for most columns and a `nextval` for a `bigserial` one. It therefore cannot be
    /// evaluated without knowing which column it is being written into, and the two statements
    /// that can say resolve it; anywhere else it is `0A000` naming itself, which is what a real
    /// server does too (`DEFAULT` in a `WHERE` is a syntax error there).
    Default,
    /// A sequence function — `nextval('s')`, `currval('s')`, `setval('s', 10)`, `lastval()`.
    ///
    /// **Never evaluated by the row evaluator**, and for a stronger reason than an aggregate: it
    /// has *side effects*. `nextval` is not a function of the row, it is a write, and it happens
    /// once per statement in the order the statement names it. The executor evaluates these before
    /// it plans and substitutes the values it got; one reaching a row evaluator is a planner bug.
    Sequence(Box<SequenceCall>),
    /// A `pg_catalog` function that prints a definition — `format_type(oid, typmod)`.
    ///
    /// An ordinary row function, unlike the two above it: a value of its arguments, evaluated once
    /// per row, with no side effect and nothing to resolve first. It is a variant rather than a
    /// name in a general call node because this node has no general call node — a function is
    /// either one of these, an aggregate, a sequence write, or `0A000` naming itself
    /// (`crate::parse::lower::lower_function`).
    CatalogFunc(Box<CatalogFuncCall>),
    /// A column of a row **outside** the plan this expression is in: a correlated reference.
    ///
    /// `WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)` resolves `b.a_id` to an
    /// [`Expr::Ordinal`] in the sub-plan's own row and `a.id` to one of these. It is never
    /// evaluated: before a correlated sub-plan is run for one outer row, every `Outer` in it whose
    /// `level` matches that row is replaced by the value it names, so the plan a cursor is opened
    /// on has none left (`docs/plans/phase-12-subquery.md` §1).
    Outer {
        /// How many scopes out, one-based: `1` is the row immediately outside this plan.
        ///
        /// Needed rather than implied, because a sub-plan inside a sub-plan has **two** rows
        /// outside it and both can be named — measured, a three-level `EXISTS` chain where the
        /// innermost query references the middle table and the outermost one. Substitution matches
        /// this against the depth it has descended to, which is why nothing has to be renumbered.
        level: usize,
        /// Position in that row.
        at: usize,
        /// The column's type, so a comparison against it resolves a literal the same way a
        /// comparison against a column of this row does.
        ty: ColumnType,
        /// The column's typmod, for the same reason [`Expr::Ordinal`] carries one.
        typmod: i32,
    },
    /// A subquery written where a value goes — `(SELECT …)`, `EXISTS (…)`, `x IN (SELECT …)`,
    /// `x = ANY (SELECT …)`.
    ///
    /// **Never evaluated with its `run` field empty**, for the same reason a
    /// [`crate::plan::routing::Columnar`] node is never opened unresolved: the value it carries
    /// comes from *running* a plan, which the row evaluator has no transaction to do until
    /// `crate::exec::subquery` has given it one. An unresolved one says so rather than answering
    /// "no rows", because no rows from a scalar subquery is a **NULL** and a NULL looks like an
    /// answer (`docs/plans/phase-12-subquery.md` §1).
    Subquery(Box<crate::plan::SubqueryExpr>),
    /// An aggregate call — `count(*)`, `sum(a)`, `min(DISTINCT b)`.
    ///
    /// **Never evaluated.** It is a value *of a group*, not of a row, so the executor's
    /// [`crate::exec`] row evaluator has no case for it: the planner replaces every one of these
    /// with an [`Expr::Ordinal`] into the aggregated row before the tree is built. One reaching a
    /// row evaluator is a planner bug and says so rather than returning a number.
    Aggregate(Box<AggregateCall>),
    /// A one-argument scalar function over a string.
    Scalar {
        /// Which one.
        func: ScalarFunc,
        /// Its argument.
        operand: Box<Expr>,
    },
    /// `expr::<type>` where the value is not known until there is a row, and the target is not
    /// `text`.
    ///
    /// **`text` has [`Expr::ToText`] and every other target had nothing**, so a cast folded when
    /// its operand was a constant and was `0A000` otherwise — `SELECT $1::integer` is the shape
    /// `connection_test.rb` sends, and `CURRENT_TIMESTAMP::date` is the one
    /// `tests/assignment_cast_date.rs` has carried as a declared divergence since the array unit.
    ///
    /// **Permission comes from `pg_cast`**, the same rows a client can read: a pair with no row
    /// there is `42846 cannot cast type X to Y`, which is what `'2020-01-01'::date::int` is on a
    /// real server. The conversion itself is the target's input function over the value's text,
    /// which is PostgreSQL's own I/O conversion for a cast with no binary function.
    Cast {
        /// What to cast.
        operand: Box<Expr>,
        /// The type it is being cast to.
        to: ColumnType,
        /// Its `atttypmod`, or `-1` — so `$1::varchar(3)` bounds the string the way a column of
        /// that type would.
        typmod: i32,
    },
    /// `<expr> COLLATE "C"` — the clause, **kept**.
    ///
    /// It used to be checked and dropped, on the reasoning that `C` and `POSIX` both order by byte
    /// so there was nothing for the plan to carry. Two things say otherwise, and both are measured
    /// ([ADR 0096](../../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md)):
    ///
    /// * **a stored expression prints it back.** `upper(t COLLATE "C")` deparses as
    ///   `upper((t COLLATE "C"))` on a real server and printed `upper(t)` here, so the catalog
    ///   disagreed with the statement that wrote it;
    /// * **it is what makes a collation *derivable***, and two of them that disagree are
    ///   `42P21 collation mismatch between explicit collations`. A clause that is dropped cannot
    ///   be compared with another one.
    ///
    /// The value is the operand's, unchanged — this node is about derivation and printing, never
    /// about bytes, because both collations this node has are byte order (ADR 0076).
    Collate {
        /// What the clause was written on.
        operand: Box<Expr>,
        /// The name, folded upper as [`crate::parse`] checks it: `C` or `POSIX`.
        collation: String,
    },
    /// `<expr>::text`, evaluated per row.
    ///
    /// The **output function** of whatever the operand turns out to be, which is what a cast to
    /// `text` is on a real server — `42::text` is `42` and `1.0::float8::text` is `1`, because
    /// each is what that type prints. Only `text` is a target here: every other cast in this crate
    /// is folded at plan time over a literal, and a per-row cast is needed exactly where the
    /// operand is a column.
    ///
    /// A `character(n)` is the one operand where the cast is not the identity on the stored text:
    /// it **strips** the padding, so a `char(3)` holding `x` casts to `x` and prints as `x  `.
    /// Measured, and the reason `tests/typmod.rs` could declare that pair as a divergence before
    /// this existed.
    ToText {
        /// What to cast.
        operand: Box<Expr>,
        /// Whether to strip trailing blanks, which is true for exactly one operand type.
        ///
        /// A `character(n)` stores its value **padded** to `n`, and the cast to `text` strips that
        /// padding back off: a `char(3)` holding `x` prints `x  ` and casts to `x`. Measured, and
        /// the reason `tests/typmod.rs` could only declare that pair as a divergence until now.
        /// Decided where the operand's type is known — at resolution — because a `Datum::Text`
        /// does not know it came from a `bpchar`.
        strip_blanks: bool,
        /// The labels of the **enum** the operand was declared as, or `None`.
        ///
        /// An enum column holds the `int2` of its label's position, so its output function is a
        /// catalog lookup rather than the number's own text — `current_mood::text` is `sad` and not
        /// `1`. Set at resolution beside `strip_blanks` and for the same reason: a `Datum::Int2`
        /// does not know it came from an enum, and only the scope does.
        ///
        /// Carried as a field of the cast that already exists rather than as an `Expr` variant of
        /// its own, which is what keeps every walker over this tree unchanged — a new variant that
        /// holds another expression has to be taught to two of them, and they have drifted before
        /// (ADR 0050).
        enum_labels: Option<Vec<String>>,
    },
    /// A **set-returning function in the target list**: `SELECT generate_series(1,3)`.
    ///
    /// The same call that stands where a table does ([`crate::plan::TableFunction`]), in the one
    /// other place PostgreSQL allows it — and there it does something no other expression does:
    /// **it makes rows**. `SELECT 'r', generate_series(1,3)` is three rows of `r`, and
    /// `SELECT id, unnest(tags) FROM t` is one row per element per input row. Everything beside it
    /// repeats.
    ///
    /// Two of them run **in lockstep**, not as a cross join: `generate_series(1,3),
    /// generate_series(1,2)` is three rows and the second column's third is NULL. Measured; a
    /// Cartesian reading would give six.
    ///
    /// It may sit **inside** an expression — `abs(generate_series(-1,1))` is `1, 0, 1` — so the
    /// expansion is over the whole target list and the expressions are evaluated per generated
    /// value, which is why this is a variant of [`Expr`] and not a kind of projection.
    SetFunc(Box<crate::plan::TableFunction>),
    /// `COALESCE(a, b, …)`: the first argument that is not NULL.
    ///
    /// **Not a function**, which is the first thing an implementation gets wrong: PostgreSQL has it
    /// in the grammar, so `COALESCE()` is `42601 syntax error at or near ")"` rather than the
    /// `42883` a zero-argument function would give. It is a variant here for the same reason it is
    /// a node there.
    ///
    /// Its arguments are typed exactly as a `CASE`'s results are — one common type, an `unknown`
    /// coerced to it rather than compared against it — and the only difference is the word in the
    /// message: `COALESCE types integer and text cannot be matched`.
    Coalesce(Vec<Expr>),
    /// `CASE WHEN … THEN … [WHEN … THEN …] [ELSE …] END` — the **searched** form.
    ///
    /// The one expression in this crate whose operands are *not all evaluated*, and that is
    /// observable rather than an optimisation: `CASE WHEN true THEN 1 ELSE 1/0 END` is `1` on a
    /// real server and `CASE WHEN false THEN 1 WHEN 1/0 = 0 THEN 2 ELSE 3 END` is
    /// `22012 division by zero`, because the second `WHEN` is reached and the first `ELSE` is not.
    /// So it is a variant rather than three operands of something generic — an evaluator that
    /// computed its children and then chose would get both of those wrong, in opposite
    /// directions.
    ///
    /// The **simple** form (`CASE x WHEN 1 THEN …`) is `0A000` naming itself where it is lowered.
    /// It is not this shape with a rewrite in front of it: a real server prints it back as
    /// `CASE x WHEN 1 THEN …`, so an index over one desugared into `WHEN x = 1` would store a
    /// definition `ActiveRecord` would not recognise.
    Case {
        /// The expression after `CASE`, for the **simple** form — `CASE a WHEN 1 THEN …` — or
        /// `None` for the searched one.
        ///
        /// **Kept rather than desugared.** `CASE a WHEN 1 THEN b END` and
        /// `CASE WHEN a = 1 THEN b END` compute the same value, and a real server still holds them
        /// apart: its `CaseExpr` has an `arg`, and `pg_get_expr` prints `CASE a` back. Desugaring
        /// at lowering would store a definition nobody wrote, and `pg_get_indexdef` would answer
        /// `ActiveRecord` with something it never sent. Measured,
        /// `tests/corpus/pg19_deparse_census.txt`'s group F.
        ///
        /// Each branch's `when` is then the **value to compare**, not a condition: the comparison
        /// is `operand = when`, with `=`'s own NULL behaviour and not `IS NOT DISTINCT FROM` —
        /// `CASE NULL WHEN NULL THEN 1 ELSE 2 END` is `2`, measured.
        operand: Option<Box<Expr>>,
        /// The `WHEN`/`THEN` pairs, in the order written. PostgreSQL's grammar has no empty one.
        branches: Vec<CaseBranch>,
        /// The `ELSE`, or `None` when it was not written — which is a NULL of the resolved type
        /// and not an error (`CASE WHEN false THEN 'a' END` is NULL).
        otherwise: Option<Box<Expr>>,
    },
}

/// One `WHEN … THEN …` of a [`Expr::Case`].
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    /// The condition. Must be `boolean` — anything else is
    /// `42804 argument of CASE/WHEN must be type boolean, not type …`, checked where the
    /// expression is resolved rather than when a row reaches it.
    pub when: Expr,
    /// What the `CASE` is worth when that condition is **true**. NULL and false are both "not
    /// this branch", which is why the check is against `true` and not against "not false".
    pub then: Expr,
}
/// The two functions that make a UUID, and the difference between them is where they live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UuidFunc {
    /// `gen_random_uuid()` — **in core since PostgreSQL 13**, so it answers with no extension
    /// installed. `pgcrypto` is where it used to live and is still where `ActiveRecord` expects to
    /// find it; measured, it works with `pg_extension` empty of both names.
    GenRandomUuid,
    /// `uuid_generate_v4()` — `uuid-ossp`'s, and **not** in core: it is
    /// `42883 function uuid_generate_v4() does not exist` until that extension is installed, and
    /// again as soon as the transaction that installed it rolls back. Measured, both directions.
    UuidGenerateV4,
    /// `uuid_generate_v1()` — a **timestamp, a clock sequence and a node id**, where v4 is
    /// sixteen random bytes. `uuid-ossp`'s, like v4, and `42883` until that extension is there.
    ///
    /// `uuid_test.rb` writes it as a column default and reads it back out of the schema dumper,
    /// which is why the name has to survive `pg_get_expr` unchanged as well as evaluate.
    UuidGenerateV1,
    /// `uuid_generate_v1mc()` — the same, with a **fresh random multicast node id per call**
    /// where the plain form keeps one. Measured: its node bytes differ between two calls and the
    /// plain form's do not.
    UuidGenerateV1Mc,
    /// `uuid_nil()` — the all-zero UUID, and a constant.
    UuidNil,
    /// `uuid_ns_dns()` — RFC 4122's DNS namespace, a constant.
    ///
    /// The four namespace constants are here and `uuid_generate_v3`/`v5` are not: those two hash
    /// a namespace and a name with **MD5** and **SHA-1**, which this project would write itself
    /// (no crate compiles C here) and which is a unit of its own. The constants cost nothing and
    /// are what that unit would need first.
    UuidNsDns,
    /// `uuid_ns_url()` — RFC 4122's URL namespace, a constant.
    UuidNsUrl,
    /// `uuid_ns_oid()` — RFC 4122's ISO OID namespace, a constant.
    UuidNsOid,
    /// `uuid_ns_x500()` — RFC 4122's X.500 DN namespace, a constant.
    UuidNsX500,
}

impl UuidFunc {
    /// What it is called.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            UuidFunc::GenRandomUuid => "gen_random_uuid",
            UuidFunc::UuidGenerateV4 => "uuid_generate_v4",
            UuidFunc::UuidGenerateV1 => "uuid_generate_v1",
            UuidFunc::UuidGenerateV1Mc => "uuid_generate_v1mc",
            UuidFunc::UuidNil => "uuid_nil",
            UuidFunc::UuidNsDns => "uuid_ns_dns",
            UuidFunc::UuidNsUrl => "uuid_ns_url",
            UuidFunc::UuidNsOid => "uuid_ns_oid",
            UuidFunc::UuidNsX500 => "uuid_ns_x500",
        }
    }

    /// The function a name is, if it is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "gen_random_uuid" => Some(UuidFunc::GenRandomUuid),
            "uuid_generate_v4" => Some(UuidFunc::UuidGenerateV4),
            "uuid_generate_v1" => Some(UuidFunc::UuidGenerateV1),
            "uuid_generate_v1mc" => Some(UuidFunc::UuidGenerateV1Mc),
            "uuid_nil" => Some(UuidFunc::UuidNil),
            "uuid_ns_dns" => Some(UuidFunc::UuidNsDns),
            "uuid_ns_url" => Some(UuidFunc::UuidNsUrl),
            "uuid_ns_oid" => Some(UuidFunc::UuidNsOid),
            "uuid_ns_x500" => Some(UuidFunc::UuidNsX500),
            _ => None,
        }
    }

    /// The extension that has to be installed for this function to exist, or `None` for the one
    /// that is in core.
    #[must_use]
    pub fn requires(self) -> Option<&'static str> {
        match self {
            UuidFunc::GenRandomUuid => None,
            // Every other name in this enum is `uuid-ossp`'s, the constants included:
            // `uuid_nil()` is `42883` on a real server until the extension is installed.
            UuidFunc::UuidGenerateV4
            | UuidFunc::UuidGenerateV1
            | UuidFunc::UuidGenerateV1Mc
            | UuidFunc::UuidNil
            | UuidFunc::UuidNsDns
            | UuidFunc::UuidNsUrl
            | UuidFunc::UuidNsOid
            | UuidFunc::UuidNsX500 => Some("uuid-ossp"),
        }
    }
}

/// The scalar functions this node has, all of them one argument over a string.
///
/// Each maps to Rust's own case conversion, which is full Unicode: `upper('àéî')` is `ÀÉÎ`, the
/// same as PostgreSQL's under a UTF-8 locale. That agreement is measured rather than assumed —
/// `tests/corpus/pg19_lower.txt` has the accented pair — and it is the reason these two could be
/// added without a collation, which is the thing this project has decided it will not link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `lower(text)`.
    Lower,
    /// `upper(text)`.
    Upper,
    /// `abs(numeric type)` — the one scalar function whose result type is its **argument's**,
    /// and whose failure is an overflow: `abs((-32768)::int2)` is `22003`, because the positive
    /// of the smallest `int2` is not one.
    Abs,
    /// `reverse(text)`: the characters back to front. Text in, text out.
    Reverse,
    /// `ascii(text)`: the code point of the **first** character, as an `int4`. An empty string
    /// is `0`.
    Ascii,
    /// `length(x)`, and it is **not** the same function as `char_length`.
    ///
    /// Eight `pg_proc` rows over four names on 19beta1, and they do not agree: `length` counts
    /// **characters** for a string, **bits** for a `bit`, **bytes** for a `bytea`, **lexemes** for
    /// a `tsvector`, and answers a **`double precision`** for an `lseg` or a `path` — the
    /// geometric length. `char_length` has two overloads and neither is any of those.
    Length,
    /// `char_length(x)`: **characters**, and only over `text` and `character`.
    CharLength,
    /// `character_length(x)`: the same two overloads under the other spelling, kept apart because
    /// a `42883` names the spelling the caller wrote.
    CharacterLength,
    /// `octet_length(text)`: **bytes**, which is a different number for anything non-ASCII — the
    /// pair is only interesting because the suite's generated columns use both.
    OctetLength,
    /// `bit_length(x)`: **bits**, and its three `pg_proc` rows are `(bit)`, `(bytea)` and `(text)`.
    ///
    /// Over a string or a `bytea` it is `octet_length` times eight; over a `bit` or a `varbit` it
    /// is the bits themselves, which is `length`'s answer and not `octet_length`'s. Measured on
    /// 19beta1 (`tests/captures/pg19_length_overloads.txt`): `bit_length('abc')` is 24,
    /// `bit_length('1'::bit)` is 1, `bit_length('\x0102'::bytea)` is 16.
    ///
    /// **It has no `(character)` row**, which is the detail that separates it from `octet_length`:
    /// a `character(5)` reaches it through the coercion to `text`, and that coercion **trims the
    /// trailing blanks** — `bit_length('ab'::character(5))` is 16 where `octet_length` of the same
    /// value is 5. Measured, and it is why this one is not exempted from `read_as_text`.
    BitLength,
}

impl ScalarFunc {
    /// The name a `42883` spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ScalarFunc::Lower => "lower",
            ScalarFunc::Upper => "upper",
            ScalarFunc::Abs => "abs",
            ScalarFunc::Reverse => "reverse",
            ScalarFunc::Ascii => "ascii",
            ScalarFunc::Length => "length",
            ScalarFunc::CharLength => "char_length",
            ScalarFunc::CharacterLength => "character_length",
            ScalarFunc::OctetLength => "octet_length",
            ScalarFunc::BitLength => "bit_length",
        }
    }

    /// The function a name is, if it is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "lower" => Some(ScalarFunc::Lower),
            "upper" => Some(ScalarFunc::Upper),
            "abs" => Some(ScalarFunc::Abs),
            "reverse" => Some(ScalarFunc::Reverse),
            "ascii" => Some(ScalarFunc::Ascii),
            "length" => Some(ScalarFunc::Length),
            "char_length" => Some(ScalarFunc::CharLength),
            "character_length" => Some(ScalarFunc::CharacterLength),
            "octet_length" => Some(ScalarFunc::OctetLength),
            "bit_length" => Some(ScalarFunc::BitLength),
            _ => None,
        }
    }
}

/// Every arity `concat` accepts: one argument up to a limit no statement reaches.
///
/// A list rather than a range because [`CatalogFunc::arities`] answers a set, and a variadic
/// function is the one member whose set is not two or three numbers long.
const CONCAT_ARITIES: [usize; 100] = {
    let mut arities = [0usize; 100];
    let mut at = 0;
    while at < 100 {
        arities[at] = at + 1;
        at += 1;
    }
    arities
};

/// Whether `subject` matches `pattern` under SQL's `LIKE` rules.
///
/// `%` spans any run of characters including none, `_` is exactly one, and `escape` quotes either
/// back into an ordinary character.
///
/// Iterative with one backtrack point rather than recursive: a pattern is user input, and
/// `%a%a%a%…` against a long subject is the shape that turns the natural recursion into a stack
/// overflow — which `CLAUDE.md`'s "never panic on user input" rules out.
#[must_use]
pub fn like_matches(subject: &[char], pattern: &[char], escape: Option<char>) -> bool {
    // One pattern element: a literal character to match, or `None` for `_`, which matches any.
    let element = |at: usize| -> (Option<char>, usize) {
        match pattern[at] {
            c if Some(c) == escape && at + 1 < pattern.len() => (Some(pattern[at + 1]), 2),
            '_' => (None, 1),
            c => (Some(c), 1),
        }
    };
    let (mut s, mut p) = (0, 0);
    // Where to resume when the tail after the last `%` turns out not to match from here.
    let (mut star_p, mut star_s) = (None, 0);
    loop {
        if p < pattern.len() && pattern[p] == '%' {
            star_p = Some(p);
            p += 1;
            star_s = s;
            continue;
        }
        let matched = s < subject.len()
            && p < pattern.len()
            && matches!(element(p), (wanted, _) if wanted.is_none_or(|c| c == subject[s]));
        if matched {
            p += element(p).1;
            s += 1;
            continue;
        }
        if s == subject.len() {
            // The subject is spent: what is left of the pattern must be all `%`.
            return pattern[p..].iter().all(|&c| c == '%');
        }
        // Not a match here, and there is subject left: give the last `%` one more character.
        let Some(star) = star_p else {
            return false;
        };
        star_s += 1;
        if star_s > subject.len() {
            return false;
        }
        p = star + 1;
        s = star_s;
    }
}

/// `current_setting('x')` or `current_setting('x', true)`, as the call was written.
///
/// Its own function because two printers need the same two spellings — the plan's and the one
/// `ALTER TABLE` deparses a stored default with — and because keeping it out of either match keeps
/// both under the line limit.
#[must_use]
pub fn current_setting_text(name: &str, missing_ok: bool) -> String {
    if missing_ok {
        format!("current_setting('{name}', true)")
    } else {
        format!("current_setting('{name}')")
    }
}

/// One call to a `pg_catalog` function that prints a definition.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogFuncCall {
    /// Which function.
    pub func: CatalogFunc,
    /// Its arguments, in the order written. The arity is checked where the call is lowered, so
    /// the evaluator can read them by position.
    pub args: Vec<Expr>,
}

impl CatalogFuncCall {
    /// Whether this call is a function of its arguments alone, the way PostgreSQL's `provolatile`
    /// means it — which is what decides whether it may be an **index key**.
    ///
    /// **One rule covers the text-search family, and it is the argument count.** A function that
    /// names its configuration is `IMMUTABLE`; the form that omits it is `STABLE`, because the
    /// omitted one reads `default_text_search_config`, a session setting. Measured against
    /// 19beta1's `pg_proc`, every pair:
    ///
    /// ```text
    /// to_tsvector(regconfig, text)  i     to_tsvector(text)  s
    /// to_tsquery(regconfig, text)   i     to_tsquery(text)   s
    /// plainto_tsquery(…)            i     plainto_tsquery    s
    /// phraseto_tsquery(…)           i     phraseto_tsquery   s
    /// websearch_to_tsquery(…)       i     websearch_to_tsquery s
    /// ts_headline(regconfig, …)     i     ts_headline(text, tsquery) s
    /// ```
    ///
    /// The ones with no configuration argument at all are immutable outright — `strip`,
    /// `setweight`, `ts_rank`, and `@@` (`ts_match_vq`), all measured `i`.
    ///
    /// **Everything else answers `false`**, which is the conservative direction and the behaviour
    /// this crate had for every catalog function before: an index whose key is not a function of
    /// the row is not a slow index, it is a wrong one. A function measured immutable is added
    /// here; nothing is added by reasoning.
    ///
    /// **And the measurement is now the whole table rather than the name in front of the reader.**
    /// `tests/captures/pg19_provolatile_census.txt` is `pg_proc.provolatile` for all 79 names
    /// [`CatalogFunc::from_name`] and [`ScalarFunc::from_name`] resolve, in one query; the arms
    /// below carry what it said. Before that, this list had grown one family per defect four times
    /// over, and every one of those commits had the query in front of it.
    #[must_use]
    pub fn is_immutable(&self) -> bool {
        match self.func {
            // The five that take `(config, …)` or `(…)`, immutable in the first spelling only.
            CatalogFunc::ToTsVector
            | CatalogFunc::ToTsQuery
            | CatalogFunc::PlainToTsQuery
            | CatalogFunc::PhraseToTsQuery
            | CatalogFunc::WebsearchToTsQuery => self.args.len() == 2,
            // `ts_headline(config, text, query)` against `ts_headline(text, query)`.
            CatalogFunc::TsHeadline => self.args.len() == 3,
            // No configuration to omit.
            CatalogFunc::TsStrip
            | CatalogFunc::SetWeight
            | CatalogFunc::TsRank
            | CatalogFunc::TsMatch
            // The three string functions, `i` on a real server — measured with the rest of the
            // family rather than assumed, since `concat` next door is `STABLE`.
            | CatalogFunc::SplitPart
            | CatalogFunc::StringToArray
            | CatalogFunc::StrPos
            | CatalogFunc::Btrim
            | CatalogFunc::Ltrim
            | CatalogFunc::Rtrim
            | CatalogFunc::Greatest
            | CatalogFunc::Least
            // `NULLIF` is a comparison, and a comparison between two immutable operands is one:
            // measured, `CREATE INDEX i ON t ((nullif(t, 'x')))` is built by a real server and
            // `pg_get_indexdef` prints `btree (NULLIF(t, 'x'::text))`.
            | CatalogFunc::NullIf
            // `provolatile = 'i'`, measured with the rest of the census.
            | CatalogFunc::Mod
            | CatalogFunc::Substr
            | CatalogFunc::Substring
            // **The JSON accessors, `i` on a real server** — measured through `pg_operator`
            // rather than assumed from the family, because `concat` two lines up is `STABLE` and
            // the guess would have gone the other way: `->`, `->>` and `||` over `json`/`jsonb`
            // all resolve to functions whose `provolatile` is `i`. It is what
            // `invertible_migration_test` builds a GIN index over —
            // `add_index :settings, "(data->'foo')", using: :gin` — which a real server creates
            // and this node refused as not immutable.
            | CatalogFunc::JsonFetch
            | CatalogFunc::JsonbFetch
            | CatalogFunc::JsonFetchText
            | CatalogFunc::JsonbConcat
            // **And `||` over text, which is the same family's fifth member and was the one left
            // out.** The comment above says "`->`, `->>` and `||` over `json`/`jsonb`" and stopped
            // there; `text || text` is `textcat`, whose `provolatile` is `i` — measured off
            // `pg_operator` beside `concat`'s `s`, which is the distinction that makes this a list
            // and not a rule about names. `CatalogFunc::HstoreConcat` is what `parse::lower` makes
            // of every `||` that is not a document merge, hstore and text alike.
            //
            // What it cost: `GENERATED ALWAYS AS (t || 'x') STORED` was `42P17 functions in index
            // expression must be marked IMMUTABLE` for a column a real server creates — measured,
            // and `length(t || 'x')` with it.
            | CatalogFunc::HstoreConcat
            // **And then the whole table was measured at once instead of a twelfth name.**
            // Everything above arrived one family per defect, and each time the reasoning that
            // added one name would have added the ones below it. #25's corpus refused
            // `replace(t, 'a', 'b')` in a generated column -- `42P17`, for a call whose
            // `provolatile` is `i` -- and the answer to that is not `Replace`. It is
            // `tests/captures/pg19_provolatile_census.txt`: `pg_proc.provolatile` for all 79
            // names `from_name` and `ScalarFunc::from_name` resolve, in one query. Twenty-two of
            // them were `i` on the oracle and `false` here, and they are these.
            //
            // *The string function the corpus found.* One name, and the reason the other
            // twenty-one are in this commit rather than in a later one.
            | CatalogFunc::Replace
            // *Array introspection.* A length, a bound and a search over an array value: no
            // catalog read, no setting, no clock.
            | CatalogFunc::ArrayLength
            | CatalogFunc::ArrayLower
            | CatalogFunc::ArrayUpper
            | CatalogFunc::ArrayPosition
            | CatalogFunc::Cardinality
            // *Range introspection and the two constructors this node resolves.* `isempty`,
            // `lower_inc`/`lower_inf`, `upper_inc`/`upper_inf` read the range value's own flags;
            // `daterange(a, b)` and `tsrange(a, b)` build one from their arguments. All `i`, both
            // arities.
            | CatalogFunc::IsEmpty
            | CatalogFunc::RangeLowerInc
            | CatalogFunc::RangeLowerInf
            | CatalogFunc::RangeUpperInc
            | CatalogFunc::RangeUpperInf
            | CatalogFunc::DateRange
            | CatalogFunc::RangeBuild
            // *The two path predicates*, which are a property of the geometry and nothing else.
            | CatalogFunc::PathIsClosed
            | CatalogFunc::PathIsOpen
            // *hstore's accessors and its constructor.* Measured in a throwaway database with the
            // extension installed, which is the second half of the capture: every `hstore`
            // overload is `i`, **`hstore(record)` included** -- the one that looks like it should
            // not be, since a record's shape comes from a relation.
            | CatalogFunc::HstoreAkeys
            | CatalogFunc::HstoreAvals
            | CatalogFunc::HstoreBuild
            // *`ltree`'s depth and its two text conversions*, and `numnode(tsquery)` beside them:
            // all four are arithmetic on the value's own bytes.
            | CatalogFunc::LtreeNlevel
            | CatalogFunc::LtreeToText
            | CatalogFunc::TextToLtree
            | CatalogFunc::NumNode => true,
            // **Everything else answers `false`**, and after the census that is a measurement
            // too: of the 79 names, `concat`, `convert_to`, `format_type`, `pg_typeof`,
            // `to_regclass`, `pg_encoding_to_char`, the seven `pg_get_*` printers, the two
            // `*_description` readers, `now`/`current_date`/`localtime`/`localtimestamp` and the
            // one-argument text-search forms are `s`; `random`, `clock_timestamp`, `pg_sleep`,
            // `pg_backend_pid`, `pg_cancel_backend` and `pg_terminate_backend` are `v`.
            //
            // `date_trunc` is the one that is **both**, and it stays here for a reason worth
            // writing down rather than leaving as an omission: `date_trunc(text, timestamp)` and
            // `date_trunc(text, interval)` are `i`, `date_trunc(text, timestamptz)` is `s`
            // because truncating an absolute instant needs `TimeZone`, and the three-argument
            // form that names the zone is `i` again. The discriminator is the *argument type*,
            // and `self.args` here are unresolved `Expr`s with no types on them -- so the
            // conservative answer is the only honest one this function can give.
            // `tests/index_expression_volatility.rs` pins that refusal as deliberate.
            _ => false,
        }
    }
}

/// The `pg_catalog` functions this node answers.
///
/// Every one of them is read-only and is a function of its arguments alone — the two properties
/// that let them be evaluated beside a row rather than planned. Most return `text`; the array
/// operators return `integer` and `'x'::regclass` a `bigint`, which is what [`Self::result_type`]
/// is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogFunc {
    /// `format_type(oid, typmod)`: the name a client is shown for a type and a modifier.
    ///
    /// The inverse of `'x'::regtype`, and the more permissive half: that one is `42601` for a
    /// typmod on a type that takes none, and this one ignores it
    /// ([`crate::catalog::def_functions`]).
    FormatType,
    /// `pg_get_expr(expr, relid)` and `pg_get_expr(expr, relid, pretty)`: a stored expression,
    /// printed.
    ///
    /// On a real server the first argument is a `pg_node_tree` and this parses and re-prints it.
    /// Here `pg_attrdef.adbin` **holds the printed text already**, so this is the identity on it —
    /// a divergence visible in exactly one statement (`SELECT adbin`) and in none `ActiveRecord`
    /// writes, because the only way it reads that column is through this function.
    PgGetExpr,
    /// `pg_get_indexdef(oid)`, `pg_get_indexdef(oid, column, pretty)`: an index's `CREATE INDEX`.
    ///
    /// Unlike `'x'::regclass` its argument is a **column** — `pg_get_indexdef(d.indexrelid)` over
    /// every row of `pg_index` — so it really does answer differently per row and cannot be
    /// resolved before the plan. The catalog it reads is snapshotted once per cursor rather than
    /// once per row (`crate::exec::cursor::Env::relations`).
    PgGetIndexdef,
    /// `pg_get_constraintdef(oid)`, `pg_get_constraintdef(oid, pretty)`: a constraint, printed.
    ///
    /// Like `pg_get_indexdef` its argument is a column, and like it, an oid that names no
    /// constraint is NULL rather than an error.
    PgGetConstraintdef,
    /// `pg_get_viewdef(oid)`, `pg_get_viewdef(oid, pretty)`: a view's `SELECT`, printed.
    ///
    /// **This node prints the definition as it was stored, not PostgreSQL's reconstruction.** A
    /// real server deparses its own parse tree — re-cased, re-qualified, re-indented, every
    /// expression parenthesised and a semicolon on the end — and reproducing that is `ruleutils.c`
    /// rather than a function. What is returned here is the same text `pg_views.definition`
    /// already returns, which is the query the user wrote; the *content* agrees and the layout
    /// does not. Declared in `tests/view_debts.rs` (ADR 0031: nothing in the suite parses the
    /// deparser's layout).
    PgGetViewdef,
    /// `pg_encoding_to_char(int)`: an encoding number's name.
    ///
    /// **6 is `UTF8`**, which is the only encoding this node speaks and the only number
    /// `pg_database.encoding` ever carries here. Every other number is the empty string, which is
    /// what a real server answers for one that names no encoding — not an error.
    PgEncodingToChar,
    /// `pg_get_serial_sequence(table, column)`: the sequence a column's default draws from.
    ///
    /// **Schema-qualified `text`** — `public.posts_id_seq` — where the name `ActiveRecord` then
    /// passes to `setval` is the quoted bare name. Both resolve, and the qualified spelling is
    /// what a real server returns. Its arguments are *names*, not oids, which is what makes it the
    /// one catalog function here that looks a relation up by name at evaluation time.
    PgGetSerialSequence,
    /// `col_description(oid, attnum)`: a column's comment.
    ///
    /// **Always NULL here, and that is the answer rather than a stub**: a comment is a row in
    /// `pg_description`, `COMMENT ON` is `0A000` naming itself, and a server with no comments has
    /// none to return. A real server answers NULL for every one of these too — an uncommented
    /// column, an attnum out of range, a negative one, an oid that names nothing, and a NULL
    /// argument are all NULL there. The one subscript that is neither out of range nor a column
    /// is **`0`, which returns the *table's* comment** — `pg_description` keys a table comment as
    /// `objsubid = 0` and this function does not special-case it. Measured; it costs nothing here
    /// and it is the value most likely to be passed by accident.
    ColDescription,
    /// `pg_sleep(seconds)`: waits, and answers the empty string a `void` prints as.
    ///
    /// **Wanted for what can interrupt it, not for what it does.** It is how a client makes a
    /// statement that is *working* rather than waiting, which is the only way to test that
    /// `statement_timeout` and a cancel reach a statement at all — `transaction_test.rb` uses it
    /// for exactly that. So it sleeps in short steps and checks `crate::exec::cancel` between
    /// them, and a `pg_sleep` that could not be cut short would be worse than not having one.
    PgSleep,
    /// `pg_cancel_backend(pid)`: asks the session at `pid` to stop its statement, and answers
    /// whether there was one to ask.
    ///
    /// **`false` for an unknown pid, and no `WARNING` with it — a declared divergence.** PostgreSQL
    /// says `WARNING: PID 999999 is not a PostgreSQL backend process` beside the `false` (measured
    /// against PG19). The expression evaluator here has no channel to raise a notice on, and giving
    /// it one is a wider change than this function; the boolean, which is what a caller branches
    /// on, is the same.
    PgCancelBackend,
    /// `pg_terminate_backend(pid)`: ends the *session* at `pid`, and answers whether there was one
    /// to end.
    ///
    /// The pair to `pg_cancel_backend` and not a louder version of it: a cancellation stops the
    /// statement and the session carries on, while this closes the connection — the victim's next
    /// message is answered `57P01 terminating connection due to administrator command` and the
    /// socket goes. Two Rails tests turn on exactly that difference; both terminate a connection
    /// and then use it, expecting to be told it is gone.
    ///
    /// **`false` for an unknown pid, and no `WARNING` with it — the same declared divergence as
    /// `PgCancelBackend` above**, for the same reason and closed by the same change: `Env` has no
    /// notice channel, so one channel would serve both.
    PgTerminateBackend,
    /// `pg_backend_pid()`: the pid of the session asking.
    ///
    /// Wanted because `pg_stat_activity` now lists **every** session in the process, so "my
    /// row" is no longer "the only row" — a client that wants its own pid has to be able to
    /// say so, and on a real server this is how.
    PgBackendPid,
    /// `obj_description(oid)` and `obj_description(oid, catalog)`: an object's comment.
    ///
    /// NULL for the same reason. The catalog-name argument is **not validated** on a real server —
    /// `obj_description(oid, 'nosuchcatalog')` is NULL rather than an error, because the name
    /// filters `pg_description.classoid` and one that matches nothing matches nothing.
    ObjDescription,
    /// `pg_get_partkeydef(oid)`: a partitioned table's key, as `RANGE (r)` / `LIST (s)` /
    /// `HASH (id, r)`.
    ///
    /// NULL here because this node has no partitioned tables — `PARTITION BY` is `0A000` naming
    /// itself — and NULL is what a real server answers for a table that is not partitioned, for
    /// an **index**, and for an oid that names nothing. Measured all three.
    PgGetPartkeydef,
    /// `daterange(low, high)`: a half-open range of dates, as text.
    ///
    /// A **function** rather than a type constructor, because a range reaches this node only as an
    /// expression (`crate::value::range`). The declared type is `text` where a real server says
    /// `daterange`, which is the standing trade and a declared divergence.
    DateRange,
    /// `isempty(range)`: whether a range contains no day at all.
    IsEmpty,
    /// `isopen(path)` and `isclosed(path)`: **which bracket the path has**, and that is the whole
    /// of it — a `path` written `[…]` is open and one written `(…)` is closed, so the answer is
    /// in the canonical text rather than in the geometry. `geometric_test.rb` reads both.
    PathIsOpen,
    /// See [`CatalogFunc::PathIsOpen`].
    PathIsClosed,
    /// `a && b`: whether two ranges share a day.
    ///
    /// Written as an operator and carried as a call, because it is not a comparison — `pg_cmp` says
    /// nothing about two ranges, and every walker already descends into a call's arguments.
    RangeOverlaps,
    /// `h -> k`: the value a key has, or NULL for one the hstore does not hold.
    ///
    /// Carried as a call for the same reason `&&` is: it is an operator that is not a comparison,
    /// and every walker already descends into a call's arguments.
    HstoreFetch,
    /// `h ? k`: whether the hstore holds the key, **including one whose value is NULL**.
    HstoreHasKey,
    /// `jsonb_compare(a, b)`: where `a` sorts against `b`, as `-1`, `0` or `1`.
    ///
    /// **Not an operator a user can write** — it is what a `jsonb` comparison becomes, so that all
    /// six of them are one implementation and the ordinary `int4` comparison does the rest:
    /// `a < b` is `jsonb_compare(a, b) < 0`. `jsonb` shares `text`'s representation, so by
    /// evaluation there is nothing to say these two are documents; carrying the intent in the call
    /// is what ADR 0042 left open, and it needs no `Datum` of its own because
    /// `crate::value::json` already canonicalises on the way in.
    JsonbCompare,
    /// `jsonb_contains(a, b)`: whether `a` contains `b`, as `@>` means for a document.
    ///
    /// Not an operator a user can write, for the same reason [`Self::JsonbCompare`] is not: `@>`
    /// is spelled the same for an hstore, a range and a document, and by evaluation a `jsonb` is a
    /// `Datum::Text` with nothing to say which it is. `<@` is this with the operands the other way
    /// round — measured, not assumed.
    JsonbContains,
    /// `polygon_contains(a, b)`: whether the polygon `a` contains the polygon `b`, as `@>` means
    /// between two of them. `<@` is this with the operands the other way round.
    PolygonContains,
    /// `polygon_overlaps(a, b)`: `&&` between two polygons, which **includes touching** — two
    /// squares sharing only an edge or only a vertex overlap. Measured.
    PolygonOverlaps,
    /// `a ~= b`: whether two geometric values are the same.
    ///
    /// **Carried rather than refused at lowering, so that it can be refused with a type.** A
    /// `point` and a `polygon` have this operator on a real server and the document types do not,
    /// which is the *opposite* of how those two groups divide for every other operator here — so
    /// the answer depends on the operand and the operand is not known until resolution. Refusing
    /// it in the parser said `0A000 the operator ~=` for `json`, where a real server says
    /// `42883 operator does not exist: json ~= json`.
    ///
    /// This node implements it for nothing yet: `exec::cursor` still answers `0A000` for the two
    /// types that *do* have it, which is unchanged behaviour and its own row of the census.
    SameAs,
    /// `a @> b`: whether every pair of `b` is in `a`.
    HstoreContains,
    /// `a || b`: the two hstores merged, **the right winning a shared key** — which is the
    /// opposite of what a repeated key inside one literal does (`crate::value::hstore`).
    HstoreConcat,
    /// `jsonb || jsonb`: **document merge**, which is a different operator from every other
    /// spelling of `||` and not a concatenation at all.
    ///
    /// Its own variant because the operand cannot decide it: `jsonb` is stored canonicalised as a
    /// `Datum::Text` (`value::json::canonicalise`), so by the time the evaluator holds two values
    /// a document and a string are the same bytes. The **lowerer** emits this when a cast says
    /// `jsonb`, and a jsonb *column* is caught in the evaluator by its `Expr::Ordinal` type —
    /// the two places the declared type still exists.
    JsonbConcat,
    /// `akeys(h)` and `avals(h)`: the keys and the values as `text[]`, in canonical order.
    HstoreAkeys,
    /// See [`CatalogFunc::HstoreAkeys`].
    HstoreAvals,
    /// `hstore(k, v)` and `hstore(keys[], vals[])`: the two constructors the adapter reaches for.
    HstoreBuild,
    /// `to_tsvector([config,] text)`: text into a sorted, deduplicated lexeme set.
    ///
    /// One argument uses `default_text_search_config`, which this node reports and honours as
    /// `pg_catalog.english`.
    ToTsVector,
    /// `to_tsquery([config,] text)`: the tsquery grammar, with each lexeme put through the
    /// configuration exactly as a vector's is — which is what lets `@@` meet a vector at all.
    ToTsQuery,
    /// `plainto_tsquery([config,] text)`: every word `AND`ed, in the order they were written.
    PlainToTsQuery,
    /// `phraseto_tsquery([config,] text)`: every word joined by `<->`, so the order is part of
    /// the question rather than incidental.
    PhraseToTsQuery,
    /// `tsvector @@ tsquery`, and `tsquery @@ tsvector`: **both argument orders exist** and the
    /// evaluator tells them apart by what it is given, measured.
    TsMatch,
    /// `strip(tsvector)`: the lexemes without their positions or weights.
    TsStrip,
    /// `setweight(tsvector, "A")`: one weight over every position.
    SetWeight,
    /// `numnode(tsquery)`: the nodes in the tree, operators included — `'fat' & 'cat'` is **3**.
    NumNode,
    /// `websearch_to_tsquery([config,] text)`: the search-box syntax — quoted phrases, a
    /// leading `-` for negation, and the bare word `or`.
    WebsearchToTsQuery,
    /// `ts_rank(tsvector, tsquery)`: how well a vector answers a query, as a `real`.
    TsRank,
    /// `ts_headline(config, text, query)`: the text with every matching token wrapped in `<b>`.
    TsHeadline,
    /// `lower_inc(range)`, `upper_inc`, `lower_inf`, `upper_inf`: the four bracket questions.
    ///
    /// `lower`/`upper` are **not** here — they are the text functions of the same name, overloaded
    /// on a range operand, which is how a real server spells them too.
    RangeLowerInc,
    /// See [`CatalogFunc::RangeLowerInc`].
    RangeUpperInc,
    /// `'<name>'::regtype` and `'<name>'::regtype::oid` where the **catalog** is what knows the
    /// name: a type a `CREATE TYPE` made.
    ///
    /// Carried rather than answered for the reason [`CatalogFunc::UserCast`] is — lowering has no
    /// catalog — and resolved once per statement in the same pass (ADR 0053). The second argument
    /// says which half of a `regtype` was asked for: `true` for the **oid**, which is what
    /// `ActiveRecord`'s `lookup_cast_type` writes (`SELECT 'color'::regtype::oid`), and `false`
    /// for the name it prints as.
    UserRegType,
    /// See [`CatalogFunc::RangeLowerInc`]. **True for an absent bound, and that is the whole
    /// distinction from `-infinity`**, which is a *value* and answers false.
    RangeLowerInf,
    /// See [`CatalogFunc::RangeLowerInc`].
    RangeUpperInf,
    /// `a @> b` and `b <@ a` over ranges: whether one contains the other, or a bare value.
    RangeContains,
    /// `tsrange(lower, upper)` and `tsrange(lower, upper, '[]')`.
    RangeBuild,
    /// `pg_get_triggerdef(oid)`: a trigger's `CREATE TRIGGER`, re-printed.
    ///
    /// **It normalises `EXECUTE PROCEDURE` to `EXECUTE FUNCTION`**, so the text that comes out is
    /// not the text that went in — statement 762 writes the first spelling and 790 the second, and
    /// only the second is ever printed.
    PgGetTriggerdef,
    /// `'name'::regclass`: the oid of a relation, by name.
    ///
    /// Not a function a client can call by that name — it is the cast, lowered to one, because a
    /// cast that has to look a name up in the catalog is a function of the catalog and not of the
    /// text. **Resolved before the plan is built** (`crate::exec::Executor::bound`), the way a
    /// sequence call is: once per statement, not once per row, or a `WHERE attrelid =
    /// 'x'::regclass` would read the catalog for every row it filtered.
    RegClass,
    /// `<oid>::regclass`: the **name** of the relation an oid names — the inverse of
    /// [`CatalogFunc::RegClass`], and the direction `ActiveRecord`'s `foreign_keys()` reads a
    /// referenced table with (`t2.oid::regclass::text`).
    ///
    /// Unlike its inverse this is a **per-row** call: its argument is a column, so the answer
    /// differs per row and cannot be resolved before the plan. The catalog it reads is the
    /// cursor's snapshot, the same one `pg_get_indexdef` uses.
    ///
    /// **An oid that names nothing is not an error.** It prints the number back —
    /// `2147483647::regclass::text` is `2147483647` — and oid **0** prints `-`, which is
    /// PostgreSQL's rendering of `InvalidOid`. Measured, both; an implementation that raised would
    /// break a `LEFT JOIN` that legitimately has no match.
    RegClassName,
    /// The inverse of `'x'::regtype`: an **oid**, read per row, answered as the type's printed
    /// name. `ActiveRecord`'s array probe writes `t.typelem::regtype`, where the operand is a
    /// catalog column and not a literal.
    ///
    /// The same three answers `RegClassName` gives, measured the same way: a type's name, `-` for
    /// oid **0** — PostgreSQL's rendering of `InvalidOid`, which every non-array row of `pg_type`
    /// has in `typelem` — and the number back for an oid this node does not know.
    RegTypeName,
    /// `doc -> key` over a **`json`**: one member, as a `json`.
    ///
    /// Its own variant beside [`Self::JsonbFetch`] because the two answer different declared
    /// types — `pg_typeof(doc->'a')` is `json` for a `json` column and `jsonb` for a `jsonb` one,
    /// measured — and the *value* cannot say which, both being a `Datum::Text`.
    JsonFetch,
    /// `doc -> key` over a `jsonb`: one member, **as a document**.
    ///
    /// Its own variant rather than `HstoreFetch`'s because `->` means two things and the values
    /// cannot tell them apart — a `jsonb` is a `Datum::Text` here, canonicalised, and so is a
    /// string. This is the spelling a **cast** produced, where the lowerer could still read the
    /// type; a jsonb *column* reaches `HstoreFetch` and is told apart there by `Expr::Ordinal`'s
    /// declared type, exactly as `||` is.
    JsonbFetch,
    /// `doc ->> key`: the same member **as text**.
    ///
    /// **No dispatch and no variant of its own needed for the column case**: `->>` is not an
    /// hstore operator, so every `->>` is this one. The two differences from [`Self::JsonFetch`]
    /// are a JSON null (SQL NULL here, the string `null` there) and a string (unquoted here).
    JsonFetchText,
    /// `ARRAY[…]::oidvector`: the elements' **oids**, space separated.
    ///
    /// **Digits, not names** — measured, `ARRAY['text'::regtype]::oidvector` is `25`, which is
    /// exactly what `pg_proc.proargtypes` holds here. An `oidvector` is its own type on a real
    /// server and text here, which is the representation `proargtypes` already uses and the
    /// reason the comparison between them is order-sensitive and exact
    /// ([ADR 0077](../../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
    OidVector,
    /// `'happy'::mood` — a cast to a **user-defined type**, which is a name until the catalog is
    /// read.
    ///
    /// Two arguments: the type's name as a string literal, and the operand. Like
    /// [`CatalogFunc::RegClass`] it is replaced before the plan is built and never reaches the row
    /// evaluator — the catalog answer is the same for every row, and reading it per row is the
    /// cost trap `::regclass` already paid for once
    /// ([ADR 0053](../../../../docs/adr/0053-a-cast-to-a-user-defined-type-is-resolved-once-per-statement.md)).
    ///
    /// What it is replaced *with* depends on where it sits, and that is what an enum is rather
    /// than a special case: **the label** when it is a projection on its own, so
    /// `SELECT 'happy'::mood` prints `happy`; **the ordinal** everywhere else, so
    /// `'sad'::mood < 'happy'::mood` is `1 < 3` and is `t`.
    UserCast,
    /// A call to a function **the catalog holds**, carried for the executor.
    ///
    /// Lowering has no catalog, so a name its own vocabulary lacks cannot be told from a user's
    /// `CREATE FUNCTION` — the same seam `UserCast` and `UserRegType` sit on, and the same one a
    /// column `DEFAULT` naming a function already uses. The first argument is the name as a
    /// string literal; the rest are the call's own arguments.
    ///
    /// It never reaches the row evaluator: `crate::exec::Executor::resolve_user_function`
    /// replaces it with the body's expression, or raises the `0A000` naming it that lowering used
    /// to raise directly.
    UserFunc,
    /// `to_regclass('name')`: the relation of that name, or **NULL** where a bare reference would
    /// raise `42P01`.
    ///
    /// The supported way to ask whether a relation is there without an error, and not the same
    /// question as `EXISTS (SELECT … FROM pg_class …)`: it walks the `search_path` exactly as a
    /// reference does, so it answers about the relation the query would actually have found.
    ///
    /// **It answers the name, as [`CatalogFunc::RegClassName`] does**, and for the reason given
    /// there: a `regclass` on a real server is an oid that *prints* as a name, this node has no
    /// such type, and the name is what every text context sees. So `to_regclass(x) IS NULL` and
    /// `to_regclass(x)::text` are both what a real server answers, and the declared type is where
    /// the difference shows. Resolved once per statement like [`CatalogFunc::RegClass`], since its
    /// argument is a literal.
    ToRegClass,
    /// `pg_typeof(x)`: the name of the type `x` has.
    ///
    /// **Read from the value, not from the plan.** A real server answers the *static* type, and
    /// the two differ only for a NULL — `pg_typeof(NULL::int4)` is `integer` there and `text`
    /// here, which is what an untyped NULL is in this crate everywhere else. Every other value
    /// carries its own type and answers for itself, arrays included: an `ARRAY(SELECT 1)` is an
    /// `integer[]` because that is what the value is.
    PgTypeof,
    /// `date_trunc(unit, timestamp | timestamptz | interval)`, and the three-argument form
    /// that names the zone to cut in.
    ///
    /// **The result type is the argument's**, so `exec::query::expr_type` answers for it the
    /// way it does for `greatest`; [`CatalogFunc::result_type`] cannot, because it is given
    /// no arguments to look at.
    DateTrunc,
    /// `nlevel(path)`: how many labels the path has, and `0` for the empty one.
    LtreeNlevel,
    /// `ltree2text(path)` and `text2ltree(text)`: the two casts under their function names. The
    /// second **validates** — `text2ltree('a..b')` is `ltree`'s own syntax error.
    LtreeToText,
    /// See [`CatalogFunc::LtreeToText`].
    TextToLtree,
    /// `now()`, and `CURRENT_TIMESTAMP` which is the same function under a keyword spelling.
    ///
    /// **The transaction's instant, not the statement's.** Two calls in one transaction are equal
    /// and their difference is exactly `00:00:00` — measured, and PostgreSQL's own rule. It comes
    /// from the TSO's physical half (`crate::time_machine::micros_of_ts`) rather than from a
    /// clock this node reads, which is `docs/DESIGN.md` §6 and not an implementation detail: a
    /// node here has no wall clock it is allowed to order by.
    ///
    /// It is the one member of this enum that is **not** a function of its arguments alone.
    Now,
    /// `CURRENT_DATE`: the date of [`CatalogFunc::Now`]'s instant.
    CurrentDate,
    /// `LOCALTIMESTAMP`: the same instant as [`CatalogFunc::Now`], as a `timestamp` **without**
    /// a time zone.
    ///
    /// A separate member rather than a spelling of `now()` because the *type* differs, and the
    /// type is what a column assignment reads: `LOCALTIMESTAMP` fills a `timestamp` column with no
    /// cast at all, where `CURRENT_TIMESTAMP` fills it through one. Both store the same
    /// microseconds here, so the two rows are equal — which is what the capture checks.
    LocalTimestamp,
    /// `LOCALTIME`: the time of day of [`CatalogFunc::Now`]'s instant, **without** a zone.
    ///
    /// Here for one reason: it is what the capture puts in a `timestamp` column to be refused.
    /// `time` is not `timestamp` and PostgreSQL offers no assignment cast between them, so the
    /// answer is the `42804` that names both types — which this node can only give if it knows
    /// the expression's type, and it only knows it by having the member.
    ///
    /// Its zoned twin `CURRENT_TIME` is **not** here: `time with time zone` is not one of the
    /// stored types (ADR 0033), and a member whose type does not exist could not answer
    /// `pg_typeof` and could not name itself in that `42804`. See the corpus's two divergences.
    LocalTime,
    /// `statement_timestamp()`: **the transaction's instant here**, where a real server advances
    /// it per statement.
    ///
    /// A declared divergence, and it is invariant 6 that forces it: the TSO's physical half is the
    /// only clock this node may read, and a transaction has exactly one. It is its own member
    /// rather than an alias of [`CatalogFunc::Now`] so that the difference stays visible — a name
    /// folded into another cannot later be made to differ, and cannot name itself in a message.
    StatementTimestamp,
    /// `clock_timestamp()`: the transaction's instant here too, for [`CatalogFunc::StatementTimestamp`]'s
    /// reason.
    ///
    /// The corpus pins the consequence rather than the value — `clock_timestamp() >=
    /// transaction_timestamp()` is `t`, which equality satisfies — and the divergence is that a
    /// real server makes it strictly greater by the time the second call runs.
    ClockTimestamp,
    /// `random()`: a `double precision` in `[0, 1)`, drawn **per call**.
    ///
    /// The one function here that is not a pure function of its arguments, and the corpus pins the
    /// consequence rather than a value: two calls in one statement differ, and every draw is
    /// inside the half-open range. It arrived with the generalised column `DEFAULT`, where
    /// `random() * 100` is an ordinary expression that happens to sit in a `DEFAULT` clause.
    Random,
    /// `concat(...)`: variadic, and **not strict**.
    ///
    /// It *skips* NULLs where `||` propagates them — `concat('a', NULL, 'b')` is `ab` and
    /// `concat(NULL, NULL)` is the empty string. Each argument is rendered by its own output
    /// function, so `concat('n=', 42, ' t=', true)` is `n=42 t=t` and a `numeric` keeps its scale.
    Concat,
    /// `split_part(text, sep, n)`: the `n`th field, counting from 1 — or from the **end** when
    /// `n` is negative.
    ///
    /// Measured on 19beta1, and the edges are most of it: past the end is the empty string rather
    /// than NULL, `-1` is the last field, an empty separator gives the whole string back, and
    /// **`n = 0` is an error** — `22023 field position must not be zero`, not an empty answer.
    SplitPart,
    /// `string_to_array(text, delimiter [, null_string])` — the splitter
    /// `ALTER COLUMN … TYPE text[] USING string_to_array(…)` is written with.
    StringToArray,
    /// `strpos(haystack, needle)`: the 1-based position of the first match, `0` for none.
    ///
    /// An empty needle is `1`, measured — it matches at the start rather than nowhere.
    StrPos,
    /// `btrim(text)` and `btrim(text, characters)` — and the three `TRIM` spellings that lower
    /// here, with [`CatalogFunc::Ltrim`] and [`CatalogFunc::Rtrim`].
    ///
    /// **The second argument is a *set* of characters, not a prefix.** Measured:
    /// `TRIM(BOTH 'ab' FROM 'abcba')` is `c`, not `cba` — every leading and trailing character
    /// that is in the set goes, in any order and any number. One argument trims whitespace.
    Btrim,
    /// `ltrim`, and `TRIM(LEADING …)`.
    Ltrim,
    /// `rtrim`, and `TRIM(TRAILING …)`.
    Rtrim,
    /// `greatest(...)` and [`CatalogFunc::Least`]: variadic, and **not strict**.
    ///
    /// They *skip* NULLs where almost everything else propagates them — measured,
    /// `GREATEST(1, NULL, 3)` is `3` and `LEAST(NULL, 2)` is `2` — and answer NULL only when
    /// every argument is one. One argument is legal and answers itself; **zero is a syntax
    /// error**, `42601 syntax error at or near ")"`, because the grammar requires an argument
    /// rather than the function refusing an empty list.
    Greatest,
    /// `least(...)`, which is [`CatalogFunc::Greatest`] with the comparison turned round.
    Least,
    /// `mod(a, b)`: the remainder, and **a function rather than the operator it shares a C
    /// implementation with**.
    ///
    /// PostgreSQL's `%` for `int8` is `int8mod`, the same function `mod()` calls, and the two agree
    /// on every sign combination — which is why this crate lowered the call into
    /// [`Expr::Arithmetic`] and got the evaluation, the tests and the immutability for free. What
    /// that threw away is the **spelling**: `pg_get_indexdef` prints back the node the tree holds,
    /// so `mod(id, 10)` came back `id % 10` where a real server prints `mod(id, 10)` — measured in
    /// every form, whole and per column, plain and pretty. `postgresql_adapter_test#test_expression_index`
    /// asserts that string exactly.
    ///
    /// So the call keeps its own node and the evaluator delegates: one implementation of the
    /// remainder, two spellings that print as they were written, which is what `%` and `mod` are
    /// on a real server too.
    Mod,
    /// `nullif(a, b)`: `a`, or NULL when the two are equal.
    ///
    /// **The third of PostgreSQL's four comparison productions to live in this enum**, beside
    /// `GREATEST` and `LEAST` -- `COALESCE` is [`Expr::Coalesce`] because it is variadic and
    /// branches. Like them it is a grammar production and not a `pg_proc` row, so its arity is
    /// enforced by the grammar there: `nullif(1)` and `nullif(1, 2, 3)` are
    /// `42601 syntax error at or near ")"` on a real server, where this node answers the `42883`
    /// its arity table gives -- one sqlstate apart on a statement nothing sends, recorded rather
    /// than special-cased in the parser.
    ///
    /// **Its result type is the comparison's *left* input type, which is not always the common
    /// type.** Measured on 19beta1, and the pair that says so is `nullif(int4, int8)` ->
    /// `integer` where `GREATEST(int4, int8)` is `bigint`: PostgreSQL resolves `=` between the
    /// two, finds `int48eq(int4, int8)`, and the left side keeps its own type. Where no cross-type
    /// operator exists both sides coerce and the answer *is* the common type --
    /// `nullif(int4, numeric)` is `numeric`, `nullif(int4, float8)` is `double precision`. And a
    /// `varchar` operand has no `=` of its own, so it resolves through `texteq` and the answer is
    /// `text`: `nullif(v, 'x')` on a `varchar(10)` column is `text`, and prints
    /// `NULLIF((v)::text, 'x'::text)`. `exec::query::nullif_type` is that rule and
    /// `tests/nullif.rs` is the twelve measurements behind it.
    ///
    /// **Not strict, and in the other direction from `GREATEST`**: `nullif(NULL, 1)` is NULL and
    /// `nullif(1, NULL)` is `1` -- the comparison against NULL is unknown, which is not equal, so
    /// the first argument comes back.
    NullIf,
    /// `substr(text, from[, count])`: the substring, 1-based and clamped.
    ///
    /// **`from` may be zero or negative**, and the clamp is what makes those work: the result is
    /// the characters at positions `max(from, 1) ..= from + count - 1`, so `substr('hello', -1, 3)`
    /// is `h` — positions -1, 0 and 1, of which only 1 exists. Without a `count` it runs to the
    /// end, and a `from` past the end is the empty string.
    Substr,
    /// `substring(text, from[, count])` and `substring(text FROM from [FOR count])`: the same
    /// function as [`CatalogFunc::Substr`], under the name a real server prints back.
    ///
    /// **Two variants for one behaviour, because the output column is named after the spelling.**
    /// Measured: `substring('abc' FROM 2)` and `substring('abc', 2)` are both `substring`, and
    /// `substr('abc', 2)` is `substr`. Folding them would rename a column in every schema dump
    /// that uses the long spelling.
    Substring,
    /// `replace(text, from, to)`: every occurrence of `from` in `text`, replaced.
    ///
    /// Three rules that a `str::replace` gets right and one it does not, all measured on 19beta1:
    /// left to right and **non-overlapping** (`replace('aaa','aa','b')` is `ba`, not `bb`),
    /// **case-sensitive** (`replace('abc','ABC','x')` is `abc`), strict — any NULL argument gives
    /// NULL — and the result is `text` whatever went in, so a `varchar` column comes back `text`.
    ///
    /// **An empty `from` is a no-op**: `replace('abc','','X')` is `abc`. Rust's `str::replace`
    /// answers `XaXbXcX` for that, matching between every character, so the empty case is the one
    /// this cannot delegate.
    Replace,
    /// `convert_to(text, encoding)`: the bytes `text` has in `encoding`.
    ///
    /// **Strict**, encoding name included: `convert_to('A', NULL)` is NULL. A name that is not an
    /// encoding is `22023` and not a refusal, which is a real server's answer and not this node's
    /// idea of one.
    ConvertTo,
    /// `array_position(array, value)`: the subscript `value` sits at, or NULL.
    ///
    /// The five below are the array operators the catalog's own columns need, and they read the
    /// array out of its text form (`crate::value::vector`) rather than out of an array *value*,
    /// for the three reasons that module gives. They are here rather than in a family of their own
    /// because they are the same shape as everything else in this one — several arguments,
    /// evaluated per row, a function of those arguments and nothing else.
    ///
    /// **The subscript is not always 1-based**: `pg_index.indkey` is an `int2vector`, whose lower
    /// bound is `0`. Measured, both.
    ArrayPosition,
    /// `array_lower(array, dimension)`: the first subscript — `0` for an `int2vector` and `1` for
    /// an ordinary array, and NULL for an empty one, which has no dimensions at all.
    ArrayLower,
    /// `array_upper(array, dimension)`: the last subscript, NULL for an empty array.
    ArrayUpper,
    /// `array_length(array, dimension)`: how many elements that dimension has, and **NULL** for an
    /// empty array rather than 0 — the shape that breaks a `LIMIT` computed from it.
    ArrayLength,
    /// `cardinality(array)`: how many elements in total, and **0** for an empty array where
    /// `array_length` is NULL. One argument, and the one of the five that disagrees with
    /// `array_length` about emptiness.
    Cardinality,
}

impl CatalogFunc {
    /// The function a name is, if it is one. Case-insensitive, as PostgreSQL resolves a function
    /// name written unquoted.
    #[must_use]
    pub fn from_name(name: &str) -> Option<CatalogFunc> {
        match () {
            // `CURRENT_TIMESTAMP` reaches here as a function name, which is what it is: a keyword
            // spelling of `now()`, recorded by PostgreSQL as the same thing.
            // `transaction_timestamp()` is the third spelling of the same value, and PostgreSQL
            // documents it as `now()`'s own definition rather than as a function that agrees with
            // it. Three names, one member.
            () if name.eq_ignore_ascii_case("now")
                || name.eq_ignore_ascii_case("current_timestamp")
                || name.eq_ignore_ascii_case("transaction_timestamp") =>
            {
                Some(CatalogFunc::Now)
            }
            () if name.eq_ignore_ascii_case("localtimestamp") => Some(CatalogFunc::LocalTimestamp),
            () if name.eq_ignore_ascii_case("localtime") => Some(CatalogFunc::LocalTime),
            () if name.eq_ignore_ascii_case("statement_timestamp") => {
                Some(CatalogFunc::StatementTimestamp)
            }
            () if name.eq_ignore_ascii_case("clock_timestamp") => Some(CatalogFunc::ClockTimestamp),
            () if name.eq_ignore_ascii_case("daterange") => Some(CatalogFunc::DateRange),
            () if name.eq_ignore_ascii_case("isempty") => Some(CatalogFunc::IsEmpty),
            () if name.eq_ignore_ascii_case("isopen") => Some(CatalogFunc::PathIsOpen),
            () if name.eq_ignore_ascii_case("isclosed") => Some(CatalogFunc::PathIsClosed),
            // The hstore functions. `hstore(…)` is two shapes of one name, told apart by whether
            // its arguments are arrays — an overload, the way a real server tells them apart.
            () if name.eq_ignore_ascii_case("lower_inc") => Some(CatalogFunc::RangeLowerInc),
            () if name.eq_ignore_ascii_case("upper_inc") => Some(CatalogFunc::RangeUpperInc),
            () if name.eq_ignore_ascii_case("lower_inf") => Some(CatalogFunc::RangeLowerInf),
            () if name.eq_ignore_ascii_case("upper_inf") => Some(CatalogFunc::RangeUpperInf),
            () if name.eq_ignore_ascii_case("tsrange") => Some(CatalogFunc::RangeBuild),
            () if name.eq_ignore_ascii_case("akeys") => Some(CatalogFunc::HstoreAkeys),
            () if name.eq_ignore_ascii_case("avals") => Some(CatalogFunc::HstoreAvals),
            () if name.eq_ignore_ascii_case("hstore") => Some(CatalogFunc::HstoreBuild),
            () if name.eq_ignore_ascii_case("date_trunc") => Some(CatalogFunc::DateTrunc),
            () if name.eq_ignore_ascii_case("to_tsvector") => Some(CatalogFunc::ToTsVector),
            () if name.eq_ignore_ascii_case("to_tsquery") => Some(CatalogFunc::ToTsQuery),
            () if name.eq_ignore_ascii_case("plainto_tsquery") => Some(CatalogFunc::PlainToTsQuery),
            () if name.eq_ignore_ascii_case("phraseto_tsquery") => {
                Some(CatalogFunc::PhraseToTsQuery)
            }
            () if name.eq_ignore_ascii_case("strip") => Some(CatalogFunc::TsStrip),
            () if name.eq_ignore_ascii_case("setweight") => Some(CatalogFunc::SetWeight),
            () if name.eq_ignore_ascii_case("numnode") => Some(CatalogFunc::NumNode),
            () if name.eq_ignore_ascii_case("ts_rank") => Some(CatalogFunc::TsRank),
            () if name.eq_ignore_ascii_case("ts_headline") => Some(CatalogFunc::TsHeadline),
            () if name.eq_ignore_ascii_case("websearch_to_tsquery") => {
                Some(CatalogFunc::WebsearchToTsQuery)
            }
            // The three `ltree` functions the corpus asks for. `nlevel('')` is 0, which is what
            // makes the empty path a value rather than a hole.
            () if name.eq_ignore_ascii_case("nlevel") => Some(CatalogFunc::LtreeNlevel),
            () if name.eq_ignore_ascii_case("ltree2text") => Some(CatalogFunc::LtreeToText),
            () if name.eq_ignore_ascii_case("text2ltree") => Some(CatalogFunc::TextToLtree),
            () if name.eq_ignore_ascii_case("pg_get_triggerdef") => {
                Some(CatalogFunc::PgGetTriggerdef)
            }
            () if name.eq_ignore_ascii_case("to_regclass") => Some(CatalogFunc::ToRegClass),
            () if name.eq_ignore_ascii_case("pg_typeof") => Some(CatalogFunc::PgTypeof),
            () if name.eq_ignore_ascii_case("current_date") => Some(CatalogFunc::CurrentDate),
            () if name.eq_ignore_ascii_case("random") => Some(CatalogFunc::Random),
            () if name.eq_ignore_ascii_case("concat") => Some(CatalogFunc::Concat),
            () if name.eq_ignore_ascii_case("split_part") => Some(CatalogFunc::SplitPart),
            () if name.eq_ignore_ascii_case("string_to_array") => Some(CatalogFunc::StringToArray),
            () if name.eq_ignore_ascii_case("strpos") => Some(CatalogFunc::StrPos),
            () if name.eq_ignore_ascii_case("btrim") => Some(CatalogFunc::Btrim),
            () if name.eq_ignore_ascii_case("ltrim") => Some(CatalogFunc::Ltrim),
            () if name.eq_ignore_ascii_case("rtrim") => Some(CatalogFunc::Rtrim),
            () if name.eq_ignore_ascii_case("greatest") => Some(CatalogFunc::Greatest),
            () if name.eq_ignore_ascii_case("nullif") => Some(CatalogFunc::NullIf),
            () if name.eq_ignore_ascii_case("mod") => Some(CatalogFunc::Mod),
            () if name.eq_ignore_ascii_case("least") => Some(CatalogFunc::Least),
            () if name.eq_ignore_ascii_case("substr") => Some(CatalogFunc::Substr),
            () if name.eq_ignore_ascii_case("substring") => Some(CatalogFunc::Substring),
            () if name.eq_ignore_ascii_case("replace") => Some(CatalogFunc::Replace),
            () if name.eq_ignore_ascii_case("convert_to") => Some(CatalogFunc::ConvertTo),
            () if name.eq_ignore_ascii_case("format_type") => Some(CatalogFunc::FormatType),
            () if name.eq_ignore_ascii_case("pg_get_expr") => Some(CatalogFunc::PgGetExpr),
            () if name.eq_ignore_ascii_case("pg_get_indexdef") => Some(CatalogFunc::PgGetIndexdef),
            () if name.eq_ignore_ascii_case("pg_get_viewdef") => Some(CatalogFunc::PgGetViewdef),
            () if name.eq_ignore_ascii_case("pg_get_constraintdef") => {
                Some(CatalogFunc::PgGetConstraintdef)
            }
            () if name.eq_ignore_ascii_case("pg_encoding_to_char") => {
                Some(CatalogFunc::PgEncodingToChar)
            }
            () if name.eq_ignore_ascii_case("pg_get_serial_sequence") => {
                Some(CatalogFunc::PgGetSerialSequence)
            }
            () if name.eq_ignore_ascii_case("col_description") => Some(CatalogFunc::ColDescription),
            () if name.eq_ignore_ascii_case("pg_sleep") => Some(CatalogFunc::PgSleep),
            () if name.eq_ignore_ascii_case("pg_cancel_backend") => {
                Some(CatalogFunc::PgCancelBackend)
            }
            () if name.eq_ignore_ascii_case("pg_terminate_backend") => {
                Some(CatalogFunc::PgTerminateBackend)
            }
            () if name.eq_ignore_ascii_case("pg_backend_pid") => Some(CatalogFunc::PgBackendPid),
            () if name.eq_ignore_ascii_case("obj_description") => Some(CatalogFunc::ObjDescription),
            () if name.eq_ignore_ascii_case("array_position") => Some(CatalogFunc::ArrayPosition),
            () if name.eq_ignore_ascii_case("array_lower") => Some(CatalogFunc::ArrayLower),
            () if name.eq_ignore_ascii_case("array_upper") => Some(CatalogFunc::ArrayUpper),
            () if name.eq_ignore_ascii_case("array_length") => Some(CatalogFunc::ArrayLength),
            () if name.eq_ignore_ascii_case("cardinality") => Some(CatalogFunc::Cardinality),
            () if name.eq_ignore_ascii_case("pg_get_partkeydef") => {
                Some(CatalogFunc::PgGetPartkeydef)
            }
            () => None,
        }
    }

    /// How it is spelled in a `42883`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            CatalogFunc::FormatType => "format_type",
            // The user's own name is in the call's first argument; this is what it is *called*.
            CatalogFunc::UserFunc => "a user-defined function",
            CatalogFunc::PgGetExpr => "pg_get_expr",
            CatalogFunc::PgGetIndexdef => "pg_get_indexdef",
            CatalogFunc::PgGetConstraintdef => "pg_get_constraintdef",
            CatalogFunc::PgGetViewdef => "pg_get_viewdef",
            CatalogFunc::PgEncodingToChar => "pg_encoding_to_char",
            CatalogFunc::PgGetSerialSequence => "pg_get_serial_sequence",
            CatalogFunc::ColDescription => "col_description",
            CatalogFunc::PgSleep => "pg_sleep",
            CatalogFunc::PgCancelBackend => "pg_cancel_backend",
            CatalogFunc::PgTerminateBackend => "pg_terminate_backend",
            CatalogFunc::PgBackendPid => "pg_backend_pid",
            CatalogFunc::ObjDescription => "obj_description",
            CatalogFunc::PgGetPartkeydef => "pg_get_partkeydef",
            CatalogFunc::PgGetTriggerdef => "pg_get_triggerdef",
            CatalogFunc::DateRange => "daterange",
            CatalogFunc::IsEmpty => "isempty",
            CatalogFunc::PathIsOpen => "isopen",
            CatalogFunc::PathIsClosed => "isclosed",
            CatalogFunc::RangeOverlaps => "&&",
            CatalogFunc::RangeLowerInc => "lower_inc",
            CatalogFunc::RangeUpperInc => "upper_inc",
            CatalogFunc::RangeLowerInf => "lower_inf",
            CatalogFunc::RangeUpperInf => "upper_inf",
            CatalogFunc::RangeBuild => "tsrange",
            // One symbol, two fetches — an hstore's and a document's — told apart by the cast at
            // lowering and by the operand's declared type at resolution, never by the values.
            CatalogFunc::HstoreFetch | CatalogFunc::JsonFetch | CatalogFunc::JsonbFetch => "->",
            CatalogFunc::HstoreHasKey => "?",
            CatalogFunc::SameAs => "~=",
            CatalogFunc::JsonbCompare => "jsonb_compare",
            CatalogFunc::JsonbContains => "jsonb_contains",
            CatalogFunc::PolygonContains => "polygon_contains",
            CatalogFunc::PolygonOverlaps => "polygon_overlaps",
            // One symbol, two containments — see `exec::cursor`, where the operand decides.
            CatalogFunc::RangeContains | CatalogFunc::HstoreContains => "@>",
            CatalogFunc::HstoreConcat | CatalogFunc::JsonbConcat => "||",
            CatalogFunc::HstoreAkeys => "akeys",
            CatalogFunc::HstoreAvals => "avals",
            CatalogFunc::HstoreBuild => "hstore",
            CatalogFunc::DateTrunc => "date_trunc",
            CatalogFunc::ToTsVector => "to_tsvector",
            CatalogFunc::ToTsQuery => "to_tsquery",
            CatalogFunc::PlainToTsQuery => "plainto_tsquery",
            CatalogFunc::PhraseToTsQuery => "phraseto_tsquery",
            CatalogFunc::TsMatch => "@@",
            CatalogFunc::TsStrip => "strip",
            CatalogFunc::SetWeight => "setweight",
            CatalogFunc::NumNode => "numnode",
            CatalogFunc::TsRank => "ts_rank",
            CatalogFunc::TsHeadline => "ts_headline",
            CatalogFunc::WebsearchToTsQuery => "websearch_to_tsquery",
            CatalogFunc::LtreeNlevel => "nlevel",
            CatalogFunc::LtreeToText => "ltree2text",
            CatalogFunc::TextToLtree => "text2ltree",
            // Two directions of one cast, and PostgreSQL names both of them `regclass`.
            CatalogFunc::RegClass | CatalogFunc::RegClassName => "regclass",
            // Both halves of a `regtype` are called that: one reads an oid and prints a name,
            // the other reads a name and answers its oid.
            CatalogFunc::RegTypeName | CatalogFunc::UserRegType => "regtype",
            CatalogFunc::OidVector => "oidvector",
            CatalogFunc::JsonFetchText => "->>",
            // What a `42883` would call it, and nothing reaches one: the pass either
            // resolves it or raises about the type by name.
            CatalogFunc::UserCast => "cast",
            CatalogFunc::ArrayPosition => "array_position",
            CatalogFunc::ArrayLower => "array_lower",
            CatalogFunc::ArrayUpper => "array_upper",
            CatalogFunc::ArrayLength => "array_length",
            CatalogFunc::Cardinality => "cardinality",
            CatalogFunc::ToRegClass => "to_regclass",
            CatalogFunc::PgTypeof => "pg_typeof",
            CatalogFunc::Now => "now",
            CatalogFunc::CurrentDate => "current_date",
            CatalogFunc::LocalTimestamp => "localtimestamp",
            CatalogFunc::LocalTime => "localtime",
            CatalogFunc::StatementTimestamp => "statement_timestamp",
            CatalogFunc::ClockTimestamp => "clock_timestamp",
            CatalogFunc::Random => "random",
            CatalogFunc::Concat => "concat",
            CatalogFunc::SplitPart => "split_part",
            CatalogFunc::StringToArray => "string_to_array",
            CatalogFunc::StrPos => "strpos",
            CatalogFunc::Btrim => "btrim",
            CatalogFunc::Ltrim => "ltrim",
            CatalogFunc::Rtrim => "rtrim",
            CatalogFunc::Greatest => "greatest",
            CatalogFunc::NullIf => "nullif",
            CatalogFunc::Mod => "mod",
            CatalogFunc::Least => "least",
            CatalogFunc::Substr => "substr",
            CatalogFunc::Substring => "substring",
            CatalogFunc::Replace => "replace",
            CatalogFunc::ConvertTo => "convert_to",
        }
    }

    /// How many arguments it takes, in the order PostgreSQL lists the overloads.
    ///
    /// A **set**, because two of these have more than one form and PostgreSQL resolves by name
    /// *and* arity: `format_type(23)` is `42883` naming the number of arguments rather than
    /// running the two-argument function it nearly matched, and `pg_get_expr` really does have
    /// both a two- and a three-argument form.
    #[must_use]
    pub fn arities(self) -> &'static [usize] {
        match self {
            // **Checked against the function's own declaration, not here.** The name is argument
            // zero and the user's arguments follow, so the count this carrier takes is whatever
            // was written — `crate::exec::Executor::resolve_user_function` is where a wrong one is
            // answered, because only the catalog knows how many the function has.
            CatalogFunc::UserFunc => &[1, 2, 3, 4, 5, 6, 7, 8],
            CatalogFunc::FormatType
            | CatalogFunc::PgGetSerialSequence
            | CatalogFunc::ColDescription
            | CatalogFunc::ArrayPosition
            // Two, exactly, and on a real server it is the **grammar** that says so: `nullif(1)`
            // is `42601 syntax error at or near ")"` there where the arity table answers the
            // `42883` this set gives everything else. One sqlstate apart, on a statement nothing
            // sends; the variant's doc records it rather than the parser special-casing it.
            | CatalogFunc::NullIf
            | CatalogFunc::Mod
            // The type's name, then the operand.
            | CatalogFunc::UserCast
            | CatalogFunc::ArrayLower
            | CatalogFunc::ArrayUpper
            | CatalogFunc::ArrayLength
            | CatalogFunc::ConvertTo
            | CatalogFunc::DateRange
            | CatalogFunc::RangeOverlaps
            | CatalogFunc::RangeContains
            | CatalogFunc::HstoreFetch
            | CatalogFunc::HstoreHasKey
            | CatalogFunc::SameAs
            | CatalogFunc::JsonbCompare
            | CatalogFunc::JsonbContains
            | CatalogFunc::PolygonContains
            | CatalogFunc::PolygonOverlaps
            | CatalogFunc::HstoreContains
            | CatalogFunc::HstoreConcat
            | CatalogFunc::JsonbConcat
            | CatalogFunc::HstoreBuild
            | CatalogFunc::TsMatch
            | CatalogFunc::TsRank
            | CatalogFunc::SetWeight
            | CatalogFunc::StrPos => &[2],
            // `tsrange(a, b)` and `tsrange(a, b, '[]')` — two shapes of one name, and
            // `pg_get_expr`'s two really are two forms as well.
            // `tsrange(a, b)` and `tsrange(a, b, '[]')` — two shapes of one name, and
            // `pg_get_expr`'s two really are two forms as well.
            // `tsrange(a, b)` and `tsrange(a, b, '[]')` — two shapes of one name, and
            // `pg_get_expr`'s two really are two forms as well. A `UserRegType` is always two:
            // the name and the flag saying which half of the `regtype` was asked for.
            // `ts_headline` joins them: `(config, text, query)`, or two arguments taking
            // `default_text_search_config`.
            CatalogFunc::Substr
            | CatalogFunc::Substring
            | CatalogFunc::RangeBuild
            | CatalogFunc::PgGetExpr
            | CatalogFunc::UserRegType
            // `string_to_array(text, delimiter)` and the form that names a `null_string`, which is
            // the same pair of arities and so the same arm.
            | CatalogFunc::StringToArray
            | CatalogFunc::TsHeadline
            // `date_trunc`'s third argument names the zone to cut in.
            | CatalogFunc::DateTrunc => &[2, 3],

            CatalogFunc::PgGetIndexdef => &[1, 3],
            // The text-search four take one argument with `default_text_search_config`, or two
            // naming the configuration.
            // One argument trims whitespace, two trim a set of characters.
            CatalogFunc::Btrim
            | CatalogFunc::Ltrim
            | CatalogFunc::Rtrim
            | CatalogFunc::PgGetConstraintdef
            | CatalogFunc::PgGetViewdef
            | CatalogFunc::ToTsVector
            | CatalogFunc::ToTsQuery
            | CatalogFunc::PlainToTsQuery
            | CatalogFunc::PhraseToTsQuery
            | CatalogFunc::WebsearchToTsQuery
            | CatalogFunc::ObjDescription => &[1, 2],
            CatalogFunc::RangeLowerInc
            | CatalogFunc::RangeUpperInc
            | CatalogFunc::RangeLowerInf
            | CatalogFunc::RangeUpperInf
            | CatalogFunc::HstoreAkeys
            | CatalogFunc::HstoreAvals
            | CatalogFunc::PgEncodingToChar
            | CatalogFunc::PgGetPartkeydef
            | CatalogFunc::PgGetTriggerdef
            | CatalogFunc::RegClass
            | CatalogFunc::RegClassName
            | CatalogFunc::RegTypeName
            | CatalogFunc::OidVector
            | CatalogFunc::JsonFetch
            | CatalogFunc::JsonbFetch
            | CatalogFunc::JsonFetchText
            | CatalogFunc::ToRegClass
            | CatalogFunc::IsEmpty
            | CatalogFunc::PathIsOpen
            | CatalogFunc::PathIsClosed
            | CatalogFunc::Cardinality
            | CatalogFunc::PgTypeof
            | CatalogFunc::LtreeNlevel
            | CatalogFunc::LtreeToText
            | CatalogFunc::TextToLtree
            // `strip(v)` and `numnode(q)` take one and only one, and so does `pg_sleep`.
            | CatalogFunc::TsStrip
            | CatalogFunc::NumNode
            | CatalogFunc::PgSleep
            | CatalogFunc::PgCancelBackend
            | CatalogFunc::PgTerminateBackend => &[1],
            CatalogFunc::Now
            | CatalogFunc::CurrentDate
            | CatalogFunc::LocalTimestamp
            | CatalogFunc::LocalTime
            | CatalogFunc::StatementTimestamp
            | CatalogFunc::ClockTimestamp
            | CatalogFunc::Random
            | CatalogFunc::PgBackendPid => &[0],
            // Variadic: every arity from one up. `concat()` is the `42883` about the *number* of
            // arguments that a real server raises, so zero is not in the set.
            // Variadic, and one argument is legal: `GREATEST(1)` is `1`.
            CatalogFunc::Concat | CatalogFunc::Greatest | CatalogFunc::Least => &CONCAT_ARITIES,
            CatalogFunc::SplitPart | CatalogFunc::Replace => &[3],
        }
    }

    /// The type of its result.
    #[must_use]
    pub fn result_type(self) -> ColumnType {
        match self {
            // **`UserFunc` is never evaluated and never typed** — it is replaced by the body's
            // expression before anything asks — so it joins the text-returning group rather than
            // claiming an answer of its own.
            // **`greatest` and `least` answer their arguments' common type**, which this
            // function cannot say: it takes no arguments. `exec::query::expr_type` has an arm
            // above the one that calls this, the way `hstore`'s `->` does, and it is the answer
            // a client is told; `text` here is only what a caller that skipped that arm would
            // see, and there is no such caller.
            CatalogFunc::Greatest
            | CatalogFunc::Least
            | CatalogFunc::NullIf
            | CatalogFunc::Mod
            | CatalogFunc::Btrim
            | CatalogFunc::Ltrim
            | CatalogFunc::Rtrim
            | CatalogFunc::UserFunc
            | CatalogFunc::FormatType
            | CatalogFunc::PgGetExpr
            | CatalogFunc::PgGetIndexdef
            | CatalogFunc::PgGetConstraintdef
            | CatalogFunc::PgGetViewdef
            | CatalogFunc::PgGetTriggerdef
            | CatalogFunc::ColDescription
            // `void` on a real server, which prints as the empty string; `text` here for the same
            // reason `regclass` is text — what it prints as is what a client sees.
            | CatalogFunc::PgSleep
            | CatalogFunc::PgGetSerialSequence
            | CatalogFunc::PgEncodingToChar
            | CatalogFunc::ObjDescription
            | CatalogFunc::PgGetPartkeydef
            | CatalogFunc::JsonFetchText
            // `concat` answers `text` for the ordinary reason: it builds a string.
            | CatalogFunc::Concat
            | CatalogFunc::SplitPart
            | CatalogFunc::Substr
            | CatalogFunc::Substring
            | CatalogFunc::Replace
            | CatalogFunc::HstoreFetch
            | CatalogFunc::LtreeToText
            // `ts_headline` answers the marked-up text.
            | CatalogFunc::TsHeadline => ColumnType::Text,
            // **`pg_typeof` answers a `regtype`**, which is its `prorettype` on a real server and
            // was `text` here while this node had no such type. It has had one since
            // [ADR 0077](../../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md), and
            // the comment that stood here — "this node has no `regtype`" — outlived the decision
            // that made it false by fifteen ADRs.
            // **And `t::regtype` is one too**, not the name it prints as: it answered a `text`
            // here where a real server says 2206 — the declared type only a `Describe` sees,
            // which is what r1's wire sweep is for.
            CatalogFunc::PgTypeof | CatalogFunc::RegTypeName => ColumnType::RegType,
            // `ts_rank` answers a `real`, measured with `pg_typeof`.
            CatalogFunc::TsRank => ColumnType::Real,
            // An `oid` on a real server, and a `bigint` here for the reason `pg_class.oid` is one.
            // **`regclass`, not `bigint`** — and the difference is a client's, not a reader's.
            // `ActiveRecord` reloads its type map when a `RowDescription` carries an oid it does
            // not know, warns once, and treats the value as a String; three of its tests assert
            // that reload. Describing this as a `bigint` gave it an oid it knew, so nothing
            // happened and all three watched the absence
            // (`tests/captures/pg19_unknown_oid.txt`). The value is the same relation id either
            // way; what changes is that it now prints as the relation's name, which is what a
            // real server answers (`tests/captures/pg19_regclass.txt`).
            // **All three of them**, and the two that were `text` are the correction. A
            // `regclass` is an oid that *prints* as a name, so answering the name under the name's
            // type read right and described wrong — 25 where a real server says 2205 — and the
            // difference is only visible through a `Describe`. It is not cosmetic: it is what
            // decides that `array_agg(oid::regclass)` is a `regclass[]` rather than a `text[]`,
            // and what makes `min` of one an `oid`. `to_regclass` is the same value with a
            // different miss, so it is the same type (`tests/captures/pg19_reg_class.txt`).
            CatalogFunc::RegClass | CatalogFunc::RegClassName | CatalogFunc::ToRegClass => {
                ColumnType::RegClass
            }
            // **The storage, which is what an enum's value is** (ADR 0050) — and the label
            // the projection form is replaced by is a `text` literal by then, so nothing
            // reads this for that shape.
            CatalogFunc::UserCast => ColumnType::Int2,
            // **Never reached**: the pass that reads the catalog replaces it with the oid or the
            // name before anything asks. The `oid` is the honest answer for the half that a
            // client actually writes — `'color'::regtype::oid` — and the one this would be
            // resolved to if the resolution were ever skipped.
            CatalogFunc::UserRegType => ColumnType::Oid,

            // Every one of the five answers `integer` on a real server, including `cardinality`,
            // which counts every element of every dimension where `array_length` counts one.
            CatalogFunc::ArrayPosition
            | CatalogFunc::ArrayLower
            | CatalogFunc::ArrayUpper
            | CatalogFunc::ArrayLength
            | CatalogFunc::Cardinality
            | CatalogFunc::LtreeNlevel
            // `numnode` counts the nodes of a query, operators included.
            | CatalogFunc::NumNode
            // `strpos` is a position, and `0` for "not found" rather than NULL.
            | CatalogFunc::StrPos
            // `integer` on a real server, and the column `pg_stat_activity.pid` is declared as.
            | CatalogFunc::PgBackendPid
            | CatalogFunc::JsonbCompare => ColumnType::Int4,
            // The two range predicates answer a boolean, which is what lets `&&` stand in a
            // `WHERE` without a comparison around it.
            CatalogFunc::PathIsOpen
            | CatalogFunc::PathIsClosed
            | CatalogFunc::IsEmpty
            | CatalogFunc::RangeOverlaps
            | CatalogFunc::RangeContains
            | CatalogFunc::RangeLowerInc
            | CatalogFunc::RangeUpperInc
            | CatalogFunc::RangeLowerInf
            | CatalogFunc::RangeUpperInf
            | CatalogFunc::HstoreHasKey
            | CatalogFunc::SameAs
            | CatalogFunc::HstoreContains
            | CatalogFunc::TsMatch
            | CatalogFunc::PgCancelBackend
            | CatalogFunc::PgTerminateBackend
            | CatalogFunc::JsonbContains
            | CatalogFunc::PolygonContains
            | CatalogFunc::PolygonOverlaps => ColumnType::Bool,
            CatalogFunc::TextToLtree => ColumnType::Ltree,
            // Measured: `akeys` is `text[]`, and `||` and `hstore(…)` are hstores. `->`'s `text`
            // and `?`/`@>`'s `boolean` are folded into the lists above and below.
            CatalogFunc::HstoreAkeys | CatalogFunc::HstoreAvals | CatalogFunc::StringToArray => {
                ColumnType::TextArray
            }
            // **`x::oidvector` is an `oidvector`, whatever built it.** It sat in the group above
            // answering `text`, which the *literal* path hid: that one folds to a value under an
            // `Expr::Cast { to: OidVector }` and never asks this. Every other operand — a `text`
            // column, `::text::oidvector`, an `ARRAY[…]` — reaches this, so `pg_typeof` said
            // `text` where 19beta1 says `oidvector`, and a client decoding by oid was told the
            // wrong thing (ADR 0086 is the same sentence from the value's side).
            CatalogFunc::OidVector => ColumnType::OidVector,
            CatalogFunc::HstoreConcat | CatalogFunc::HstoreBuild => ColumnType::Hstore,
            // `->` keeps the document type and `->>` is text — measured,
            // `pg_typeof(payload->'b')` is `jsonb` and `pg_typeof(payload->>'b')` is `text`.
            CatalogFunc::JsonbConcat | CatalogFunc::JsonbFetch => ColumnType::Jsonb,
            CatalogFunc::JsonFetch => ColumnType::Json,
            CatalogFunc::ToTsVector | CatalogFunc::TsStrip | CatalogFunc::SetWeight => {
                ColumnType::TsVector
            }
            CatalogFunc::ToTsQuery
            | CatalogFunc::PlainToTsQuery
            | CatalogFunc::PhraseToTsQuery
            | CatalogFunc::WebsearchToTsQuery => ColumnType::TsQuery,
            CatalogFunc::RangeBuild => ColumnType::TsRange,
            // **`daterange(a, b)` is a `daterange`**, which it always was as a *value* — the
            // evaluator builds a `Datum::Range` over `date` — and was called `text` only because
            // `pg_typeof` read the datum and nothing else asked. Resolving that function against
            // the declared type (ADR 0093) made the two disagree out loud, which is what a
            // measured answer is for.
            CatalogFunc::DateRange => ColumnType::DateRange,
            // **`LOCALTIMESTAMP` is the one of the four without a zone**, which is the whole
            // reason it is a separate member: the type is what decides whether a column takes it.
            CatalogFunc::Now
            | CatalogFunc::StatementTimestamp
            | CatalogFunc::ClockTimestamp => ColumnType::TimestampTz,
            // **`date_trunc` answers its argument's type**, which this function cannot say:
            // it takes no arguments. `exec::query::expr_type` has an arm above the one that
            // calls this, exactly as `greatest` does, and that is the answer a client is
            // told. The unzoned type here is what the two-argument form over a `timestamp`
            // gives — the shape the suite sends — and is only what a caller that skipped
            // that arm would see.
            CatalogFunc::DateTrunc | CatalogFunc::LocalTimestamp => ColumnType::Timestamp,
            CatalogFunc::LocalTime => ColumnType::Time,
            CatalogFunc::CurrentDate => ColumnType::Date,
            CatalogFunc::ConvertTo => ColumnType::Bytea,
            CatalogFunc::Random => ColumnType::Double,
        }
    }
}

/// The four functions a sequence answers to.
///
/// PostgreSQL has one more, `nextval`'s sibling `setval` in its two-argument and three-argument
/// forms, which are one function here because they differ only in a boolean. Everything else in
/// `pg_sequence`'s surface — `ALTER SEQUENCE`, `CREATE SEQUENCE`, reading a sequence as a relation
/// — is `0A000` naming itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceFunc {
    /// `nextval(regclass)`: the next value, and a write.
    NextVal,
    /// `currval(regclass)`: the last value **this session** got from that sequence.
    CurrVal,
    /// `setval(regclass, bigint [, boolean])`: where the sequence resumes from.
    SetVal,
    /// `lastval()`: the last value this session got from *any* sequence.
    LastVal,
}

impl SequenceFunc {
    /// The four names, matched case-insensitively as PostgreSQL matches them.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "nextval" => Some(SequenceFunc::NextVal),
            "currval" => Some(SequenceFunc::CurrVal),
            "setval" => Some(SequenceFunc::SetVal),
            "lastval" => Some(SequenceFunc::LastVal),
            _ => None,
        }
    }

    /// What it is called — and, because PostgreSQL names an output column after the function that
    /// filled it, what `SELECT nextval('s')` calls its one column.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SequenceFunc::NextVal => "nextval",
            SequenceFunc::CurrVal => "currval",
            SequenceFunc::SetVal => "setval",
            SequenceFunc::LastVal => "lastval",
        }
    }
}

/// One sequence-function call, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceCall {
    /// Which one.
    pub func: SequenceFunc,
    /// The sequence's name, **folded the way an identifier is folded**, or `None` for `lastval()`.
    ///
    /// The argument is a string and PostgreSQL reads it as a *name*: measured,
    /// `nextval('Q1_ID_SEQ')` finds `q1_id_seq` and `nextval('"q1_id_seq"')` finds it too. So the
    /// quoting rules that apply to an identifier apply inside the quotes, which is not a thing a
    /// reader would guess about a `text` argument.
    pub name: Option<String>,
    /// `setval`'s value.
    pub value: Option<i64>,
    /// `setval`'s third argument, `is_called`, which defaults to true.
    ///
    /// True means the value has been handed out and the next `nextval` answers one *past* it;
    /// false means it has not and the next `nextval` answers it. Measured both ways.
    pub is_called: bool,
}

/// The five aggregates this node computes.
///
/// PostgreSQL has dozens; these are the five `ActiveRecord`'s own calculations use — `count`, `sum`,
/// `minimum`, `maximum`, `average` — and the four `esker-columnar`'s fragment evaluator already
/// defines (`docs/plans/phase-7-columnar.md` M2), which is why the semantics below are a match
/// rather than a second opinion. Everything else is `0A000` naming itself, contract C2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    /// `count(*)` and `count(expr)`, which are different aggregates wearing one name.
    Count,
    /// `sum`, and the width it goes to is not uniform: `int2` and `int4` widen to `bigint`,
    /// `int8` and `numeric` to `numeric` — which is why an `int8` sum cannot overflow — `money`
    /// and `float8` stay themselves, and an `interval` sums to an `interval`. **So does a
    /// `time`**, which is the pair that says the aggregate set is per (aggregate, type) and not
    /// per type: `min(time)` is a `time` where `sum(time)` is an `interval`.
    Sum,
    /// `min`, in [`crate::value::PgDatum::pg_cmp`] order.
    Min,
    /// `max`, likewise.
    Max,
    /// `avg`: `numeric` over every exact number since ADR 0045 gave this node one, `float8` over a
    /// `float8`, and an `interval` over an `interval` or a `time`.
    ///
    /// The interval one is not a division in a number type at all — the exact sum is kept and
    /// divided **once**, through the same cascade `interval / n` uses, because a running mean
    /// would round at every row.
    Avg,
    /// `array_agg(expr [ORDER BY …])`: every value of the group, in one array.
    ///
    /// The one aggregate here that is **not** a fold — it keeps every value rather than combining
    /// them, so it is the one whose memory is the group's size and the one that carries the
    /// `ORDER BY` clause. Over **no rows it is NULL**, not an empty array, which is the answer
    /// that surprises: `array_agg(id) FROM t WHERE false` is NULL and `count(id)` is 0.
    ArrayAgg,
    /// `string_agg(expr, delimiter [ORDER BY …])`: every value of the group, joined.
    ///
    /// The second aggregate here that takes **two** arguments' worth of input, and the only one
    /// whose second is read per row: PostgreSQL's transition function takes the delimiter with
    /// each value, so the separator between rows *i* and *i+1* is the delimiter row *i+1* carried.
    /// Constant in every statement anybody writes, and not constant by rule.
    ///
    /// A **NULL delimiter is not a NULL answer** — it is an empty separator, measured:
    /// `string_agg(t, NULL)` over `a` and `b` is `ab`. Over no rows the answer is NULL, as it is
    /// for `array_agg` and for every fold but `count`.
    StringAgg,
}

impl AggregateFunc {
    /// Whether this aggregate's result is **the argument's own type**.
    ///
    /// `min` and `max` are, and it is what makes `min(current_mood)` a `mood` on a real server
    /// rather than the `int2` an enum is stored as. `count` is a `bigint` whatever it counts,
    /// `sum` and `avg` promote, and `array_agg` makes an array — none of the four can hand a
    /// user-defined type back (ADR 0050).
    #[must_use]
    pub fn keeps_its_argument_type(self) -> bool {
        matches!(self, AggregateFunc::Min | AggregateFunc::Max)
    }

    /// The five names, matched the way PostgreSQL matches them: case-insensitively, so `COUNT(*)`
    /// and `Count(*)` are the same call. Measured — both forms execute on a real server.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(AggregateFunc::Count),
            "sum" => Some(AggregateFunc::Sum),
            "min" => Some(AggregateFunc::Min),
            "max" => Some(AggregateFunc::Max),
            "avg" => Some(AggregateFunc::Avg),
            "array_agg" => Some(AggregateFunc::ArrayAgg),
            "string_agg" => Some(AggregateFunc::StringAgg),
            _ => None,
        }
    }

    /// What it is called, in the lower case PostgreSQL's own messages use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
            AggregateFunc::Avg => "avg",
            AggregateFunc::ArrayAgg => "array_agg",
            AggregateFunc::StringAgg => "string_agg",
        }
    }
}

/// One aggregate call, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    /// Which one.
    pub func: AggregateFunc,
    /// The arguments as written. Every one of the five takes exactly one; the rest are carried so
    /// that the refusal can name their **types** the way a real server does — measured,
    /// `count(n, g)` is `function count(bigint, text) does not exist`, and the types are not known
    /// until the planner has resolved them.
    pub args: Vec<Expr>,
    /// `count(*)`: the argument list was a single `*`, so the call reads no value at all — which
    /// is why it counts a row whose every column is NULL.
    pub star: bool,
    /// `DISTINCT` *inside* the parentheses: `count(DISTINCT a)`. Not the same clause as
    /// `SELECT DISTINCT`, which is on [`crate::plan::Select`].
    pub distinct: bool,
    /// `ORDER BY` *inside* the parentheses: `array_agg(x ORDER BY y DESC)`. Not the query's
    /// `ORDER BY` — it orders the values **within one group**, by expressions the aggregate does
    /// not return.
    ///
    /// Carried for every aggregate rather than for `array_agg` alone, because a real server takes
    /// the clause on all of them; on the four that fold, it changes nothing and is honoured by
    /// costing a sort nobody can observe. Empty for every call that does not write it.
    pub order_by: Vec<crate::plan::OrderItem>,
}

impl AggregateCall {
    /// The single argument this call folds over, or `None` for `count(*)`.
    ///
    /// `None` for a call of the wrong arity too, which is why the planner checks the arity before
    /// it asks.
    #[must_use]
    pub fn arg(&self) -> Option<&Expr> {
        if self.star { None } else { self.args.first() }
    }
}

/// A binary arithmetic operator.
///
/// **A separate enum from [`BinaryOp`], deliberately.** Every match on `BinaryOp` in this crate
/// assumes the expression yields a boolean — a comparison or a connective — and there are enough
/// of them that adding `+` there would mean auditing each one for a case it was never written to
/// have. Arithmetic yields a *value* whose type depends on both operands, which is a different
/// shape of question, so it gets a different node (`crate::value::arith`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `*`.
    Multiply,
    /// `/` — integer division truncates **toward zero**.
    Divide,
    /// `%` — the sign of the **dividend**, and undefined for the floats.
    Modulo,
    /// `^` — **left-associative** (`2 ^ 3 ^ 2` is 64) and looser than unary minus
    /// (`-2 ^ 2` is 4). Both measured, both the opposite of the mathematical convention.
    Power,
    /// `&` — bitwise AND, integers only.
    BitAnd,
    /// `|` — bitwise OR, integers only. `ActiveRecord` builds every advisory-lock key with it.
    BitOr,
    /// `#` — bitwise XOR. **`^` is exponentiation** in this dialect, which is why the two symbols
    /// are not the pair a reader coming from C expects.
    BitXor,
    /// `<<` — left shift. **Keeps the left operand's type**, and the count wraps modulo that
    /// type's width: `1::int4 << 32` is `1`. Measured.
    ShiftLeft,
    /// `>>` — **arithmetic** right shift: `(-1) >> 1` is `-1`, not a large positive number.
    ShiftRight,
}

impl ArithOp {
    /// The symbol, for the `operator does not exist: boolean + integer` message.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Subtract => "-",
            ArithOp::Multiply => "*",
            ArithOp::Divide => "/",
            ArithOp::Modulo => "%",
            ArithOp::Power => "^",
            ArithOp::BitAnd => "&",
            ArithOp::BitOr => "|",
            ArithOp::BitXor => "#",
            ArithOp::ShiftLeft => "<<",
            ArithOp::ShiftRight => ">>",
        }
    }
}

/// The operators phase 6a evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `=`.
    Eq,
    /// `<>`, and `!=` which PostgreSQL treats as the same operator.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
    /// `AND`.
    And,
    /// `OR`.
    Or,
    /// `IS DISTINCT FROM`: `<>` made **total**, so two NULLs are not distinct and one NULL is.
    ///
    /// A [`BinaryOp`] rather than a shape of its own, because that is what PostgreSQL makes it:
    /// its operands are typed by the same rules `=`'s are, which is what lets
    /// `x IS NOT DISTINCT FROM 1` read the constant as the column's type. The one rule it does
    /// **not** share is the NULL rule — these two are the comparisons that never answer unknown,
    /// and that is the whole of why they exist.
    Distinct,
    /// `IS NOT DISTINCT FROM`: null-safe equality, and the operator `upsert_all` writes.
    ///
    /// Rails' `upsert_all` template is
    /// `CASE WHEN (t.c IS NOT DISTINCT FROM excluded.c) THEN t.updated_at ELSE CURRENT_TIMESTAMP END`,
    /// which is how it leaves `updated_at` alone when nothing changed. With `=` there instead, a
    /// NULL column would make the `WHEN` unknown and touch the timestamp on every upsert — the
    /// bug this operator exists to prevent.
    NotDistinct,
}

impl BinaryOp {
    /// The symbol, for the `operator does not exist: text = integer` message.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
            BinaryOp::Distinct => "IS DISTINCT FROM",
            BinaryOp::NotDistinct => "IS NOT DISTINCT FROM",
        }
    }

    /// The operator PostgreSQL names when *this* one has none for a type.
    ///
    /// `IS DISTINCT FROM` is built on `=`, and a real server refusing it says
    /// `operator does not exist: json = json` rather than repeating the spelling the user wrote —
    /// measured, and the same for `IS NOT DISTINCT FROM`. Everything else names itself.
    ///
    /// One function because two places ask: the literal reconciliation in `exec::query::retype`,
    /// which refuses before a type is even settled, and the type check after it. They disagreed,
    /// and the one that fires first is the one nobody had looked at.
    #[must_use]
    pub fn missing_symbol(self) -> &'static str {
        match self {
            BinaryOp::Distinct | BinaryOp::NotDistinct => BinaryOp::Eq.symbol(),
            other => other.symbol(),
        }
    }

    /// Whether this is a comparison rather than a connective.
    #[must_use]
    pub fn is_comparison(self) -> bool {
        !matches!(self, BinaryOp::And | BinaryOp::Or)
    }
}

/// A constant, still untyped.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `NULL`, which fits every column and no type.
    Null,
    /// `NULL::bigint` — a NULL that **knows what it is**.
    ///
    /// PostgreSQL types a NULL the moment a cast names one, and everything after resolves against
    /// that type rather than around it: `WHERE id IN (SELECT NULL::bigint)` matches nothing, where
    /// the same subquery over an *untyped* NULL is `42883 operator does not exist: bigint = text`
    /// — an untyped NULL is `text` in this crate and there is no such operator. Dropping the cast,
    /// which is what `NULL::anything is NULL` did, loses exactly that.
    ///
    /// The **value** is still nothing: it assigns as `Datum::Null`, compares as unknown and prints
    /// as NULL. Only its type survives, which is the whole of what the cast was for.
    TypedNull(ColumnType),
    /// An integer.
    Integer(i64),
    /// A decimal, kept as written — see the module note on why the digits matter.
    Decimal(String),
    /// A quoted string: PostgreSQL's `unknown`, and the reason [`Literal::assign`] can lean on
    /// [`Datum::from_text`].
    String(String),
    /// `TRUE` or `FALSE`.
    Bool(bool),
    /// A value the planner already resolved against a column's type — a `timestamptz` read out of
    /// a quoted literal, say. It carries no ambiguity left to resolve, which is the point: the
    /// executor evaluates it and nothing re-reads the text.
    ///
    /// **`user` is the type it was *declared* as, when a `ColumnType` cannot say.** An enum is
    /// stored as its label's ordinal, so `'sad'::mood` resolves to a `Datum::Int2` and the name
    /// `mood` has nowhere to live: `ColumnType` is a closed enum of storage types and
    /// `plan::SelectItem::user_type` is a slot only the **projection** has. Without it a
    /// comparison against an enum column was `42883 operator does not exist: mood = smallint`, a
    /// write was `42804`, and a `UNION` answered ordinals under `smallint` — three readers, one
    /// missing fact (`debts-v1.1.md` #57, ADR 0050's unfinished half).
    ///
    /// **The type and not its oid**, which is the same slot [`crate::plan::SelectItem::Expr`]
    /// holds one position over. An oid settles *identity* — is this the enum the column was
    /// declared as — and there are two readers that need more than that: `42883 operator does not
    /// exist: mood = other_mood` and `42804 … expression is of type other_mood` both name the
    /// other type, and the row evaluator has no catalog to look a number up in. Measured on
    /// 19beta1; this node said `smallint` for both.
    ///
    /// `None` is every other literal, which is most of them: the field says *which user-defined
    /// type this value is a value of*, and a `bigint` is not one.
    Typed {
        /// The resolved value — an enum's ordinal, a date's day count, a range's canonical text.
        value: Box<Datum>,
        /// The `catalog::TypeDef` it was declared as, when the value's type is one this vocabulary
        /// cannot spell. Boxed so that a literal stays the size it was.
        user: Option<Box<crate::catalog::TypeDef>>,
    },
}

impl Literal {
    /// A resolved value with no user-defined type over it — the ordinary case, and the only one
    /// there was before [`Literal::Typed::user`] existed.
    #[must_use]
    pub fn typed(value: Box<Datum>) -> Self {
        Literal::Typed { value, user: None }
    }

    /// The type name PostgreSQL uses for this literal when it complains about it.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            // The type the cast named, which is the whole point of carrying it.
            Literal::TypedNull(ty) => ty.name(),
            // A NULL has no type to name, and never reaches a mismatch anyway; a quoted string
            // is PostgreSQL's `unknown` and takes whatever type the column gives it.
            Literal::Null | Literal::String(_) => "unknown",
            // PostgreSQL types a small integer constant `integer`, not `bigint`.
            Literal::Integer(value) if i32::try_from(*value).is_ok() => "integer",
            Literal::Integer(_) => "bigint",
            Literal::Decimal(_) => "numeric",
            Literal::Typed { value, .. } => value.column_type().map_or("unknown", ColumnType::name),
            Literal::Bool(_) => "boolean",
        }
    }

    /// Whether this literal can be *compared* against a column of `ty`.
    ///
    /// Comparison is stricter than assignment, and the difference is not a detail: PostgreSQL
    /// stores `42` in a `text` column happily — that is an assignment cast — and answers
    /// `WHERE txt = 42` with `operator does not exist: text = integer`, because there is no such
    /// operator to call. Using the assignment rule for both would turn that error into a silent
    /// `false`, which is a wrong answer rather than a missing feature. Measured on both sides.
    #[must_use]
    pub fn comparable_with(&self, ty: ColumnType) -> bool {
        match self {
            // **A typed NULL is comparable where its type is**, and not otherwise: the value being
            // nothing does not make an operator exist, so `id = NULL::text` over a `bigint` should
            // be the same `42883` that `id = 'x'::text` is.
            //
            // **Not captured.** It follows from PostgreSQL resolving an operator by the *types* of
            // its arguments, and `tests/corpus/pg19_operator_types.txt` measures that rule for two
            // columns — but this exact pair has not been put to the oracle, which was unreachable
            // when this landed. `docs/plans/phase-9-rails.md` owes it a corpus.
            Literal::TypedNull(null) => crate::exec::query::same_family(*null, ty),
            // `unknown` takes whatever type the other side has -- if it can be read as one.
            Literal::Null | Literal::String(_) => true,
            Literal::Integer(_) => matches!(
                ty,
                ColumnType::Int8
                    | ColumnType::Int4
                    | ColumnType::Int2
                    | ColumnType::Double
                    | ColumnType::Real
                    // Measured: `26::oid = 26` is `t` on a real server — there is an implicit
                    // cast from an integer to an `oid`, which is how every catalog query
                    // compares one against a plain number.
                    | ColumnType::Oid
                    | ColumnType::RegProc
                    // **And its two siblings, which were missing.** Measured on 19beta1:
                    // `'pg_class'::regclass = 1259` is `t`, `'int4'::regtype = 23` is `t` and
                    // `'int4'::regtype > 20` is `t` — every `reg*` compares as the oid it is, so
                    // a plain number reaches all four and not two of them. This node answered
                    // `42883 operator does not exist: regtype = integer` for the third
                    // (wire v3 family F8).
                    | ColumnType::RegClass
                    | ColumnType::RegType
                    | ColumnType::Numeric
            ),
            Literal::Decimal(_) => matches!(
                ty,
                ColumnType::Int8
                    | ColumnType::Int4
                    | ColumnType::Int2
                    | ColumnType::Double
                    | ColumnType::Real
                    | ColumnType::Numeric
            ),
            Literal::Bool(_) => matches!(ty, ColumnType::Bool),
            // **Three arms used to stand here, one per `Datum` family, and the measured table
            // subsumes all three.** They were a `numeric` value's list, an integer value's list
            // and an oid-ish value's list, each written the day a pair of its own was found and
            // each stopping where its author stopped: the integer one reached `oid` and not
            // `regclass`, `regproc` or `regtype`, so `regtype_col = 1::bigint` was
            // `42883 operator does not exist: regtype = bigint` for a comparison a real server
            // answers — nine rows of `tests/captures/pg19_comparison_matrix_column.txt`, and they
            // were the last nine, found only after the other seventy-six had gone. `same_family`
            // gives each of the three lists exactly the answers it gave, and gives the nine the
            // right one.
            //
            // **A range goes through the same arm, and the representative is the right answer
            // here.** `Datum::Range` carries its *subtype*, so `column_type` names a set — but
            // only one pair shares one, `int4range` and `int8range`, both being ranges of an
            // `int8` in this crate. Every other range spelling has a subtype of its own and so a
            // representative that is exactly itself. An `int8range` **literal** cannot reach this
            // arm at all: `column_type` disagrees with the type it was cast to, so
            // `parse::lower::lower_cast` keeps the `Cast` node and the pair takes `reconcile`'s
            // two-typed arm instead. So the family test is right for every range that gets here,
            // and it is what refuses `r = '[1,3)'::int4range` over an `int8range` column — the
            // last of the 86 and the one `Datum::fits` cannot see, because `fits` compares
            // subtypes and these two share one.
            // **And everything else asks the measured table, where it used to ask `fits`.** `fits`
            // is the **assignment** rule — this function's own first paragraph says that is the
            // wrong question for a comparison — and it was still the last arm here, so a literal
            // written with a cast was admitted or refused by whether it could be *stored* in the
            // column. That refused 31 comparisons a real server answers, one per pair whose two
            // types are related by an operator and not by a coercion: `bigint = double precision`,
            // `date = timestamp`, `citext = text`, `inet = cidr`, `interval = time`,
            // `regtype = bigint`.
            //
            // `same_family` is the authority `exec::query::reconcile`'s two-column arm and its
            // two-literal arm already ask, and it is measured twice over — 2,704 pairs as two
            // columns and 2,704 as two literals. Three readers of one question, and this was the
            // last one still answering it its own way (`debts-v1.1.md` #43).
            Literal::Typed { value, .. } => value
                .column_type()
                .is_none_or(|held| crate::exec::query::same_family(held, ty)),
        }
    }

    /// Resolves this literal against the column it is being assigned to.
    ///
    /// `column` is only for the message; PostgreSQL names the column in a type mismatch and a
    /// client reading "is of type bytea but expression is of type integer" needs to know which one.
    ///
    /// One arm per literal kind per column type, which is why it is long: it grows by a line each
    /// time a type is added, and every arm is a measured answer rather than a fallthrough — a `_`
    /// here is how a new type would silently take some other type's assignment rule.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per literal kind per column type, and a `_` would hide the next one"
    )]
    pub fn assign(&self, ty: ColumnType, column: &str) -> Result<Datum> {
        let mismatch = || {
            Err(SqlError::DatatypeMismatchInColumn {
                column: column.to_owned(),
                column_type: ty.name().to_owned(),
                expression_type: self.type_name().to_owned(),
            })
        };
        match self {
            // A typed NULL assigns like an untyped one: the type was for resolution, and the
            // value is nothing whatever column it lands in.
            Literal::Null | Literal::TypedNull(_) => Ok(Datum::Null),

            // The `unknown` literal: whatever the column is, read it as that. This is one
            // function, checked against a real server for all six types, rather than six rules.
            Literal::String(text) => Datum::from_text(ty, text),

            Literal::Integer(value) => match ty {
                // **An integer is a `regtype`**, printing as the type it names or as its own
                // digits: `23::regtype` is `integer` and `999999::regtype` is `999999`, neither an
                // error. Measured.
                ColumnType::RegType => u32::try_from(*value).map_or_else(
                    |_| mismatch(),
                    |oid| Ok(crate::value::regtype_of_oid(oid)),
                ),
                // **And an integer is a `regproc`**, on the same measurement: `42::regproc` is
                // `int4in` and `24::regproc` is `24`, neither an error.
                ColumnType::RegProc => u32::try_from(*value).map_or_else(
                    |_| mismatch(),
                    |oid| {
                        Ok(Datum::RegProc {
                            oid,
                            name: crate::value::reg_proc::to_text(oid).into_boxed_str(),
                        })
                    },
                ),
                // **And an integer is a `regclass`**, on the same measurement one letter along:
                // `1259::regclass` is `pg_class` and `999999::regclass` is `999999`. The name a
                // resolvable oid prints is put on the datum where the catalog is, so what is
                // built here carries the digits.
                ColumnType::RegClass => Ok(crate::value::regclass_of_oid(*value)),
                ColumnType::Int8 => Ok(Datum::Int8(*value)),
                // **A whole number of currency units, not of cents.** `VALUES (123)` into a
                // money column is `$123.00` on a real server, which is the assignment cast
                // `int8 -> money` and not a reinterpretation of the bits.
                ColumnType::Money => value
                    .checked_mul(100)
                    .map(Datum::Money)
                    .ok_or_else(|| SqlError::IntegerLiteralOutOfRange(ColumnType::Money.name())),
                // The range check is the type: a constant a real server refuses with `22003`
                // must not be quietly accepted here, which is the whole argument ADR 0033 made
                // for a distinct `int4` rather than an alias.
                ColumnType::Int4 => i32::try_from(*value)
                    .map(Datum::Int4)
                    .map_err(|_| SqlError::IntegerLiteralOutOfRange(ColumnType::Int4.name())),
                // **An `oid` takes an integer literal**, which is what makes it usable at
                // all: every catalog identifier is written as a plain number.
                ColumnType::Oid => crate::value::oid::from_text(&value.to_string()).map(Datum::Oid),
                ColumnType::Int2 => i16::try_from(*value)
                    .map(Datum::Int2)
                    .map_err(|_| SqlError::IntegerLiteralOutOfRange(ColumnType::Int2.name())),
                // Exactly, at scale zero — there is no width to overflow, which is what an
                // arbitrary-precision type means.
                ColumnType::Numeric => Datum::from_text(ColumnType::Numeric, &value.to_string()),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the widening is PostgreSQL's own assignment cast, and lossy the same way"
                )]
                ColumnType::Double => Ok(Datum::Double(*value as f64)),
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the widening is PostgreSQL's own assignment cast, and lossy the same way"
                )]
                ColumnType::Real => Ok(Datum::Real(*value as f32)),
                // PostgreSQL's assignment cast to text is the value's own text.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Name | ColumnType::Char | ColumnType::Bpchar => {
                    Ok(Datum::Text(value.to_string()))
                }
                ColumnType::Bool
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Timestamp
                // **A date is not a number.** `1::date` is `42846 cannot cast type integer to
                // date` on a real server; the Julian day is an implementation detail with no cast
                // to reach it, in either direction.
                | ColumnType::Date
                // A time is not a number either, and for the same reason: `1::time` is `42846`,
                // and a microsecond count since midnight has no cast to reach it.
                | ColumnType::Time
                // Sixteen bytes are not a number: `1::uuid` is `42846` on a real server.
                | ColumnType::Uuid
                | ColumnType::Interval
                // Neither takes a number or a boolean: `INSERT INTO t (j) VALUES (1)` is a type
                // mismatch on a real server, not a one-element document.
                | ColumnType::Json
                | ColumnType::Jsonb
                // **A scalar constant is not a one-element array.** PostgreSQL says
                // `column "a" is of type integer[] but expression is of type integer` and offers
                // a cast; writing `{1}` for `1` here would be inventing the user's intent.
                | ColumnType::Int8Array
                | ColumnType::Int4Array
        | ColumnType::Int2Array
                | ColumnType::NumericArray
                | ColumnType::TextArray
                // A number or a boolean is not an hstore literal, and a real server says so with the
                // same `42804` every other pair here gives.
                | ColumnType::Hstore
                | ColumnType::HstoreArray
                | ColumnType::TsVector
                | ColumnType::TsQuery
                | ColumnType::TsVectorArray
                | ColumnType::TsQueryArray
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::Inet | ColumnType::Cidr | ColumnType::MacAddr | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray
                | ColumnType::Bit | ColumnType::VarBit | ColumnType::BitArray | ColumnType::VarBitArray
                | ColumnType::Lseg | ColumnType::Box | ColumnType::Path | ColumnType::Polygon | ColumnType::Circle | ColumnType::Line
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::RegClassArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::CitextArray | ColumnType::Point | ColumnType::Xml | ColumnType::XmlArray | ColumnType::Ltree | ColumnType::LtreeArray | ColumnType::LQuery | ColumnType::LQueryArray | ColumnType::Int2VectorArray | ColumnType::OidVectorArray
                // **A pseudo-type takes no value at all**: no column is declared `void`, so an
                // assignment to one is a type mismatch like any other.
                | ColumnType::Void => mismatch(),
            },

            Literal::Decimal(digits) => match ty {
                // The same assignment cast one arm up, and the digits are already the spelling
                // `money`'s input function reads: `VALUES (123.45)` is `$123.45`.
                ColumnType::Money => Datum::from_text(ColumnType::Money, digits),
                // **The digits as written, trailing zeros and all.** This is the assignment the
                // type exists for: `1.000` into a `numeric` column is `1.000`, where the same
                // literal into a `double precision` one is `1`. No rounding and no widening —
                // the column's declared scale, if it has one, is applied afterwards by
                // `value::fit_to_typmod`, which is where the rounding rule lives.
                ColumnType::Numeric => Datum::from_text(ColumnType::Numeric, digits),
                // The same refusal `int8` gets, naming the column's own type.
                ColumnType::Int4 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the integer column \"{column}\""
                ))),
                ColumnType::Int2 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the smallint column \"{column}\""
                ))),
                // `numeric` has no signed zero, so `-0.0` in a `double precision` column is `0`
                // and not `-0`. Measured: the literal goes through `numeric` on its way, and that
                // is where the sign is lost.
                // The same `numeric` road one width down, and the same signed-zero loss.
                ColumnType::Real => {
                    numeric_text(Datum::from_text(ColumnType::Real, digits), digits).map(|value| {
                        match value {
                            Datum::Real(0.0) => Datum::Real(0.0),
                            other => other,
                        }
                    })
                }
                ColumnType::Double => {
                    numeric_text(Datum::from_text(ColumnType::Double, digits), digits).map(
                        |value| match value {
                            // `0.0` as a pattern already matches `-0.0`, which is
                            // exactly the case being normalised away.
                            Datum::Double(0.0) => Datum::Double(0.0),
                            other => other,
                        },
                    )
                }
                // The digits as written, which is what `numeric`'s own text is.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Name | ColumnType::Char | ColumnType::Bpchar => {
                    Ok(Datum::Text(digits.clone()))
                }
                ColumnType::Int8 => Err(SqlError::unsupported(format!(
                    "assigning the numeric literal {digits} to the bigint column \"{column}\""
                ))),
                ColumnType::Bool
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Date
                // A number is not a document, whichever way it is written.
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Time
                | ColumnType::Uuid
                | ColumnType::Interval
                | ColumnType::Oid
                | ColumnType::Timestamp
                // **A scalar constant is not a one-element array.** PostgreSQL says
                // `column "a" is of type integer[] but expression is of type integer` and offers
                // a cast; writing `{1}` for `1` here would be inventing the user's intent.
                | ColumnType::Int8Array
                | ColumnType::Int4Array
        | ColumnType::Int2Array
                | ColumnType::NumericArray
                | ColumnType::TextArray
                // A number or a boolean is not an hstore literal, and a real server says so with the
                // same `42804` every other pair here gives.
                | ColumnType::Hstore
                | ColumnType::HstoreArray
                | ColumnType::TsVector
                | ColumnType::TsQuery
                | ColumnType::TsVectorArray
                | ColumnType::TsQueryArray
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::Inet | ColumnType::Cidr | ColumnType::MacAddr | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray
                | ColumnType::Bit | ColumnType::VarBit | ColumnType::BitArray | ColumnType::VarBitArray
                | ColumnType::Lseg | ColumnType::Box | ColumnType::Path | ColumnType::Polygon | ColumnType::Circle | ColumnType::Line
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::RegClassArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::RegClass | ColumnType::RegType | ColumnType::RegProc | ColumnType::CitextArray | ColumnType::Point | ColumnType::Xml | ColumnType::XmlArray | ColumnType::Ltree | ColumnType::LtreeArray | ColumnType::LQuery | ColumnType::LQueryArray | ColumnType::Int2VectorArray | ColumnType::OidVectorArray
                // **A pseudo-type takes no value at all**: no column is declared `void`, so an
                // assignment to one is a type mismatch like any other.
                | ColumnType::Void => mismatch(),
            },

            // Already resolved. It fits the column it was resolved against and nothing else.
            // **A `regclass` or `regtype` into an integer or `oid` column is the number it is**,
            // taken before `fits` can hand the name-carrying datum through: the same rule as
            // `into_column`'s, because this is the other write path (`crate::value::stored_shape`).
            Literal::Typed { value, .. }
                if matches!(**value, Datum::RegClass { .. } | Datum::RegType { .. })
                    && matches!(
                        ty,
                        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Oid
                    ) =>
            {
                crate::value::stored_shape(
                    (**value).clone(),
                    ty,
                    crate::value::Rendering::default(),
                )
            }
            Literal::Typed { value, .. } if value.fits(ty) => Ok((**value).clone()),
            // **A whole `numeric` into an integer column is an assignment that can overflow,
            // and the overflow is the answer.** An integer literal past `int8` is a `numeric`
            // (see `lower_value`), so `INSERT INTO t (a_bigint) VALUES (9223372036854775808)` is
            // this arm — and on PostgreSQL it is `22003 bigint out of range`, measured: the
            // literal is fine and *storing* it is not.
            //
            // **The literal's own three-word message**, which is a different sentence from the one
            // the input function gives: PostgreSQL says `bigint out of range` for
            // `VALUES (9223372036854775808)` and `value "9223372036854775808" is out of range for
            // type bigint` for `VALUES ('9223372036854775808')`. Two paths, two messages, both
            // measured — so the value is *read* through the column's input function and the
            // refusal is written here.
            //
            // Whole numbers only. A fractional `numeric` in an integer column is a rounding
            // assignment cast on a real server and neither this nor the `42804` below is that
            // answer; it is left where it was rather than given a second wrong one.
            Literal::Typed { value, .. }
                if matches!(**value, Datum::Numeric(_))
                    && matches!(ty, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
                    && value
                        .to_text()
                        .is_some_and(|text| !text.contains(['.', 'e', 'E'])) =>
            {
                let fits = value
                    .to_text()
                    .and_then(|text| Datum::from_text(ty, &text).ok());
                fits.ok_or(SqlError::IntegerLiteralOutOfRange(match ty {
                    ColumnType::Int2 => "smallint",
                    ColumnType::Int4 => "integer",
                    _ => "bigint",
                }))
            }
            // **An `ARRAY[…]`'s element type is settled by the column**, the way an integer
            // literal's is one level down. `ARRAY[1,2,3]` is `integer[]` on a real server and
            // `bigint[]` here — an integer literal is an `int8` in this crate until a column says
            // otherwise, which is what `Literal::Integer` above does — and both servers write the
            // same row into an `integer[]` column. Re-read through the array's own text, which is
            // `array_in` doing the element conversion: `ARRAY[2147483648]` into an `integer[]`
            // column then fails with `int4`'s own `22003` rather than with a type mismatch.
            //
            // A `Literal` is a constant in the statement, never a column reference, so this
            // settles a *literal's* type. It used to add that "`int8[]` into an `integer[]` column
            // is still `42804`, from `exec::assign`", and **that was wrong about PostgreSQL**:
            // measured, `UPDATE t SET int4arr = int8arr` is accepted, because an array's cast is
            // its element's and `bigint → integer` is an assignment cast. `exec::assign::coerce`
            // does it now, element by element, and so does the arm below this one.
            Literal::Typed { value, .. }
                if matches!(**value, Datum::Array(_))
                    && esker_keys::array::ArrayValue::element_of(ty).is_some() =>
            {
                match value.to_text() {
                    Some(text) => Datum::from_text(ty, &text),
                    None => mismatch(),
                }
            }
            // **A `B'…'` literal assigns to either bit type**, which is a real server's
            // `bit -> bit varying` assignment cast: `B'1100'` into a `bit varying(4)` column is
            // that column's value and not a `42804`. The `varying` flag on a `Datum::Bit` is the
            // *column's* everywhere in this crate — a value whose flag disagrees does not `fit` —
            // so the literal is re-read as the column's type, the road the array arm above takes.
            // The length rule then applies as it does to any assignment, `22026` and all.
            Literal::Typed { value, .. }
                if matches!(**value, Datum::Bit { .. })
                    && matches!(ty, ColumnType::Bit | ColumnType::VarBit) =>
            {
                match value.to_text() {
                    Some(text) => Datum::from_text(ty, &text),
                    None => mismatch(),
                }
            }
            // **A typed literal that PostgreSQL would assign-cast.** `VALUES (7::bigint)` into an
            // `integer` column is accepted on a real server and was `42804` here, because this arm
            // asked `fits` — storage equality — where assignment context asks a wider question.
            // The gate is `pg_cast` read off the server; the conversion is the one a `DEFAULT`
            // already used, so an overflow is the *short* `22003 integer out of range` rather than
            // the input function's longer sentence.
            //
            // After the two arms above, not before: an array and a `B'…'` literal have their own
            // measured rules and this must not take them.
            Literal::Typed { value, .. }
                if crate::value::has_assignment_cast(value.column_type(), ty) =>
            {
                // **The boot rendering, because lowering has no session.** A cast folded here cannot ask
                // which zone the client is in, which is the gap `tests/assignment_cast_date.rs`
                // declares for `'…'::timestamptz::date` written as a literal.
                crate::value::assignment_cast(
                    (**value).clone(),
                    ty,
                    crate::value::Rendering::default(),
                )
            }
            Literal::Typed { .. } => mismatch(),

            Literal::Bool(value) => match ty {
                ColumnType::Bool => Ok(Datum::Bool(*value)),
                // `true`, not `t`: the cast, not the output function.
                ColumnType::Text
                | ColumnType::Varchar
                | ColumnType::Name | ColumnType::Char
                | ColumnType::Bpchar => Ok(Datum::Text(
                    if *value { "true" } else { "false" }.to_owned(),
                )),
                // A boolean is not a number, so it is not a money either: `money` takes the
                // two numeric assignment casts above and nothing else.
                ColumnType::Money
                | ColumnType::Int8
                | ColumnType::Int4
                | ColumnType::Int2
                | ColumnType::Bytea
                | ColumnType::TimestampTz
                | ColumnType::Timestamp
                | ColumnType::Double
                // `true` *is* a JSON document, and `INSERT INTO t (j) VALUES (true)` is still a
                // type mismatch on a real server: the literal is a `boolean`, and there is no
                // assignment cast from one to `json`.
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Date
                | ColumnType::Numeric
                | ColumnType::Time
                | ColumnType::Uuid
                | ColumnType::Interval
                | ColumnType::Oid
                | ColumnType::Real
                // **A scalar constant is not a one-element array.** PostgreSQL says
                // `column "a" is of type integer[] but expression is of type integer` and offers
                // a cast; writing `{1}` for `1` here would be inventing the user's intent.
                | ColumnType::Int8Array
                | ColumnType::Int4Array
        | ColumnType::Int2Array
                | ColumnType::NumericArray
                | ColumnType::TextArray
                // A number or a boolean is not an hstore literal, and a real server says so with the
                // same `42804` every other pair here gives.
                | ColumnType::Hstore
                | ColumnType::HstoreArray
                | ColumnType::TsVector
                | ColumnType::TsQuery
                | ColumnType::TsVectorArray
                | ColumnType::TsQueryArray
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::Inet | ColumnType::Cidr | ColumnType::MacAddr | ColumnType::InetArray | ColumnType::CidrArray | ColumnType::MacAddrArray
                | ColumnType::Bit | ColumnType::VarBit | ColumnType::BitArray | ColumnType::VarBitArray
                | ColumnType::Lseg | ColumnType::Box | ColumnType::Path | ColumnType::Polygon | ColumnType::Circle | ColumnType::Line
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoxArray | ColumnType::LsegArray | ColumnType::PathArray | ColumnType::PolygonArray | ColumnType::CircleArray | ColumnType::LineArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::NameArray | ColumnType::CharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::RegTypeArray | ColumnType::RegProcArray | ColumnType::RegClassArray | ColumnType::Int2Vector | ColumnType::OidVector | ColumnType::RegClass | ColumnType::RegType | ColumnType::RegProc | ColumnType::CitextArray | ColumnType::Point | ColumnType::Xml | ColumnType::XmlArray | ColumnType::Ltree | ColumnType::LtreeArray | ColumnType::LQuery | ColumnType::LQueryArray | ColumnType::Int2VectorArray | ColumnType::OidVectorArray
                // **A pseudo-type takes no value at all**: no column is declared `void`, so an
                // assignment to one is a type mismatch like any other.
                | ColumnType::Void => mismatch(),
            },
        }
    }
}

/// Rewrites a float's out-of-range message to quote the literal as **`numeric`** would print it.
///
/// A bare `1e400` is a `numeric` before anything casts it, so the value PostgreSQL quotes is
/// `numeric`'s own text — plain decimal, no exponent — where a *string* `'1e400'` is quoted
/// exactly as written. Measured on 19beta1 for both float widths
/// ([`crate::value::float::plain_decimal`] carries the two statements).
fn numeric_text(outcome: Result<Datum>, digits: &str) -> Result<Datum> {
    outcome.map_err(|error| match error {
        SqlError::FloatOutOfRange { ty, .. } => SqlError::FloatOutOfRange {
            ty,
            value: crate::value::float::plain_decimal(digits),
        },
        other => other,
    })
}

impl Expr {
    /// Evaluates to a value for a column of `ty`, which is what `INSERT` needs.
    ///
    /// A `$1` with nothing bound to it is `42P02`, which is what PostgreSQL answers a simple query
    /// that contains one — the simple query protocol has no way to carry a parameter.
    pub fn evaluate(&self, ty: ColumnType, column: &str) -> Result<Datum> {
        match self {
            Expr::Literal(literal) => literal.assign(ty, column),
            Expr::Parameter(number) => Err(SqlError::UndefinedParameter(*number)),
            other => Err(SqlError::unsupported(format!(
                "{} in a VALUES list",
                describe(other)
            ))),
        }
    }
}

/// `~`, `~*`, `!~` or `!~*` — which of the four an [`Expr::RegexMatch`] is.
///
/// Written from *our* side rather than from the AST's, the way every other name in this crate is:
/// it is what a plan prints and what a message quotes back.
#[must_use]
pub fn regex_operator(negated: bool, case_insensitive: bool) -> &'static str {
    match (negated, case_insensitive) {
        (false, false) => "~",
        (false, true) => "~*",
        (true, false) => "!~",
        (true, true) => "!~*",
    }
}

fn describe(expr: &Expr) -> &'static str {
    match expr {
        Expr::ToText { .. } => "a cast to text",
        Expr::Cast { .. } => "a cast",
        Expr::Collate { .. } => "a COLLATE clause",
        Expr::Array { .. } => "an ARRAY constructor",
        Expr::Scalar { func, .. } => func.name(),
        Expr::Literal(_) => "a literal",
        Expr::Parameter(_) => "a parameter",
        Expr::Column { .. } | Expr::Ordinal { .. } => "a column reference",
        Expr::Outer { .. } => "a correlated column reference",
        Expr::Binary { .. } => "an operator",
        Expr::Arithmetic { .. } | Expr::Negate(_) => "an arithmetic operator",
        Expr::Not(_) => "NOT",
        Expr::IsNull { .. } => "IS NULL",
        Expr::InList { negated: false, .. } => "IN",
        // **Named by its own spelling**, now that there are twelve of them: this string reaches a
        // user inside a `0A000`, and "= ANY" was a lie for `> ALL` the moment the node could hold
        // one. Written out rather than formatted because the answer is a `&'static str`, and
        // written as a pair rather than two lookups because "the operator" and "the quantifier"
        // are not separately meaningful in the sentence a refusal makes.
        Expr::QuantifiedArray { op, all, .. } => match (op, all) {
            (BinaryOp::Eq, false) => "= ANY",
            (BinaryOp::Eq, true) => "= ALL",
            (BinaryOp::NotEq, false) => "<> ANY",
            (BinaryOp::NotEq, true) => "<> ALL",
            (BinaryOp::Lt, false) => "< ANY",
            (BinaryOp::Lt, true) => "< ALL",
            (BinaryOp::LtEq, false) => "<= ANY",
            (BinaryOp::LtEq, true) => "<= ALL",
            (BinaryOp::Gt, false) => "> ANY",
            (BinaryOp::Gt, true) => "> ALL",
            (BinaryOp::GtEq, false) => ">= ANY",
            (BinaryOp::GtEq, true) => ">= ALL",
            // `AND`, `OR` and the two `DISTINCT` forms are not comparisons a quantifier accepts,
            // and `parse::lower` refuses them before this node can be built
            // (`the operator <op> with ANY/ALL`).
            _ => "a quantified comparison",
        },
        Expr::Subscript { .. } => "a subscript",
        Expr::Uuid(func) => func.name(),
        Expr::CurrentSetting { .. } => "current_setting",
        Expr::CurrentSchema { all: None } => "current_schema",
        Expr::CurrentSchema { .. } => "current_schemas",
        Expr::Advisory { call, .. } => call.name(),
        Expr::CurrentDatabase => "current_database",
        Expr::CurrentUser => "current_user",
        Expr::Like {
            case_insensitive: false,
            ..
        } => "LIKE",
        Expr::Like { .. } => "ILIKE",
        Expr::RegexMatch {
            negated,
            case_insensitive,
            ..
        } => regex_operator(*negated, *case_insensitive),
        Expr::InList { negated: true, .. } => "NOT IN",
        Expr::Aggregate(_) => "an aggregate function",
        Expr::Default => "DEFAULT",
        Expr::Sequence(_) => "a sequence function",
        Expr::CatalogFunc(_) => "a catalog function",
        Expr::SetFunc(_) => "a set-returning function",
        Expr::Coalesce(_) => "COALESCE",
        Expr::Case { .. } => "CASE",
        Expr::Subquery(sub) => sub.kind.describe(),
    }
}

/// Which advisory-lock function was written.
///
/// The two blocking forms wait: they poll the table on the statement's own clock and answer only
/// once they hold the lock, which is what separates them from the `try` pair. They were refused by
/// name until `connection_test.rb`'s *get and release advisory lock* turned up sending
/// `pg_advisory_lock` — `ActiveRecord`'s migrator sends the `try` shape
/// (`postgresql_adapter.rb:474`) and its connection tests do not.
///
/// The `xact` family (`pg_advisory_xact_lock` and friends) is still refused by name: those are
/// released by the *transaction* ending rather than by an unlock, which is a lifetime this table
/// does not model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryCall {
    /// `pg_advisory_lock(bigint)` / `(int4, int4)` — the **blocking** form, which waits.
    Lock,
    /// `pg_advisory_lock_shared(bigint)` / `(int4, int4)`.
    LockShared,
    /// `pg_try_advisory_lock(bigint)` / `(int4, int4)`.
    TryLock,
    /// `pg_try_advisory_lock_shared(bigint)` / `(int4, int4)`.
    TryLockShared,
    /// `pg_advisory_unlock(bigint)` / `(int4, int4)`.
    Unlock,
    /// `pg_advisory_unlock_shared(bigint)` / `(int4, int4)`.
    UnlockShared,
    /// `pg_advisory_unlock_all()` — no arguments, and it releases every lock this session holds.
    UnlockAll,
}

impl AdvisoryCall {
    /// The name as written, for a message and for `EXPLAIN`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AdvisoryCall::Lock => "pg_advisory_lock",
            AdvisoryCall::LockShared => "pg_advisory_lock_shared",
            AdvisoryCall::TryLock => "pg_try_advisory_lock",
            AdvisoryCall::TryLockShared => "pg_try_advisory_lock_shared",
            AdvisoryCall::Unlock => "pg_advisory_unlock",
            AdvisoryCall::UnlockShared => "pg_advisory_unlock_shared",
            AdvisoryCall::UnlockAll => "pg_advisory_unlock_all",
        }
    }

    /// Whether this one takes a lock (rather than releasing one).
    #[must_use]
    pub fn takes(self) -> bool {
        matches!(
            self,
            AdvisoryCall::TryLock
                | AdvisoryCall::TryLockShared
                | AdvisoryCall::Lock
                | AdvisoryCall::LockShared
        )
    }

    /// Whether it **waits** for the lock rather than answering `false`.
    ///
    /// The difference is the whole of the two families: `pg_try_advisory_lock` answers now, and
    /// `pg_advisory_lock` does not answer until it has the lock. It also decides what the call
    /// evaluates to — a `boolean` for the first and `void` for the second.
    #[must_use]
    pub fn blocks(self) -> bool {
        matches!(self, AdvisoryCall::Lock | AdvisoryCall::LockShared)
    }

    /// Whether it answers `void` rather than a `boolean`.
    ///
    /// **The split is "is there anything to report".** Measured from `pg_proc.prorettype` for all
    /// eleven advisory functions a real server has: the *blocking* acquires and
    /// `pg_advisory_unlock_all` are `void`, because they can only succeed, and the `try_` forms and
    /// the single-lock `unlock`s are `boolean`, because they can fail to do what was asked. A rule
    /// that made the whole family one type would be wrong for four of the seven here, and
    /// `connection_test.rb#test_get_and_release_advisory_lock` reads exactly the boolean half.
    ///
    /// **`void` is not NULL**, which is the trap: `pg_advisory_unlock_all() IS NULL` is `f` on a
    /// real server. Its value is an **empty string** — a NULL would have answered `t` to that
    /// `IS NULL`, a wrong answer rather than a missing type — and since
    /// `ColumnType::Void` exists the *declared* type is 2278 as well
    /// (`tests/captures/pg19_void.txt`).
    #[must_use]
    pub fn is_void(self) -> bool {
        matches!(
            self,
            AdvisoryCall::Lock | AdvisoryCall::LockShared | AdvisoryCall::UnlockAll
        )
    }

    /// The mode it works in.
    #[must_use]
    pub fn mode(self) -> crate::advisory::Mode {
        match self {
            AdvisoryCall::Lock | AdvisoryCall::TryLock | AdvisoryCall::Unlock => {
                crate::advisory::Mode::Exclusive
            }
            AdvisoryCall::LockShared
            | AdvisoryCall::TryLockShared
            | AdvisoryCall::UnlockShared
            // It releases both modes; the mode is not consulted.
            | AdvisoryCall::UnlockAll => {
                crate::advisory::Mode::Shared
            }
        }
    }
}
