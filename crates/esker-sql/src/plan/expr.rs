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
/// 0046](../../docs/adr/0046-arithmetic-is-its-own-node-and-postgresql-s-promotion-table.md)).
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
    },
    /// `x = ANY(<array>)` where the array is a **value of the row** rather than a list the
    /// lowering could see — `a.attnum = ANY(i.indkey)`.
    ///
    /// [`Expr::InList`] is the same rule over a list known at plan time, and every array this node
    /// had until now was one: `ARRAY[1,2]`, `'{a,b}'` and `current_schemas(false)` are all expanded
    /// where they are lowered. A **column** cannot be, because its value differs per row — which is
    /// why this is a variant and not a rewrite, and why boot statement 17 was refused by name until
    /// it existed.
    ///
    /// The three-valued rule is [`Expr::InList`]'s, shared rather than copied: a match wins over a
    /// NULL, a NULL wins over no match, and a NULL operand is NULL whatever the array holds.
    AnyArray {
        /// The left-hand side, evaluated once.
        operand: Box<Expr>,
        /// The array, evaluated once per row and read from its own text form
        /// (`crate::value::vector`). A NULL array makes the whole comparison NULL — measured,
        /// `1 = ANY(NULL::int[])` is NULL where `1 = ANY('{}')` is false.
        array: Box<Expr>,
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
    /// `length(text)`, and its two aliases `char_length` and `character_length`: **characters**,
    /// not bytes.
    Length,
    /// `octet_length(text)`: **bytes**, which is a different number for anything non-ASCII — the
    /// pair is only interesting because the suite's generated columns use both.
    OctetLength,
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
            ScalarFunc::OctetLength => "octet_length",
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
            "length" | "char_length" | "character_length" => Some(ScalarFunc::Length),
            "octet_length" => Some(ScalarFunc::OctetLength),
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
    /// `a @> b`: whether every pair of `b` is in `a`.
    HstoreContains,
    /// `a || b`: the two hstores merged, **the right winning a shared key** — which is the
    /// opposite of what a repeated key inside one literal does (`crate::value::hstore`).
    HstoreConcat,
    /// `akeys(h)` and `avals(h)`: the keys and the values as `text[]`, in canonical order.
    HstoreAkeys,
    /// See [`CatalogFunc::HstoreAkeys`].
    HstoreAvals,
    /// `hstore(k, v)` and `hstore(keys[], vals[])`: the two constructors the adapter reaches for.
    HstoreBuild,
    /// `lower_inc(range)`, `upper_inc`, `lower_inf`, `upper_inf`: the four bracket questions.
    ///
    /// `lower`/`upper` are **not** here — they are the text functions of the same name, overloaded
    /// on a range operand, which is how a real server spells them too.
    RangeLowerInc,
    /// See [`CatalogFunc::RangeLowerInc`].
    RangeUpperInc,
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
    /// `'happy'::mood` — a cast to a **user-defined type**, which is a name until the catalog is
    /// read.
    ///
    /// Two arguments: the type's name as a string literal, and the operand. Like
    /// [`CatalogFunc::RegClass`] it is replaced before the plan is built and never reaches the row
    /// evaluator — the catalog answer is the same for every row, and reading it per row is the
    /// cost trap `::regclass` already paid for once
    /// ([ADR 0053](../../docs/adr/0053-a-cast-to-a-user-defined-type-is-resolved-once-per-statement.md)).
    ///
    /// What it is replaced *with* depends on where it sits, and that is what an enum is rather
    /// than a special case: **the label** when it is a projection on its own, so
    /// `SELECT 'happy'::mood` prints `happy`; **the ordinal** everywhere else, so
    /// `'sad'::mood < 'happy'::mood` is `1 < 3` and is `t`.
    UserCast,
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
            () if name.eq_ignore_ascii_case("pg_get_triggerdef") => {
                Some(CatalogFunc::PgGetTriggerdef)
            }
            () if name.eq_ignore_ascii_case("to_regclass") => Some(CatalogFunc::ToRegClass),
            () if name.eq_ignore_ascii_case("pg_typeof") => Some(CatalogFunc::PgTypeof),
            () if name.eq_ignore_ascii_case("current_date") => Some(CatalogFunc::CurrentDate),
            () if name.eq_ignore_ascii_case("random") => Some(CatalogFunc::Random),
            () if name.eq_ignore_ascii_case("concat") => Some(CatalogFunc::Concat),
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
            CatalogFunc::PgGetExpr => "pg_get_expr",
            CatalogFunc::PgGetIndexdef => "pg_get_indexdef",
            CatalogFunc::PgGetConstraintdef => "pg_get_constraintdef",
            CatalogFunc::PgGetViewdef => "pg_get_viewdef",
            CatalogFunc::PgEncodingToChar => "pg_encoding_to_char",
            CatalogFunc::PgGetSerialSequence => "pg_get_serial_sequence",
            CatalogFunc::ColDescription => "col_description",
            CatalogFunc::ObjDescription => "obj_description",
            CatalogFunc::PgGetPartkeydef => "pg_get_partkeydef",
            CatalogFunc::PgGetTriggerdef => "pg_get_triggerdef",
            CatalogFunc::DateRange => "daterange",
            CatalogFunc::IsEmpty => "isempty",
            CatalogFunc::RangeOverlaps => "&&",
            CatalogFunc::RangeLowerInc => "lower_inc",
            CatalogFunc::RangeUpperInc => "upper_inc",
            CatalogFunc::RangeLowerInf => "lower_inf",
            CatalogFunc::RangeUpperInf => "upper_inf",
            CatalogFunc::RangeBuild => "tsrange",
            CatalogFunc::HstoreFetch => "->",
            CatalogFunc::HstoreHasKey => "?",
            // One symbol, two containments — see `exec::cursor`, where the operand decides.
            CatalogFunc::RangeContains | CatalogFunc::HstoreContains => "@>",
            CatalogFunc::HstoreConcat => "||",
            CatalogFunc::HstoreAkeys => "akeys",
            CatalogFunc::HstoreAvals => "avals",
            CatalogFunc::HstoreBuild => "hstore",
            // Two directions of one cast, and PostgreSQL names both of them `regclass`.
            CatalogFunc::RegClass | CatalogFunc::RegClassName => "regclass",
            CatalogFunc::RegTypeName => "regtype",
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
            CatalogFunc::FormatType
            | CatalogFunc::PgGetSerialSequence
            | CatalogFunc::ColDescription
            | CatalogFunc::ArrayPosition
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
            | CatalogFunc::HstoreContains
            | CatalogFunc::HstoreConcat
            | CatalogFunc::HstoreBuild => &[2],
            // `tsrange(a, b)` and `tsrange(a, b, '[]')` — two shapes of one name, and
            // `pg_get_expr`'s two really are two forms as well.
            CatalogFunc::RangeBuild | CatalogFunc::PgGetExpr => &[2, 3],
            CatalogFunc::PgGetIndexdef => &[1, 3],
            CatalogFunc::PgGetConstraintdef
            | CatalogFunc::PgGetViewdef
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
            | CatalogFunc::ToRegClass
            | CatalogFunc::IsEmpty
            | CatalogFunc::Cardinality
            | CatalogFunc::PgTypeof => &[1],
            CatalogFunc::Now
            | CatalogFunc::CurrentDate
            | CatalogFunc::LocalTimestamp
            | CatalogFunc::LocalTime
            | CatalogFunc::StatementTimestamp
            | CatalogFunc::ClockTimestamp
            | CatalogFunc::Random => &[0],
            // Variadic: every arity from one up. `concat()` is the `42883` about the *number* of
            // arguments that a real server raises, so zero is not in the set.
            CatalogFunc::Concat => &CONCAT_ARITIES,
        }
    }

    /// The type of its result.
    #[must_use]
    pub fn result_type(self) -> ColumnType {
        match self {
            CatalogFunc::FormatType
            | CatalogFunc::PgGetExpr
            | CatalogFunc::PgGetIndexdef
            | CatalogFunc::PgGetConstraintdef
            | CatalogFunc::PgGetViewdef
            | CatalogFunc::PgGetTriggerdef
            | CatalogFunc::DateRange
            | CatalogFunc::ColDescription
            | CatalogFunc::PgGetSerialSequence
            | CatalogFunc::PgEncodingToChar
            | CatalogFunc::ObjDescription
            // A `regclass` on a real server is an oid that *prints* as a name; `text` here, which
            // is what it prints as. The one place the difference shows is the declared type.
            | CatalogFunc::PgGetPartkeydef
            | CatalogFunc::RegClassName
            | CatalogFunc::RegTypeName
            | CatalogFunc::ToRegClass
            // `concat` answers `text` for the ordinary reason: it builds a string.
            | CatalogFunc::Concat
            // A `regtype` on a real server, and `text` here for the reason `'x'::regtype` is:
            // this node has no `regtype`, and what it prints is the name either way.
            | CatalogFunc::PgTypeof
            | CatalogFunc::HstoreFetch => ColumnType::Text,
            // An `oid` on a real server, and a `bigint` here for the reason `pg_class.oid` is one.
            CatalogFunc::RegClass => ColumnType::Int8,
            // **The storage, which is what an enum's value is** (ADR 0050) — and the label
            // the projection form is replaced by is a `text` literal by then, so nothing
            // reads this for that shape.
            CatalogFunc::UserCast => ColumnType::Int2,

            // Every one of the five answers `integer` on a real server, including `cardinality`,
            // which counts every element of every dimension where `array_length` counts one.
            CatalogFunc::ArrayPosition
            | CatalogFunc::ArrayLower
            | CatalogFunc::ArrayUpper
            | CatalogFunc::ArrayLength
            | CatalogFunc::Cardinality => ColumnType::Int4,
            // The two range predicates answer a boolean, which is what lets `&&` stand in a
            // `WHERE` without a comparison around it.
            CatalogFunc::IsEmpty
            | CatalogFunc::RangeOverlaps
            | CatalogFunc::RangeContains
            | CatalogFunc::RangeLowerInc
            | CatalogFunc::RangeUpperInc
            | CatalogFunc::RangeLowerInf
            | CatalogFunc::RangeUpperInf
            | CatalogFunc::HstoreHasKey
            | CatalogFunc::HstoreContains => ColumnType::Bool,
            // Measured: `akeys` is `text[]`, and `||` and `hstore(…)` are hstores. `->`'s `text`
            // and `?`/`@>`'s `boolean` are folded into the lists above and below.
            CatalogFunc::HstoreAkeys | CatalogFunc::HstoreAvals => ColumnType::TextArray,
            CatalogFunc::HstoreConcat | CatalogFunc::HstoreBuild => ColumnType::Hstore,
            CatalogFunc::RangeBuild => ColumnType::TsRange,
            // **`LOCALTIMESTAMP` is the one of the four without a zone**, which is the whole
            // reason it is a separate member: the type is what decides whether a column takes it.
            CatalogFunc::Now
            | CatalogFunc::StatementTimestamp
            | CatalogFunc::ClockTimestamp => ColumnType::TimestampTz,
            CatalogFunc::LocalTimestamp => ColumnType::Timestamp,
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
    /// `sum`, over `int8` and `float8`.
    Sum,
    /// `min`, in [`crate::value::PgDatum::pg_cmp`] order.
    Min,
    /// `max`, likewise.
    Max,
    /// `avg`, over `float8` only — `avg(int8)` is `numeric` on a real server and this node has no
    /// `numeric` to be right with (`docs/adr/0031-rails-compatibility-is-measured.md`).
    Avg,
    /// `array_agg(expr [ORDER BY …])`: every value of the group, in one array.
    ///
    /// The one aggregate here that is **not** a fold — it keeps every value rather than combining
    /// them, so it is the one whose memory is the group's size and the one that carries the
    /// `ORDER BY` clause. Over **no rows it is NULL**, not an empty array, which is the answer
    /// that surprises: `array_agg(id) FROM t WHERE false` is NULL and `count(id)` is 0.
    ArrayAgg,
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
    Typed(Box<Datum>),
}

impl Literal {
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
            Literal::Typed(value) => value.column_type().map_or("unknown", ColumnType::name),
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
            Literal::Typed(value) => value.fits(ty),
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
                expression_type: self.type_name(),
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
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
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
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::CitextArray | ColumnType::Point => mismatch(),
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
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
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
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::CitextArray | ColumnType::Point => mismatch(),
            },

            // Already resolved. It fits the column it was resolved against and nothing else.
            Literal::Typed(value) if value.fits(ty) => Ok((**value).clone()),
            // **An `ARRAY[…]`'s element type is settled by the column**, the way an integer
            // literal's is one level down. `ARRAY[1,2,3]` is `integer[]` on a real server and
            // `bigint[]` here — an integer literal is an `int8` in this crate until a column says
            // otherwise, which is what `Literal::Integer` above does — and both servers write the
            // same row into an `integer[]` column. Re-read through the array's own text, which is
            // `array_in` doing the element conversion: `ARRAY[2147483648]` into an `integer[]`
            // column then fails with `int4`'s own `22003` rather than with a type mismatch.
            //
            // A `Literal` is a constant in the statement, never a column reference, so this
            // settles a *literal's* type and does not widen assignment between two columns:
            // `int8[]` into an `integer[]` column is still `42804`, from `exec::assign`.
            Literal::Typed(value)
                if matches!(**value, Datum::Array(_))
                    && esker_keys::array::ArrayValue::element_of(ty).is_some() =>
            {
                match value.to_text() {
                    Some(text) => Datum::from_text(ty, &text),
                    None => mismatch(),
                }
            }
            Literal::Typed(_) => mismatch(),

            Literal::Bool(value) => match ty {
                ColumnType::Bool => Ok(Datum::Bool(*value)),
                // `true`, not `t`: the cast, not the output function.
                ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => Ok(Datum::Text(
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
                // A number or a boolean is not a citext literal either.
                | ColumnType::Citext
                // A number or a boolean is not a range literal either.
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
                | ColumnType::FloatRange | ColumnType::VarcharRange | ColumnType::MoneyArray
                | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray | ColumnType::BoolArray | ColumnType::ByteaArray | ColumnType::BpcharArray | ColumnType::VarcharArray | ColumnType::DateArray | ColumnType::TimeArray | ColumnType::TimestampArray | ColumnType::TimestampTzArray | ColumnType::IntervalArray | ColumnType::RealArray | ColumnType::DoubleArray | ColumnType::UuidArray | ColumnType::JsonArray | ColumnType::JsonbArray | ColumnType::OidArray | ColumnType::CitextArray | ColumnType::Point => mismatch(),
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
        Expr::AnyArray { .. } => "= ANY",
        Expr::Subscript { .. } => "a subscript",
        Expr::Uuid(func) => func.name(),
        Expr::CurrentSetting { .. } => "current_setting",
        Expr::CurrentSchema { all: None } => "current_schema",
        Expr::CurrentSchema { .. } => "current_schemas",
        Expr::Advisory { call, .. } => call.name(),
        Expr::CurrentDatabase => "current_database",
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
/// The **blocking** forms (`pg_advisory_lock`, `pg_advisory_lock_shared` and the `xact` family)
/// are deliberately not here: they wait, and nothing in this node has anything to wait on — a
/// `pg_try_advisory_lock` that cannot take the lock answers `false` instead. `ActiveRecord` sends
/// only the two `try`/`unlock` shapes (`postgresql_adapter.rb:474`), so the blocking ones are
/// refused by name in `crate::parse` rather than approximated by a spin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryCall {
    /// `pg_try_advisory_lock(bigint)` / `(int4, int4)`.
    TryLock,
    /// `pg_try_advisory_lock_shared(bigint)` / `(int4, int4)`.
    TryLockShared,
    /// `pg_advisory_unlock(bigint)` / `(int4, int4)`.
    Unlock,
    /// `pg_advisory_unlock_shared(bigint)` / `(int4, int4)`.
    UnlockShared,
}

impl AdvisoryCall {
    /// The name as written, for a message and for `EXPLAIN`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AdvisoryCall::TryLock => "pg_try_advisory_lock",
            AdvisoryCall::TryLockShared => "pg_try_advisory_lock_shared",
            AdvisoryCall::Unlock => "pg_advisory_unlock",
            AdvisoryCall::UnlockShared => "pg_advisory_unlock_shared",
        }
    }

    /// Whether this one takes a lock (rather than releasing one).
    #[must_use]
    pub fn takes(self) -> bool {
        matches!(self, AdvisoryCall::TryLock | AdvisoryCall::TryLockShared)
    }

    /// The mode it works in.
    #[must_use]
    pub fn mode(self) -> crate::advisory::Mode {
        match self {
            AdvisoryCall::TryLock | AdvisoryCall::Unlock => crate::advisory::Mode::Exclusive,
            AdvisoryCall::TryLockShared | AdvisoryCall::UnlockShared => {
                crate::advisory::Mode::Shared
            }
        }
    }
}
